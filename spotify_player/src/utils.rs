use std::borrow::Cow;

/// formats a time duration into a "{minutes}:{seconds}" format
pub fn format_duration(duration: &chrono::Duration) -> String {
    let secs = duration.num_seconds();
    format!("{}:{:02}", secs / 60, secs % 60)
}

pub fn map_join<T, F>(v: &[T], f: F, sep: &str) -> String
where
    F: Fn(&T) -> &str,
{
    v.iter().map(f).fold(String::new(), |x, y| {
        if x.is_empty() {
            x + y
        } else {
            x + sep + y
        }
    })
}

#[allow(dead_code)]
pub fn get_track_album_image_url(track: &rspotify::model::FullTrack) -> Option<&str> {
    if track.album.images.is_empty() {
        None
    } else {
        Some(&track.album.images[0].url)
    }
}

#[allow(dead_code)]
pub fn get_episode_show_image_url(episode: &rspotify::model::FullEpisode) -> Option<&str> {
    if episode.show.images.is_empty() {
        None
    } else {
        Some(&episode.show.images[0].url)
    }
}

pub fn parse_uri(uri: &str) -> Cow<'_, str> {
    let parts = uri.split(':').collect::<Vec<_>>();
    // The below URI probably has a format of `spotify:user:{user_id}:{type}:{id}`,
    // but `rspotify` library expects to receive an URI of format `spotify:{type}:{id}`.
    // We have to modify the URI to a corresponding format.
    // See: https://github.com/aome510/spotify-player/issues/57#issuecomment-1160868626
    if parts.len() == 5 {
        Cow::Owned([parts[0], parts[3], parts[4]].join(":"))
    } else {
        Cow::Borrowed(uri)
    }
}

#[cfg(feature = "fzf")]
use fuzzy_matcher::skim::SkimMatcherV2;

#[cfg(feature = "fzf")]
pub fn fuzzy_search_items<'a, T: std::fmt::Display>(items: &'a [T], query: &str) -> Vec<&'a T> {
    let matcher = SkimMatcherV2::default();
    let mut result = items
        .iter()
        .filter_map(|t| {
            matcher
                .fuzzy(&t.to_string(), query, false)
                .map(|(score, _)| (t, score))
        })
        .collect::<Vec<_>>();

    result.sort_by(|(_, a), (_, b)| b.cmp(a));
    result.into_iter().map(|(t, _)| t).collect::<Vec<_>>()
}

/// Get a list of items filtered by a search query.
pub fn filtered_items_from_query<'a, T: std::fmt::Display>(
    query: &str,
    items: &'a [T],
) -> Vec<&'a T> {
    let query = query.to_lowercase();

    #[cfg(feature = "fzf")]
    return fuzzy_search_items(items, &query);

    #[cfg(not(feature = "fzf"))]
    items
        .iter()
        .filter(|t| {
            if query.is_empty() {
                true
            } else {
                let t = t.to_string().to_lowercase();
                query
                    .split(' ')
                    .filter(|q| !q.is_empty())
                    .all(|q| t.contains(q))
            }
        })
        .collect::<Vec<_>>()
}

/// How often memory freed inside glibc's allocator is handed back to the system
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const MEMORY_TRIM_INTERVAL: std::time::Duration = std::time::Duration::from_mins(1);

/// Hand the memory glibc's allocator keeps after it's freed back to the system, every minute.
/// glibc returns a thread arena's memory only from its end, so after a burst, such as covers
/// decoded on a tokio worker or the liked tracks parsed, each arena stays at its peak size. A trim
/// takes about 3 ms after a burst and 0.1 ms when there's nothing to hand back.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(unsafe_code)]
pub fn trim_memory_periodically() {
    loop {
        std::thread::sleep(MEMORY_TRIM_INTERVAL);
        // SAFETY: `malloc_trim` has no preconditions and takes each arena's lock itself
        unsafe { libc::malloc_trim(0) };
    }
}
