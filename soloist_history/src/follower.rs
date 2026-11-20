//! The plays in Soloist's events

use serde_json::Value;
use soloist_history::soloist::track_record;
use soloist_history::Record;

/// A position this close to a track's start, after it has played longer, is the track again
const REPLAY_POSITION_MS: u64 = 1500;
/// Shorter plays are event noise (a track change racing a position update), not listening
const MIN_RECORDED_MS: u64 = 1000;

/// A track being played, and how long it has played so far
struct Play {
    uri: String,
    /// The record of the track, `None` for what isn't a track
    record: Option<Record>,
    played_at: u64,
    listened_ms: u64,
    playing_since: Option<u64>,
}

impl Play {
    fn new(uri: &str, record: Option<Record>, now: u64) -> Self {
        Self {
            uri: uri.to_owned(),
            record,
            played_at: now,
            listened_ms: 0,
            playing_since: None,
        }
    }

    fn set_playing(&mut self, playing: bool, now: u64) {
        match (playing, self.playing_since) {
            (true, None) => {
                if self.listened_ms == 0 {
                    self.played_at = now;
                }
                self.playing_since = Some(now);
            }
            (false, Some(since)) => {
                self.listened_ms += now.saturating_sub(since);
                self.playing_since = None;
            }
            _ => {}
        }
    }

    fn listened(&self, now: u64) -> u64 {
        self.listened_ms
            + self
                .playing_since
                .map_or(0, |since| now.saturating_sub(since))
    }

    /// The same track played again from its start
    fn again(&self, now: u64) -> Self {
        let mut play = Self::new(&self.uri, self.record.clone(), now);
        play.set_playing(true, now);
        play
    }
}

/// Follows the plays of a Soloist device through its events
#[derive(Default)]
pub struct Follower {
    play: Option<Play>,
}

impl Follower {
    /// Follow an event received at `now` (milliseconds since the Unix epoch); returns the record
    /// of the play it ends, if any
    pub fn handle(&mut self, now: u64, event: &Value) -> Option<Record> {
        match event["type"].as_str()? {
            "playback_state" => self.playback_state(now, event),
            "device_changed" if event["is_active"] == false => {
                if let Some(play) = &mut self.play {
                    play.set_playing(false, now);
                }
                None
            }
            "position_sync" => self.position_sync(now, event),
            _ => None,
        }
    }

    /// End the open play at `now`; returns its record if it's a track that played
    pub fn finish(&mut self, now: u64) -> Option<Record> {
        let mut play = self.play.take()?;
        play.set_playing(false, now);
        let record = play.record?;
        (play.listened_ms >= MIN_RECORDED_MS).then_some(Record {
            played_at: play.played_at,
            listened_ms: play.listened_ms,
            ..record
        })
    }

    fn playback_state(&mut self, now: u64, event: &Value) -> Option<Record> {
        let item = &event["item"];
        let uri = item["uri"].as_str();
        let ended = if self
            .play
            .as_ref()
            .is_some_and(|play| Some(play.uri.as_str()) != uri)
        {
            self.finish(now)
        } else {
            None
        };

        if let (None, Some(uri)) = (&self.play, uri) {
            let context = event["context"]["uri"].as_str();
            self.play = Some(Play::new(uri, track_record(item, context), now));
        }
        if let Some(play) = &mut self.play {
            play.set_playing(
                event["status"] == "playing" && event["is_active"] != false,
                now,
            );
        }
        ended
    }

    /// The same track again (repeat, or seeking back to its start) is another play
    fn position_sync(&mut self, now: u64, event: &Value) -> Option<Record> {
        let position = event["position"]["position_ms"].as_u64()?;
        let play = self.play.as_ref()?;
        if position >= REPLAY_POSITION_MS || play.listened(now) <= 2 * REPLAY_POSITION_MS {
            return None;
        }
        let again = play.again(now);
        let ended = self.finish(now);
        self.play = Some(again);
        ended
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::Follower;

    const START: u64 = 1_795_122_000_000;

    /// A `playback_state` event of track `letter` with the fields Soloist sends
    fn track(letter: &str, status: &str, active: bool) -> Value {
        let named = |kind: &str, id: &str, name: &str| json!({"uri": format!("spotify:{kind}:{id}"), "entity_type": kind, "decorations": {"identity": {"name": name}}});
        let covers: Vec<_> = ["small", "default", "large", "xlarge"]
            .iter()
            .map(|size| json!({"url": format!("https://i.scdn.co/image/{letter}{size}"), "size": size}))
            .collect();
        json!({
            "type": "playback_state",
            "status": status,
            "is_active": active,
            "item": {
                "uri": format!("spotify:track:{}", letter.repeat(22)),
                "entity_type": "track",
                "decorations": {
                    "identity": {"name": letter},
                    "visual_identity": {"cover": covers},
                    "parent": {"entity": named("album", &"L".repeat(22), "Album")},
                    "creators": [{"entity": named("artist", &"R".repeat(22), letter)}],
                    "playback": {"duration_ms": 241_678, "content_ratings": []},
                },
            },
            "context": {"uri": format!("spotify:playlist:{}", "P".repeat(22)), "entity_type": "playlist"},
        })
    }

    fn position(ms: u64) -> Value {
        json!({"type": "position_sync", "position": {"position_ms": ms, "timestamp_ms": 0, "speed": 1}})
    }

    #[test]
    fn records_how_long_each_track_played_here() {
        let events = [
            (0, track("A", "paused", true)), // paused at startup: starts counting once playing
            (1_000, track("A", "playing", true)),
            (21_000, track("A", "paused", true)), // 20 s
            (60_000, track("A", "playing", true)),
            (65_000, position(0)), // the next track's position racing its state: no entry of its own
            (65_010, track("B", "playing", true)), // A: 25 s
            (
                70_000,
                json!({"type": "device_changed", "is_active": false}),
            ), // B: 5 s, then elsewhere
            (900_000, track("B", "playing", true)), // back on this device
            (903_000, track("C", "playing", true)), // B: 5 s + 3 s
            (943_000, position(200)), // C repeated after 40 s
            (950_000, track("D", "playing", true)), // C again: 7 s
        ];
        let mut follower = Follower::default();
        let mut records: Vec<_> = events
            .iter()
            .filter_map(|(t, event)| follower.handle(START + t, event))
            .collect();
        // D is still playing when the trace ends, 30 s later
        records.extend(follower.finish(START + 980_000));

        let plays: Vec<_> = records
            .iter()
            .map(|r| (r.name.as_str(), r.played_at - START, r.listened_ms))
            .collect();
        assert_eq!(
            plays,
            [
                ("A", 1_000, 25_000),
                ("B", 65_010, 7_990),
                ("C", 903_000, 40_000),
                ("C", 943_000, 7_000),
                ("D", 950_000, 30_000)
            ]
        );

        let a = &records[0];
        assert_eq!(
            (a.id.as_str(), &a.artists),
            (
                "A".repeat(22).as_str(),
                &vec![("R".repeat(22), "A".to_owned())]
            )
        );
        assert_eq!(
            (a.album_id.as_str(), a.album.as_str()),
            ("L".repeat(22).as_str(), "Album")
        );
        assert_eq!(a.cover.as_deref(), Some("Alarge"));
        assert_eq!(
            a.context,
            Some(format!("spotify:playlist:{}", "P".repeat(22)))
        );
        assert_eq!((a.duration_ms, a.explicit), (241_678, false));
    }

    #[test]
    fn a_paused_track_that_never_played_isnt_recorded() {
        let mut follower = Follower::default();
        assert_eq!(follower.handle(START, &track("A", "paused", true)), None);
        assert_eq!(follower.finish(START + 60_000), None);
    }
}
