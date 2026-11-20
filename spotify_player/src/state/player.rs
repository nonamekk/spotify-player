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
}
