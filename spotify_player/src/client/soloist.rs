//! The Spotify Soloist device running on this machine: its name, and its playback as it changes

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rspotify::model::PlayableItem;
use soloist_history::soloist::{self as api, Message};

use super::ClientRequest;
use crate::state::{played_history, SharedState};

/// Wait before connecting to Soloist again, after it went away or while it isn't running
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Whether `watch_events` follows a Soloist's events
static FOLLOWING: AtomicBool = AtomicBool::new(false);

/// Whether the playback of a Soloist on this machine is followed through its events
pub fn is_following() -> bool {
    FOLLOWING.load(Ordering::Relaxed)
}

/// Follow the playback of a Soloist running on this machine through its events, so that its
/// changes show as they happen instead of when Spotify is next asked. Runs as long as the app.
pub fn watch_events(state: &SharedState, client_pub: &flume::Sender<ClientRequest>) {
    let Some(home) = dirs_next::home_dir() else {
        return;
    };
    let data_dir = api::data_dir(&home);
    loop {
        match api::connect(&data_dir) {
            Ok(socket) => {
                tracing::info!("Following the playback of the Soloist on this machine");
                FOLLOWING.store(true, Ordering::Relaxed);
                follow(state, client_pub, socket);
                FOLLOWING.store(false, Ordering::Relaxed);
            }
            Err(err) => tracing::debug!("No Soloist events to follow: {err:#}"),
        }
        std::thread::sleep(RECONNECT_DELAY);
    }
}

/// Handle a Soloist's events until it closes the connection
fn follow(state: &SharedState, client_pub: &flume::Sender<ClientRequest>, mut socket: api::Socket) {
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Err(err) = handle_event(state, client_pub, &text) {
                    tracing::error!("Failed to handle a Soloist event: {err:#}");
                }
            }
            Ok(Message::Close(_)) => return,
            Ok(_) => {}
            Err(err) if api::is_wait(&err) => {}
            Err(err) => {
                tracing::info!("Soloist's events ended: {err:#}");
                return;
            }
        }
    }
}

/// Show the track a Soloist plays, as soon as it reports it
fn handle_event(
    state: &SharedState,
    client_pub: &flume::Sender<ClientRequest>,
    text: &str,
) -> anyhow::Result<()> {
    let event: serde_json::Value = serde_json::from_str(text)?;
    if event["type"] != "playback_state" || event["is_active"] == false {
        return Ok(());
    }
    let Some(track) = api::track_record(&event["item"], event["context"]["uri"].as_str())
        .as_ref()
        .and_then(played_history::full_track)
    else {
        return Ok(());
    };

    // buffering is the start of playing, not a pause
    let playing = event["status"] == "playing" || event["status"] == "buffering";
    let position_ms = event["position"]["position_ms"].as_i64();
    let shown =
        state
            .player
            .write()
            .show_device_playback(PlayableItem::Track(track), playing, position_ms);
    tracing::debug!(
        "Soloist plays {} ({}): shown {shown:?}",
        event["item"]["uri"],
        event["status"]
    );
    match shown {
        Some(true) => client_pub.send(ClientRequest::NewPlaybackShown)?,
        Some(false) => {}
        None => client_pub.send(ClientRequest::GetCurrentPlayback)?,
    }
    Ok(())
}

/// Name of the Spotify Connect device of a Soloist running on this machine: its `-n` argument
pub fn local_device_name() -> Option<String> {
    std::fs::read_dir("/proc")
        .ok()?
        .filter_map(Result::ok)
        .filter(|process| {
            std::fs::read_to_string(process.path().join("comm"))
                .is_ok_and(|comm| comm.trim_end() == "soloist")
        })
        .find_map(|process| device_name(&std::fs::read(process.path().join("cmdline")).ok()?))
}

/// The device name in a Soloist command line (NUL-separated arguments), if it's the player's
fn device_name(cmdline: &[u8]) -> Option<String> {
    let mut args = cmdline.split(|&b| b == 0).map(String::from_utf8_lossy);
    while let Some(arg) = args.next() {
        if arg == "-n" || arg == "--device-name" {
            return args.next().map(Cow::into_owned);
        }
        if let Some(name) = arg.strip_prefix("--device-name=") {
            return Some(name.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::device_name;

    #[test]
    fn reads_the_device_name_argument() {
        let player = b"/opt/soloist/soloist\0-n\0box-spot\0-k\0spak_x\0-w\x00127.0.0.1:0\0";
        assert_eq!(device_name(player).as_deref(), Some("box-spot"));
        let long = b"soloist\0--device-name=Living room\0";
        assert_eq!(device_name(long).as_deref(), Some("Living room"));
        // `soloist ctl ...` runs as `soloist` too, without a device
        assert_eq!(device_name(b"soloist\0ctl\0trace\0"), None);
    }
}
