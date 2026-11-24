//! The lists of the radios fetched: Spotify makes a radio up anew every time it's asked for one, so
//! a radio fetched again would no longer hold the tracks that were played from it.

use std::path::Path;

use anyhow::{Context as _, Result};

use super::{load_data_from_file_cache, store_data_into_file_cache, FileCacheKey, Track};

/// Radios whose lists are kept, the most recently fetched
const KEPT: usize = 20;

/// The list of the radio `uri`, as first fetched and then extended while it played
pub fn load(cache_folder: &Path, uri: &str) -> Option<Vec<Track>> {
    let radios: Vec<(String, Vec<Track>)> =
        load_data_from_file_cache(FileCacheKey::Radios, cache_folder)?;
    radios
        .into_iter()
        .find_map(|(radio, tracks)| (radio == uri).then_some(tracks))
}

/// Keep the list of the radio `uri`
pub fn store(cache_folder: &Path, uri: &str, tracks: &[Track]) -> Result<()> {
    let mut radios: Vec<(String, Vec<Track>)> =
        load_data_from_file_cache(FileCacheKey::Radios, cache_folder).unwrap_or_default();
    radios.retain(|(radio, _)| radio != uri);
    radios.push((uri.to_owned(), tracks.to_vec()));
    let excess = radios.len().saturating_sub(KEPT);
    radios.drain(..excess);
    store_data_into_file_cache(FileCacheKey::Radios, cache_folder, &radios)
        .context("store the radios into the cache folder")
}

#[cfg(test)]
mod tests {
    use rspotify::model::TrackId;

    use super::{load, store, KEPT};
    use crate::state::Track;

    fn track(id: &str) -> Track {
        Track {
            id: TrackId::from_id(id.repeat(22)).unwrap().into_static(),
            name: id.to_owned(),
            artists: vec![],
            album: None,
            duration: std::time::Duration::default(),
            explicit: false,
            added_at: 0,
        }
    }

    #[test]
    fn keeps_each_radio_as_first_stored_and_only_the_latest() {
        let dir =
            std::env::temp_dir().join(format!("spotify-player-radios-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        store(&dir, "radio:a", &[track("A"), track("B")]).unwrap();
        store(&dir, "radio:b", &[track("C")]).unwrap();
        let names = |uri| {
            load(&dir, uri).map(|tracks| tracks.iter().map(|t| t.name.clone()).collect::<Vec<_>>())
        };
        assert_eq!(names("radio:a"), Some(vec!["A".to_owned(), "B".to_owned()]));

        // storing a radio again replaces it rather than adding another
        store(&dir, "radio:a", &[track("D")]).unwrap();
        assert_eq!(names("radio:a"), Some(vec!["D".to_owned()]));

        for i in 0..KEPT {
            store(&dir, &format!("radio:{i}"), &[track("E")]).unwrap();
        }
        assert_eq!(names("radio:a"), None);
        assert!(names(&format!("radio:{}", KEPT - 1)).is_some());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
