//! The played history of a Spotify Soloist device: `PlayedHistory.msgpack` in `spotify_player`'s
//! cache folder, one `MessagePack` array per played track. The `soloist-history` listener writes
//! it, `spotify_player` reads it.

pub mod soloist;

use std::io::{Cursor, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "PlayedHistory.msgpack";
/// Records kept when `played_history_size` isn't set
pub const DEFAULT_SIZE: usize = 1500;
/// Address of a record's cover image, followed by its ID
pub const COVER_URL: &str = "https://i.scdn.co/image/";
/// Size of the cover images the history keeps
pub const COVER_PX: u32 = 640;

/// A played track, stored as an array of these fields in this order
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// When it started playing, in milliseconds since the Unix epoch
    pub played_at: u64,
    /// How long it played on the device, in milliseconds
    pub listened_ms: u64,
    /// URI of what it played from
    pub context: Option<String>,
    pub id: String,
    pub name: String,
    /// IDs and names
    pub artists: Vec<(String, String)>,
    pub album_id: String,
    pub album: String,
    /// ID of the album's cover image
    pub cover: Option<String>,
    pub duration_ms: u64,
    pub explicit: bool,
}

impl Record {
    /// # Panics
    /// Never: a record is plain data.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        rmp_serde::to_vec(self).expect("a record serializes")
    }
}

/// The whole records of a played history, oldest first, and the bytes they take: a record a
/// crash cut short ends it
#[must_use]
pub fn parse(bytes: &[u8]) -> (Vec<Record>, usize) {
    let mut records = rmp_serde::Deserializer::new(Cursor::new(bytes));
    let (mut parsed, mut end) = (Vec::new(), 0);
    while let Ok(record) = Record::deserialize(&mut records) {
        parsed.push(record);
        end = records.get_ref().position() as usize;
    }
    (parsed, end)
}

/// Append a record to the history at `path`
pub fn append(path: &Path, record: &Record) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(&record.to_bytes())
}

/// Drop a record a crash cut short (the records after it couldn't be read), and keep the last
/// `size` records once there are `size + slack`. Returns the number of records kept.
pub fn tidy(path: &Path, size: usize, slack: usize) -> std::io::Result<usize> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let (records, end) = parse(&bytes);
    let first = if records.len() >= size + slack {
        records.len() - size
    } else {
        0
    };
    if first == 0 && end == bytes.len() {
        return Ok(records.len());
    }

    let tmp = path.with_extension("msgpack.tmp");
    std::fs::write(
        &tmp,
        records[first..]
            .iter()
            .flat_map(Record::to_bytes)
            .collect::<Vec<_>>(),
    )?;
    std::fs::rename(tmp, path)?;
    Ok(records.len() - first)
}

#[cfg(test)]
mod tests {
    use super::{append, parse, tidy, Record};

    fn record(name: &str) -> Record {
        Record {
            name: name.to_owned(),
            artists: vec![("R".repeat(22), "Artist".to_owned())],
            listened_ms: 60_000,
            ..Record::default()
        }
    }

    #[test]
    fn parse_stops_at_a_cut_record() {
        let cut = record("C").to_bytes();
        let bytes = [
            record("A").to_bytes(),
            record("B").to_bytes(),
            cut[..5].to_vec(),
        ]
        .concat();
        let (records, end) = parse(&bytes);
        assert_eq!(records, [record("A"), record("B")]);
        assert_eq!(end, bytes.len() - 5);
    }

    #[test]
    fn tidy_drops_a_cut_record_and_keeps_the_last_ones() {
        let path =
            std::env::temp_dir().join(format!("soloist-history-{}.msgpack", std::process::id()));
        let _ = std::fs::remove_file(&path);
        for name in ["A", "B", "C", "D"] {
            append(&path, &record(name)).unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&record("E").to_bytes()[..5]);
        std::fs::write(&path, &bytes).unwrap();

        assert_eq!(tidy(&path, 10, 0).unwrap(), 4);
        assert_eq!(parse(&std::fs::read(&path).unwrap()).0.len(), 4);
        // 4 records, under 2 + 3: kept
        assert_eq!(tidy(&path, 2, 3).unwrap(), 4);
        // 4 records, 2 + 2 reached: trimmed to 2
        assert_eq!(tidy(&path, 2, 2).unwrap(), 2);
        assert_eq!(
            parse(&std::fs::read(&path).unwrap()).0,
            [record("C"), record("D")]
        );
        std::fs::remove_file(&path).unwrap();
    }
}
