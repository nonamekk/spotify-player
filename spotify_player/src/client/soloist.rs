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

/// How long a read waits during a silent start, so its deadlines are checked often
const SILENT_READ_TIMEOUT: Duration = Duration::from_millis(50);
/// How long a silent start waits for the first state and the mute
const SILENT_SETUP_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a silent start waits for the playback to start and pause
const SILENT_START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the volume stays down once paused, for volume changes Spotify sends late
const SILENT_SETTLE: Duration = Duration::from_secs(1);

/// What the Soloist on this machine holds, from its state
pub struct Held {
    pub playing: bool,
    /// URI of its item, if it has one
    pub uri: Option<String>,
}

/// What the Soloist on this machine holds; `None` when none runs here with its WebSocket API
pub fn held() -> Option<Held> {
    let mut socket = connect_quick().ok()?;
    let state = read_until(&mut socket, SILENT_SETUP_TIMEOUT, |event| {
        event["type"] == "playback_state"
    })
    .ok()??;
    Some(Held {
        playing: is_playing(&state),
        uri: state["item"]["uri"].as_str().map(str::to_owned),
    })
}

/// A playback being started on the Soloist on this machine without a sound. Spotify's Web API
/// can only start a playback playing, and as it starts it puts the device's volume back up, so
/// over Soloist's own API the volume is held at 0 and the playback paused the moment it plays.
pub struct SilentStart {
    done: flume::Receiver<anyhow::Result<()>>,
}

impl SilentStart {
    /// Mute the Soloist on this machine and watch it: what plays next is paused at once and moved
    /// to `position_ms`, then the volume goes back. `None` when no Soloist runs here, or it plays.
    pub async fn begin(position_ms: i64) -> Option<Self> {
        let (ready_tx, ready_rx) = flume::bounded(1);
        let (done_tx, done) = flume::bounded(1);
        std::thread::Builder::new()
            .name("soloist-silent-start".into())
            .spawn(move || {
                let result = hold_silent(position_ms, &ready_tx);
                if let Err(err) = &result {
                    tracing::warn!("Soloist's silent start: {err:#}");
                }
                let _ = done_tx.send(result);
            })
            .ok()?;
        // the sender is dropped without a value when the mute failed
        ready_rx.recv_async().await.ok()?;
        Some(Self { done })
    }

    /// Wait until the playback is paused at its position and the volume is back
    pub async fn finish(self) -> anyhow::Result<()> {
        self.done.recv_async().await?
    }
}

/// The silent start: mute, signal `ready`, pause what plays, seek, put the volume back
fn hold_silent(position_ms: i64, ready: &flume::Sender<()>) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::time::Instant;

    let mut socket = connect_quick()?;
    let state = read_until(&mut socket, SILENT_SETUP_TIMEOUT, |event| {
        event["type"] == "playback_state"
    })?
    .context("no state from Soloist")?;
    anyhow::ensure!(!is_playing(&state), "Soloist plays already");
    let volume = state["volume"]
        .as_u64()
        .context("Soloist reports no volume")?;
    send(
        &mut socket,
        &serde_json::json!({"command": "set_volume", "volume": 0}),
    )?;
    read_until(&mut socket, SILENT_SETUP_TIMEOUT, |event| {
        event["type"] == "volume_changed" && event["volume"] == 0
    })?
    .context("Soloist didn't mute")?;
    ready.send(())?;

    let deadline = Instant::now() + SILENT_START_TIMEOUT;
    let mut started = false;
    // since when the playback is paused with the volume at 0
    let mut quiet_since: Option<Instant> = None;
    let result = loop {
        if quiet_since.is_some_and(|since| since.elapsed() >= SILENT_SETTLE) {
            break Ok(());
        }
        if Instant::now() > deadline {
            break Err(anyhow::anyhow!(
                "the playback didn't start and pause in time"
            ));
        }
        let event = match socket.read() {
            Ok(Message::Text(text)) => serde_json::from_str::<serde_json::Value>(&text)?,
            Ok(_) => continue,
            Err(err) if api::is_wait(&err) => continue,
            Err(err) => break Err(err.into()),
        };
        let is_status = event["type"] == "playback_changed" || event["type"] == "playback_state";
        if is_status && is_playing(&event) {
            send(&mut socket, &serde_json::json!({"command": "pause"}))?;
            started = true;
            quiet_since = None;
        } else if is_status && started && event["status"] == "paused" && quiet_since.is_none() {
            send(
                &mut socket,
                &serde_json::json!({"command": "seek", "position_ms": position_ms}),
            )?;
            quiet_since = Some(Instant::now());
        } else if event["type"] == "volume_changed" && event["volume"] != 0 {
            send(
                &mut socket,
                &serde_json::json!({"command": "set_volume", "volume": 0}),
            )?;
            quiet_since = quiet_since.map(|_| Instant::now());
        }
    };
    send(
        &mut socket,
        &serde_json::json!({"command": "set_volume", "volume": volume}),
    )?;
    let _ = socket.close(None);
    result
}

/// A connection to the Soloist on this machine whose reads wait `SILENT_READ_TIMEOUT`
fn connect_quick() -> anyhow::Result<api::Socket> {
    let home = dirs_next::home_dir().ok_or_else(|| anyhow::anyhow!("no home folder"))?;
    let socket = api::connect(&api::data_dir(&home))?;
    socket
        .get_ref()
        .set_read_timeout(Some(SILENT_READ_TIMEOUT))?;
    Ok(socket)
}

/// Send a command, given without its `"type"`
fn send(socket: &mut api::Socket, command: &serde_json::Value) -> anyhow::Result<()> {
    let mut command = command.clone();
    command["type"] = "command".into();
    socket.send(Message::text(command.to_string()))?;
    Ok(())
}

/// The first event `wanted` accepts within `timeout`, or `None`
fn read_until(
    socket: &mut api::Socket,
    timeout: Duration,
    wanted: impl Fn(&serde_json::Value) -> bool,
) -> anyhow::Result<Option<serde_json::Value>> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let event: serde_json::Value = serde_json::from_str(&text)?;
                if wanted(&event) {
                    return Ok(Some(event));
                }
            }
            Ok(_) => {}
            Err(err) if api::is_wait(&err) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(None)
}

/// Whether a state or status event says it plays; buffering is the start of playing
fn is_playing(event: &serde_json::Value) -> bool {
    event["status"] == "playing" || event["status"] == "buffering"
}

/// Name of the Spotify Connect device of a Soloist running on this machine: its `-n` argument
pub fn local_device_name() -> Option<String> {
    local_player().map(|(name, _)| name)
}

/// The device name and the binary of the Soloist player running on this machine
fn local_player() -> Option<(String, std::path::PathBuf)> {
    std::fs::read_dir("/proc")
        .ok()?
        .filter_map(Result::ok)
        .filter(|process| {
            std::fs::read_to_string(process.path().join("comm"))
                .is_ok_and(|comm| comm.trim_end() == "soloist")
        })
        .find_map(|process| {
            let name = device_name(&std::fs::read(process.path().join("cmdline")).ok()?)?;
            Some((name, std::fs::read_link(process.path().join("exe")).ok()?))
        })
}

/// What the Soloist running on this machine says of its login and build, through its own
/// commands; `None` when none runs
pub fn login() -> Option<crate::state::logins::SoloistLogin> {
    let (name, binary) = local_player()?;
    let output = |args: &[&str]| {
        std::process::Command::new(&binary)
            .args(args)
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default()
    };
    Some(crate::state::logins::SoloistLogin {
        name,
        logged_in: output(&["ctl", "status"]).contains("logged in: yes"),
        build_expires: crate::state::logins::soloist_build_expires(&output(&["--version"])),
    })
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
