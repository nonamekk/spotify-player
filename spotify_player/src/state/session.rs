//! The application's session: the open pages, kept across restarts.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{
    load_data_from_file_cache, store_data_into_file_cache, FileCacheKey, PageState, SharedState,
    UIState,
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
pub fn restore(ui: &mut UIState, cache_folder: &Path) {
    let session: Session =
        load_data_from_file_cache(FileCacheKey::Session, cache_folder).unwrap_or_default();
    if !session.history.is_empty() {
        ui.history = session.history;
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

/// Store the session whenever it changes, so it survives the application being killed
pub fn start_session_saver(state: &SharedState) {
    let mut stored = Session::default();

    loop {
        std::thread::sleep(SAVE_INTERVAL);

        let session = Session::capture(&state.ui.lock());
        if session != stored {
            store_session(&session);
            stored = session;
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
