//! Bringing the cached liked tracks up to date with few requests. The cached list and Spotify's
//! are both newest first, so where tracks were unliked elsewhere shows by comparing a few of
//! Spotify's pages with the cache, without requesting every liked track again.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

/// Items in a page of Spotify's liked tracks
pub(super) const PAGE: usize = 50;
/// How many of the cached newest items are looked for among Spotify's newest
const ANCHOR_DEPTH: usize = 50;

/// A liked item as compared: a track's URI, or `None` for an item that isn't a track (a local
/// file, an unplayable track), which only holds a position
pub(super) type Key = Option<String>;

/// Where Spotify's newest liked items meet the cached ones
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Anchor {
    /// Position in Spotify's list of the first cached track found: the items before it are new
    pub new: usize,
    /// Position of that track in the cached list: the cached items before it are gone
    pub cached: usize,
}

/// Find the first of the cached newest tracks among Spotify's newest liked items
pub(super) fn find_anchor(newest: &[Key], cached: &[Key]) -> Option<Anchor> {
    let head: HashMap<&str, usize> = cached
        .iter()
        .take(ANCHOR_DEPTH)
        .enumerate()
        .filter_map(|(i, key)| Some((key.as_deref()?, i)))
        .collect();
    newest.iter().enumerate().find_map(|(new, key)| {
        let cached = *head.get(key.as_deref()?)?;
        Some(Anchor { new, cached })
    })
}

/// What a removal search needs next
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// The page of Spotify's list starting at this position after the anchor
    Probe(usize),
    /// Every unliked item is found: their positions in the cached list after the anchor
    Done(Vec<usize>),
}

/// Finds the cached items Spotify no longer has, after the anchor. There Spotify's list is the
/// cached one without them, so the two line up position by position until the first of them,
/// and past it Spotify's items sit one position earlier: a binary search over pages finds it,
/// then the next one the same way. It trusts Spotify's count: once it found as many as are
/// missing, the rest of the list isn't read.
#[derive(Debug)]
pub(super) struct RemovalSearch {
    /// The cached items after the anchor
    cached: Vec<Key>,
    /// The number of Spotify's items after the anchor
    len: usize,
    /// Spotify's items before `lo` line up with `cached[i + removed.len()]`
    lo: usize,
    /// Spotify's first item not lining up is at `hi` or before
    hi: usize,
    removed: Vec<usize>,
}

impl RemovalSearch {
    /// `None` when Spotify has more items than the cache, which no unlike explains
    pub(super) fn new(cached: Vec<Key>, len: usize) -> Option<Self> {
        (len <= cached.len()).then_some(Self {
            cached,
            len,
            lo: 0,
            hi: len,
            removed: Vec::new(),
        })
    }

    fn remaining(&self) -> usize {
        self.cached.len() - self.len - self.removed.len()
    }

    pub(super) fn next(&self) -> Step {
        if self.remaining() == 0 {
            return Step::Done(self.removed.clone());
        }
        if self.lo >= self.len {
            // Spotify's list ends here: the cached items left over are the unliked ones
            let mut removed = self.removed.clone();
            removed.extend(self.lo + self.removed.len()..self.cached.len());
            return Step::Done(removed);
        }
        if self.hi - self.lo <= PAGE {
            Step::Probe(self.lo)
        } else {
            Step::Probe(self.lo + (self.hi - self.lo) / 2)
        }
    }

    /// Take in the page of Spotify's list starting at `offset` after the anchor. `false` when it
    /// doesn't fit the cache, for example an item the cache doesn't have: then only requesting
    /// every liked track tells what changed.
    pub(super) fn feed(&mut self, offset: usize, page: &[Key]) -> bool {
        if offset < self.lo || offset >= self.len || page.is_empty() {
            return false;
        }
        let page = &page[..page.len().min(self.len - offset)];
        let mut start = 0;
        if offset > self.lo {
            // the page's first track tells on which side of it the next unliked item is
            let Some(first) = page.iter().position(Option::is_some) else {
                return false;
            };
            if self.cached.get(offset + first + self.removed.len()) != Some(&page[first]) {
                self.hi = offset + first;
                return true;
            }
            start = first;
        }

        let found = self.removed.len();
        for (i, key) in page.iter().enumerate().skip(start) {
            let position = offset + i;
            loop {
                let Some(cached) = self.cached.get(position + self.removed.len()) else {
                    return false;
                };
                if cached == key {
                    break;
                }
                // only a cached item can be missing: an item that isn't a track, where the
                // cache has a track, means the lists don't fit
                if key.is_none() || self.remaining() == 0 {
                    return false;
                }
                self.removed.push(position + self.removed.len());
            }
        }
        self.lo = offset + page.len();
        if self.removed.len() > found {
            self.hi = self.len;
        }
        true
    }
}

/// Bring the shown liked tracks to `latest` in place, so a sorted list stays sorted: drop the
/// ones no longer liked and put newly liked ones first. Returns where the selected track is now.
pub(super) fn apply_changes<T: Clone>(
    shown: &mut Vec<T>,
    latest: &[T],
    key: impl Fn(&T) -> String,
    selected: Option<usize>,
) -> Option<usize> {
    let latest_keys: HashSet<String> = latest.iter().map(&key).collect();
    let shown_keys: HashSet<String> = shown.iter().map(&key).collect();
    let selected_key = selected.and_then(|i| shown.get(i)).map(&key);
    let added: Vec<T> = latest
        .iter()
        .filter(|t| !shown_keys.contains(&key(t)))
        .cloned()
        .collect();
    shown.retain(|t| latest_keys.contains(&key(t)));
    shown.splice(0..0, added);
    let last = shown.len().checked_sub(1)?;
    selected_key
        .and_then(|k| shown.iter().position(|t| key(t) == k))
        // the selected track itself was unliked: stay at its row
        .or(selected.map(|i| i.min(last)))
}

/// Set while a liked tracks refresh runs, so opening the page again doesn't start a second one
static RUNNING: AtomicBool = AtomicBool::new(false);

/// A liked tracks refresh running; dropping it ends it
pub(super) struct Running;

impl Running {
    /// `None` while another refresh runs
    pub(super) fn start() -> Option<Self> {
        RUNNING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Self)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_changes, find_anchor, Anchor, Key, RemovalSearch, Step, PAGE};

    fn keys(range: std::ops::Range<usize>) -> Vec<Key> {
        range.map(|i| Some(format!("t{i}"))).collect()
    }

    /// Runs a search against Spotify's list `spotify`; the removed positions and the probe count,
    /// or `None` when the search gave up
    fn search(cached: Vec<Key>, spotify: &[Key]) -> Option<(Vec<usize>, usize)> {
        let mut search = RemovalSearch::new(cached, spotify.len())?;
        let mut probes = 0;
        loop {
            match search.next() {
                Step::Done(removed) => return Some((removed, probes)),
                Step::Probe(offset) => {
                    probes += 1;
                    assert!(probes < 100, "the search doesn't end");
                    let end = (offset + PAGE).min(spotify.len());
                    if !search.feed(offset, &spotify[offset..end]) {
                        return None;
                    }
                }
            }
        }
    }

    /// The cached list `0..len` with the items at `removed` unliked on Spotify
    fn without(len: usize, removed: &[usize]) -> (Vec<Key>, Vec<Key>) {
        let cached = keys(0..len);
        let spotify = cached
            .iter()
            .enumerate()
            .filter(|(i, _)| !removed.contains(i))
            .map(|(_, k)| k.clone())
            .collect();
        (cached, spotify)
    }

    #[test]
    fn the_anchor_is_the_cached_newest_track_after_the_new_ones() {
        let mut newest = keys(100..103);
        newest.extend(keys(0..5));
        assert_eq!(
            find_anchor(&newest, &keys(0..10)),
            Some(Anchor { new: 3, cached: 0 })
        );
    }

    #[test]
    fn an_unliked_newest_track_moves_the_anchor_to_the_next_one() {
        assert_eq!(
            find_anchor(&keys(1..5), &keys(0..10)),
            Some(Anchor { new: 0, cached: 1 })
        );
    }

    #[test]
    fn no_anchor_when_the_newest_items_are_all_new() {
        assert_eq!(find_anchor(&keys(100..150), &keys(0..10)), None);
    }

    #[test]
    fn one_unliked_track_in_a_large_library_takes_a_few_probes() {
        for removed in [1, 49, 50, 3210, 6419] {
            let (cached, spotify) = without(6420, &[removed]);
            let (found, probes) = search(cached, &spotify).expect("a search result");
            assert_eq!(found, vec![removed]);
            assert!(probes <= 9, "{probes} probes for position {removed}");
        }
    }

    #[test]
    fn unliked_tracks_far_apart_are_each_found() {
        let (cached, spotify) = without(6420, &[17, 2000, 5000]);
        let (found, probes) = search(cached, &spotify).expect("a search result");
        assert_eq!(found, vec![17, 2000, 5000]);
        assert!(probes <= 27, "{probes} probes");
    }

    #[test]
    fn unliked_tracks_close_together_are_found_in_one_page() {
        let (cached, spotify) = without(500, &[300, 301, 310]);
        assert_eq!(
            search(cached, &spotify).expect("a search result").0,
            vec![300, 301, 310]
        );
    }

    #[test]
    fn unliked_tracks_at_the_end_are_found_without_probing_past_it() {
        let (cached, spotify) = without(120, &[118, 119]);
        assert_eq!(
            search(cached, &spotify).expect("a search result").0,
            vec![118, 119]
        );
    }

    #[test]
    fn items_that_arent_tracks_keep_their_positions() {
        let mut cached = keys(0..200);
        cached[10] = None;
        cached[150] = None;
        let mut spotify = cached.clone();
        spotify.remove(120);
        assert_eq!(
            search(cached, &spotify).expect("a search result").0,
            vec![120]
        );
    }

    #[test]
    fn an_item_the_cache_doesnt_have_gives_up() {
        // in the page read after the unliked ones: a replaced item further on that no probe
        // reads goes unnoticed, as with the count check alone
        let (cached, mut spotify) = without(300, &[100, 101]);
        spotify[110] = Some("unknown".to_string());
        assert_eq!(search(cached, &spotify), None);
    }

    #[test]
    fn more_items_than_cached_gives_up() {
        assert!(RemovalSearch::new(keys(0..10), 11).is_none());
    }

    #[test]
    fn changes_keep_the_selection_on_the_same_track() {
        let mut shown: Vec<String> = (0..6).map(|i| format!("t{i}")).collect();
        let latest: Vec<String> = ["new", "t0", "t2", "t3", "t5"]
            .iter()
            .map(ToString::to_string)
            .collect();
        // t3 selected; t1 above it and t4 below it unliked, "new" liked
        let selected = apply_changes(&mut shown, &latest, Clone::clone, Some(3));
        assert_eq!(shown, latest);
        assert_eq!(shown[selected.expect("a selection")], "t3");
    }

    #[test]
    fn changes_keep_a_sorted_list_sorted() {
        // shown sorted by name; Spotify's order is newest first
        let mut shown: Vec<String> = ["a", "b", "c"].iter().map(ToString::to_string).collect();
        let latest: Vec<String> = ["c", "a"].iter().map(ToString::to_string).collect();
        let selected = apply_changes(&mut shown, &latest, Clone::clone, Some(1));
        assert_eq!(shown, ["a", "c"]);
        // the selected "b" was unliked: the selection stays at its row
        assert_eq!(selected, Some(1));
    }
}
