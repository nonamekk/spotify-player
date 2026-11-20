//! The application's session: the open pages and the playing list, kept across restarts.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{
    load_data_from_file_cache, store_data_into_file_cache, AppData, FileCacheKey, PageState,
    PlayerState, PlayingTracks, SharedState, TracksId, UIState,
};
use crate::config;

const SAVE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    history: Vec<PageState>,
}

impl Session {
    fn capture(ui: &UIState) -> Self {
        Self {
            history: ui.history.clone(),
        }
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

    let playing: Option<PlayingTracks> =
        load_data_from_file_cache(FileCacheKey::PlayingTracks, cache_folder).flatten();
    if let Some(playing) = playing {
        player.currently_playing_tracks_id = Some(playing.id.clone());
        data.restore_playing_tracks(playing);
    }
}

/// Store the session of the running application
pub fn store(ui: &UIState) {
    store_session(&Session::capture(ui));
}

fn store_session(session: &Session) {
    let cache_folder = &config::get_config().cache_folder;
    if let Err(err) = store_data_into_file_cache(FileCacheKey::Session, cache_folder, session) {
        tracing::error!("Failed to store the session: {err:#}");
    }
}

/// Store the Tracks context being played, kept in memory for as long as it plays
fn store_playing_tracks(state: &SharedState, id: Option<&TracksId>) {
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
        let session = Session::capture(&ui);
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
    use super::Session;
    use crate::state::{
        ContextId, ContextPageType, ContextPageUIState, LibraryPageUIState, PageState, TracksId,
    };

    const SEED: &str = "4uLU6hMCjMI75M1A2tKUQC";

    #[test]
    fn history_survives_storing() {
        let radio_id = ContextId::Tracks(TracksId::new(
            format!("radio:spotify:track:{SEED}"),
            "Track Radio",
        ));
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
        };

        let stored = serde_json::to_string(&session).unwrap();
        let restored: Session = serde_json::from_str(&stored).unwrap();

        assert_eq!(restored, session);
    }
}
