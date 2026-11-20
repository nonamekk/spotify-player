//! The played history of a Spotify Soloist device, written by the `soloist-history` listener
//! (the `soloist_history` crate): `PlayedHistory.msgpack` in the cache folder.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context as _, Result};
use rspotify::model::FullTrack;
use serde_json::json;
use soloist_history::{Record, COVER_PX, COVER_URL, FILE_NAME};

use super::Track;
use crate::config;

/// The Web API track object of a played track
pub fn full_track(record: &Record) -> Option<FullTrack> {
    let artists: Vec<_> = record
        .artists
        .iter()
        .map(|(id, name)| json!({"external_urls": {}, "href": null, "id": id, "name": name}))
        .collect();
    let images: Vec<_> = record
        .cover
        .iter()
        .map(|cover| json!({"url": format!("{COVER_URL}{cover}"), "height": COVER_PX, "width": COVER_PX}))
        .collect();
    let album = json!({
        "album_type": null, "artists": artists, "external_urls": {}, "href": null,
        "id": record.album_id, "images": images, "name": record.album,
    });
    serde_json::from_value(json!({
        "album": album, "artists": artists, "disc_number": 0, "duration_ms": record.duration_ms,
        "explicit": record.explicit, "external_ids": {}, "external_urls": {}, "href": null,
        "id": record.id, "is_local": false, "name": record.name, "preview_url": null,
        "track_number": 0, "type": "track",
    }))
    .ok()
}

/// The last `played_history_size` records of the played history, oldest first, or `None` when
/// there is none
fn load(cache_folder: &Path) -> Result<Option<Vec<Record>>> {
    let path = cache_folder.join(FILE_NAME);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let (mut records, _) = soloist_history::parse(&bytes);
    let size = config::get_config().app_config.played_history_size;
    Ok(Some(records.split_off(records.len().saturating_sub(size))))
}

/// The tracks of the played history, newest first and each once, leaving out those
/// played for less than `played_history_min_played_secs`; `None` when there is no played history
pub fn recently_played_tracks(cache_folder: &Path) -> Result<Option<Vec<Track>>> {
    let min_listened_ms = config::get_config()
        .app_config
        .played_history_min_played_secs
        * 1000;
    Ok(load(cache_folder)?.map(|records| newest_tracks(records, min_listened_ms)))
}

fn newest_tracks(records: Vec<Record>, min_listened_ms: u64) -> Vec<Track> {
    let mut seen = HashSet::new();
    records
        .into_iter()
        .rev()
        .filter(|r| r.listened_ms >= min_listened_ms && seen.insert(r.id.clone()))
        .filter_map(|r| {
            let mut track = Track::try_from_full_track(full_track(&r)?)?;
            track.added_at = r.played_at / 1000;
            Some(track)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use soloist_history::Record;

    use super::{full_track, newest_tracks};

    const A: &str = "1YzD6bcLtR7EGeiO2Vu3Fr";
    const B: &str = "4uLU6hMCjMI75M1A2tKUQC";
    const C: &str = "3n3Ppam7vgaVa1iaRUc9Lp";

    fn record(id: &str, listened_ms: u64) -> Record {
        Record {
            played_at: 1_795_122_000_000,
            listened_ms,
            id: id.to_owned(),
            name: id.to_owned(),
            artists: vec![("4yAfjEDlkmMb7uLcN3Ot9K".to_owned(), "Artist".to_owned())],
            album_id: "4LXJ5XxclRopTjLBXpyptg".to_owned(),
            album: "Album".to_owned(),
            cover: Some("ab67616d0000b2739e313b3e2c38f105628b0c0d".to_owned()),
            duration_ms: 241_678,
            ..Record::default()
        }
    }

    #[test]
    fn lists_newest_first_once_and_hides_short_plays() {
        let records = vec![
            record(A, 60_000),
            record(B, 60_000),
            record(A, 60_000),
            record(C, 3_000),
        ];
        let tracks = newest_tracks(records, 10_000);
        let names: Vec<_> = tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, [A, B]);
        assert_eq!(tracks[0].album.as_ref().unwrap().name, "Album");
        assert_eq!(tracks[0].added_at, 1_795_122_000);
    }

    #[test]
    fn makes_the_web_api_track_with_its_cover() {
        let track = full_track(&record(A, 1)).unwrap();
        assert_eq!(
            track.album.images[0].url,
            "https://i.scdn.co/image/ab67616d0000b2739e313b3e2c38f105628b0c0d"
        );
        assert_eq!(track.album.artists[0].name, "Artist");
        assert_eq!(track.duration.num_milliseconds(), 241_678);
    }
}
