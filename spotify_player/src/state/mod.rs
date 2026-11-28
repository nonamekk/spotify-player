mod constant;
mod data;
pub mod logins;
mod model;
pub mod other_devices;
pub mod played_history;
mod player;
mod queue;
pub mod radios;
pub mod session;
mod ui;

use std::{collections::VecDeque, sync::Arc};

pub use constant::*;
pub use data::*;
pub use model::*;
pub use player::*;
#[allow(unused_imports)]
pub use queue::*;
pub use ui::*;

use rand::RngExt as _;

use crate::config;

pub use parking_lot::{Mutex, RwLock};

/// Items of its queue Spotify lists at most, so a full list may not be all of it
const QUEUE_LISTED: usize = 20;

/// What keeps the playing list going once Spotify has none of its tracks left to play
#[derive(Debug)]
pub enum ListContinuation {
    /// Queue this track of the list
    Queue(Track),
    /// The list ends with the playing track
    End(Track),
}

/// Application's shared state
pub type SharedState = Arc<State>;

/// Application's state
pub struct State {
    pub ui: Mutex<UIState>,
    pub player: RwLock<PlayerState>,
    pub data: RwLock<AppData>,

    pub is_daemon: bool,

    /// Shared FFT frequency-band data written by the audio sink and read by the UI.
    /// `Some` only when `enable_audio_visualization` is `true`; avoids allocating
    /// the mutex/state entirely when the feature is not in use.
    #[cfg(feature = "streaming")]
    pub vis_bands: Option<Arc<Mutex<crate::ui::streaming::VisBands>>>,

    pub logs: Arc<Mutex<VecDeque<String>>>,
}

impl State {
    pub fn new(is_daemon: bool, log_buffer: Arc<Mutex<VecDeque<String>>>) -> Self {
        let mut ui = UIState::default();
        let configs = config::get_config();

        if let Some(theme) = configs.theme_config.find_theme(&configs.app_config.theme) {
            // update the UI's theme based on the `theme` config option
            ui.theme = theme;
        }

        let mut player = PlayerState::default();
        let mut app_data = AppData::new(&configs.cache_folder);
        session::restore(&mut ui, &mut player, &mut app_data, &configs.cache_folder);

        Self {
            ui: Mutex::new(ui),
            player: RwLock::new(player),
            data: RwLock::new(app_data),
            is_daemon,
            #[cfg(feature = "streaming")]
            vis_bands: if configs.app_config.enable_audio_visualization {
                Some(Arc::new(Mutex::new(
                    crate::ui::streaming::VisBands::default(),
                )))
            } else {
                None
            },

            logs: log_buffer,
        }
    }

    /// The track a Previous goes back to: the one before the current track in its known,
    /// unshuffled context, as this device last played it
    pub fn previous_track(&self) -> Option<rspotify::model::PlayableItem> {
        let (context_id, current) = {
            let player = self.player.read();
            if player.current_playback()?.shuffle_state {
                return None;
            }
            let current = player.currently_playing()?.id()?.uri();
            (player.playing_context_id()?, current)
        };
        let previous = {
            let data = self.data.read();
            let tracks = data.context_tracks(&context_id)?;
            let i = tracks.iter().position(|t| t.id.uri() == current)?;
            tracks.get(i.checked_sub(1)?)?.clone()
        };
        let records = played_history::records(&config::get_config().cache_folder);
        played_history::track_to_show(&records, &previous.id, Some(&previous))
            .map(rspotify::model::PlayableItem::Track)
    }

    /// The track a playback starts, to show before Spotify reports it: a queued one as Spotify
    /// has it, else as the played history and the list it starts from have it
    pub fn started_track(&self, playback: &Playback) -> Option<rspotify::model::PlayableItem> {
        use rspotify::model::Offset;
        let (context_id, uri) = match playback {
            Playback::Context(id, Some(Offset::Uri(uri))) => (Some(id.clone()), uri.clone()),
            Playback::URIs(ids, offset) => {
                let uri = match offset {
                    Some(Offset::Uri(uri)) => uri.clone(),
                    None => ids.first()?.uri(),
                    Some(_) => return None,
                };
                let tracks_id = self.player.read().currently_playing_tracks_id.clone();
                (tracks_id.map(ContextId::Tracks), uri)
            }
            Playback::Context(..) => return None,
        };

        let queued = self.player.read().queue.as_ref().and_then(|queue| {
            queue
                .queue
                .iter()
                .find(|item| item.id().is_some_and(|id| id.uri() == uri))
                .cloned()
        });
        if queued.is_some() {
            return queued;
        }
        let id = TrackId::from_uri(&uri).ok()?.into_static();
        let track = context_id.and_then(|context_id| {
            let data = self.data.read();
            data.context_tracks(&context_id)?
                .iter()
                .find(|t| t.id == id)
                .cloned()
        });
        let records = played_history::records(&config::get_config().cache_folder);
        played_history::track_to_show(&records, &id, track.as_ref())
            .map(rspotify::model::PlayableItem::Track)
    }

    /// Up to `count` tracks before the playing one in its context, the nearest first, as they
    /// would show when played
    pub fn tracks_before_current(&self, count: usize) -> Vec<rspotify::model::PlayableItem> {
        let Some((context_id, current)) = ({
            let player = self.player.read();
            let current = player
                .currently_playing()
                .and_then(rspotify::model::PlayableItem::id);
            player.playing_context_id().zip(current.map(|id| id.uri()))
        }) else {
            return Vec::new();
        };
        let before: Vec<Track> = {
            let data = self.data.read();
            let Some(tracks) = data.context_tracks(&context_id) else {
                return Vec::new();
            };
            let i = tracks
                .iter()
                .position(|t| t.id.uri() == current)
                .unwrap_or(0);
            tracks[..i].iter().rev().take(count).cloned().collect()
        };

        let records = played_history::records(&config::get_config().cache_folder);
        before
            .iter()
            .filter_map(|t| played_history::track_to_show(&records, &t.id, Some(t)))
            .map(rspotify::model::PlayableItem::Track)
            .collect()
    }

    /// Select the playing track in the page shown, if it lists it: a resumed page comes back with
    /// the selection it had, while the device may have played on since
    pub fn select_playing_track(&self) {
        let Some(uri) = self
            .player
            .read()
            .currently_playing()
            .and_then(rspotify::model::PlayableItem::id)
            .map(|id| id.uri())
        else {
            return;
        };
        self.select_track(&uri);
    }

    /// Select the track `uri` on the current page, if it lists it
    pub fn select_track(&self, uri: &str) {
        let mut ui = self.ui.lock();
        let PageState::Context {
            id: Some(context_id),
            ..
        } = ui.current_page()
        else {
            return;
        };
        let position = self
            .data
            .read()
            .context_tracks(context_id)
            .and_then(|tracks| tracks.iter().position(|t| t.id.uri() == uri));
        if let Some(i) = position {
            ui.current_page_mut().select(i);
        }
    }

    /// Forget the playing Tracks context once Spotify plays something outside of it with none of
    /// its tracks queued after: a track queued in between keeps it. Decided once Spotify's queue is
    /// known for the playing track.
    pub fn forget_stale_playing_tracks(&self) {
        let (id, uri, queued) = {
            let player = self.player.read();
            let item_uri = player
                .currently_playing()
                .and_then(rspotify::model::PlayableItem::id)
                .map(|id| id.uri());
            let (Some(id), Some(uri), Some(queue)) = (
                player.currently_playing_tracks_id.clone(),
                item_uri,
                player.queue_after_current(),
            ) else {
                return;
            };
            (id, uri, queued_track_ids(queue))
        };

        let is_stale = self
            .data
            .read()
            .context_tracks(&ContextId::Tracks(id))
            .is_some_and(|tracks| {
                tracks.iter().all(|t| t.id.uri() != uri)
                    && !tracks.iter().any(|t| queued.contains(&t.id))
            });
        if is_stale {
            self.player.write().currently_playing_tracks_id = None;
        }
    }

    /// The step that keeps the playing list going when Spotify's queue holds none of its tracks
    /// after the playing one, which Spotify only has a window of (`tracks_playback_limit`). Taken
    /// once per playing track; `None` while Spotify has more of the list, or can't tell.
    pub fn continue_playing_list(&self) -> Option<ListContinuation> {
        let (id, current, queued, shuffle) = {
            let player = self.player.read();
            let playback = player.current_playback()?;
            // a list plays as a list of tracks, without a context; with repeat, Spotify repeats them
            if playback.context.is_some()
                || playback.repeat_state != rspotify::model::RepeatState::Off
            {
                return None;
            }
            let rspotify::model::PlayableItem::Track(track) = player.currently_playing()? else {
                return None;
            };
            let current = track.id.clone()?;
            let queue = player.queue_after_current()?;
            if queue.len() >= QUEUE_LISTED || player.continued_after.as_ref() == Some(&current) {
                return None;
            }
            let id = player.currently_playing_tracks_id.clone()?;
            (id, current, queued_track_ids(queue), playback.shuffle_state)
        };

        let step = {
            let data = self.data.read();
            let tracks = data.context_tracks(&ContextId::Tracks(id))?;
            let i = tracks.iter().position(|t| t.id == current)?;
            if tracks.iter().any(|t| queued.contains(&t.id)) {
                return None;
            }
            let next = if shuffle && tracks.len() > 1 {
                // any other track of the list, as Spotify shuffles the whole of a context
                let j = rand::rng().random_range(0..tracks.len() - 1);
                Some(j + usize::from(j >= i))
            } else {
                Some(i + 1).filter(|&next| next < tracks.len())
            };
            match next {
                Some(next) => ListContinuation::Queue(tracks[next].clone()),
                None => ListContinuation::End(tracks[i].clone()),
            }
        };

        // queue checks run side by side: only one of them takes the step
        let mut player = self.player.write();
        if player.continued_after.as_ref() == Some(&current) {
            return None;
        }
        player.continued_after = Some(current);
        Some(step)
    }

    /// Append the tracks of `more` the playing list `id` doesn't have yet. Returns the first one
    /// appended.
    pub fn extend_playing_list(&self, id: &TracksId, more: Vec<Track>) -> Option<Track> {
        let mut data = self.data.write();
        let tracks = data.context_tracks_mut(&ContextId::Tracks(id.clone()))?;
        let mut known: std::collections::HashSet<_> = tracks.iter().map(|t| t.id.clone()).collect();
        let start = tracks.len();
        tracks.extend(more.into_iter().filter(|t| known.insert(t.id.clone())));
        tracks.get(start).cloned()
    }

    /// Let the playing track's continuation be taken again, after it failed
    pub fn release_list_continuation(&self) {
        self.player.write().continued_after = None;
    }

    #[cfg(feature = "streaming")]
    pub fn is_streaming_enabled(&self) -> bool {
        let configs = config::get_config();
        configs.app_config.enable_streaming == config::StreamingType::Always
            || (configs.app_config.enable_streaming == config::StreamingType::DaemonOnly
                && self.is_daemon)
    }

    /// Returns `true` when the custom queue system should be used for new playback.
    ///
    /// Requires streaming to be enabled and the `custom_queue` config option
    /// to be `true`.
    #[cfg(feature = "streaming")]
    #[allow(dead_code)]
    pub fn should_use_custom_queue(&self) -> bool {
        self.is_streaming_enabled() && config::get_config().app_config.custom_queue
    }

    /// Returns `true` when the local librespot player is actively streaming
    /// audio (i.e. a `Playing` event has been received and no `Paused` / `stop`
    /// has occurred since).  Used by the UI to decide whether to allocate and
    /// render the audio-visualization area.
    #[cfg(feature = "streaming")]
    pub fn is_local_streaming_active(&self) -> bool {
        self.vis_bands.as_ref().is_some_and(|b| b.lock().is_active)
    }
}

/// The tracks among queued items
fn queued_track_ids(
    queue: &[rspotify::model::PlayableItem],
) -> std::collections::HashSet<TrackId<'static>> {
    queue
        .iter()
        .filter_map(|item| match item {
            rspotify::model::PlayableItem::Track(track) => track.id.clone(),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rspotify::model::{CurrentUserQueue, PlayableItem, TrackId};
    use serde_json::json;
    use soloist_history::Record;

    use super::{
        played_history::full_track, AppData, ListContinuation, PlayerState, PlayingTracks, State,
        Track,
    };
    use crate::state::{TracksId, UIState};

    fn id(name: &str) -> TrackId<'static> {
        TrackId::from_id(name.repeat(22)).unwrap().into_static()
    }

    /// The track `name` as Spotify reports it
    fn item(name: &str) -> PlayableItem {
        let record = Record {
            id: name.repeat(22),
            name: name.to_owned(),
            artists: vec![("R".repeat(22), "Artist".to_owned())],
            album_id: "L".repeat(22),
            album: "Album".to_owned(),
            duration_ms: 200_000,
            ..Record::default()
        };
        PlayableItem::Track(full_track(&record).unwrap())
    }

    /// The track `name` as a list has it
    fn list_track(name: &str) -> Track {
        Track {
            id: id(name),
            name: name.to_owned(),
            artists: vec![],
            album: None,
            duration: std::time::Duration::from_secs(200),
            explicit: false,
            added_at: 0,
        }
    }

    fn radio_id() -> TracksId {
        TracksId::new(format!("radio:spotify:track:{}", "A".repeat(22)), "Radio")
    }

    /// The list `list` playing from the app, Spotify playing `playing` with `queued` after it
    /// (`None`: Spotify's queue is still the one of the track before)
    fn state(list: &[&str], playing: &str, queued: Option<&[&str]>) -> State {
        let mut playback: rspotify::model::CurrentPlaybackContext = serde_json::from_value(json!({
            "device": {"id": "box", "is_active": true, "is_private_session": false, "is_restricted": false,
                       "name": "box-spot", "type": "Computer", "volume_percent": 50},
            "repeat_state": "off", "shuffle_state": false, "context": null, "timestamp": 0,
            "progress_ms": 0, "is_playing": true, "item": null,
            "currently_playing_type": "track", "actions": {"disallows": {}},
        }))
        .unwrap();
        playback.item = Some(item(playing));
        let queue = CurrentUserQueue {
            currently_playing: Some(item(if queued.is_some() { playing } else { "Z" })),
            queue: queued
                .unwrap_or_default()
                .iter()
                .map(|name| item(name))
                .collect(),
        };
        let player = PlayerState {
            playback: Some(playback),
            playback_last_updated_time: Some(std::time::Instant::now()),
            queue: Some(queue),
            currently_playing_tracks_id: Some(radio_id()),
            ..PlayerState::default()
        };

        let mut data = AppData::new(Path::new("/nonexistent"));
        let tracks = list.iter().map(|name| list_track(name)).collect();
        data.restore_playing_tracks(PlayingTracks {
            id: radio_id(),
            desc: "Radio".to_owned(),
            tracks,
        });

        State {
            ui: UIState::default().into(),
            player: player.into(),
            data: data.into(),
            is_daemon: false,
            #[cfg(feature = "streaming")]
            vis_bands: None,
            logs: std::sync::Arc::default(),
        }
    }

    fn keeps_list(state: &State) -> bool {
        state.player.read().currently_playing_tracks_id.is_some()
    }

    #[test]
    fn a_track_queued_in_the_middle_of_the_list_keeps_it() {
        let state = state(&["A", "B", "C"], "X", Some(&["B", "C"]));
        state.forget_stale_playing_tracks();
        assert!(keeps_list(&state));
    }

    #[test]
    fn a_track_outside_the_list_with_none_of_it_queued_ends_it() {
        let state = state(&["A", "B", "C"], "X", Some(&["Y"]));
        state.forget_stale_playing_tracks();
        assert!(!keeps_list(&state));
    }

    #[test]
    fn the_list_stays_until_the_queue_of_the_playing_track_is_known() {
        let state = state(&["A", "B", "C"], "X", None);
        state.forget_stale_playing_tracks();
        assert!(keeps_list(&state));
    }

    fn queued_next(state: &State) -> Option<String> {
        match state.continue_playing_list() {
            Some(ListContinuation::Queue(track)) => Some(track.name),
            Some(ListContinuation::End(last)) => Some(format!("end after {}", last.name)),
            None => None,
        }
    }

    #[test]
    fn spotify_playing_on_in_the_list_needs_nothing() {
        let state = state(&["A", "B", "C", "D"], "B", Some(&["C", "D"]));
        assert_eq!(queued_next(&state), None);
    }

    #[test]
    fn the_end_of_what_spotify_has_queues_the_next_list_track_once() {
        let state = state(&["A", "B", "C", "D"], "B", Some(&[]));
        assert_eq!(queued_next(&state).as_deref(), Some("C"));
        // another queue check for the same track takes no step
        assert_eq!(queued_next(&state), None);
        // unless the first one failed
        state.release_list_continuation();
        assert_eq!(queued_next(&state).as_deref(), Some("C"));
    }

    #[test]
    fn tracks_queued_by_hand_outside_the_list_still_get_the_list_queued_after() {
        let state = state(&["A", "B", "C", "D"], "B", Some(&["X", "Y"]));
        assert_eq!(queued_next(&state).as_deref(), Some("C"));
    }

    #[test]
    fn the_last_track_of_the_list_ends_it() {
        let state = state(&["A", "B", "C", "D"], "D", Some(&[]));
        assert_eq!(queued_next(&state).as_deref(), Some("end after D"));
    }

    #[test]
    fn nothing_is_queued_without_the_queue_of_the_playing_track() {
        let state = state(&["A", "B", "C", "D"], "B", None);
        assert_eq!(queued_next(&state), None);
    }

    #[test]
    fn a_full_queue_listing_may_hide_the_list_after_it() {
        let queued: Vec<&str> = ["X"; super::QUEUE_LISTED].into();
        let state = state(&["A", "B", "C", "D"], "B", Some(&queued));
        assert_eq!(queued_next(&state), None);
    }

    #[test]
    fn a_shuffled_list_goes_on_with_another_of_its_tracks() {
        let state = state(&["A", "B", "C", "D"], "D", Some(&[]));
        state
            .player
            .write()
            .playback
            .as_mut()
            .unwrap()
            .shuffle_state = true;
        let next = queued_next(&state).unwrap();
        assert!(["A", "B", "C"].contains(&next.as_str()), "{next}");
    }

    #[test]
    fn a_repeated_playback_is_left_to_spotify() {
        let state = state(&["A", "B", "C", "D"], "B", Some(&[]));
        state.player.write().playback.as_mut().unwrap().repeat_state =
            rspotify::model::RepeatState::Context;
        assert_eq!(queued_next(&state), None);
    }

    #[test]
    fn a_list_track_playing_in_a_context_is_left_to_spotify() {
        let state = state(&["A", "B", "C", "D"], "B", Some(&["X"]));
        state.player.write().playback.as_mut().unwrap().context = Some(rspotify::model::Context {
            uri: "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".to_owned(),
            href: String::new(),
            external_urls: std::collections::HashMap::new(),
            _type: rspotify::model::Type::Playlist,
        });
        assert_eq!(queued_next(&state), None);
    }

    #[test]
    fn a_radio_grows_by_the_tracks_it_doesnt_have() {
        let state = state(&["A", "B"], "B", Some(&[]));
        let more = ["B", "C", "C", "D"]
            .iter()
            .map(|name| list_track(name))
            .collect();
        let first = state.extend_playing_list(&radio_id(), more);
        assert_eq!(first.map(|t| t.name).as_deref(), Some("C"));
        let names: Vec<_> = state
            .data
            .read()
            .context_tracks(&crate::state::ContextId::Tracks(radio_id()))
            .unwrap()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert_eq!(names, ["A", "B", "C", "D"]);
        // the step after the playing track is the first new one
        assert_eq!(queued_next(&state).as_deref(), Some("C"));
    }

    #[test]
    fn a_radio_with_nothing_new_doesnt_grow() {
        let state = state(&["A", "B"], "B", Some(&[]));
        let more = vec![list_track("A")];
        assert!(state.extend_playing_list(&radio_id(), more).is_none());
    }
}
