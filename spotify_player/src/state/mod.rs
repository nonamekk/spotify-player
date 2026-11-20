mod constant;
mod data;
mod model;
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

use crate::config;

pub use parking_lot::{Mutex, RwLock};

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

    /// Forget the playing Tracks context once Spotify plays something outside of it
    pub fn forget_stale_playing_tracks(&self) {
        let (id, uri) = {
            let player = self.player.read();
            let item_uri = player
                .currently_playing()
                .and_then(rspotify::model::PlayableItem::id)
                .map(|id| id.uri());
            match (player.currently_playing_tracks_id.clone(), item_uri) {
                (Some(id), Some(uri)) => (id, uri),
                _ => return,
            }
        };

        let is_stale = self
            .data
            .read()
            .context_tracks(&ContextId::Tracks(id))
            .is_some_and(|tracks| tracks.iter().all(|t| t.id.uri() != uri));
        if is_stale {
            self.player.write().currently_playing_tracks_id = None;
        }
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
