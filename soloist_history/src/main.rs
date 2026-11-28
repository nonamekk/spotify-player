//! `soloist-history`: follows the tracks a Spotify Soloist device plays, through Soloist's local
//! WebSocket API (`soloist -w 127.0.0.1:0`), appends them to `spotify_player`'s played history, and
//! presents Soloist to the desktop as an MPRIS player.
//!
//! A track is written when another one starts, or when the listener ends: Soloist closed the
//! connection (it restarted; the service starts the listener again), or the service stopped
//! (SIGTERM). Either way the track playing at that moment counts until then.
//!
//! With `--restart-after-resume <unit>`, it restarts Soloist's user unit after the system slept
//! for a minute or more, since Soloist's connection to Spotify doesn't come back from it whole.

mod follower;
mod mpris;
mod resume;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use soloist_history::Record;
use tungstenite::Message;

use follower::Follower;

/// Records over `played_history_size` at which the history is trimmed back to it
const TRIM_SLACK: usize = 100;

fn main() -> ExitCode {
    // to stderr, which the service manager keeps in its journal
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let restart_unit = restart_unit(std::env::args().skip(1))?;
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME isn't set")?);
    let cache = home.join(".cache/spotify-player");
    std::fs::create_dir_all(&cache).with_context(|| format!("create {}", cache.display()))?;
    let mut history = History::open(
        cache.join(soloist_history::FILE_NAME),
        history_size(&home.join(".config/spotify-player/app.toml")),
        TRIM_SLACK,
    )?;

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&stop))?;
    }

    let data_dir = soloist_history::soloist::data_dir(&home);
    let mut socket = soloist_history::soloist::connect(&data_dir)?;
    let mut follower = Follower::default();
    // the history doesn't need the media controls: without a session bus, it goes on without them
    let mut mpris = mpris::Mpris::new(data_dir)
        .inspect_err(|err| tracing::warn!("No MPRIS player for Soloist: {err:#}"))
        .ok();
    // the history doesn't need the restart either: without the clock, it goes on without it
    let mut sleep_watch = restart_unit.as_ref().and_then(|_| {
        resume::SleepWatch::new()
            .inspect_err(|err| tracing::warn!("Soloist won't be restarted after a sleep: {err:#}"))
            .ok()
    });
    while !stop.load(Ordering::Relaxed) {
        // reads time out every second, so this runs within a second of a resume
        if let (Some(watch), Some(unit)) = (sleep_watch.as_mut(), restart_unit.as_deref()) {
            if let Some(slept) = watch.check().unwrap_or_default() {
                tracing::info!(
                    "The system slept for {} s: restarting {unit}",
                    slept.as_secs()
                );
                // Soloist closes the connection as it restarts, which ends this loop
                if let Err(err) = resume::restart(unit) {
                    tracing::warn!("Failed to restart Soloist after the sleep: {err:#}");
                }
            }
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                let Ok(event) = serde_json::from_str(&text) else {
                    continue;
                };
                if let Some(record) = follower.handle(now_ms(), &event) {
                    history.write(&record)?;
                }
                if let Some(Err(err)) = mpris.as_mut().map(|mpris| mpris.handle(&event)) {
                    tracing::warn!("Failed to show Soloist's playback over MPRIS: {err:#}");
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            // no event within the read timeout, or a stop signal interrupted the wait
            Err(err) if soloist_history::soloist::is_wait(&err) => {}
            Err(err) => {
                tracing::info!("The connection to Soloist ended: {err:#}");
                break;
            }
        }
    }

    if let Some(record) = follower.finish(now_ms()) {
        history.write(&record)?;
    }
    Ok(())
}

/// The history file: `size` to `size + slack - 1` records once it's full, trimmed back to `size`
/// when it reaches `size + slack`
struct History {
    path: PathBuf,
    size: usize,
    slack: usize,
    /// Records in the file; the listener is its only writer
    records: usize,
}

impl History {
    fn open(path: PathBuf, size: usize, slack: usize) -> Result<Self> {
        let records = soloist_history::tidy(&path, size, slack).context("tidy the history")?;
        Ok(Self {
            path,
            size,
            slack,
            records,
        })
    }

    fn write(&mut self, record: &Record) -> Result<()> {
        soloist_history::append(&self.path, record)
            .with_context(|| format!("append to {}", self.path.display()))?;
        self.records += 1;
        if self.records >= self.size + self.slack {
            self.records = soloist_history::tidy(&self.path, self.size, self.slack)
                .context("trim the history")?;
        }
        Ok(())
    }
}

/// The unit given with `--restart-after-resume`, the only argument
fn restart_unit(mut args: impl Iterator<Item = String>) -> Result<Option<String>> {
    let mut unit = None;
    while let Some(arg) = args.next() {
        anyhow::ensure!(
            arg == "--restart-after-resume",
            "unknown argument {arg}; usage: soloist-history [--restart-after-resume <unit>]"
        );
        unit = Some(args.next().context("--restart-after-resume needs a unit")?);
    }
    Ok(unit)
}

/// `played_history_size` of `spotify_player`'s app.toml
fn history_size(app_config: &Path) -> usize {
    std::fs::read_to_string(app_config)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|config| config.get("played_history_size")?.as_integer())
        .and_then(|size| usize::try_from(size).ok())
        .unwrap_or(soloist_history::DEFAULT_SIZE)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::{restart_unit, History};
    use soloist_history::{parse, Record};

    fn record(i: u64) -> Record {
        Record {
            played_at: i,
            ..Record::default()
        }
    }

    #[test]
    fn reads_the_unit_to_restart() {
        let args = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(restart_unit(args(&[]).into_iter()).unwrap(), None);
        assert_eq!(
            restart_unit(args(&["--restart-after-resume", "soloist.service"]).into_iter())
                .unwrap()
                .as_deref(),
            Some("soloist.service")
        );
        assert!(restart_unit(args(&["--restart-after-resume"]).into_iter()).is_err());
        assert!(restart_unit(args(&["--other"]).into_iter()).is_err());
    }

    #[test]
    fn the_history_stays_under_size_plus_slack() {
        let path = std::env::temp_dir().join(format!(
            "soloist-history-trim-{}.msgpack",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let count = || parse(&std::fs::read(&path).unwrap()).0.len();

        let mut history = History::open(path.clone(), 3, 2).unwrap();
        for i in 0..6 {
            history.write(&record(i)).unwrap();
            assert!(count() < 5, "{} records after {} written", count(), i + 1);
        }
        // a restart counts what's in the file: 2 more reach 3 + 2
        let mut history = History::open(path.clone(), 3, 2).unwrap();
        assert_eq!(history.records, 4);
        history.write(&record(6)).unwrap();
        assert_eq!(count(), 3);
        assert_eq!(
            parse(&std::fs::read(&path).unwrap()).0,
            [record(4), record(5), record(6)]
        );
        std::fs::remove_file(&path).unwrap();
    }
}
