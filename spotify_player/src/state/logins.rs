//! The `g A` page: when the app's Spotify logins end, signing in again from the app, and the
//! Soloist device's own state

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Months, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::{load_data_from_file_cache, store_data_into_file_cache, FileCacheKey};

/// Spotify ends a Web API login this long after it was authorized; refreshing its access token
/// doesn't extend it (<https://developer.spotify.com/blog/2026-06-18-refresh-token-expiration>)
pub const LOGIN_MONTHS: u32 = 6;
/// Days left from which the desktop is told, once a day
pub const WARN_DAYS: i64 = 5;
/// How long a Soloist build runs after its build date
pub const SOLOIST_BUILD_DAYS: u64 = 90;

/// Kept in `Logins_cache.json`: when each client was authorized, and the day the desktop was last
/// told that a login ends
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoginRecord {
    /// By client ID
    pub authorized_at: HashMap<String, DateTime<Utc>>,
    pub notified_on: Option<NaiveDate>,
}

impl LoginRecord {
    pub fn load(cache_folder: &Path) -> Self {
        load_data_from_file_cache(FileCacheKey::Logins, cache_folder).unwrap_or_default()
    }

    pub fn store(&self, cache_folder: &Path) {
        if let Err(err) = store_data_into_file_cache(FileCacheKey::Logins, cache_folder, self) {
            tracing::warn!("Failed to store when the logins were authorized: {err:#}");
        }
    }
}

/// Record that `client_id` was authorized at `at`
pub fn record_authorized(cache_folder: &Path, client_id: &str, at: DateTime<Utc>) {
    let mut record = LoginRecord::load(cache_folder);
    record.authorized_at.insert(client_id.to_owned(), at);
    record.store(cache_folder);
}

/// A Spotify Web API login of the app
#[derive(Clone, Debug, PartialEq)]
pub struct Login {
    /// Which of the app's clients it is
    pub name: &'static str,
    pub client_id: String,
    /// When it was authorized: as recorded, else when its token file was made
    pub authorized_at: Option<DateTime<Utc>>,
    /// Whether it has a refresh token, without which it can't be renewed
    pub renewable: bool,
}

impl Login {
    /// When Spotify ends it
    pub fn ends_at(&self) -> Option<DateTime<Utc>> {
        self.authorized_at?
            .checked_add_months(Months::new(LOGIN_MONTHS))
    }

    /// Whole days until it ends; negative once it has
    pub fn days_left(&self, now: DateTime<Utc>) -> Option<i64> {
        Some((self.ends_at()? - now).num_days())
    }
}

/// The Soloist device on this machine, as far as its own commands tell
#[derive(Clone, Debug, PartialEq)]
pub struct SoloistLogin {
    pub name: String,
    pub logged_in: bool,
    /// The day its build stops running
    pub build_expires: Option<NaiveDate>,
}

/// A sign-in started from the page
#[derive(Clone, Debug, PartialEq)]
pub enum SignIn {
    /// The browser was sent to `url`; waiting for its redirect, or for a pasted one
    Waiting {
        login: usize,
        url: String,
    },
    Done {
        login: usize,
    },
    Failed {
        login: usize,
        error: String,
    },
}

/// What the `g A` page shows
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Logins {
    /// The app's Web API logins: its configured client, then the fallback
    pub web_api: Vec<Login>,
    pub soloist: Option<SoloistLogin>,
    pub sign_in: Option<SignIn>,
}

impl Logins {
    /// The fewest days any login has left
    pub fn fewest_days_left(&self, now: DateTime<Utc>) -> Option<i64> {
        self.web_api
            .iter()
            .filter_map(|login| login.days_left(now))
            .min()
    }
}

/// Whether to tell the desktop today: a login ends within `WARN_DAYS`, and it wasn't told today
pub fn should_notify(
    days_left: Option<i64>,
    today: NaiveDate,
    notified_on: Option<NaiveDate>,
) -> bool {
    days_left.is_some_and(|days| days <= WARN_DAYS) && notified_on != Some(today)
}

/// The auth code in what was pasted: the address the browser was sent back to, or the code alone
pub fn pasted_code(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.contains("code=") {
        return crate::auth::code_from_redirect(text);
    }
    // a code is URL-safe base64; anything else is a mistyped address
    text.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        .then(|| text.to_owned())
}

/// The day a Soloist build stops running, from its `--version` line, e.g.
/// `soloist 1.4.0.15 build 1795629673 (20261125) (gcedbef4d51) (linux/x86_64)`
pub fn soloist_build_expires(version: &str) -> Option<NaiveDate> {
    let date = version.split(['(', ')']).find(|part| {
        part.len() == 8 && part.starts_with("20") && part.chars().all(|c| c.is_ascii_digit())
    })?;
    NaiveDate::parse_from_str(date, "%Y%m%d")
        .ok()?
        .checked_add_days(chrono::Days::new(SOLOIST_BUILD_DAYS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().into()
    }

    #[test]
    fn a_login_ends_six_months_after_it_was_authorized() {
        let login = Login {
            name: "this app",
            client_id: "id".into(),
            authorized_at: Some(at("2026-11-19T14:43:32Z")),
            renewable: true,
        };
        assert_eq!(login.ends_at(), Some(at("2027-05-19T14:43:32Z")));
        assert_eq!(login.days_left(at("2027-05-14T14:43:32Z")), Some(5));
        assert_eq!(login.days_left(at("2027-05-20T14:43:32Z")), Some(-1));
        let unknown = Login {
            authorized_at: None,
            ..login
        };
        assert_eq!(unknown.days_left(at("2027-05-14T00:00:00Z")), None);
    }

    #[test]
    fn the_desktop_is_told_once_a_day_from_five_days_left() {
        let today = day("2027-05-14");
        assert!(should_notify(Some(5), today, None));
        assert!(should_notify(Some(-2), today, Some(day("2027-05-13"))));
        assert!(!should_notify(Some(5), today, Some(today)));
        assert!(!should_notify(Some(6), today, None));
        assert!(!should_notify(None, today, None));
    }

    #[test]
    fn a_pasted_address_or_code_gives_the_code() {
        assert_eq!(
            pasted_code("http://127.0.0.1:8989/login?code=AQB-x_1&state=s").as_deref(),
            Some("AQB-x_1")
        );
        assert_eq!(pasted_code("  AQB-x_1\n").as_deref(), Some("AQB-x_1"));
        assert_eq!(
            pasted_code("http://127.0.0.1:8989/login?error=access_denied"),
            None
        );
        assert_eq!(pasted_code(""), None);
    }

    #[test]
    fn a_soloist_build_runs_ninety_days() {
        let version = "soloist 1.4.0.15 build 1795629673 (20261125) (gcedbef4d51) (linux/x86_64)";
        assert_eq!(soloist_build_expires(version), Some(day("2027-02-23")));
        assert_eq!(soloist_build_expires("soloist"), None);
    }
}
