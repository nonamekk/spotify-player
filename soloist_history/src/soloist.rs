//! Spotify Soloist's local WebSocket API (`soloist -w 127.0.0.1:0`): the connection, and the
//! tracks in its events

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use serde_json::Value;
use tungstenite::WebSocket;

pub use tungstenite::Message;

/// A connection to Soloist's WebSocket API
pub type Socket = WebSocket<TcpStream>;

use crate::Record;

/// How long a read waits for an event, so a reader can check for other work
pub const READ_TIMEOUT: Duration = Duration::from_secs(1);
/// The cover size kept; `spotify_player` only uses an album's largest image
const COVER_SIZE: &str = "large";

/// Soloist's data directory: `$SOLOIST_DATA_DIR`, else Soloist's default under `home`
#[must_use]
pub fn data_dir(home: &Path) -> PathBuf {
    std::env::var_os("SOLOIST_DATA_DIR")
        .map_or_else(|| home.join(".local/share/soloist"), PathBuf::from)
}

/// Connect to the WebSocket API of the Soloist whose data directory is `data_dir`. Reads time out
/// after `READ_TIMEOUT`.
pub fn connect(data_dir: &Path) -> Result<WebSocket<TcpStream>> {
    let read = |name: &str| {
        let path = data_dir.join(name);
        std::fs::read_to_string(&path)
            .map(|text| text.trim().to_owned())
            .with_context(|| format!("read {} (is Soloist running with -w?)", path.display()))
    };
    let (addr, port) = (read("ws.addr")?, read("ws.port")?);
    let port: u16 = port
        .parse()
        .with_context(|| format!("Soloist's port {port}"))?;

    let stream = TcpStream::connect((addr.as_str(), port))
        .with_context(|| format!("connect to Soloist at {addr}:{port}"))?;
    let (socket, _) = tungstenite::client(format!("ws://{addr}:{port}/"), stream)
        .map_err(|err| anyhow!("WebSocket handshake with Soloist: {err}"))?;
    socket.get_ref().set_read_timeout(Some(READ_TIMEOUT))?;
    Ok(socket)
}

/// The record of a Soloist item, without its play, or `None` if it isn't a track
#[must_use]
pub fn track_record(item: &Value, context: Option<&str>) -> Option<Record> {
    if item["entity_type"] != "track" {
        return None;
    }
    let deco = &item["decorations"];
    let list = |value: &Value| value.as_array().cloned().unwrap_or_default();
    let album = &deco["parent"]["entity"];

    Some(Record {
        context: context.map(str::to_owned),
        id: last_part(item["uri"].as_str()?),
        name: name(item),
        artists: list(&deco["creators"])
            .iter()
            .map(|creator| {
                let artist = &creator["entity"];
                (
                    last_part(artist["uri"].as_str().unwrap_or_default()),
                    name(artist),
                )
            })
            .collect(),
        album_id: last_part(album["uri"].as_str().unwrap_or_default()),
        album: name(album),
        cover: list(&deco["visual_identity"]["cover"])
            .iter()
            .find(|cover| cover["size"] == COVER_SIZE)
            .and_then(|cover| cover["url"].as_str())
            .map(last_part),
        duration_ms: deco["playback"]["duration_ms"].as_u64().unwrap_or_default(),
        explicit: list(&deco["playback"]["content_ratings"])
            .iter()
            .any(|rating| rating.to_string().to_uppercase().contains("EXPLICIT")),
        ..Record::default()
    })
}

fn name(entity: &Value) -> String {
    entity["decorations"]["identity"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The ID at the end of a URI or URL
fn last_part(uri: &str) -> String {
    uri.rsplit([':', '/']).next().unwrap_or_default().to_owned()
}

/// Send a command to the Soloist whose data directory is `data_dir`, such as
/// `{"type":"command","command":"pause"}`, over a connection of its own
pub fn send_command(data_dir: &Path, command: &Value) -> Result<()> {
    let mut socket = connect(data_dir)?;
    socket
        .send(Message::text(command.to_string()))
        .context("send a command to Soloist")?;
    let _ = socket.close(None);
    Ok(())
}

/// Whether a read error is only the read timing out or being interrupted, not the connection ending
#[must_use]
pub fn is_wait(err: &tungstenite::Error) -> bool {
    matches!(err, tungstenite::Error::Io(err) if matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
    ))
}
