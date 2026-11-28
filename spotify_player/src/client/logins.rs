//! The `g A` page's work: when the app's Spotify logins end, signing in again from the app, and the
//! daily desktop notice when one is about to end

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use rspotify::clients::{BaseClient as _, OAuthClient as _};

use super::spotify::PkceWebApiClient;
use super::AppClient;
use crate::auth;
use crate::config;
use crate::state::logins::{self, Login, LoginRecord, SignIn};
use crate::state::SharedState;

/// How often the logins are checked for the daily notice
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_hours(1);

/// A sign-in waiting for the browser's redirect or a pasted one
pub(super) struct PendingSignIn {
    login: usize,
    /// A copy of the login's client: it shares the token with the app's, and holds this
    /// sign-in's PKCE verifier
    client: PkceWebApiClient,
    /// Stops the redirect listener
    stop: Arc<AtomicBool>,
}

impl AppClient {
    /// The app's Spotify logins, with when each was authorized
    async fn logins(&self) -> Vec<Login> {
        let cache_folder = &config::get_config().cache_folder;
        let record = LoginRecord::load(cache_folder);
        let mut logins = Vec::new();
        for client in self.api_client.clients() {
            let client_id = client.get_creds().id.clone();
            let renewable = client
                .get_token()
                .lock()
                .await
                .is_ok_and(|token| token.as_ref().is_some_and(|t| t.refresh_token.is_some()));
            let authorized_at = record
                .authorized_at
                .get(&client_id)
                .copied()
                .or_else(|| made_at(&client.get_config().cache_path));
            logins.push(Login {
                name: if client_id == auth::NCSPOT_CLIENT_ID {
                    "ncspot's client"
                } else {
                    "your Spotify app"
                },
                client_id,
                authorized_at,
                renewable,
            });
        }
        logins
    }

    /// Fill the `g A` page: the logins and the Soloist on this machine
    pub async fn get_logins(&self, state: &SharedState) {
        let logins = self.logins().await;
        let soloist = tokio::task::spawn_blocking(super::soloist::login)
            .await
            .ok()
            .flatten();
        let mut data = state.data.write();
        data.logins.web_api = logins;
        data.logins.soloist = soloist;
    }

    /// Start signing in to the `login`-th login again: send the browser to Spotify's
    /// authorization page and wait for its redirect, which a pasted one can stand in for
    pub fn start_sign_in(&self, state: &SharedState, login: usize) -> Result<()> {
        self.cancel_sign_in(state);
        let mut client = self
            .api_client
            .clients()
            .get(login)
            .map(|client| (*client).clone())
            .context("no such login")?;
        let url = client.get_authorize_url(None)?;
        open::that_in_background(&url);
        // for opening it by hand when no browser came up
        if let Err(err) = crate::event::clipboard::execute_copy_command(url.clone()) {
            tracing::warn!("Failed to copy the sign-in address: {err:#}");
        }
        tracing::info!("Signing in to {} again: {url}", client.get_creds().id);

        let stop = Arc::new(AtomicBool::new(false));
        let addr = auth::redirect_socket_address(&client.get_oauth().redirect_uri);
        *self.sign_in.lock() = Some(PendingSignIn {
            login,
            client,
            stop: Arc::clone(&stop),
        });
        state.data.write().logins.sign_in = Some(SignIn::Waiting { login, url });

        // without a local redirect address, only a pasted redirect finishes it
        if let Some(addr) = addr {
            let client = self.clone();
            let state = state.clone();
            tokio::spawn(async move {
                let listened = tokio::task::spawn_blocking(move || {
                    auth::listen_for_auth_code_until(addr, &stop)
                })
                .await;
                match listened {
                    Ok(Ok(Some(code))) => client.finish_sign_in(&state, &code).await,
                    Ok(Ok(None)) => {}
                    Ok(Err(err)) => {
                        tracing::warn!("No redirect from the browser: {err:#}; paste it instead");
                    }
                    Err(err) => tracing::warn!("The redirect listener failed: {err:#}"),
                }
            });
        }
        Ok(())
    }

    /// Finish the sign-in with what was pasted: the address the browser was sent back to, or its
    /// code
    pub async fn finish_sign_in_pasted(&self, state: &SharedState, text: &str) {
        if let Some(code) = logins::pasted_code(text) {
            self.finish_sign_in(state, &code).await;
        } else {
            tracing::warn!("No auth code in the pasted text");
        }
    }

    /// Exchange `code` for the login's new token, which the app uses from then on
    async fn finish_sign_in(&self, state: &SharedState, code: &str) {
        let Some(pending) = self.sign_in.lock().take() else {
            return;
        };
        pending.stop.store(true, Ordering::Relaxed);
        let result = async {
            pending.client.request_token(code).await?;
            pending.client.write_token_cache().await?;
            anyhow::Ok(())
        }
        .await;
        let sign_in = match result {
            Ok(()) => {
                logins::record_authorized(
                    &config::get_config().cache_folder,
                    &pending.client.get_creds().id,
                    Utc::now(),
                );
                tracing::info!("Signed in to {} again", pending.client.get_creds().id);
                SignIn::Done {
                    login: pending.login,
                }
            }
            Err(err) => {
                tracing::warn!("Failed to sign in again: {err:#}");
                SignIn::Failed {
                    login: pending.login,
                    error: format!("{err:#}"),
                }
            }
        };
        state.data.write().logins.sign_in = Some(sign_in);
        self.get_logins(state).await;
    }

    /// Give up a sign-in that's waiting
    pub fn cancel_sign_in(&self, state: &SharedState) {
        if let Some(pending) = self.sign_in.lock().take() {
            pending.stop.store(true, Ordering::Relaxed);
        }
        let mut data = state.data.write();
        if matches!(data.logins.sign_in, Some(SignIn::Waiting { .. })) {
            data.logins.sign_in = None;
        }
    }

    /// Tell the desktop once a day when a login ends within `WARN_DAYS`, whatever the app's
    /// notification settings. Runs as long as the app.
    pub async fn notify_ending_logins(&self, state: &SharedState) {
        loop {
            self.get_logins(state).await;
            let now = Utc::now();
            let today = chrono::Local::now().date_naive();
            let days_left = state.data.read().logins.fewest_days_left(now);
            let cache_folder = &config::get_config().cache_folder;
            let mut record = LoginRecord::load(cache_folder);
            if logins::should_notify(days_left, today, record.notified_on) {
                if let Some(days) = days_left {
                    notify_login_ends(days);
                }
                record.notified_on = Some(today);
                record.store(cache_folder);
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    }
}

/// When a file was made, as the file system records it
fn made_at(path: &std::path::Path) -> Option<DateTime<Utc>> {
    std::fs::metadata(path).ok()?.created().ok().map(Into::into)
}

/// The app name of the login notice, apart from the app's other notifications so that a
/// notification daemon's rules can keep it while blocking those
const NOTICE_APP_NAME: &str = "spotify_player login";

/// The daily desktop notice; shown even when the app's own notifications are off
fn notify_login_ends(days: i64) {
    let summary = "Spotify login ends soon";
    let body = if days < 0 {
        "The Spotify login has ended. Open spotify_player and press g A to sign in again."
            .to_owned()
    } else {
        format!(
            "The Spotify login ends in {days} day{}. Open spotify_player and press g A to sign in again.",
            if days == 1 { "" } else { "s" }
        )
    };
    tracing::info!("{summary}: {body}");
    #[cfg(feature = "notify")]
    if let Err(err) = notify_rust::Notification::new()
        .appname(NOTICE_APP_NAME)
        .summary(summary)
        .body(&body)
        .show()
    {
        tracing::warn!("Failed to show the login notice: {err:#}");
    }
    #[cfg(not(feature = "notify"))]
    if let Err(err) = std::process::Command::new("notify-send")
        .args(["-a", NOTICE_APP_NAME, summary, &body])
        .status()
    {
        tracing::warn!("Failed to show the login notice: {err:#}");
    }
}
