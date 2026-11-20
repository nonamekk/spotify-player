//! Soloist as an MPRIS player: what it plays, as its events tell, and the play, pause, next,
//! previous and seek requests of the desktop's media controls, sent to it as commands.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use soloist_history::soloist::{send_command, track_record};
use soloist_history::{Record, COVER_URL};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};

/// How far a seek without an amount (a media key's) goes
const SEEK_STEP: Duration = Duration::from_secs(10);

/// What the media controls act on: whether Soloist plays, and where in the track
#[derive(Default)]
struct Shown {
    playing: AtomicBool,
    position_ms: AtomicU64,
}

pub struct Mpris {
    controls: MediaControls,
    shown: Arc<Shown>,
    track_id: Option<String>,
}

impl Mpris {
    /// Register the player on the session bus, sending its controls' requests to the Soloist
    /// whose data directory is `data_dir`
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        let config = PlatformConfig {
            display_name: "Spotify Soloist",
            dbus_name: "soloist",
            hwnd: None,
        };
        let mut controls = MediaControls::new(config)
            .map_err(|err| anyhow!("register the MPRIS player: {err:?}"))?;
        let shown = Arc::new(Shown::default());
        let state = Arc::clone(&shown);
        controls
            .attach(move |event| {
                let Some(command) = command(&event, &state) else {
                    return;
                };
                if let Err(err) = send_command(&data_dir, &command) {
                    tracing::warn!("Failed to send {command} to Soloist: {err:#}");
                }
            })
            .map_err(|err| anyhow!("attach to the MPRIS player: {err:?}"))?;
        Ok(Self {
            controls,
            shown,
            track_id: None,
        })
    }

    /// Show the playback a Soloist event reports
    pub fn handle(&mut self, event: &Value) -> Result<()> {
        if event["type"] != "playback_state" {
            return Ok(());
        }
        let Some(record) = track_record(&event["item"], None) else {
            return Ok(());
        };
        if self.track_id.as_ref() != Some(&record.id) {
            self.show_track(&record)?;
            self.track_id = Some(record.id);
        }

        let playing = event["is_active"] != false
            && (event["status"] == "playing" || event["status"] == "buffering");
        let position_ms = event["position"]["position_ms"]
            .as_u64()
            .unwrap_or_default();
        self.shown.playing.store(playing, Ordering::Relaxed);
        self.shown.position_ms.store(position_ms, Ordering::Relaxed);
        let progress = Some(MediaPosition(Duration::from_millis(position_ms)));
        let playback = if playing {
            MediaPlayback::Playing { progress }
        } else {
            MediaPlayback::Paused { progress }
        };
        self.controls
            .set_playback(playback)
            .map_err(|err| anyhow!("set the MPRIS playback: {err:?}"))
    }

    fn show_track(&mut self, record: &Record) -> Result<()> {
        let artists: Vec<_> = record
            .artists
            .iter()
            .map(|(_, name)| name.as_str())
            .collect();
        let artists = artists.join(", ");
        let cover_url = record
            .cover
            .as_ref()
            .map(|cover| format!("{COVER_URL}{cover}"));
        self.controls
            .set_metadata(MediaMetadata {
                title: Some(&record.name),
                album: Some(&record.album),
                artist: Some(&artists),
                cover_url: cover_url.as_deref(),
                duration: Some(Duration::from_millis(record.duration_ms)),
            })
            .map_err(|err| anyhow!("set the MPRIS metadata: {err:?}"))
    }
}

/// The Soloist command for a media control request, if it has one
fn command(event: &MediaControlEvent, shown: &Shown) -> Option<Value> {
    let named = |name: &str| json!({"type": "command", "command": name});
    let seek = |position_ms: u64| json!({"type": "command", "command": "seek", "position_ms": position_ms});
    let position_ms = shown.position_ms.load(Ordering::Relaxed);
    let step = |direction: &SeekDirection, by: Duration| {
        let by = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        match direction {
            SeekDirection::Forward => position_ms.saturating_add(by),
            SeekDirection::Backward => position_ms.saturating_sub(by),
        }
    };
    Some(match event {
        MediaControlEvent::Pause | MediaControlEvent::Stop => named("pause"),
        MediaControlEvent::Toggle if shown.playing.load(Ordering::Relaxed) => named("pause"),
        MediaControlEvent::Play | MediaControlEvent::Toggle => named("play"),
        MediaControlEvent::Next => named("skip_next"),
        MediaControlEvent::Previous => named("skip_prev"),
        MediaControlEvent::Seek(direction) => seek(step(direction, SEEK_STEP)),
        MediaControlEvent::SeekBy(direction, by) => seek(step(direction, *by)),
        MediaControlEvent::SetPosition(MediaPosition(at)) => {
            seek(u64::try_from(at.as_millis()).unwrap_or(u64::MAX))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use serde_json::json;
    use souvlaki::{MediaControlEvent, MediaPosition, SeekDirection};

    use super::{command, Shown};

    #[test]
    fn media_controls_become_soloist_commands() {
        let shown = Shown::default();
        shown.playing.store(true, Ordering::Relaxed);
        shown.position_ms.store(30_000, Ordering::Relaxed);
        let named = |name: &str| Some(json!({"type": "command", "command": name}));
        let seek = |ms: u64| Some(json!({"type": "command", "command": "seek", "position_ms": ms}));

        assert_eq!(command(&MediaControlEvent::Toggle, &shown), named("pause"));
        assert_eq!(
            command(&MediaControlEvent::Next, &shown),
            named("skip_next")
        );
        assert_eq!(
            command(&MediaControlEvent::Previous, &shown),
            named("skip_prev")
        );
        assert_eq!(
            command(&MediaControlEvent::Seek(SeekDirection::Backward), &shown),
            seek(20_000)
        );
        assert_eq!(
            command(
                &MediaControlEvent::SeekBy(SeekDirection::Forward, Duration::from_secs(5)),
                &shown
            ),
            seek(35_000)
        );
        assert_eq!(
            command(
                &MediaControlEvent::SetPosition(MediaPosition(Duration::from_secs(90))),
                &shown
            ),
            seek(90_000)
        );
        assert_eq!(command(&MediaControlEvent::Raise, &shown), None);

        shown.playing.store(false, Ordering::Relaxed);
        assert_eq!(command(&MediaControlEvent::Toggle, &shown), named("play"));
    }
}
