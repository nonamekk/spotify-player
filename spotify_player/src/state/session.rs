//! The application's session: the open pages and the playback, kept across restarts.

use std::path::Path;
use std::time::Duration;

use rspotify::model::{EpisodeId, Id, Offset, PlayableId, TrackId};
use serde::{Deserialize, Serialize};

use super::{
    load_data_from_file_cache, store_data_into_file_cache, AppData, ContextId, FileCacheKey,
    PageState, Playback, PlayerState, PlayingTracks, SharedState, TracksId, UIState,
};
use crate::config;

const SAVE_INTERVAL: Duration = Duration::from_secs(1);
/// Stored playback positions are rounded down to this step, so a playing track
/// doesn't make the session change every second.
const PROGRESS_STEP_SECS: i64 = 10;

#[derive(Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    history: Vec<PageState>,
    playback: Option<SavedPlayback>,
}

/// The playback of a previous session
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedPlayback {
    context: Option<rspotify::model::Context>,
    uri: String,
    progress_secs: i64,
}

impl Session {
    fn capture(ui: &UIState, player: &PlayerState) -> Self {
        Self {
            history: ui.history.clone(),
            playback: SavedPlayback::capture(player),
        }
    }
}

impl SavedPlayback {
    fn capture(player: &PlayerState) -> Option<Self> {
        let playback = player.current_playback()?;
        let uri = playback.item.as_ref()?.id()?.uri();
        let progress_secs =
            playback.progress?.num_seconds() / PROGRESS_STEP_SECS * PROGRESS_STEP_SECS;
        Some(Self {
            context: playback.context,
            uri,
            progress_secs,
        })
    }

    pub fn progress(&self) -> chrono::Duration {
        chrono::Duration::seconds(self.progress_secs)
    }

    /// The playback that starts this one again, with `tracks_id` as its context when Spotify reported none.
    /// `limit` is the `tracks_playback_limit` of a list of tracks.
    pub fn playback(
        &self,
        data: &AppData,
        tracks_id: Option<&TracksId>,
        limit: usize,
    ) -> Option<Playback> {
        if let Some(id) = self.context.as_ref().and_then(ContextId::from_context) {
            return Some(Playback::Context(id, Some(Offset::Uri(self.uri.clone()))));
        }

        let tracks = tracks_id.and_then(|id| data.context_tracks(&ContextId::Tracks(id.clone())));
        let ids = match tracks {
            Some(tracks) => tracks.iter().map(|t| t.id.clone().into()).collect(),
            None => vec![playable_id(&self.uri)?],
        };
        Some(Playback::URIs(ids, None).uri_offset(self.uri.clone(), limit))
    }
}

fn playable_id(uri: &str) -> Option<PlayableId<'static>> {
    match TrackId::from_uri(uri) {
        Ok(id) => Some(id.into_static().into()),
        Err(_) => Some(EpisodeId::from_uri(uri).ok()?.into_static().into()),
    }
}

/// Restore the previous session into the state of a starting application
pub fn restore(
    ui: &mut UIState,
    player: &mut PlayerState,
    data: &mut AppData,
    cache_folder: &Path,
) {
    let session: Session =
        load_data_from_file_cache(FileCacheKey::Session, cache_folder).unwrap_or_default();
    if !session.history.is_empty() {
        ui.history = session.history;
    }
    player.saved_playback = session.playback;

    let playing: Option<PlayingTracks> =
        load_data_from_file_cache(FileCacheKey::PlayingTracks, cache_folder).flatten();
    if let Some(playing) = playing {
        player.currently_playing_tracks_id = Some(playing.id.clone());
        data.restore_playing_tracks(playing);
    }
}

/// Store the session of the running application
pub fn store(ui: &UIState, state: &SharedState) {
    let session = Session::capture(ui, &state.player.read());
    store_session(&session);
}

fn store_session(session: &Session) {
    let cache_folder = &config::get_config().cache_folder;
    if let Err(err) = store_data_into_file_cache(FileCacheKey::Session, cache_folder, session) {
        tracing::error!("Failed to store the session: {err:#}");
    }
}

/// Store the Tracks context being played, kept in memory for as long as it plays
pub fn store_playing_tracks(state: &SharedState, id: Option<&TracksId>) {
    let playing = id.and_then(|id| {
        let mut data = state.data.write();
        data.caches.keep_playing_context(&id.uri);
        data.playing_tracks(id)
    });
    let cache_folder = &config::get_config().cache_folder;
    if let Err(err) =
        store_data_into_file_cache(FileCacheKey::PlayingTracks, cache_folder, &playing)
    {
        tracing::error!("Failed to store the playing tracks: {err:#}");
    }
    // the radio's page then shows the list played from, even once the playing list is forgotten
    if let Some(radio) = playing.filter(|p| p.id.uri.starts_with("radio:")) {
        if let Err(err) = super::radios::store(cache_folder, &radio.id.uri, &radio.tracks) {
            tracing::error!("Failed to store the playing radio: {err:#}");
        }
    }
}

/// Store the session whenever it changes, so it survives the application being killed
pub fn start_session_saver(state: &SharedState) {
    let mut stored = Session::default();
    let mut stored_tracks_id = state.player.read().currently_playing_tracks_id.clone();

    loop {
        std::thread::sleep(SAVE_INTERVAL);

        // the UI stays locked until the session is written: the store on quit needs that lock,
        // so it always writes after this one and an older session can't replace the last one
        let ui = state.ui.lock();
        let session = Session::capture(&ui, &state.player.read());
        if session != stored {
            store_session(&session);
            stored = session;
        }
        drop(ui);

        let tracks_id = state.player.read().currently_playing_tracks_id.clone();
        if tracks_id != stored_tracks_id {
            store_playing_tracks(state, tracks_id.as_ref());
            stored_tracks_id = tracks_id;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use rspotify::model::{Id, Offset, TrackId, Type};

    use super::{SavedPlayback, Session};
    use crate::state::{
        store_data_into_file_cache, AppData, ContextId, ContextPageType, ContextPageUIState,
        FileCacheKey, LibraryPageUIState, PageState, Playback, PlayingTracks, Track, TracksId,
    };

    const SEED: &str = "4uLU6hMCjMI75M1A2tKUQC";
    const NEXT: &str = "3n3Ppam7vgaVa1iaRUc9Lp";

    fn track(id: &'static str) -> Track {
        Track {
            id: TrackId::from_id(id).unwrap().into_static(),
            name: id.to_string(),
            artists: vec![],
            album: None,
            duration: std::time::Duration::default(),
            explicit: false,
            added_at: 0,
        }
    }

    fn radio() -> PlayingTracks {
        PlayingTracks {
            id: TracksId::new(format!("radio:spotify:track:{SEED}"), "Track Radio"),
            desc: "Track Radio".to_string(),
            tracks: vec![track(SEED), track(NEXT)],
        }
    }

    fn saved_playback(context: Option<rspotify::model::Context>) -> SavedPlayback {
        SavedPlayback {
            context,
            uri: track(NEXT).id.uri(),
            progress_secs: 40,
        }
    }

    #[test]
    fn history_survives_storing() {
        let radio_id = ContextId::Tracks(radio().id);
        let mut library = PageState::Library {
            state: LibraryPageUIState::new(),
        };
        library.select(3);
        let mut radio_page = PageState::Context {
            id: Some(radio_id.clone()),
            context_page_type: ContextPageType::Browsing(radio_id),
            state: Some(ContextPageUIState::new_tracks()),
        };
        radio_page.select(1);
        let session = Session {
            history: vec![library, radio_page],
            playback: Some(saved_playback(None)),
        };

        let stored = serde_json::to_string(&session).unwrap();
        let restored: Session = serde_json::from_str(&stored).unwrap();

        assert_eq!(restored, session);
    }

    #[test]
    fn radio_resumes_as_its_tracks_at_the_saved_track() {
        let mut data = AppData::new(Path::new("/nonexistent"));
        let radio = radio();
        let radio_id = radio.id.clone();
        data.restore_playing_tracks(radio);
        let saved = saved_playback(None);

        let Some(Playback::URIs(ids, Some(Offset::Uri(uri)))) =
            saved.playback(&data, Some(&radio_id), 50)
        else {
            panic!("expected the radio's tracks");
        };

        assert_eq!(ids.len(), 2);
        assert_eq!(uri, saved.uri);
    }

    #[test]
    fn context_resumes_as_the_context() {
        let data = AppData::new(Path::new("/nonexistent"));
        let saved = saved_playback(Some(rspotify::model::Context {
            uri: "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".to_string(),
            href: String::new(),
            external_urls: HashMap::new(),
            _type: Type::Playlist,
        }));

        assert!(matches!(
            saved.playback(&data, Some(&radio().id), 50),
            Some(Playback::Context(
                ContextId::Playlist(_),
                Some(Offset::Uri(_))
            ))
        ));
    }

    /// A playback of the track `id` on Soloist
    fn playback_of(id: &str) -> rspotify::model::CurrentPlaybackContext {
        let mut playback: rspotify::model::CurrentPlaybackContext =
            serde_json::from_value(serde_json::json!({
                "device": {"id": "box", "is_active": true, "is_private_session": false,
                           "is_restricted": false, "name": "box-spot", "type": "Computer",
                           "volume_percent": 50},
                "repeat_state": "off", "shuffle_state": false, "context": null, "timestamp": 0,
                "progress_ms": 0, "is_playing": true, "item": null,
                "currently_playing_type": "track", "actions": {"disallows": {}},
            }))
            .unwrap();
        let record = soloist_history::Record {
            id: id.to_owned(),
            name: id.to_owned(),
            artists: vec![("4yAfjEDlkmMb7uLcN3Ot9K".to_owned(), "Artist".to_owned())],
            album_id: "4LXJ5XxclRopTjLBXpyptg".to_owned(),
            album: "Album".to_owned(),
            ..soloist_history::Record::default()
        };
        playback.item = crate::state::played_history::full_track(&record)
            .map(rspotify::model::PlayableItem::Track);
        playback
    }

    #[test]
    fn a_resumed_radio_page_keeps_its_list_and_selects_the_playing_track() {
        let dir =
            std::env::temp_dir().join(format!("spotify-player-resume-{}", std::process::id()));
        let (config_dir, cache_dir) = (dir.join("config"), dir.join("cache"));
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();

        // the previous session ended on the playing radio's page
        let radio = radio();
        let radio_id = ContextId::Tracks(radio.id.clone());
        let page = PageState::Context {
            id: Some(radio_id.clone()),
            context_page_type: ContextPageType::Browsing(radio_id.clone()),
            state: Some(ContextPageUIState::new_tracks()),
        };
        let session = Session {
            history: vec![page],
            playback: None,
        };
        store_data_into_file_cache(FileCacheKey::Session, &cache_dir, &session).unwrap();
        store_data_into_file_cache(FileCacheKey::PlayingTracks, &cache_dir, &Some(radio)).unwrap();

        crate::config::set_config(crate::config::Configs::new(&config_dir, &cache_dir).unwrap());
        let state = crate::state::State::new(false, std::sync::Arc::default());

        // the page lists the radio as it was played, not a new one
        let names: Vec<_> = state
            .data
            .read()
            .context_tracks(&radio_id)
            .unwrap()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert_eq!(names, [SEED, NEXT]);

        // the page comes back selecting the first track, while the device has moved on to NEXT
        state.ui.lock().current_page_mut().select(0);
        state.player.write().playback = Some(playback_of(NEXT));
        state.select_playing_track();
        assert_eq!(state.ui.lock().current_page_mut().selected(), Some(1));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
