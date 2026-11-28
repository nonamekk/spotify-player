//! Restarting Soloist after the system slept. Soloist survives a long suspend or hibernation, but
//! its connection to Spotify comes back half working: Spotify can't reach the device, and Soloist
//! doesn't notice. A restart connects it again.

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

/// The shortest sleep after which Soloist is restarted
pub const MIN_SLEEP: Duration = Duration::from_secs(60);

/// Notices that the system slept: the time since boot counts the sleep, `Instant` doesn't
pub struct SleepWatch {
    /// The time since boot and the `Instant` at the last check
    last: (Duration, Instant),
}

impl SleepWatch {
    pub fn new() -> Result<Self> {
        Ok(Self {
            last: (since_boot()?, Instant::now()),
        })
    }

    /// How long the system slept since the last check, when it was `MIN_SLEEP` or longer
    pub fn check(&mut self) -> Result<Option<Duration>> {
        let now = (since_boot()?, Instant::now());
        let slept = slept(
            (self.last.0, Duration::ZERO),
            (now.0, now.1.duration_since(self.last.1)),
        );
        self.last = now;
        Ok((slept >= MIN_SLEEP).then_some(slept))
    }
}

/// Time slept between two readings of (time since boot, time awake)
fn slept(before: (Duration, Duration), after: (Duration, Duration)) -> Duration {
    let passed = after.0.saturating_sub(before.0);
    let awake = after.1.saturating_sub(before.1);
    passed.saturating_sub(awake)
}

/// The time since boot, sleep included (`CLOCK_BOOTTIME`), from `/proc/uptime`
fn since_boot() -> Result<Duration> {
    let uptime = std::fs::read_to_string("/proc/uptime").context("read /proc/uptime")?;
    let seconds = uptime
        .split_whitespace()
        .next()
        .and_then(|seconds| seconds.parse::<f64>().ok())
        .context("parse /proc/uptime")?;
    Ok(Duration::from_secs_f64(seconds))
}

/// Restart the user unit `unit` if it runs, without waiting for it
pub fn restart(unit: &str) -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "--no-block", "try-restart", unit])
        .status()
        .context("run systemctl")?;
    anyhow::ensure!(status.success(), "systemctl try-restart {unit}: {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::slept;
    use std::time::Duration;

    #[test]
    fn the_sleep_is_the_time_since_boot_the_process_was_not_awake_for() {
        let s = Duration::from_secs;
        // awake the whole time
        assert_eq!(slept((s(100), s(0)), (s(101), s(1))), s(0));
        // 12 hours hibernated between two reads a second apart while awake
        assert_eq!(
            slept((s(100), s(0)), (s(100 + 43_200 + 1), s(1))),
            s(43_200)
        );
        // the clocks' readings can differ by a little in either direction
        assert_eq!(
            slept((s(100), s(0)), (s(101), Duration::from_millis(1_010))),
            s(0)
        );
    }
}
