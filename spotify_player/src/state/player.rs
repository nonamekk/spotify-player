use super::model::{ContextId, Device, PlaybackMetadata, TracksId};
use super::queue::CustomQueue;
use super::session::SavedPlayback;

/// How long a track shown on request stays while the device still reports the one before
const REQUEST_GRACE: std::time::Duration = std::time::Duration::from_secs(4);

/// A track shown on request, before the device plays it
#[derive(Debug)]
pub struct Requested {
    /// The tracks shown before it in a series of requests, which the device may still report
    /// until it follows the last one
    before: Vec<String>,
    at: std::time::Instant,
}

fn item_uri(item: Option<&rspotify::model::PlayableItem>) -> Option<String> {
    use rspotify::model::Id as _;
    Some(item?.id()?.uri())
}

/// Player state
#[derive(Default, Debug)]
pub struct PlayerState {
    pub devices: Vec<Device>,

    pub playback: Option<rspotify::model::CurrentPlaybackContext>,
    pub playback_last_updated_time: Option<std::time::Instant>,
    /// A buffered state to speedup the feedback of playback metadata update to user
    // Related issue: https://github.com/aome510/spotify-player/issues/109
    pub buffered_playback: Option<PlaybackMetadata>,

    pub queue: Option<rspotify::model::CurrentUserQueue>,

    /// The currently playing Tracks context (for contexts not tracked by Spotify's playback, e.g. liked/top tracks)
    pub currently_playing_tracks_id: Option<TracksId>,

    /// App-managed custom queue for full playlist/album playback.
    /// Active when the integrated librespot player is streaming and the user
    /// started playback from a track-table context.
    pub custom_queue: Option<CustomQueue>,

    /// Playback of the previous session, restored at startup when Spotify has no playback to resume
    pub saved_playback: Option<SavedPlayback>,

    /// A track a player request showed before the device plays it
    pub requested: Option<Requested>,
}

impl PlayerState {
    /// Get the current playback
    ///
    /// # Note
    /// Because playback metadata stored inside the player state is buffered,
    /// the returned playback is estimated based on the available data.
    pub fn current_playback(&self) -> Option<rspotify::model::CurrentPlaybackContext> {
        let mut playback = self.playback.clone()?;

        // update the playback's progress based on the `playback_last_updated_time`
        playback.progress = playback.progress.map(|d| {
            d + if playback.is_playing {
                chrono::Duration::from_std(self.playback_last_updated_time.unwrap().elapsed())
                    .unwrap()
            } else {
                chrono::Duration::zero()
            }
        });

        // update the playback's metadata based on the `buffered_playback` metadata
        if let Some(ref p) = self.buffered_playback {
            playback.device.name.clone_from(&p.device_name);
            playback.device.id.clone_from(&p.device_id);
            playback.is_playing = p.is_playing;
            playback.device.volume_percent = p.volume;
            playback.repeat_state = p.repeat_state;
            playback.shuffle_state = p.shuffle_state;
        }

        Some(playback)
    }

    /// Show `item` playing from its start right after a skip, until Spotify reports the new playback.
    /// Returns whether it's shown.
    pub fn show_skipped_to(&mut self, item: rspotify::model::PlayableItem) -> bool {
        // an item that didn't parse as a track or an episode can't be shown
        let (Some(playback), false) = (
            self.playback.as_mut(),
            matches!(item, rspotify::model::PlayableItem::Unknown(_)),
        ) else {
            return false;
        };
        // the queue's own `currently_playing` stays, so a new queue is requested for this track
        playback.item = Some(item);
        playback.progress = Some(chrono::Duration::zero());
        self.playback_last_updated_time = Some(std::time::Instant::now());
        true
    }

    /// Show the first queued item playing right after a Next. Returns whether another track is shown.
    pub fn skip_to_queued(&mut self) -> bool {
        let next = self
            .queue
            .as_mut()
            .filter(|queue| !queue.queue.is_empty())
            .map(|queue| queue.queue.remove(0));
        next.is_some_and(|next| self.show_skipped_to(next))
    }

    /// Show what a Previous plays right away: `previous`, with the current track queued again.
    /// Returns whether another track is shown.
    pub fn skip_back(&mut self, previous: Option<rspotify::model::PlayableItem>) -> bool {
        let (Some(previous), Some(current)) = (previous, self.currently_playing().cloned()) else {
            return false;
        };
        if let Some(queue) = self.queue.as_mut() {
            queue.queue.insert(0, current);
        }
        self.show_skipped_to(previous)
    }

    /// Show the playback a device reports as it changes (Soloist's events): `item`, playing or
    /// not, at `position_ms`. Returns whether `item` is another track than the one shown, or `None`
    /// when no playback is known yet.
    pub fn show_device_playback(
        &mut self,
        item: rspotify::model::PlayableItem,
        playing: bool,
        position_ms: Option<i64>,
    ) -> Option<bool> {
        fn has_cover(item: Option<&rspotify::model::PlayableItem>) -> bool {
            matches!(item, Some(rspotify::model::PlayableItem::Track(track)) if !track.album.images.is_empty())
        }
        self.playback.as_ref()?;
        if self.is_before_request(Some(&item)) {
            return Some(false);
        }
        // a track shown from a list may lack its cover, which the device's report has
        let changed = item_uri(self.currently_playing()) != item_uri(Some(&item))
            || (!has_cover(self.currently_playing()) && has_cover(Some(&item)));
        if changed {
            let queue = self.queue.as_mut().map(|queue| &mut queue.queue);
            if let Some(queue) =
                queue.filter(|queue| item_uri(queue.first()) == item_uri(Some(&item)))
            {
                queue.remove(0);
            }
            self.show_skipped_to(item);
        }

        let playback = self.playback.as_mut()?;
        playback.is_playing = playing;
        if let Some(position_ms) = position_ms {
            playback.progress = Some(chrono::Duration::milliseconds(position_ms));
            self.playback_last_updated_time = Some(std::time::Instant::now());
        }
        if let Some(buffered) = self.buffered_playback.as_mut() {
            buffered.is_playing = playing;
        }
        Some(changed)
    }

    /// Mark the shown track as shown on request, in place of the track `before`, so reports of
    /// that track, or of those earlier requests in a quick series replaced, don't bring them back
    /// while the device hasn't followed yet
    pub fn mark_requested(&mut self, before: Option<&rspotify::model::PlayableItem>) {
        let Some(uri) = item_uri(self.currently_playing()) else {
            return;
        };
        let mut earlier = match self.requested.take() {
            Some(pending) if pending.at.elapsed() < REQUEST_GRACE => pending.before,
            _ => Vec::new(),
        };
        earlier.extend(item_uri(before));
        earlier.retain(|earlier| *earlier != uri);
        self.requested = Some(Requested {
            before: earlier,
            at: std::time::Instant::now(),
        });
    }

    /// Whether a report of `item` (from the device, or Spotify) is of a track a request replaced,
    /// which the device keeps reporting until it follows. Any other track ends the wait: the
    /// requested one, or another the device went to instead.
    pub fn is_before_request(&mut self, item: Option<&rspotify::model::PlayableItem>) -> bool {
        let Some(requested) = &self.requested else {
            return false;
        };
        let waiting = requested.at.elapsed() < REQUEST_GRACE
            && item_uri(item).is_some_and(|reported| requested.before.contains(&reported));
        if !waiting {
            self.requested = None;
        }
        waiting
    }

    /// Set the queue Spotify reports. Right after a track change Spotify may still list the playing
    /// track ahead: the queue then starts after it.
    pub fn set_queue(&mut self, mut queue: rspotify::model::CurrentUserQueue) {
        let current = item_uri(self.currently_playing());
        if item_uri(queue.currently_playing.as_ref()) != current {
            if let Some(i) = queue
                .queue
                .iter()
                .position(|item| item_uri(Some(item)) == current)
            {
                queue.queue.drain(..=i);
            }
        }
        self.queue = Some(queue);
    }

    /// The items Spotify plays after the current one, once its queue is known for that item
    pub fn queue_after_current(&self) -> Option<&[rspotify::model::PlayableItem]> {
        let queue = self.queue.as_ref()?;
        let current = item_uri(self.currently_playing())?;
        (item_uri(queue.currently_playing.as_ref()) == Some(current)).then_some(&queue.queue)
    }

    pub fn currently_playing(&self) -> Option<&rspotify::model::PlayableItem> {
        self.playback.as_ref().and_then(|p| p.item.as_ref())
    }

    pub fn playback_progress(&self) -> Option<chrono::Duration> {
        match self.playback {
            None => None,
            Some(ref playback) => {
                let progress = playback.progress.unwrap()
                    + if playback.is_playing {
                        chrono::Duration::from_std(
                            self.playback_last_updated_time.unwrap().elapsed(),
                        )
                        .ok()?
                    } else {
                        chrono::Duration::zero()
                    };
                Some(progress)
            }
        }
    }

    pub fn playing_context_id(&self) -> Option<ContextId> {
        match self.playback {
            Some(ref playback) => match playback.context {
                Some(ref context) => ContextId::from_context(context),
                None => self
                    .custom_queue
                    .as_ref()
                    .and_then(|q| q.source_context().cloned())
                    .or_else(|| {
                        self.currently_playing_tracks_id
                            .clone()
                            .map(ContextId::Tracks)
                    }),
            },
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use rspotify::model::{CurrentUserQueue, PlayableItem};
    use serde_json::json;
    use soloist_history::Record;

    use super::PlayerState;
    use crate::state::played_history::full_track;

    fn track(id: &str) -> PlayableItem {
        let record = Record {
            id: id.repeat(22),
            name: id.to_owned(),
            artists: vec![("R".repeat(22), "Artist".to_owned())],
            album_id: "L".repeat(22),
            album: "Album".to_owned(),
            duration_ms: 200_000,
            ..Record::default()
        };
        PlayableItem::Track(full_track(&record).unwrap())
    }

    /// Track A playing, B and C queued
    fn player() -> PlayerState {
        let mut playback: rspotify::model::CurrentPlaybackContext = serde_json::from_value(json!({
            "device": {"id": "box", "is_active": true, "is_private_session": false, "is_restricted": false,
                       "name": "box-spot", "type": "Computer", "volume_percent": 50},
            "repeat_state": "off", "shuffle_state": false, "context": null, "timestamp": 0,
            "progress_ms": 90_000, "is_playing": true, "item": null,
            "currently_playing_type": "track", "actions": {"disallows": {}},
        }))
        .unwrap();
        playback.item = Some(track("A"));
        PlayerState {
            playback: Some(playback),
            playback_last_updated_time: Some(std::time::Instant::now()),
            queue: Some(CurrentUserQueue {
                currently_playing: Some(track("A")),
                queue: vec![track("B"), track("C")],
            }),
            ..PlayerState::default()
        }
    }

    fn shown_name(player: &PlayerState) -> String {
        match player.currently_playing() {
            Some(PlayableItem::Track(track)) => track.name.clone(),
            _ => String::new(),
        }
    }

    #[test]
    fn a_device_moving_on_shows_the_track_and_drops_it_from_the_queue() {
        let mut player = player();
        assert_eq!(
            player.show_device_playback(track("B"), true, Some(0)),
            Some(true)
        );
        assert_eq!(shown_name(&player), "B");
        let queue = &player.queue.as_ref().unwrap().queue;
        assert_eq!(queue.len(), 1);
        assert!(player.playback_progress().unwrap().num_seconds() < 1);
    }

    #[test]
    fn a_device_pausing_keeps_the_track() {
        let mut player = player();
        assert_eq!(
            player.show_device_playback(track("A"), false, Some(95_000)),
            Some(false)
        );
        assert_eq!(shown_name(&player), "A");
        assert!(!player.playback.as_ref().unwrap().is_playing);
        assert_eq!(player.playback_progress().unwrap().num_seconds(), 95);
        assert_eq!(player.queue.as_ref().unwrap().queue.len(), 2);
    }

    #[test]
    fn a_device_report_fills_in_the_cover_of_a_track_shown_without_one() {
        let mut player = player();
        let with_cover = |item: PlayableItem| match item {
            PlayableItem::Track(mut track) => {
                track.album.images = vec![rspotify::model::Image {
                    url: "https://i.scdn.co/image/cover".to_owned(),
                    height: Some(640),
                    width: Some(640),
                }];
                PlayableItem::Track(track)
            }
            item => item,
        };
        assert_eq!(
            player.show_device_playback(with_cover(track("A")), true, Some(91_000)),
            Some(true)
        );
        assert_eq!(
            player.show_device_playback(with_cover(track("A")), true, Some(92_000)),
            Some(false)
        );
        assert_eq!(shown_name(&player), "A");
        assert_eq!(player.queue.as_ref().unwrap().queue.len(), 2);
    }

    #[test]
    fn a_quick_series_of_requests_holds_every_track_it_passed() {
        let mut player = player();
        let a = player.currently_playing().cloned();
        assert!(player.skip_to_queued()); // B
        player.mark_requested(a.as_ref());
        let b = player.currently_playing().cloned();
        assert!(player.skip_to_queued()); // C
        player.mark_requested(b.as_ref());
        // the device reports A, then B, on its way to C
        assert_eq!(
            player.show_device_playback(track("A"), true, Some(0)),
            Some(false)
        );
        assert_eq!(
            player.show_device_playback(track("B"), true, Some(0)),
            Some(false)
        );
        assert_eq!(shown_name(&player), "C");
        assert_eq!(
            player.show_device_playback(track("C"), true, Some(0)),
            Some(false)
        );
        assert!(player.requested.is_none());
    }

    #[test]
    fn a_device_going_elsewhere_than_requested_wins_at_once() {
        let mut player = player();
        let before = player.currently_playing().cloned();
        assert!(player.skip_to_queued());
        player.mark_requested(before.as_ref());
        assert_eq!(
            player.show_device_playback(track("C"), true, Some(0)),
            Some(true)
        );
        assert_eq!(shown_name(&player), "C");
        assert!(player.requested.is_none());
    }

    #[test]
    fn a_queue_still_listing_the_playing_track_starts_after_it() {
        let mut player = player();
        assert!(player.skip_to_queued()); // B shows; Spotify's queue still lists B, C
        player.set_queue(CurrentUserQueue {
            currently_playing: Some(track("A")),
            queue: vec![track("B"), track("C")],
        });
        assert_eq!(player.queue.as_ref().unwrap().queue.len(), 1);
        assert!(player.skip_to_queued());
        assert_eq!(shown_name(&player), "C");
    }

    #[test]
    fn a_requested_track_stays_until_the_device_plays_it() {
        let mut player = player();
        let before = player.currently_playing().cloned();
        assert!(player.skip_to_queued());
        player.mark_requested(before.as_ref());
        // the device still reports the track before
        assert_eq!(
            player.show_device_playback(track("A"), true, Some(91_000)),
            Some(false)
        );
        assert_eq!(shown_name(&player), "B");
        // then it plays the requested one
        assert_eq!(
            player.show_device_playback(track("B"), true, Some(0)),
            Some(false)
        );
        assert!(player.requested.is_none());
        assert_eq!(shown_name(&player), "B");
    }

    #[test]
    fn a_request_the_device_never_follows_stops_holding_the_track() {
        let mut player = player();
        assert!(player.skip_to_queued());
        let long_ago = std::time::Instant::now() - std::time::Duration::from_secs(60);
        player.requested = Some(super::Requested {
            before: vec![track_uri("A")],
            at: long_ago,
        });
        assert_eq!(
            player.show_device_playback(track("A"), true, Some(91_000)),
            Some(true)
        );
        assert_eq!(shown_name(&player), "A");
    }

    fn track_uri(id: &str) -> String {
        format!("spotify:track:{}", id.repeat(22))
    }

    #[test]
    fn without_a_playback_it_is_left_to_spotify() {
        let mut player = PlayerState::default();
        assert_eq!(player.show_device_playback(track("A"), true, Some(0)), None);
        assert!(player.currently_playing().is_none());
    }
}
