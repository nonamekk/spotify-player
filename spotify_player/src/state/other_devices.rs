//! What plays and played on the account's devices, for the `g D` and `g d` pages. Spotify reports
//! the playback of the active device only; what devices played comes from its recently played
//! tracks, which it adds to as each track ends.

use std::collections::{HashMap, HashSet};

use super::{ContextId, Playback, Track, TracksId, USER_LIKED_TRACKS_ID};
use rspotify::model::{
    CurrentPlaybackContext, FullTrack, Id, Offset, PlayHistory, PlayableItem, TrackId,
};

/// The `g D` page beyond its tracks
#[derive(Default, Debug)]
pub struct OtherDevices {
    /// The track another device plays or holds paused now: the page's first row
    pub live: Option<TrackId<'static>>,
    /// What each listed track was played from, by track URI
    pub contexts: HashMap<String, ContextId>,
}

impl OtherDevices {
    /// The playback that continues `track` here, or `None` for the live track, which is
    /// continued by moving its device's playback here
    pub fn playback(&self, track: &Track) -> Option<Playback> {
        if self.live.as_ref() == Some(&track.id) {
            return None;
        }
        let uri = track.id.uri();
        Some(match self.contexts.get(&uri) {
            Some(context) => Playback::Context(context.clone(), Some(Offset::Uri(uri))),
            None => Playback::URIs(vec![track.id.clone().into()], None),
        })
    }
}

/// The page's tracks, its description and its other data, from the playback of a device other
/// than this machine's (`live`) and Spotify's recently played tracks, of every device
pub fn page(
    live: Option<CurrentPlaybackContext>,
    history: Vec<PlayHistory>,
) -> (Vec<Track>, String, OtherDevices) {
    let mut tracks = Vec::new();
    let mut page = OtherDevices::default();
    let mut desc = String::new();
    let mut seen = HashSet::new();

    if let Some(live) = live {
        if let Some(PlayableItem::Track(full)) = live.item {
            let context = live.context.as_ref().and_then(ContextId::from_context);
            if let Some(mut track) = Track::try_from_full_track(full) {
                track.added_at = u64::try_from(live.timestamp.timestamp()).unwrap_or_default();
                let state = if live.is_playing { "Playing" } else { "Paused" };
                desc = format!("{state} on {}: the first track. ", live.device.name);
                remember(&mut page, &track, context);
                page.live = Some(track.id.clone());
                seen.insert(track.id.uri());
                tracks.push(track);
            }
        }
    }
    desc.push_str("Played on any device, newest first");

    for play in history {
        let played_at = u64::try_from(play.played_at.timestamp_millis()).unwrap_or_default();
        let context = play.context.as_ref().and_then(ContextId::from_context);
        let Some(mut track) = Track::try_from_full_track(play.track) else {
            continue;
        };
        if !seen.insert(track.id.uri()) {
            continue;
        }
        track.added_at = played_at / 1000;
        remember(&mut page, &track, context);
        tracks.push(track);
    }
    (tracks, desc, page)
}

fn remember(page: &mut OtherDevices, track: &Track, context: Option<ContextId>) {
    if let Some(context) = context {
        page.contexts.insert(track.id.uri(), context);
    }
}

/// The list being played, or played last, on any device: the `g d` page
pub struct ListSource {
    /// The device that plays it now and whether it plays; `None` when it was played before
    pub device: Option<(String, bool)>,
    /// Whether that device is this machine's
    pub is_here: bool,
    /// The list: a Spotify context, or the liked tracks for Spotify's collection
    pub context: Option<ContextId>,
    /// The track the device is at, or played last
    pub at: Track,
    /// The tracks played from the list before `at`, oldest first, each once
    pub played: Vec<Track>,
}

/// The list of the account's playback (`live`, when it plays from one), else the last list
/// played from on any device, according to Spotify's recently played tracks (`history`, newest
/// first). `here` is this machine's device.
pub fn list_source(
    live: Option<CurrentPlaybackContext>,
    history: Vec<PlayHistory>,
    here: Option<&str>,
) -> Option<ListSource> {
    let live = live.filter(|live| live.context.is_some());
    let (device, is_here, context_uri, context, at) = if let Some(live) = live {
        let Some(PlayableItem::Track(at)) = live.item else {
            return None;
        };
        let context = live.context?;
        let list = list_id(&context);
        let is_here = here == Some(live.device.name.as_str());
        let device = Some((live.device.name, live.is_playing));
        (device, is_here, context.uri, list, at)
    } else {
        let last = history.iter().find(|play| play.context.is_some())?;
        let context = last.context.as_ref()?;
        let list = list_id(context);
        (None, false, context.uri.clone(), list, last.track.clone())
    };
    let at = Track::try_from_full_track(at)?;

    let mut seen = HashSet::from([at.id.uri()]);
    let mut played: Vec<Track> = history
        .into_iter()
        .filter(|play| play.context.as_ref().is_some_and(|c| c.uri == context_uri))
        .filter_map(|play| Track::try_from_full_track(play.track))
        .filter(|track| seen.insert(track.id.uri()))
        .collect();
    played.reverse();
    Some(ListSource {
        device,
        is_here,
        context,
        at,
        played,
    })
}

/// The ID of the list a context is: a Spotify context, or the liked tracks for the collection
fn list_id(context: &rspotify::model::Context) -> Option<ContextId> {
    if context.uri.ends_with(":collection") {
        return Some(ContextId::Tracks(USER_LIKED_TRACKS_ID.clone()));
    }
    ContextId::from_context(context)
}

impl ListSource {
    /// The list's tracks when Spotify doesn't give the list itself: the tracks played from it,
    /// the track it's at, then `upcoming` (Spotify's queue while it plays)
    pub fn rebuilt(&self, upcoming: Vec<FullTrack>) -> Vec<Track> {
        let mut seen: HashSet<String> = self.played.iter().map(|t| t.id.uri()).collect();
        seen.insert(self.at.id.uri());
        let upcoming = upcoming
            .into_iter()
            .filter_map(Track::try_from_full_track)
            .filter(|track| seen.insert(track.id.uri()));
        self.played
            .iter()
            .cloned()
            .chain(std::iter::once(self.at.clone()))
            .chain(upcoming)
            .collect()
    }

    /// The page's data beyond its tracks
    pub fn page(&self) -> OtherDeviceList {
        OtherDeviceList {
            live: (self.device.is_some() && !self.is_here).then(|| self.at.id.clone()),
            at: Some(self.at.id.clone()),
            context: self.context.clone(),
        }
    }
}

/// The `g d` page beyond its tracks
#[derive(Default, Debug)]
pub struct OtherDeviceList {
    /// The track another device plays or holds paused now
    pub live: Option<TrackId<'static>>,
    /// The track it's at or played last, selected when the page opens
    pub at: Option<TrackId<'static>>,
    pub context: Option<ContextId>,
}

/// What choosing a track of the `g d` page does
#[derive(Debug)]
pub enum ListChoice {
    /// Move the device's playback here
    Take,
    /// Start this playback here, with this Tracks context as the playing list
    Play(Playback, Option<TracksId>),
}

impl OtherDeviceList {
    /// What choosing `track` of the page's `tracks` does: the live track moves its device's
    /// playback here, another plays the list from that track
    pub fn choose(&self, track: &Track, tracks: &[Track], limit: usize) -> ListChoice {
        if self.live.as_ref() == Some(&track.id) {
            return ListChoice::Take;
        }
        let uri = track.id.uri();
        let list = || Playback::URIs(tracks.iter().map(|t| t.id.clone().into()).collect(), None);
        match &self.context {
            Some(ContextId::Tracks(id)) => {
                ListChoice::Play(list().uri_offset(uri, limit), Some(id.clone()))
            }
            Some(context) => ListChoice::Play(
                Playback::Context(context.clone(), Some(Offset::Uri(uri))),
                None,
            ),
            None => ListChoice::Play(list().uri_offset(uri, limit), None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rspotify::model::{AlbumId, PlaylistId};
    use soloist_history::Record;

    fn record(id: &str, played_at: u64, listened_ms: u64) -> Record {
        Record {
            played_at,
            listened_ms,
            context: None,
            id: id.to_owned(),
            name: String::new(),
            artists: Vec::new(),
            album_id: String::new(),
            album: String::new(),
            cover: None,
            duration_ms: 0,
            explicit: false,
        }
    }

    fn track(id: &str) -> Track {
        Track {
            id: TrackId::from_id(id.to_owned()).unwrap(),
            name: String::new(),
            artists: Vec::new(),
            album: None,
            duration: std::time::Duration::ZERO,
            explicit: false,
            added_at: 0,
        }
    }

    #[test]
    fn the_live_track_is_moved_here_and_the_others_continue_their_context() {
        let live = track("4uLU6hMCjMI75M1A2tKUQC");
        let from_album = track("1301WleyT98MSxVHPZCA6M");
        let alone = track("7whI6rFGDbQPPMrvtEPDxT");
        let album = ContextId::Album(AlbumId::from_id("4aawyAB9vmqN3uQ7FjRGTy").unwrap());
        let page = OtherDevices {
            live: Some(live.id.clone()),
            contexts: HashMap::from([
                (
                    live.id.uri(),
                    ContextId::Playlist(PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M").unwrap()),
                ),
                (from_album.id.uri(), album.clone()),
            ]),
        };

        assert!(page.playback(&live).is_none());
        assert!(matches!(
            page.playback(&from_album),
            Some(Playback::Context(ContextId::Album(_), Some(Offset::Uri(uri)))) if uri == from_album.id.uri()
        ));
        assert!(matches!(page.playback(&alone), Some(Playback::URIs(ids, None)) if ids.len() == 1));
    }

    fn play(id: &str, played_at_ms: i64, context: Option<&str>) -> PlayHistory {
        let track = crate::state::played_history::full_track(&record(id, 0, 0)).unwrap();
        let context = context.map(|uri| {
            serde_json::json!({"uri": uri, "href": "", "external_urls": {}, "type": "playlist"})
        });
        let played_at = chrono::DateTime::from_timestamp_millis(played_at_ms).unwrap();
        serde_json::from_value(serde_json::json!({
            "track": track, "played_at": played_at.to_rfc3339(), "context": context,
        }))
        .unwrap()
    }

    #[test]
    fn the_last_list_played_ends_at_its_last_track_and_keeps_only_its_tracks() {
        let mix = "spotify:playlist:37i9dQZF1E8Myhe4XH0J2t";
        let (a, b, c, here) = (
            "4uLU6hMCjMI75M1A2tKUQC",
            "1301WleyT98MSxVHPZCA6M",
            "7whI6rFGDbQPPMrvtEPDxT",
            "6rqhFgbbKwnb9MLmUQDhG6",
        );
        // newest first, as Spotify gives them; a track played alone isn't a list
        let history = vec![
            play(here, 3_001_000, None),
            play(c, 2_000_000, Some(mix)),
            play(b, 1_500_000, None),
            play(a, 1_000_000, Some(mix)),
        ];

        let source = list_source(None, history, Some("box-spot")).unwrap();
        assert!(source.device.is_none());
        assert_eq!(source.at.id.id(), c);
        assert!(matches!(source.context, Some(ContextId::Playlist(_))));
        let played: Vec<_> = source.played.iter().map(|t| t.id.id().to_owned()).collect();
        assert_eq!(played, [a]);

        let next = crate::state::played_history::full_track(&record(b, 0, 0)).unwrap();
        let rebuilt: Vec<_> = source
            .rebuilt(vec![next])
            .iter()
            .map(|t| t.id.id().to_owned())
            .collect();
        assert_eq!(rebuilt, [a, c, b]);
        // nothing plays now, so every track starts the list again
        assert!(source.page().live.is_none());
    }

    #[test]
    fn choosing_on_the_list_takes_the_live_track_and_plays_the_rest_from_there() {
        let (live, other) = (
            track("4uLU6hMCjMI75M1A2tKUQC"),
            track("1301WleyT98MSxVHPZCA6M"),
        );
        let tracks = [live.clone(), other.clone()];
        let playlist = ContextId::Playlist(PlaylistId::from_id("37i9dQZF1E8Myhe4XH0J2t").unwrap());
        let mut list = OtherDeviceList {
            live: Some(live.id.clone()),
            at: Some(live.id.clone()),
            context: Some(playlist),
        };
        assert!(matches!(list.choose(&live, &tracks, 50), ListChoice::Take));
        assert!(matches!(
            list.choose(&other, &tracks, 50),
            ListChoice::Play(Playback::Context(ContextId::Playlist(_), Some(Offset::Uri(uri))), None)
                if uri == other.id.uri()
        ));

        list.context = Some(ContextId::Tracks(USER_LIKED_TRACKS_ID.clone()));
        assert!(matches!(
            list.choose(&other, &tracks, 50),
            ListChoice::Play(Playback::URIs(ids, Some(Offset::Uri(_))), Some(id))
                if ids.len() == 2 && id.uri == USER_LIKED_TRACKS_ID.uri
        ));
    }
}
