use std::collections::HashSet;
use std::ops::Deref;
use std::{borrow::Cow, collections::HashMap, sync::Arc};

use crate::state::Lyrics;
use crate::{auth, config};
use crate::{
    auth::AuthConfig,
    state::{
        load_data_from_file_cache, played_history, radios, store_data_into_file_cache, Album,
        AlbumId, Artist, ArtistId, Category, Context, ContextId, Device, FileCacheKey, Item,
        ItemId, MemoryCaches, PageState, Playback, PlaybackMetadata, Playlist, PlaylistFolderItem,
        PlaylistId, SearchResults, SharedState, Show, ShowId, Track, TrackId, UserId,
        TTL_CACHE_DURATION, USER_LIKED_TRACKS_ID, USER_LIKED_TRACKS_URI,
        USER_RECENTLY_PLAYED_TRACKS_URI, USER_TOP_TRACKS_URI,
    },
};

use std::io::Write;

use anyhow::Context as _;
use anyhow::Result;

use librespot_core::SpotifyUri;
use parking_lot::Mutex;

use rspotify::model::LibraryId;
use rspotify::{http::Query, prelude::*};

mod handlers;
mod liked_tracks;
mod middleware;
mod request;
mod soloist;
mod spotify;

pub use handlers::*;
use middleware::SpotifyApiMiddleware;
pub use request::*;
use serde::Deserialize;
pub(crate) use spotify::{PkceWebApiClient, WebApiClient};

fn patch_missing_track_external_ids(response: &mut serde_json::Value) {
    if let Some(item) = response.get_mut("item") {
        add_missing_external_ids(item);
    }
}

/// The queue response, parsed with the `external_ids` its tracks lack added as for the playback:
/// without them every track parses as `PlayableItem::Unknown`
fn parse_queue_response(text: &str) -> Result<rspotify::model::CurrentUserQueue> {
    let mut response: serde_json::Value = serde_json::from_str(text)?;
    if let Some(item) = response.get_mut("currently_playing") {
        add_missing_external_ids(item);
    }
    if let Some(items) = response
        .get_mut("queue")
        .and_then(serde_json::Value::as_array_mut)
    {
        items.iter_mut().for_each(add_missing_external_ids);
    }
    Ok(serde_json::from_value(response)?)
}

fn add_missing_external_ids(item: &mut serde_json::Value) {
    if item.get("type").and_then(serde_json::Value::as_str) != Some("track")
        || item.get("external_ids").is_some()
    {
        return;
    }
    if let Some(item) = item.as_object_mut() {
        item.insert(
            "external_ids".to_string(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
    }
}

fn parse_current_playback_response(
    text: &str,
) -> Result<Option<rspotify::model::CurrentPlaybackContext>> {
    if text.is_empty() {
        return Ok(None);
    }

    let mut response = serde_json::from_str(text)?;
    patch_missing_track_external_ids(&mut response);
    Ok(Some(serde_json::from_value(response)?))
}

/// The application's Spotify client
#[derive(Clone)]
pub struct AppClient {
    http: reqwest::Client,
    /// The integrated Spotify client, mainly used for streaming and librespot integration
    spotify: Arc<spotify::Spotify>,
    auth_config: AuthConfig,
    /// The Spotify Web API client, used for interacting with Spotify Web APIs
    api_client: WebApiClient,
    #[cfg(feature = "streaming")]
    stream_conn: Arc<Mutex<Option<librespot_connect::Spirc>>>,
    /// The device last chosen from this app, which `take_over_playback` leaves the playback on
    chosen_device_id: Arc<Mutex<Option<String>>>,
}

impl Deref for AppClient {
    type Target = WebApiClient;
    fn deref(&self) -> &Self::Target {
        &self.api_client
    }
}

/// Build the Spotify Web API client from the configured client ID.
///
/// The returned client is unauthenticated; call [`auth::prompt_for_user_token`] to obtain an
/// access token.
pub fn new_api_client() -> Result<WebApiClient> {
    let configs = config::get_config();

    let id = configs.app_config.get_client_id()?;
    let ncspot_only_get_endpoints = configs.app_config.ncspot_only_get_endpoints.clone();
    let mut scopes = auth::OAUTH_SCOPES
        .iter()
        .map(ToString::to_string)
        .collect::<HashSet<_>>();
    // `user-personalized` scope is not supported by the Web API client and only available to the official Spotify client
    scopes.remove("user-personalized");

    let build_config = |cache_file: &str| rspotify::Config {
        token_cached: true,
        cache_path: configs.cache_folder.join(cache_file),
        ..Default::default()
    };
    let build_oauth = |redirect_uri: String| rspotify::OAuth {
        redirect_uri,
        scopes: scopes.clone(),
        ..Default::default()
    };
    let primary_cache_file = format!("{id}_token.json");

    if id == auth::NCSPOT_CLIENT_ID {
        let config = build_config(&primary_cache_file);
        let middleware = SpotifyApiMiddleware::new(
            &config.api_base_url,
            configs.app_config.api_rate_limit_retries,
        )?;
        let client = rspotify::AuthCodePkceSpotify::with_config(
            rspotify::Credentials { id, secret: None },
            build_oauth(configs.app_config.login_redirect_uri.clone()),
            config,
        )
        .with_middleware(middleware);
        return Ok(WebApiClient::new(client, None)
            .with_ncspot_only_get_endpoints(ncspot_only_get_endpoints));
    }

    tracing::info!(
        %id,
        "Using custom Spotify Web API client with ncspot fallback"
    );
    let primary = rspotify::AuthCodePkceSpotify::with_config(
        rspotify::Credentials { id, secret: None },
        build_oauth(configs.app_config.login_redirect_uri.clone()),
        build_config(&primary_cache_file),
    );

    let fallback_cache_file = format!("{}_token.json", auth::NCSPOT_CLIENT_ID);
    let fallback_config = build_config(&fallback_cache_file);
    let fallback_middleware = SpotifyApiMiddleware::new(
        &fallback_config.api_base_url,
        configs.app_config.api_rate_limit_retries,
    )?;
    let fallback = rspotify::AuthCodePkceSpotify::with_config(
        rspotify::Credentials {
            id: auth::NCSPOT_CLIENT_ID.to_string(),
            secret: None,
        },
        build_oauth(auth::NCSPOT_REDIRECT_URI.to_string()),
        fallback_config,
    )
    .with_middleware(fallback_middleware);

    Ok(WebApiClient::new(primary, Some(fallback))
        .with_ncspot_only_get_endpoints(ncspot_only_get_endpoints))
}

impl AppClient {
    /// Construct a new client
    pub async fn new() -> Result<Self> {
        let configs = config::get_config();
        let auth_config = AuthConfig::new(configs)?;

        let mut api_client = new_api_client()?;
        auth::prompt_for_user_token(&mut api_client, false)
            .await
            .context("authenticate Spotify Web API client")?;

        Ok(Self {
            spotify: Arc::new(spotify::Spotify::new()),
            http: reqwest::Client::new(),
            auth_config,
            api_client,

            #[cfg(feature = "streaming")]
            stream_conn: Arc::new(Mutex::new(None)),
            chosen_device_id: Arc::new(Mutex::new(None)),
        })
    }

    /// Initialize the application's playback upon creating a new session or during startup.
    ///
    /// `resume` controls whether playback should be (re)started on the device we connect to.
    pub fn initialize_playback(&self, state: &SharedState, resume: bool) {
        tokio::task::spawn({
            let client = self.clone();
            let state = state.clone();
            async move {
                // The main playback initialization logic is simple:
                // if there is no playback, connect to an available device
                //
                // However, because it takes time for Spotify server to show up new changes,
                // a retry logic is implemented to ensure the application's state is properly initialized
                let max_retries = 3;
                let mut selected_playing = false;
                for i in 0..max_retries {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                    if let Err(err) = client.retrieve_current_playback(&state, false).await {
                        tracing::error!("Failed to retrieve current playback: {err:#}");
                        return;
                    }

                    // the resumed page selects the track playing now, once
                    if !selected_playing && state.player.read().playback.is_some() {
                        state.select_playing_track();
                        selected_playing = true;
                    }

                    // if playback exists, don't connect to a new device
                    if state.player.read().playback.is_some() {
                        tracing::info!("Playback already exists, skipping device connection.");
                        continue;
                    }

                    let id = match client.find_available_device(&state).await {
                        Ok(Some(id)) => Some(Cow::Owned(id)),
                        Ok(None) => None,
                        Err(err) => {
                            tracing::error!("Failed to find an available device: {err:#}");
                            None
                        }
                    };

                    if let Some(device_id) = id {
                        tracing::info!(
                            %device_id,
                            resume,
                            retry = i,
                            max_retries,
                            "Trying to connect to device"
                        );
                        if let Err(err) = client.transfer_playback(&device_id, Some(false)).await {
                            tracing::warn!(%device_id, "Connection failed: {err:#}");
                        } else {
                            tracing::info!(%device_id, "Connection succeeded!");
                            if resume {
                                if let Err(err) =
                                    client.resume_playback(Some(device_id.as_ref()), None).await
                                {
                                    tracing::warn!(
                                        "Failed to resume playback after reconnect: {err:#}"
                                    );
                                }
                            } else if let Err(err) =
                                client.restore_saved_playback(&state, &device_id).await
                            {
                                tracing::warn!("Failed to restore the saved playback: {err:#}");
                            }
                            // upon new connection, reset the buffered playback
                            state.player.write().buffered_playback = None;
                            client.update_playback_non_blocking(&state);
                            break;
                        }
                    }
                }
            }
        });
    }

    /// Create a new client session
    pub async fn new_session(&self, state: Option<&SharedState>, reauth: bool) -> Result<()> {
        // Capture whether playback was active *before* tearing down any existing streaming
        // connection. Shutting down the old `librespot` spirc pauses playback Spotify-side
        // (and a broken session leaves it paused too), so we use this to resume on the new
        // device rather than reconnecting in a paused state.
        let was_playing = state.is_some_and(|state| {
            state
                .player
                .read()
                .buffered_playback
                .as_ref()
                .is_some_and(|p| p.is_playing)
        });

        let session = self.auth_config.session();
        let creds = auth::get_creds(&self.auth_config, reauth, true).context("get credentials")?;
        self.spotify.set_session(session.clone()).await;

        #[allow(unused_mut)]
        let mut connected = false;

        #[cfg(feature = "streaming")]
        if let Some(state) = state {
            if state.is_streaming_enabled() {
                self.new_streaming_connection(state.clone(), session.clone(), creds.clone())
                    .await
                    .context("new streaming connection")?;
                connected = true;
            }
        }

        if !connected {
            // if session is not connected (triggered by `new_streaming_connection`), connect to the session
            session
                .connect(creds, true)
                .await
                .context("connect to a session")?;
        }

        tracing::info!("Used a new session for Spotify client.");

        if let Err(err) = self.refresh_token().await {
            tracing::warn!("Failed to refresh auth token after creating a new session: {err:#}");
        }

        if let Some(state) = state {
            // reset the application's caches
            state.data.write().caches = MemoryCaches::new();
            self.initialize_playback(state, was_playing);
        }

        Ok(())
    }

    /// Check if the current session is valid and if invalid, create a new session
    pub async fn check_valid_session(&self, state: &SharedState) -> Result<()> {
        if self.spotify.session().await.is_invalid() {
            tracing::info!("Client's current session is invalid, creating a new session...");
            self.new_session(Some(state), false)
                .await
                .context("create new client session")?;
        }
        Ok(())
    }

    /// Create a new streaming connection
    #[cfg(feature = "streaming")]
    pub async fn new_streaming_connection(
        &self,
        state: SharedState,
        session: librespot_core::Session,
        creds: librespot_core::authentication::Credentials,
    ) -> Result<()> {
        let new_conn =
            crate::streaming::new_connection(self.clone(), state, session, creds).await?;
        let mut stream_conn = self.stream_conn.lock();
        // shutdown old streaming connection and replace it with a new connection
        if let Some(conn) = stream_conn.as_ref() {
            if let Err(err) = conn.shutdown() {
                log::error!("Failed to shutdown old streaming connection: {err:#}");
            }
        }
        *stream_conn = Some(new_conn);
        Ok(())
    }

    /// Pause the integrated streaming client, if a connection exists.
    ///
    /// Returns `true` if a streaming connection was present and the pause
    /// command was issued. Used to suppress Spotify's auto-resume of the
    /// previous session on startup when `pause_on_startup` is enabled.
    #[cfg(feature = "streaming")]
    pub fn pause_streaming_on_startup(&self) -> bool {
        match self.stream_conn.lock().as_ref() {
            Some(spirc) => {
                if let Err(err) = spirc.pause() {
                    tracing::warn!("Failed to pause integrated client on startup: {err:#}");
                }
                true
            }
            None => false,
        }
    }

    /// Handle a player request, return a new playback metadata on success
    pub async fn handle_player_request(
        &self,
        request: PlayerRequest,
        mut playback: Option<PlaybackMetadata>,
    ) -> Result<Option<PlaybackMetadata>> {
        if !matches!(request, PlayerRequest::TransferPlayback(..)) {
            if let Some(playback) = playback.as_mut() {
                self.take_over_playback(playback).await?;
            }
        }

        // handle requests that don't require an active playback
        match request {
            PlayerRequest::TransferPlayback(device_id, force_play) => {
                // `TransferPlayback` needs to be handled separately from other player requests
                // because `TransferPlayback` doesn't require an active playback
                self.transfer_playback(&device_id, Some(force_play)).await?;
                tracing::info!("Transferred playback to device with id={}", device_id);
                *self.chosen_device_id.lock() = Some(device_id);
                return Ok(None);
            }
            PlayerRequest::StartPlayback(p, shuffle) => {
                // Set the playback's shuffle state if specified in the request
                if let (Some(shuffle), Some(playback)) = (shuffle, playback.as_mut()) {
                    playback.shuffle_state = shuffle;
                }
                let device_id = playback.as_ref().and_then(|p| p.device_id.as_deref());
                self.start_playback(p, device_id).await?;
                // For some reasons, when starting a new playback, the integrated `spotify_player`
                // client doesn't respect the initial shuffle state, so we need to manually update the state
                if let Some(ref playback) = playback {
                    self.shuffle(playback.shuffle_state, device_id).await?;
                }
                return Ok(None);
            }
            _ => {}
        }

        let mut playback = playback.context("no playback found")?;
        let device_id = playback.device_id.as_deref();

        match request {
            PlayerRequest::NextTrack => self.next_track(device_id).await?,
            PlayerRequest::PreviousTrack => self.previous_track(device_id).await?,
            PlayerRequest::Resume => {
                if !playback.is_playing {
                    self.resume_playback(device_id, None).await?;
                    playback.is_playing = true;
                }
            }

            PlayerRequest::Pause => {
                if playback.is_playing {
                    self.pause_playback(device_id).await?;
                    playback.is_playing = false;
                }
            }
            PlayerRequest::ResumePause => {
                if playback.is_playing {
                    self.pause_playback(device_id).await?;
                } else {
                    self.resume_playback(device_id, None).await?;
                }
                playback.is_playing = !playback.is_playing;
            }
            PlayerRequest::SeekTrack(position_ms) => {
                self.seek_track(position_ms, device_id).await?;
            }
            PlayerRequest::Repeat => {
                let next_repeat_state = match playback.repeat_state {
                    rspotify::model::RepeatState::Off => rspotify::model::RepeatState::Track,
                    rspotify::model::RepeatState::Track => rspotify::model::RepeatState::Context,
                    rspotify::model::RepeatState::Context => rspotify::model::RepeatState::Off,
                };

                self.repeat(next_repeat_state, device_id).await?;

                playback.repeat_state = next_repeat_state;
            }
            PlayerRequest::Shuffle => {
                self.shuffle(!playback.shuffle_state, device_id).await?;

                playback.shuffle_state = !playback.shuffle_state;
            }
            PlayerRequest::Volume(volume) => {
                self.volume(volume, device_id).await?;

                playback.volume = Some(u32::from(volume));
                playback.mute_state = None;
            }
            PlayerRequest::ToggleMute => {
                let new_mute_state = match playback.mute_state {
                    None => {
                        self.volume(0, device_id).await?;
                        Some(playback.volume.unwrap_or_default())
                    }
                    Some(volume) => {
                        self.volume(volume as u8, device_id).await?;
                        None
                    }
                };

                playback.mute_state = new_mute_state;
            }
            PlayerRequest::StartPlayback(..) => {
                anyhow::bail!("`StartPlayback` should be handled earlier")
            }
            PlayerRequest::TransferPlayback(..) => {
                anyhow::bail!("`TransferPlayback` should be handled earlier")
            }
        }

        Ok(Some(playback))
    }

    /// Move the playback to the preferred device when `take_over_playback` is enabled,
    /// so that a command from this app plays here rather than on another device.
    async fn take_over_playback(&self, playback: &mut PlaybackMetadata) -> Result<()> {
        let is_chosen =
            playback.device_id.is_some() && *self.chosen_device_id.lock() == playback.device_id;
        if !config::get_config().app_config.take_over_playback || is_chosen {
            return Ok(());
        }
        let Some(name) = preferred_device() else {
            return Ok(());
        };
        if playback.device_name == name {
            return Ok(());
        }

        let device = self
            .available_devices()
            .await?
            .into_iter()
            .find(|d| d.name == name);
        let Some(device_id) = device.and_then(|d| d.id) else {
            tracing::warn!("Preferred device {name} is not available to take over the playback");
            return Ok(());
        };

        self.transfer_playback(&device_id, Some(playback.is_playing))
            .await?;
        tracing::info!("Took over the playback on the preferred device {name}");
        playback.device_name = name;
        playback.device_id = Some(device_id);
        Ok(())
    }

    /// Restore the previous session's playback on a device, paused, if Spotify has none to resume
    async fn restore_saved_playback(&self, state: &SharedState, device_id: &str) -> Result<()> {
        let Some(saved) = state.player.write().saved_playback.take() else {
            return Ok(());
        };

        // the playback Spotify resumes on transfer takes a moment to show up
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        self.retrieve_current_playback(state, true).await?;
        if state.player.read().currently_playing().is_some() {
            return Ok(());
        }

        let tracks_id = state.player.read().currently_playing_tracks_id.clone();
        let playback = saved
            .playback(
                &state.data.read(),
                tracks_id.as_ref(),
                config::get_config().app_config.tracks_playback_limit,
            )
            .context("no playback to restore")?;
        let volume = self
            .available_devices()
            .await?
            .into_iter()
            .find(|d| d.id.as_deref() == Some(device_id))
            .and_then(|d| d.volume_percent);

        // Spotify can't start a playback paused, so it starts muted and is paused right away
        if volume.is_some() {
            self.volume(0, Some(device_id)).await?;
        }
        self.start_playback(playback, Some(device_id)).await?;
        self.pause_playback(Some(device_id)).await?;
        self.seek_track(saved.progress(), Some(device_id)).await?;
        if let Some(volume) = volume {
            self.volume(volume as u8, Some(device_id)).await?;
        }
        tracing::info!("Restored the previous session's playback");
        Ok(())
    }

    /// Handle a client request
    pub(crate) async fn handle_request(
        &self,
        state: &SharedState,
        request: ClientRequest,
    ) -> Result<()> {
        let timer = tokio::time::Instant::now();

        match request {
            ClientRequest::GetBrowseCategories => {
                let categories = self.browse_categories().await?;
                state.data.write().browse.categories = Some(categories);
            }
            ClientRequest::GetBrowseCategoryPlaylists(category) => {
                let playlists = self.browse_category_playlists(&category.id).await?;
                state
                    .data
                    .write()
                    .browse
                    .category_playlists
                    .insert(category.id, playlists);
            }
            ClientRequest::GetLyrics { track_id } => {
                let uri = track_id.uri();
                if !state.data.read().caches.lyrics.contains_key(&uri) {
                    let lyrics = self.lyrics(track_id).await?;
                    state
                        .data
                        .write()
                        .caches
                        .lyrics
                        .insert(uri, lyrics, *TTL_CACHE_DURATION);
                }
            }
            #[cfg(feature = "streaming")]
            ClientRequest::RestartIntegratedClient => {
                self.new_session(Some(state), false).await?;
            }
            ClientRequest::GetCurrentUser => {
                let user = self.current_user().await?;
                store_data_into_file_cache(
                    FileCacheKey::CurrentUser,
                    &config::get_config().cache_folder,
                    &user,
                )
                .context("store the current user into the cache folder")?;
                state.data.write().user_data.user = Some(user);
            }
            ClientRequest::Player(request) => {
                let playback = state.player.read().buffered_playback.clone();
                let playback = self.handle_player_request(request, playback).await?;
                state.player.write().buffered_playback = playback;
                self.update_playback_non_blocking(state);
            }
            ClientRequest::GetCurrentPlayback => {
                self.retrieve_current_playback(state, true).await?;
            }
            ClientRequest::GetDevices => {
                #[allow(unused_mut)]
                let mut devices: Vec<Device> = self
                    .available_devices()
                    .await?
                    .into_iter()
                    .filter_map(Device::try_from_device)
                    .collect();

                #[cfg(feature = "streaming")]
                if state.is_streaming_enabled() {
                    self.ensure_integrated_device(&mut devices).await;
                }

                state.player.write().devices = devices;
            }
            ClientRequest::GetUserPlaylists => {
                let playlists = self.current_user_playlists().await?;
                let node = state.data.read().user_data.playlist_folder_node.clone();
                let playlists = if let Some(node) = node.filter(|n| !n.children.is_empty()) {
                    crate::playlist_folders::structurize(playlists, &node.children)
                } else {
                    playlists
                        .into_iter()
                        .map(PlaylistFolderItem::Playlist)
                        .collect()
                };
                store_data_into_file_cache(
                    FileCacheKey::Playlists,
                    &config::get_config().cache_folder,
                    &playlists,
                )
                .context("store user's playlists into the cache folder")?;
                state.data.write().user_data.playlists = playlists;
            }
            ClientRequest::GetUserFollowedArtists => {
                let artists = self.current_user_followed_artists().await?;
                store_data_into_file_cache(
                    FileCacheKey::FollowedArtists,
                    &config::get_config().cache_folder,
                    &artists,
                )
                .context("store user's followed artists into the cache folder")?;
                state.data.write().user_data.followed_artists = artists;
            }
            ClientRequest::GetUserSavedAlbums => {
                let albums = self.current_user_saved_albums().await?;
                store_data_into_file_cache(
                    FileCacheKey::SavedAlbums,
                    &config::get_config().cache_folder,
                    &albums,
                )
                .context("store user's saved albums into the cache folder")?;
                state.data.write().user_data.saved_albums = albums;
            }
            ClientRequest::GetUserSavedShows => {
                let shows = self.current_user_saved_shows().await?;
                store_data_into_file_cache(
                    FileCacheKey::SavedShows,
                    &config::get_config().cache_folder,
                    &shows,
                )
                .context("store user's saved shows into the cache folder")?;
                state.data.write().user_data.saved_shows = shows;
            }
            ClientRequest::GetContext(context) => {
                let uri = context.uri();
                // Liked tracks must always be refreshed to keep user_data.saved_tracks in sync,
                // and the recently played tracks to show the latest of the played history.
                let always_refresh =
                    uri == USER_LIKED_TRACKS_URI || uri == USER_RECENTLY_PLAYED_TRACKS_URI;
                let cache_miss =
                    !always_refresh && !state.data.read().caches.context.contains_key(&uri);
                if cache_miss || always_refresh {
                    let ctx = match context {
                        ContextId::Playlist(playlist_id) => {
                            self.playlist_context(playlist_id).await?
                        }
                        ContextId::Album(album_id) => self.album_context(album_id).await?,
                        ContextId::Artist(artist_id) => self.artist_context(artist_id).await?,
                        ContextId::Tracks(tracks_id) => match tracks_id.uri.as_str() {
                            USER_TOP_TRACKS_URI => Context::Tracks {
                                tracks: self.current_user_top_tracks().await?,
                                desc: "User's top tracks".to_string(),
                            },
                            USER_RECENTLY_PLAYED_TRACKS_URI => Context::Tracks {
                                tracks: self.current_user_recently_played_tracks().await?,
                                desc: "User's recently played tracks".to_string(),
                            },
                            USER_LIKED_TRACKS_URI => {
                                let tracks = self.liked_tracks_for_page(state).await?;
                                let tracks_hm = tracks
                                    .iter()
                                    .map(|t| (t.id.uri(), t.clone()))
                                    .collect::<HashMap<_, _>>();
                                store_data_into_file_cache(
                                    FileCacheKey::SavedTracks,
                                    &config::get_config().cache_folder,
                                    &tracks_hm,
                                )
                                .context("store user's saved tracks into the cache folder")?;
                                state.data.write().user_data.saved_tracks = tracks_hm;
                                Context::Tracks {
                                    tracks,
                                    desc: "User's liked tracks".to_string(),
                                }
                            }
                            u if u.starts_with("radio:") => Context::Tracks {
                                tracks: self.radio(u).await?,
                                desc: tracks_id.kind.clone(),
                            },
                            uri => anyhow::bail!("unsupported Tracks context: {uri}"),
                        },
                        ContextId::Show(show_id) => self.show_context(show_id).await?,
                    };

                    state
                        .data
                        .write()
                        .caches
                        .context
                        .insert(uri, ctx, *TTL_CACHE_DURATION);
                }
            }
            ClientRequest::Search(query) => match self.search(&query).await {
                Ok(results) => state.data.write().caches.complete_search(query, results),
                Err(err) => {
                    state.data.write().caches.fail_search(query);
                    return Err(err);
                }
            },

            ClientRequest::AddPlayableToQueue(playable_id) => {
                self.add_item_to_queue(playable_id, None).await?;
            }
            ClientRequest::AddPlayableToPlaylist(playlist_id, playable_id) => {
                self.add_item_to_playlist(state, playlist_id, playable_id)
                    .await?;
            }
            ClientRequest::AddAlbumToQueue(album_id) => {
                let album_context = self.album_context(album_id).await?;

                if let Context::Album { album: _, tracks } = album_context {
                    for track in tracks {
                        self.add_item_to_queue(PlayableId::Track(track.id), None)
                            .await?;
                    }
                }
            }
            ClientRequest::DeleteTrackFromPlaylist(playlist_id, track_id) => {
                self.delete_track_from_playlist(state, playlist_id, track_id)
                    .await?;
            }
            ClientRequest::AddToLibrary(item) => {
                self.add_to_library(state, item).await?;
            }
            ClientRequest::DeleteFromLibrary(id) => {
                self.delete_from_library(state, id).await?;
            }
            ClientRequest::GetCurrentUserQueue => {
                let queue = self.current_user_queue2().await?;
                state.player.write().queue = Some(queue);
            }
            ClientRequest::ReorderPlaylistItems {
                playlist_id,
                insert_index,
                range_start,
                range_length,
                snapshot_id,
            } => {
                self.reorder_playlist_items(
                    state,
                    playlist_id,
                    insert_index,
                    range_start,
                    range_length,
                    snapshot_id.as_deref(),
                )
                .await?;
            }
            ClientRequest::CreatePlaylist {
                playlist_name,
                public,
                collab,
                desc,
            } => {
                let user_id = state
                    .data
                    .read()
                    .user_data
                    .user
                    .as_ref()
                    .map(|u| u.id.clone())
                    .unwrap();
                self.create_new_playlist(
                    state,
                    user_id,
                    playlist_name.as_str(),
                    public,
                    collab,
                    desc.as_str(),
                )
                .await?;
            }
        }

        tracing::info!(
            "Successfully handled the client request, took: {}ms",
            timer.elapsed().as_millis()
        );

        Ok(())
    }

    /// Get lyrics of a given track, return None if no lyrics is available
    pub async fn lyrics(&self, track_id: TrackId<'static>) -> Result<Option<Lyrics>> {
        let session = self.spotify.session().await;
        let uri = SpotifyUri::from_uri(&track_id.uri())?;
        match uri {
            SpotifyUri::Track { id } => {
                match librespot_metadata::Lyrics::get(&session, &id).await {
                    Ok(lyrics) => Ok(Some(lyrics.into())),
                    Err(err) => {
                        if err.to_string().to_lowercase().contains("not found") {
                            Ok(None)
                        } else {
                            Err(err.into())
                        }
                    }
                }
            }
            _ => Ok(None),
        }
    }

    /// Get user available devices
    pub async fn available_devices(&self) -> Result<Vec<rspotify::model::Device>> {
        Ok(self.device().await?)
    }

    /// After handling a request changing the player's playback,
    /// update the playback state **in a non-blocking manner**.
    pub fn update_playback_non_blocking(&self, state: &SharedState) {
        // Q: Why do we need more than one request to update the playback?
        // A: It might take a while for Spotify server to reflect the new change,
        // making additional requests can help ensure that the playback state is always up-to-date.
        let client = self.clone();
        let state = state.clone();
        tokio::task::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let _ = client.retrieve_current_playback(&state, false).await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let _ = client.retrieve_current_playback(&state, false).await;
        });
    }

    /// Get Spotify's available browse categories
    pub async fn browse_categories(&self) -> Result<Vec<Category>> {
        #[allow(deprecated)]
        let first_page = self
            .categories_manual(Some("EN"), None, Some(50), None)
            .await?;

        Ok(first_page.items.into_iter().map(Category::from).collect())
    }

    /// Get Spotify's available browse playlists of a given category
    pub async fn browse_category_playlists(&self, category_id: &str) -> Result<Vec<Playlist>> {
        // TODO: this should use `rspotify::category_playlists_manual` API instead of `http_get`
        // The current implementation is a workaround for https://github.com/ramsayleung/rspotify/issues/535

        // Ok(self
        //     .category_playlists_manual(
        //         category_id,
        //         Some(rspotify::model::Market::FromToken),
        //         Some(50),
        //         None,
        //     )
        //     .await?
        //     .items
        //     .into_iter()
        //     .map(Into::into)
        //     .collect())

        #[derive(Deserialize, Debug)]
        struct BrowseCategoryPlaylistsResponse {
            playlists: rspotify::model::Page<serde_json::Value>,
        }

        Ok(self
            .http_get::<BrowseCategoryPlaylistsResponse>(
                &format!("browse/categories/{category_id}/playlists"),
                &Query::from([("limit", "50")]),
            )
            .await?
            .playlists
            .items
            .into_iter()
            .filter_map(|item| {
                serde_json::from_value::<rspotify::model::SimplifiedPlaylist>(item).ok()
            })
            .map(Into::into)
            .collect())
    }

    /// Find an available device. If found, return the device's ID.
    #[cfg_attr(not(feature = "streaming"), allow(unused_variables))]
    async fn find_available_device(&self, state: &SharedState) -> Result<Option<String>> {
        let devices = self.available_devices().await?;

        // if there is an active device, return it
        if let Some(d) = devices.iter().find(|d| d.is_active) {
            return Ok(d.id.clone());
        }

        #[allow(unused_mut)]
        let mut devices = devices
            .into_iter()
            .filter_map(Device::try_from_device)
            .collect::<Vec<_>>();

        #[cfg(feature = "streaming")]
        if state.is_streaming_enabled() {
            self.ensure_integrated_device(&mut devices).await;
        }

        tracing::info!("no active device found, available devices: {devices:?}");

        if devices.is_empty() {
            return Ok(None);
        }

        // Prioritize the preferred device, then the integrated device; otherwise, use the first available device.
        let preferred = preferred_device();
        let id = devices
            .iter()
            .position(|d| Some(&d.name) == preferred.as_ref())
            .or_else(|| devices.iter().position(|d| d.is_integrated))
            .unwrap_or_default();

        Ok(Some(devices.remove(id).id))
    }

    /// Ensures the integrated librespot device (of *this* running instance) is present in `devices`.
    ///
    /// The integrated device may not show up in the device list returned by the Spotify API because
    /// 1. The device is just initialized and hasn't been registered in Spotify server.
    ///    Related issue/discussion: <https://github.com/aome510/spotify-player/issues/79>
    /// 2. The device list is empty. This might be because user doesn't specify their own client ID.
    ///    By default, the application uses Spotify web app's client ID, which doesn't have
    ///    access to user's active devices.
    ///
    /// Callers must check [`SharedState::is_streaming_enabled`] first. The `streaming` feature
    /// being compiled in does not mean *this* instance runs an integrated player: with
    /// `enable_streaming = "DaemonOnly"` only the daemon does, so a non-daemon instance would
    /// otherwise advertise a device that does not exist.
    #[cfg(feature = "streaming")]
    async fn ensure_integrated_device(&self, devices: &mut Vec<Device>) {
        let session = self.spotify.session().await;
        let session_device_id = session.device_id().to_string();

        // Mark the integrated device if it's already in the list; otherwise, add it, so it's
        // always present without duplicating an entry the API already returned.
        match devices.iter_mut().find(|d| d.id == session_device_id) {
            Some(device) => device.is_integrated = true,
            None => devices.insert(
                0,
                Device {
                    id: session_device_id,
                    name: config::get_config().app_config.device.name.clone(),
                    is_integrated: true,
                },
            ),
        }
    }

    /// Get the saved (liked) tracks of the current user, up to date when it returns
    pub async fn current_user_saved_tracks(&self) -> Result<Vec<Track>> {
        let cached = load_liked_tracks();
        let refresh = match check_liked_tracks(&cached, |offset| self.liked_page(offset)).await {
            Ok(refresh) => refresh,
            // e.g. rate limited: the cached tracks are still the liked tracks but for recent changes
            Err(err) if !cached.tracks.is_empty() => {
                tracing::warn!(
                    "Failed to check the liked tracks, showing the cached ones: {err:#}"
                );
                return Ok(cached.tracks);
            }
            Err(err) => return Err(err),
        };
        let liked = liked_tracks_up_to_date(refresh, Pace::Now, |offset| self.liked_page(offset))
            .await?
            .context("the liked tracks weren't brought up to date")?;
        store_liked_tracks(&liked)?;
        Ok(liked.tracks)
    }

    /// The liked tracks to show on their page at once. When some were unliked in another app,
    /// they're found in the background, a request every few seconds, and the page updates then.
    async fn liked_tracks_for_page(&self, state: &SharedState) -> Result<Vec<Track>> {
        let cached = load_liked_tracks();
        let Some(running) = liked_tracks::Running::start() else {
            return Ok(cached.tracks);
        };
        let refresh = match check_liked_tracks(&cached, |offset| self.liked_page(offset)).await {
            Ok(refresh) => refresh,
            Err(err) if !cached.tracks.is_empty() => {
                tracing::warn!(
                    "Failed to check the liked tracks, showing the cached ones: {err:#}"
                );
                return Ok(cached.tracks);
            }
            Err(err) => return Err(err),
        };
        let refresh = match refresh {
            LikedRefresh::Done(liked) => {
                let liked = LikedTracks::from_slots(liked);
                store_liked_tracks(&liked)?;
                return Ok(liked.tracks);
            }
            // nothing cached to show: request them all now, as before
            LikedRefresh::FetchAll if cached.tracks.is_empty() => {
                let liked = liked_tracks_up_to_date(LikedRefresh::FetchAll, Pace::Now, |offset| {
                    self.liked_page(offset)
                })
                .await?
                .context("the liked tracks weren't requested")?;
                store_liked_tracks(&liked)?;
                return Ok(liked.tracks);
            }
            refresh => refresh,
        };
        let shown = match &refresh {
            LikedRefresh::Search { new, cached, .. } => {
                new.iter().chain(cached).flatten().cloned().collect()
            }
            _ => cached.tracks,
        };

        let client = self.clone();
        let state = state.clone();
        tokio::task::spawn(async move {
            let _running = running;
            match liked_tracks_up_to_date(refresh, Pace::Background, |offset| {
                client.liked_page(offset)
            })
            .await
            {
                Ok(Some(liked)) => {
                    if let Err(err) = keep_liked_tracks(&state, &liked) {
                        tracing::warn!("Failed to keep the liked tracks: {err:#}");
                    }
                }
                Ok(None) => tracing::info!(
                    "Stopped updating the liked tracks: Spotify limits the rate of requests"
                ),
                Err(err) => tracing::warn!("Failed to update the liked tracks: {err:#}"),
            }
        });
        Ok(shown)
    }

    /// Keep the liked tracks, and so the liked marks, current in the background: once shortly
    /// after startup, then every few minutes while nothing plays (`liked_refresh_delay_secs` and
    /// `liked_refresh_interval_mins`). Likes and unlikes made in this app update the cache
    /// themselves; this catches the ones made in other apps.
    pub async fn refresh_liked_tracks_while_idle(&self, state: &SharedState) {
        let app_config = &config::get_config().app_config;
        let delay = std::time::Duration::from_secs(app_config.liked_refresh_delay_secs);
        // `from_mins` would panic on a huge value
        let interval = std::time::Duration::from_secs(
            app_config.liked_refresh_interval_mins.saturating_mul(60),
        );
        tokio::time::sleep(delay).await;
        loop {
            if let Err(err) = self.refresh_liked_tracks_quietly(state).await {
                tracing::warn!("Failed to refresh the liked tracks in the background: {err:#}");
            }
            if interval.is_zero() {
                return;
            }
            loop {
                tokio::time::sleep(interval).await;
                let playing = state
                    .player
                    .read()
                    .buffered_playback
                    .as_ref()
                    .is_some_and(|playback| playback.is_playing);
                if !playing {
                    break;
                }
            }
        }
    }

    /// Bring the liked tracks up to date at the background pace, writing only what changed
    async fn refresh_liked_tracks_quietly(&self, state: &SharedState) -> Result<()> {
        let Some(_running) = liked_tracks::Running::start() else {
            return Ok(());
        };
        let cached = load_liked_tracks();
        // nothing cached: the liked tracks page requests them when it's first opened
        if cached.tracks.is_empty() {
            return Ok(());
        }
        let refresh = check_liked_tracks(&cached, |offset| self.liked_page(offset)).await?;
        if let LikedRefresh::Done(liked) = &refresh {
            if liked_unchanged(&cached, liked) {
                return Ok(());
            }
        }
        let Some(liked) =
            liked_tracks_up_to_date(refresh, Pace::Background, |offset| self.liked_page(offset))
                .await?
        else {
            tracing::info!(
                "Stopped refreshing the liked tracks: Spotify limits the rate of requests"
            );
            return Ok(());
        };
        keep_liked_tracks(state, &liked)
    }

    /// A page of Spotify's liked items from `offset`
    async fn liked_page(&self, offset: usize) -> Result<LikedPage> {
        let page = self.liked_tracks_page(u32::try_from(offset)?).await?;
        Ok(LikedPage {
            total: usize::try_from(page.total)?,
            has_next: page.next.is_some(),
            items: page
                .items
                .into_iter()
                .map(|saved| Track::try_from_full_track(saved.track))
                .collect(),
        })
    }

    async fn liked_tracks_page(
        &self,
        offset: u32,
    ) -> Result<rspotify::model::Page<rspotify::model::SavedTrack>> {
        let offset = offset.to_string();
        let params = Query::from([
            ("market", "from_token"),
            ("limit", "50"),
            ("offset", &offset),
        ]);
        self.http_get("me/tracks", &params).await
    }

    /// Get the recently played tracks of the current user
    pub async fn current_user_recently_played_tracks(&self) -> Result<Vec<Track>> {
        if let Some(tracks) =
            played_history::recently_played_tracks(&config::get_config().cache_folder)?
        {
            return Ok(tracks);
        }

        let play_histories = self
            .current_user_recently_played(Some(50), None)
            .await?
            .items;

        // de-duplicate the tracks returned from the recently-played API
        let mut tracks = Vec::<Track>::new();
        for history in play_histories {
            if !tracks.iter().any(|t| t.name == history.track.name) {
                if let Some(track) = Track::try_from_full_track(history.track) {
                    tracks.push(track);
                }
            }
        }
        Ok(tracks)
    }

    /// Get the top tracks of the current user
    pub async fn current_user_top_tracks(&self) -> Result<Vec<Track>> {
        let limit = config::get_config().app_config.top_tracks_limit;
        if limit == 0 {
            return Ok(Vec::new());
        }

        let tracks = self
            .all_paging_items::<rspotify::model::FullTrack>("me/top/tracks", limit)
            .await?;

        Ok(tracks
            .into_iter()
            .filter_map(Track::try_from_full_track)
            .collect())
    }

    /// Get all playlists of the current user
    pub async fn current_user_playlists(&self) -> Result<Vec<Playlist>> {
        let playlists = self
            .all_paging_items::<rspotify::model::SimplifiedPlaylist>(
                "me/playlists",
                0, // we don't know the total number of playlists beforehand
            )
            .await?;

        Ok(playlists
            .into_iter()
            .map(std::convert::Into::into)
            .collect())
    }

    /// Get all followed artists of the current user
    pub async fn current_user_followed_artists(&self) -> Result<Vec<Artist>> {
        let first_page = self
            .deref()
            .current_user_followed_artists(None, None)
            .await?;

        // followed artists pagination is handled different from
        // other paginations. The endpoint uses cursor-based pagination.
        let mut artists = first_page.items;
        let mut maybe_next = first_page.next;
        while let Some(url) = maybe_next {
            let endpoint = url
                .split_once("/v1/")
                .map_or(url.as_str(), |(_, endpoint)| endpoint);
            let mut next_page = self
                .http_get::<rspotify::model::CursorPageFullArtists>(endpoint, &Query::new())
                .await?
                .artists;
            artists.append(&mut next_page.items);
            maybe_next = next_page.next;
        }

        // converts `rspotify::model::FullArtist` into `state::Artist`
        Ok(artists.into_iter().map(std::convert::Into::into).collect())
    }

    /// Get all saved albums of the current user
    pub async fn current_user_saved_albums(&self) -> Result<Vec<Album>> {
        let albums = self
            .all_paging_items::<rspotify::model::SavedAlbum>(
                "me/albums",
                0, // we don't know the total number of saved albums beforehand
            )
            .await?;

        // Converts `rspotify::model::SavedAlbum` into `state::Album`
        Ok(albums.into_iter().map(Album::from).collect())
    }

    /// Get all saved shows of the current user
    pub async fn current_user_saved_shows(&self) -> Result<Vec<Show>> {
        let shows = self
            .all_paging_items::<rspotify::model::Show>(
                "me/shows", 0, // we don't know the total number of saved shows beforehand
            )
            .await?;

        Ok(shows.into_iter().map(|s| s.show.into()).collect())
    }

    /// Get all albums of an artist
    pub async fn artist_albums(&self, artist_id: ArtistId<'_>) -> Result<Vec<Album>> {
        let albums = self
            .all_paging_items::<rspotify::model::SimplifiedAlbum>(
                &format!(
                    "artists/{}/albums?include_groups=album,single",
                    artist_id.id()
                ),
                0, // we don't know the total number of artist albums beforehand
            )
            .await?
            .into_iter()
            .filter_map(Album::try_from_simplified_album)
            .collect();

        Ok(AppClient::process_artist_albums(albums))
    }

    /// Start a playback
    async fn start_playback(&self, playback: Playback, device_id: Option<&str>) -> Result<()> {
        match playback {
            Playback::Context(id, offset) => match id {
                ContextId::Album(id) => {
                    self.start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Artist(id) => {
                    self.start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Playlist(id) => {
                    self.start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Show(id) => {
                    self.start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Tracks(_) => {
                    anyhow::bail!("`StartPlayback` request for `tracks` context is not supported")
                }
            },
            Playback::URIs(ids, offset) => {
                self.start_uris_playback(ids, device_id, offset, None)
                    .await?;
            }
        }

        Ok(())
    }

    /// Get recommendation (radio) tracks based on a seed
    /// The tracks of the radio `uri` (`radio:<seed URI>`): the list kept from when it was first
    /// fetched, so that it still holds the tracks played from it
    async fn radio(&self, uri: &str) -> Result<Vec<Track>> {
        let cache_folder = &config::get_config().cache_folder;
        if let Some(tracks) = radios::load(cache_folder, uri) {
            return Ok(tracks);
        }
        let tracks = self.radio_tracks(uri["radio:".len()..].to_string()).await?;
        radios::store(cache_folder, uri, &tracks)?;
        Ok(tracks)
    }

    pub async fn radio_tracks(&self, seed_uri: String) -> Result<Vec<Track>> {
        #[derive(Debug, Deserialize)]
        struct TrackData {
            original_gid: String,
        }
        #[derive(Debug, Deserialize)]
        struct RadioStationResponse {
            tracks: Vec<TrackData>,
        }

        let session = self.spotify.session().await;

        // Get an autoplay URI from the seed URI.
        // The return URI is a Spotify station's URI
        let autoplay_query_url = format!("hm://autoplay-enabled/query?uri={seed_uri}");
        let response = session
            .mercury()
            .get(autoplay_query_url)
            .map_err(|err| anyhow::anyhow!("Failed to get autoplay URI: {err:#}"))?
            .await?;
        if response.status_code != 200 {
            anyhow::bail!(
                "Failed to get autoplay URI: got non-OK status code: {}",
                response.status_code
            );
        }
        let autoplay_uri = String::from_utf8(response.payload[0].clone())?;

        // Retrieve radio's data based on the autoplay URI
        let radio_query_url = format!("hm://radio-apollo/v3/stations/{autoplay_uri}");
        let response = session
            .mercury()
            .get(radio_query_url)
            .map_err(|err| anyhow::anyhow!("Failed to get radio data of {autoplay_uri}: {err:#}"))?
            .await?;
        if response.status_code != 200 {
            anyhow::bail!(
                "Failed to get radio data of {autoplay_uri}: got non-OK status code: {}",
                response.status_code
            );
        }

        // Parse a list consisting of IDs of tracks inside the radio station
        let track_ids = serde_json::from_slice::<RadioStationResponse>(&response.payload[0])?
            .tracks
            .into_iter()
            .filter_map(|t| TrackId::from_id(t.original_gid).ok());

        // Retrieve tracks based on IDs
        #[allow(deprecated)]
        let tracks = self
            .tracks(track_ids, Some(rspotify::model::Market::FromToken))
            .await?;
        let mut tracks: Vec<_> = tracks
            .into_iter()
            .filter_map(Track::try_from_full_track)
            .collect();

        // Track-seeded radios in the official Spotify clients include the seed track itself
        // as the first item in the generated session.
        if let Ok(track_id) = TrackId::from_uri(&seed_uri) {
            match self.track(track_id).await {
                Ok(track) => move_seed_track_to_front(&mut tracks, track),
                Err(err) => {
                    tracing::warn!("Failed to fetch track radio seed {seed_uri}: {err:#}");
                }
            }
        }

        Ok(tracks)
    }

    /// Search for items (tracks, artists, albums, playlists) matching a given query
    pub async fn search(&self, query: &str) -> Result<SearchResults> {
        use rspotify::model::SearchType;

        let result = self
            .deref()
            .search_multiple(
                query,
                [
                    SearchType::Track,
                    SearchType::Artist,
                    SearchType::Album,
                    SearchType::Playlist,
                    SearchType::Show,
                    SearchType::Episode,
                ],
                None,
                None,
                Some(10),
                None,
            )
            .await?;

        let tracks = result
            .tracks
            .into_iter()
            .flat_map(|page| page.items)
            .filter_map(Track::try_from_full_track)
            .collect();
        let artists = result
            .artists
            .into_iter()
            .flat_map(|page| page.items)
            .map(std::convert::Into::into)
            .collect();
        let albums = result
            .albums
            .into_iter()
            .flat_map(|page| page.items)
            .filter_map(Album::try_from_simplified_album)
            .collect();
        let playlists = result
            .playlists
            .into_iter()
            .flat_map(|page| page.items)
            .map(std::convert::Into::into)
            .collect();
        let shows = result
            .shows
            .into_iter()
            .flat_map(|page| page.items)
            .map(std::convert::Into::into)
            .collect();
        let episodes = result
            .episodes
            .into_iter()
            .flat_map(|page| page.items)
            .map(std::convert::Into::into)
            .collect();

        Ok(SearchResults {
            tracks,
            artists,
            albums,
            playlists,
            shows,
            episodes,
        })
    }

    /// Search for items of a specific type matching a given query
    pub async fn search_specific_type(
        &self,
        query: &str,
        typ: rspotify::model::SearchType,
    ) -> Result<rspotify::model::SearchResult> {
        Ok(self
            .deref()
            .search(query, typ, None, None, None, None)
            .await?)
    }

    /// Add a playable item to a playlist
    pub async fn add_item_to_playlist(
        &self,
        state: &SharedState,
        playlist_id: PlaylistId<'_>,
        playable_id: PlayableId<'_>,
    ) -> Result<()> {
        // remove all the occurrences of the track to ensure no duplication in the playlist
        self.playlist_remove_all_occurrences_of_items(
            playlist_id.as_ref(),
            [playable_id.as_ref()],
            None,
        )
        .await?;

        self.playlist_add_items(playlist_id.as_ref(), [playable_id.as_ref()], None)
            .await?;

        // After adding a new track to a playlist, remove the cache of that playlist to force refetching new data
        state.data.write().caches.context.remove(&playlist_id.uri());

        Ok(())
    }

    /// Remove a track from a playlist
    pub async fn delete_track_from_playlist(
        &self,
        state: &SharedState,
        playlist_id: PlaylistId<'_>,
        track_id: TrackId<'_>,
    ) -> Result<()> {
        // remove all the occurrences of the track to ensure no duplication in the playlist
        self.playlist_remove_all_occurrences_of_items(
            playlist_id.as_ref(),
            [PlayableId::Track(track_id.as_ref())],
            None,
        )
        .await?;

        // After making a delete request, update the playlist in-memory data stored inside the app caches.
        if let Some(Context::Playlist { tracks, .. }) = state
            .data
            .write()
            .caches
            .context
            .get_mut(&playlist_id.uri())
        {
            tracks.retain(|t| t.id != track_id);
        }

        Ok(())
    }

    /// Reorder items in a playlist
    async fn reorder_playlist_items(
        &self,
        state: &SharedState,
        playlist_id: PlaylistId<'_>,
        insert_index: usize,
        range_start: usize,
        range_length: Option<usize>,
        snapshot_id: Option<&str>,
    ) -> Result<()> {
        let insert_before = if insert_index > range_start {
            insert_index + 1
        } else {
            insert_index
        };

        self.playlist_reorder_items(
            playlist_id.clone(),
            Some(range_start as i32),
            Some(insert_before as i32),
            range_length.map(|range_length| range_length as u32),
            snapshot_id,
        )
        .await?;

        // After making a reorder request, update the playlist in-memory data stored inside the app caches.
        if let Some(Context::Playlist { tracks, .. }) = state
            .data
            .write()
            .caches
            .context
            .get_mut(&playlist_id.uri())
        {
            let track = tracks.remove(range_start);
            tracks.insert(insert_index, track);
        }

        Ok(())
    }

    /// Add a Spotify item to current user's library.
    async fn add_to_library(&self, state: &SharedState, item: Item) -> Result<()> {
        // Before adding new item, checks if that item already exists in the library to avoid adding a duplicated item.
        match item {
            Item::Track(track) => {
                let contains = self
                    .library_contains([LibraryId::Track(track.id.as_ref())])
                    .await?;
                if !contains[0] {
                    self.library_add([LibraryId::Track(track.id.as_ref())])
                        .await?;
                    // update the in-memory `user_data`
                    state
                        .data
                        .write()
                        .user_data
                        .saved_tracks
                        .insert(track.id.uri(), track);
                }
            }
            Item::Album(album) => {
                let contains = self
                    .library_contains([LibraryId::Album(album.id.as_ref())])
                    .await?;
                if !contains[0] {
                    self.library_add([LibraryId::Album(album.id.as_ref())])
                        .await?;
                    // update the in-memory `user_data`
                    state.data.write().user_data.saved_albums.insert(0, album);
                }
            }
            Item::Artist(artist) => {
                let follows = self
                    .library_contains([LibraryId::Artist(artist.id.as_ref())])
                    .await?;
                if !follows[0] {
                    self.library_add([LibraryId::Artist(artist.id.as_ref())])
                        .await?;
                    // update the in-memory `user_data`
                    state
                        .data
                        .write()
                        .user_data
                        .followed_artists
                        .insert(0, artist);
                }
            }
            Item::Playlist(playlist) => {
                let user_id = state
                    .data
                    .read()
                    .user_data
                    .user
                    .as_ref()
                    .map(|u| u.id.clone());

                if let Some(_user_id) = user_id {
                    let follows = self
                        .library_contains([LibraryId::Playlist(playlist.id.as_ref())])
                        .await?;
                    if !follows[0] {
                        self.library_add([LibraryId::Playlist(playlist.id.as_ref())])
                            .await?;
                        // update the in-memory `user_data`
                        state
                            .data
                            .write()
                            .user_data
                            .playlists
                            .insert(0, PlaylistFolderItem::Playlist(playlist));
                    }
                }
            }
            Item::Show(show) => {
                let follows = self
                    .library_contains([LibraryId::Show(show.id.as_ref())])
                    .await?;
                if !follows[0] {
                    self.library_add([LibraryId::Show(show.id.as_ref())])
                        .await?;
                    // update the in-memory `user_data`
                    state.data.write().user_data.saved_shows.insert(0, show);
                }
            }
        }
        Ok(())
    }

    // Delete a Spotify item from user's library
    async fn delete_from_library(&self, state: &SharedState, id: ItemId) -> Result<()> {
        match id {
            ItemId::Track(id) => {
                let uri = id.uri();
                self.library_remove([LibraryId::Track(id)]).await?;
                state.data.write().user_data.saved_tracks.remove(&uri);
                forget_cached_liked_track(&uri);
            }
            ItemId::Album(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .saved_albums
                    .retain(|a| a.id != id);
                self.library_remove([LibraryId::Album(id)]).await?;
            }
            ItemId::Artist(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .followed_artists
                    .retain(|a| a.id != id);
                self.library_remove([LibraryId::Artist(id)]).await?;
            }
            ItemId::Playlist(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .playlists
                    .retain(|item| match item {
                        PlaylistFolderItem::Playlist(p) => p.id != id,
                        PlaylistFolderItem::Folder(_) => true,
                    });
                self.library_remove([LibraryId::Playlist(id)]).await?;
            }
            ItemId::Show(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .saved_shows
                    .retain(|s| s.id != id);
                self.library_remove([LibraryId::Show(id)]).await?;
            }
        }
        Ok(())
    }

    /// Get a track data
    pub async fn track(&self, track_id: TrackId<'_>) -> Result<Track> {
        Track::try_from_full_track(
            self.deref()
                .track(track_id, Some(rspotify::model::Market::FromToken))
                .await?,
        )
        .context("convert FullTrack into Track")
    }

    /// Get a playlist context data
    pub async fn playlist_context(&self, playlist_id: PlaylistId<'_>) -> Result<Context> {
        let playlist_uri = playlist_id.uri();
        tracing::info!("Get playlist context: {}", playlist_uri);

        let playlist = self
            .playlist(
                playlist_id.clone(),
                None,
                Some(rspotify::model::Market::FromToken),
            )
            .await?;

        let tracks = self
            .all_paging_items(
                &format!("playlists/{}/items", playlist_id.id()),
                playlist.items.total as usize,
            )
            .await?
            .into_iter()
            .filter_map(Track::try_from_playlist_item)
            .collect::<Vec<_>>();

        Ok(Context::Playlist {
            playlist: playlist.into(),
            tracks,
        })
    }

    /// Get an album context data
    pub async fn album_context(&self, album_id: AlbumId<'_>) -> Result<Context> {
        let album_uri = album_id.uri();
        tracing::info!("Get album context: {}", album_uri);

        let album = self
            .album(album_id.clone(), Some(rspotify::model::Market::FromToken))
            .await?;

        let total_tracks = album.tracks.total as usize;

        // converts `rspotify::model::FullAlbum` into `state::Album`
        let album: Album = album.into();

        // get the album's tracks
        let tracks = self
            .all_paging_items(&format!("albums/{}/tracks", album_id.id()), total_tracks)
            .await?
            .into_iter()
            .filter_map(|t| {
                // simplified track doesn't have album so
                // we need to manually include one during
                // converting into `state::Track`
                Track::try_from_simplified_track(t).map(|mut t| {
                    t.album = Some(album.clone());
                    t
                })
            })
            .collect::<Vec<_>>();

        Ok(Context::Album { album, tracks })
    }

    /// Get an artist context data
    pub async fn artist_context(&self, artist_id: ArtistId<'_>) -> Result<Context> {
        let artist_uri = artist_id.uri();
        tracing::info!("Get artist context: {}", artist_uri);

        // get the artist's information, including top tracks, related artists, and albums

        let artist = self
            .artist(artist_id.as_ref())
            .await
            .context("get artist")?
            .into();

        #[allow(deprecated)]
        let top_tracks = self
            .artist_top_tracks(artist_id.as_ref(), Some(rspotify::model::Market::FromToken))
            .await
            .context("get artist's top tracks")?
            .into_iter()
            .filter_map(Track::try_from_full_track)
            .collect::<Vec<_>>();

        #[allow(deprecated)]
        let related_artists = self
            .artist_related_artists(artist_id.as_ref())
            .await
            .ok()
            .unwrap_or_default()
            .into_iter()
            .map(std::convert::Into::into)
            .collect::<Vec<_>>();

        let albums = self
            .artist_albums(artist_id.as_ref())
            .await
            .context("get artist's albums")?;

        Ok(Context::Artist {
            artist,
            top_tracks,
            albums,
            related_artists,
        })
    }

    /// Get a show context data
    pub async fn show_context(&self, show_id: ShowId<'_>) -> Result<Context> {
        let show_uri = show_id.uri();
        tracing::info!("Get show context: {}", show_uri);

        let show = self.get_a_show(show_id.clone(), None).await?;

        // get the show's episodes
        let episodes = self
            .all_paging_items::<rspotify::model::SimplifiedEpisode>(
                &format!("shows/{}/episodes", show_id.id()),
                show.episodes.total as usize,
            )
            .await?
            .into_iter()
            .map(std::convert::Into::into)
            .collect::<Vec<_>>();

        // converts `rspotify::model::FullShow` into `state::Show`
        let show: Show = show.into();

        Ok(Context::Show { show, episodes })
    }

    /// Make a GET HTTP request to the Spotify server
    async fn http_get<T>(&self, url: &str, payload: &Query<'_>) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let text = self.api_client.api_get(url, payload).await?;
        Ok(serde_json::from_str(&text)?)
    }

    async fn all_paging_items<T>(&self, base_url: &str, mut count: usize) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned + std::fmt::Debug,
    {
        const PAGE_LIMIT: usize = 50;
        const MAX_PARALLEL: usize = 4;

        let mut all_items = Vec::new();
        let mut offset = 0;
        let mut n_jobs = 1;

        // if count is 0 (i.e., unknown), set it to usize::MAX to fetch until no more items
        if count == 0 {
            count = usize::MAX;
        }

        while offset < count {
            n_jobs = std::cmp::min(n_jobs, (count - offset).div_ceil(PAGE_LIMIT));

            let mut futures = Vec::with_capacity(n_jobs);

            for i in 0..n_jobs {
                let current_offset = offset + i * PAGE_LIMIT;
                let limit_str = PAGE_LIMIT.to_string();
                let offset_str = current_offset.to_string();

                futures.push(async move {
                    let params = Query::from([
                        ("market", "from_token"),
                        ("limit", &limit_str),
                        ("offset", &offset_str),
                    ]);
                    self.http_get::<rspotify::model::Page<T>>(base_url, &params)
                        .await
                });
            }

            let results = futures::future::try_join_all(futures).await?;

            let mut found_partial_page = false;
            for mut page in results {
                if page.items.len() < PAGE_LIMIT {
                    found_partial_page = true;
                }
                all_items.append(&mut page.items);
            }

            if found_partial_page {
                break;
            }

            offset += n_jobs * PAGE_LIMIT;
            // gradually increase the number of parallel jobs, but do not exceed MAX_PARALLEL
            n_jobs = std::cmp::min(n_jobs + 1, MAX_PARALLEL);
        }

        Ok(all_items)
    }

    /// Get the user's queue
    pub async fn current_user_queue2(&self) -> Result<rspotify::model::CurrentUserQueue> {
        let text = self
            .api_client
            .api_get("me/player/queue", &Query::new())
            .await?;
        parse_queue_response(&text)
    }

    pub async fn current_playback2(
        &self,
    ) -> Result<Option<rspotify::model::CurrentPlaybackContext>> {
        let params = Query::from([("additional_types", "track,episode")]);
        let text = self.api_client.api_get("me/player", &params).await?;
        parse_current_playback_response(&text)
    }

    /// Retrieve the latest playback state
    pub async fn retrieve_current_playback(
        &self,
        state: &SharedState,
        reset_buffered_playback: bool,
    ) -> Result<()> {
        let new_playback = {
            // update the playback state
            let playback = self.current_playback2().await?;
            let mut player = state.player.write();

            let prev_item = player.currently_playing();

            let prev_name = match prev_item {
                Some(rspotify::model::PlayableItem::Track(track)) => track.name.clone(),
                Some(rspotify::model::PlayableItem::Episode(episode)) => episode.name.clone(),
                Some(rspotify::model::PlayableItem::Unknown(_)) | None => String::new(),
            };

            player.playback = playback;
            player.playback_last_updated_time = Some(std::time::Instant::now());

            let curr_item = player.currently_playing();

            let curr_name = match curr_item {
                Some(rspotify::model::PlayableItem::Track(track)) => track.name.clone(),
                Some(rspotify::model::PlayableItem::Episode(episode)) => episode.name.clone(),
                Some(rspotify::model::PlayableItem::Unknown(_)) | None => String::new(),
            };

            let new_playback = prev_name != curr_name && !curr_name.is_empty();
            // check if we need to update the buffered playback
            let needs_update = match (&player.buffered_playback, &player.playback) {
                (Some(bp), Some(p)) => bp.device_id != p.device.id || new_playback,
                (None, None) => false,
                _ => true,
            };

            if reset_buffered_playback || needs_update {
                player.buffered_playback = player.playback.as_ref().map(|p| {
                    let mut playback = PlaybackMetadata::from_playback(p);

                    // handle additional data from the previous buffered state
                    // that is not available in a standard Spotify playback's state
                    if let Some(bp) = &player.buffered_playback {
                        if let Some(volume) = bp.mute_state {
                            playback.volume = Some(volume);
                        }
                        playback.mute_state = bp.mute_state;
                    }
                    playback
                });
            }

            new_playback
        };

        if !new_playback {
            return Ok(());
        }
        state.forget_stale_playing_tracks();
        self.handle_new_playback_event(state).await?;

        Ok(())
    }

    // Handle new track event
    async fn handle_new_playback_event(&self, state: &SharedState) -> Result<()> {
        let configs = config::get_config();

        let curr_item = {
            let player = state.player.read();
            let Some(track_or_episode) = player.currently_playing() else {
                return Ok(());
            };
            track_or_episode.clone()
        };

        // retrieve current artist for genres if not in cache
        let curr_artist = match &curr_item {
            rspotify::model::PlayableItem::Track(full_track) => {
                let cached = state
                    .data
                    .read()
                    .caches
                    .genres
                    .contains_key(&full_track.artists[0].name);

                if cached {
                    None
                } else {
                    match &full_track.artists[0].id {
                        Some(id) => self.artist(id.clone()).await.ok(),
                        None => None,
                    }
                }
            }
            rspotify::model::PlayableItem::Episode(_)
            | rspotify::model::PlayableItem::Unknown(_) => None,
        };

        if let Some(artist) = curr_artist {
            #[allow(deprecated)]
            if !artist.genres.is_empty() {
                state.data.write().caches.genres.insert(
                    artist.name,
                    artist.genres,
                    *TTL_CACHE_DURATION,
                );
            }
        }

        let url = match curr_item {
            rspotify::model::PlayableItem::Track(ref track) => {
                crate::utils::get_track_album_image_url(track)
                    .ok_or(anyhow::anyhow!("missing image"))?
            }
            rspotify::model::PlayableItem::Episode(ref episode) => {
                crate::utils::get_episode_show_image_url(episode)
                    .ok_or(anyhow::anyhow!("missing image"))?
            }
            rspotify::model::PlayableItem::Unknown(_) => return Ok(()),
        };

        let filename = (match curr_item {
            rspotify::model::PlayableItem::Track(ref track) => {
                format!(
                    "{}-{}-cover-{}.jpg",
                    track.album.name,
                    track.album.artists.first().unwrap().name,
                    // first 6 characters of the album's id
                    &track.album.id.as_ref().unwrap().id()[..6]
                )
            }
            #[allow(deprecated)]
            rspotify::model::PlayableItem::Episode(ref episode) => {
                format!(
                    "{}-{}-cover-{}.jpg",
                    episode.show.name,
                    episode.show.publisher,
                    // first 6 characters of the show's id
                    &episode.show.id.as_ref().id()[..6]
                )
            }
            rspotify::model::PlayableItem::Unknown(_) => return Ok(()),
        })
        .replace('/', ""); // remove invalid characters from the file's name
        let path = configs.cache_folder.join("image").join(filename);

        if configs.app_config.enable_cover_image_cache {
            self.retrieve_image(url, &path, true).await?;
        }

        #[cfg(feature = "image")]
        if !state.data.read().caches.images.contains_key(url) {
            let bytes = self.retrieve_image(url, &path, false).await?;

            #[cfg(not(feature = "pixelate"))]
            let image =
                image::load_from_memory(&bytes).context("Failed to load image from memory")?;
            #[cfg(feature = "pixelate")]
            let mut image =
                image::load_from_memory(&bytes).context("Failed to load image from memory")?;

            #[cfg(feature = "pixelate")]
            {
                Self::pixelate_image(&mut image);
            }

            state
                .data
                .write()
                .caches
                .images
                .insert(url.to_owned(), image, *TTL_CACHE_DURATION);
        }

        // notify user about the playback's change if any
        #[cfg(all(feature = "notify", feature = "streaming"))]
        if configs.app_config.enable_notify
            && (!configs.app_config.notify_streaming_only || self.stream_conn.lock().is_some())
        {
            Self::notify_new_playback(&curr_item, &path)?;
        }

        #[cfg(all(feature = "notify", not(feature = "streaming")))]
        if configs.app_config.enable_notify {
            Self::notify_new_playback(&curr_item, &path)?;
        }

        Ok(())
    }

    /// Create a new playlist
    async fn create_new_playlist(
        &self,
        state: &SharedState,
        user_id: UserId<'static>,
        playlist_name: &str,
        public: bool,
        collab: bool,
        desc: &str,
    ) -> Result<()> {
        let playlist: Playlist = self
            .user_playlist_create(
                user_id,
                playlist_name,
                Some(public),
                Some(collab),
                Some(desc),
            )
            .await?
            .into();
        tracing::info!(
            "new playlist (name={},id={}) was successfully created",
            playlist.name,
            playlist.id
        );
        state
            .data
            .write()
            .user_data
            .playlists
            .insert(0, PlaylistFolderItem::Playlist(playlist));
        Ok(())
    }

    #[cfg(feature = "notify")]
    /// Create a notification for a new playback
    fn notify_new_playback(
        playable: &rspotify::model::PlayableItem,
        cover_img_path: &std::path::Path,
    ) -> Result<()> {
        let mut n = notify_rust::Notification::new();

        let re = regex::Regex::new(r"\{.*?\}").unwrap();
        // Generate a text described a track from a format string.
        // For example, a format string "{track} - {artists}" will generate
        // a text consisting of the track's name followed by a dash then artists' names.
        let get_text_from_format_str = |format_str: &str| {
            let mut text = String::new();

            let mut ptr = 0;
            for m in re.find_iter(format_str) {
                let s = m.start();
                let e = m.end();

                if ptr < s {
                    text += &format_str[ptr..s];
                }
                ptr = e;
                match m.as_str() {
                    "{track}" => {
                        let name = match playable {
                            rspotify::model::PlayableItem::Track(ref track) => &track.name,
                            rspotify::model::PlayableItem::Episode(ref episode) => &episode.name,
                            rspotify::model::PlayableItem::Unknown(_) => continue,
                        };
                        text += name;
                    }
                    "{artists}" => {
                        if let rspotify::model::PlayableItem::Track(ref track) = playable {
                            text += &crate::utils::map_join(&track.artists, |a| &a.name, ", ");
                        }
                    }
                    "{album}" => match playable {
                        rspotify::model::PlayableItem::Track(ref track) => {
                            text += &track.album.name;
                        }
                        rspotify::model::PlayableItem::Episode(ref episode) => {
                            text += &episode.show.name;
                        }
                        rspotify::model::PlayableItem::Unknown(_) => {}
                    },
                    &_ => {}
                }
            }
            if ptr < format_str.len() {
                text += &format_str[ptr..];
            }

            text
        };

        let configs = config::get_config();

        n.appname("spotify_player")
            .summary(&get_text_from_format_str(
                &configs.app_config.notify_format.summary,
            ))
            .body(&get_text_from_format_str(
                &configs.app_config.notify_format.body,
            ));
        if cover_img_path.exists() {
            n.icon(cover_img_path.to_str().context("valid cover_img_path")?);
        }
        if configs.app_config.notify_timeout_in_secs > 0 {
            n.timeout(std::time::Duration::from_secs(
                configs.app_config.notify_timeout_in_secs,
            ));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        if configs.app_config.notify_transient {
            use notify_rust::Hint;
            n.hint(Hint::Transient(true));
        }
        n.show()?;

        Ok(())
    }

    /// Retrieve an image from a `url` or a cached `path`.
    /// If `saved` is specified, the retrieved image is saved to the cached `path`.
    async fn retrieve_image(
        &self,
        url: &str,
        path: &std::path::Path,
        saved: bool,
    ) -> Result<Vec<u8>> {
        if path.exists() {
            tracing::debug!("Retrieving image from file: {}", path.display());
            return Ok(std::fs::read(path)?);
        }

        tracing::info!("Retrieving image from url: {url}");

        let bytes = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("get image from url {url}"))?
            .bytes()
            .await?;

        if saved {
            tracing::info!("Saving the retrieved image into {}", path.display());
            let mut file = std::fs::File::create(path)?;
            file.write_all(&bytes)?;
        }

        Ok(bytes.to_vec())
    }

    #[cfg(feature = "pixelate")]
    fn pixelate_image(image: &mut image::DynamicImage) {
        let pixels = config::get_config().app_config.cover_img_pixels;
        let pixelated_image = image.resize(pixels, pixels, image::imageops::FilterType::Nearest);
        *image = pixelated_image.resize(
            image.width(),
            image.height(),
            image::imageops::FilterType::Nearest,
        );
    }

    /// Process a list of albums, which includes
    /// - sort albums by the release date
    /// - sort albums by the type if `sort_artist_albums_by_type` config is enabled
    fn process_artist_albums(mut albums: Vec<Album>) -> Vec<Album> {
        albums.sort_by(|x, y| y.release_date.partial_cmp(&x.release_date).unwrap());

        if config::get_config().app_config.sort_artist_albums_by_type {
            fn get_priority(album_type: &str) -> usize {
                match album_type {
                    "album" => 0,
                    "single" => 1,
                    "appears_on" => 2,
                    "compilation" => 3,
                    _ => 4,
                }
            }
            albums.sort_by_key(|a| get_priority(&a.album_type()));
        }

        albums
    }
}

/// The `preferred_device`, else the Soloist device running on this machine
fn preferred_device() -> Option<String> {
    config::get_config()
        .app_config
        .preferred_device
        .clone()
        .or_else(soloist::local_device_name)
}

/// The liked tracks, newest first, and Spotify's count of them (which counts items without a track)
#[derive(Default, serde::Serialize, Deserialize)]
struct LikedTracks {
    total: u32,
    tracks: Vec<Track>,
    /// Positions in Spotify's list of the items that aren't tracks
    #[serde(default)]
    skipped: Vec<u32>,
}

impl LikedTracks {
    /// Remove an unliked track, `false` if it wasn't cached
    fn remove(&mut self, uri: &str) -> bool {
        let Some(index) = self.tracks.iter().position(|track| track.id.uri() == uri) else {
            return false;
        };
        self.tracks.remove(index);
        // the items that aren't tracks after it move up by one
        let mut position = u32::try_from(index).unwrap_or(u32::MAX);
        for &skipped in &self.skipped {
            if skipped <= position {
                position += 1;
            }
        }
        for skipped in &mut self.skipped {
            if *skipped > position {
                *skipped -= 1;
            }
        }
        self.total = self.total.saturating_sub(1);
        true
    }

    /// The liked items in Spotify's order, `None` for those that aren't tracks. `None` when the
    /// cache doesn't tell where those are (written before it kept their positions).
    fn slots(&self) -> Option<Vec<Option<Track>>> {
        if self.tracks.len() + self.skipped.len() != usize::try_from(self.total).ok()? {
            return None;
        }
        let skipped: HashSet<u32> = self.skipped.iter().copied().collect();
        let mut tracks = self.tracks.iter();
        (0..self.total)
            .map(|position| {
                if skipped.contains(&position) {
                    Some(None)
                } else {
                    tracks.next().map(|track| Some(track.clone()))
                }
            })
            .collect()
    }

    fn from_slots(slots: Vec<Option<Track>>) -> Self {
        let mut liked = Self {
            total: u32::try_from(slots.len()).unwrap_or(u32::MAX),
            ..Self::default()
        };
        for (position, slot) in slots.into_iter().enumerate() {
            match slot {
                Some(track) => liked.tracks.push(track),
                None => liked
                    .skipped
                    .push(u32::try_from(position).unwrap_or(u32::MAX)),
            }
        }
        liked
    }
}

/// Requests of a background liked tracks refresh are this far apart, so they never add up to
/// Spotify's rate limit, which would hold back every other request too
const BACKGROUND_PACE: std::time::Duration = std::time::Duration::from_secs(2);
/// Pages a search for unliked tracks requests at most before it requests every liked track:
/// finding one takes up to 9 in a list of 6400
const MAX_UNLIKED_PROBES: usize = 20;

/// How a liked tracks refresh spaces its requests after the first pages
#[derive(Clone, Copy)]
enum Pace {
    /// One after the other, for an answer now
    Now,
    /// `BACKGROUND_PACE` apart, stopping once Spotify limits the rate of requests
    Background,
}

impl Pace {
    /// Wait before the next request; `false` when a background refresh should stop
    async fn wait(self, started: std::time::Instant) -> bool {
        match self {
            Self::Now => true,
            Self::Background => {
                tokio::time::sleep(BACKGROUND_PACE).await;
                !middleware::rate_limited_since(started)
            }
        }
    }
}

/// What comparing Spotify's newest liked items with the cached ones tells
enum LikedRefresh {
    /// The liked items, complete
    Done(Vec<Option<Track>>),
    /// Some cached items after the anchor were unliked elsewhere: `search` finds them
    Search {
        /// The items liked since the cache was written
        new: Vec<Option<Track>>,
        /// The cached items from the anchor on
        cached: Vec<Option<Track>>,
        search: liked_tracks::RemovalSearch,
    },
    /// The cache doesn't line up with Spotify's list: only requesting it all tells what changed
    FetchAll,
}

/// The result of a search for unliked tracks
enum Unliked {
    Found(Vec<Option<Track>>),
    /// Stopped for Spotify's rate limit
    Stopped,
    /// The lists didn't line up, or the search took too many requests
    Unclear,
}

/// A page of Spotify's liked items, `None` for those that aren't tracks
struct LikedPage {
    items: Vec<Option<Track>>,
    /// Spotify's count of the liked items
    total: usize,
    has_next: bool,
}

/// Compare Spotify's newest liked items (at most 4 pages, from `fetch`) with the cached ones
async fn check_liked_tracks<F, Fut>(cached: &LikedTracks, fetch: F) -> Result<LikedRefresh>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = Result<LikedPage>>,
{
    const PAGES: usize = 4;
    let Some(cached) = cached.slots().filter(|slots| !slots.is_empty()) else {
        return Ok(LikedRefresh::FetchAll);
    };
    let cached_keys = liked_keys(&cached);

    let mut newest = Vec::new();
    let mut total = 0;
    let mut anchor = None;
    for page_index in 0..PAGES {
        let page = fetch(page_index * liked_tracks::PAGE).await?;
        total = page.total;
        newest.extend(page.items);
        anchor = liked_tracks::find_anchor(&liked_keys(&newest), &cached_keys);
        if anchor.is_some() || !page.has_next {
            break;
        }
    }
    let Some(anchor) = anchor else {
        return Ok(LikedRefresh::FetchAll);
    };

    newest.truncate(anchor.new);
    let mut cached = cached;
    let cached = cached.split_off(anchor.cached);
    let expected = newest.len() + cached.len();
    if total == expected {
        newest.extend(cached);
        return Ok(LikedRefresh::Done(newest));
    }
    // the anchor's own item lines up, so the search starts after it
    let Some(search) = liked_tracks::RemovalSearch::new(
        liked_keys(&cached[1..]),
        total.saturating_sub(newest.len() + 1),
    )
    .filter(|_| total < expected) else {
        return Ok(LikedRefresh::FetchAll);
    };
    Ok(LikedRefresh::Search {
        new: newest,
        cached,
        search,
    })
}

/// Finish a liked tracks refresh: find the unliked tracks, or request every liked track when
/// that doesn't work out. `None` when a background refresh stopped for Spotify's rate limit.
async fn liked_tracks_up_to_date<F, Fut>(
    refresh: LikedRefresh,
    pace: Pace,
    fetch: F,
) -> Result<Option<LikedTracks>>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = Result<LikedPage>>,
{
    let started = std::time::Instant::now();
    match refresh {
        LikedRefresh::Done(liked) => Ok(Some(LikedTracks::from_slots(liked))),
        LikedRefresh::Search {
            new,
            cached,
            search,
        } => match find_unliked(new, cached, search, pace, started, &fetch).await? {
            Unliked::Found(liked) => Ok(Some(LikedTracks::from_slots(liked))),
            Unliked::Stopped => Ok(None),
            Unliked::Unclear => all_liked_tracks(pace, started, &fetch).await,
        },
        LikedRefresh::FetchAll => all_liked_tracks(pace, started, &fetch).await,
    }
}

/// Search Spotify's list after the anchor for the cached tracks it no longer has
async fn find_unliked<F, Fut>(
    new: Vec<Option<Track>>,
    cached: Vec<Option<Track>>,
    mut search: liked_tracks::RemovalSearch,
    pace: Pace,
    started: std::time::Instant,
    fetch: &F,
) -> Result<Unliked>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = Result<LikedPage>>,
{
    // where the items after the anchor start in Spotify's list
    let base = new.len() + 1;
    for _ in 0..MAX_UNLIKED_PROBES {
        let offset = match search.next() {
            liked_tracks::Step::Done(removed) => {
                let removed: HashSet<usize> = removed.into_iter().map(|i| i + 1).collect();
                let mut liked = new;
                liked.extend(
                    cached
                        .into_iter()
                        .enumerate()
                        .filter(|(i, _)| !removed.contains(i))
                        .map(|(_, item)| item),
                );
                return Ok(Unliked::Found(liked));
            }
            liked_tracks::Step::Probe(offset) => offset,
        };
        if !pace.wait(started).await {
            return Ok(Unliked::Stopped);
        }
        let page = fetch(base + offset).await?;
        if !search.feed(offset, &liked_keys(&page.items)) {
            return Ok(Unliked::Unclear);
        }
    }
    Ok(Unliked::Unclear)
}

/// Request every liked track, page by page. `None` when a background refresh stopped for
/// Spotify's rate limit.
async fn all_liked_tracks<F, Fut>(
    pace: Pace,
    started: std::time::Instant,
    fetch: &F,
) -> Result<Option<LikedTracks>>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = Result<LikedPage>>,
{
    let mut liked = Vec::new();
    loop {
        let page = fetch(liked.len()).await?;
        let has_next = page.has_next && !page.items.is_empty();
        liked.extend(page.items);
        if !has_next {
            return Ok(Some(LikedTracks::from_slots(liked)));
        }
        if !pace.wait(started).await {
            return Ok(None);
        }
    }
}

/// Whether a refresh found the same liked items as cached, so nothing needs writing
fn liked_unchanged(cached: &LikedTracks, liked: &[Option<Track>]) -> bool {
    cached
        .slots()
        .is_some_and(|cached| liked_keys(&cached) == liked_keys(liked))
}

fn liked_keys(items: &[Option<Track>]) -> Vec<liked_tracks::Key> {
    items
        .iter()
        .map(|item| item.as_ref().map(|track| track.id.uri()))
        .collect()
}

fn load_liked_tracks() -> LikedTracks {
    load_data_from_file_cache(
        FileCacheKey::LikedTracks,
        &config::get_config().cache_folder,
    )
    .unwrap_or_default()
}

fn store_liked_tracks(liked: &LikedTracks) -> Result<()> {
    store_data_into_file_cache(
        FileCacheKey::LikedTracks,
        &config::get_config().cache_folder,
        liked,
    )
    .context("store the liked tracks into the cache folder")
}

/// Keep liked tracks brought up to date in the background: in the cache, in the liked marks and
/// on the shown page, where the selection stays on the same track
fn keep_liked_tracks(state: &SharedState, liked: &LikedTracks) -> Result<()> {
    store_liked_tracks(liked)?;
    let saved: HashMap<String, Track> = liked
        .tracks
        .iter()
        .map(|t| (t.id.uri(), t.clone()))
        .collect();
    store_data_into_file_cache(
        FileCacheKey::SavedTracks,
        &config::get_config().cache_folder,
        &saved,
    )
    .context("store user's saved tracks into the cache folder")?;

    let liked_id = ContextId::Tracks(USER_LIKED_TRACKS_ID.to_owned());
    let mut ui = state.ui.lock();
    let shows_liked = matches!(
        ui.current_page(),
        PageState::Context { id: Some(id), .. } if *id == liked_id
    );
    let selected = if shows_liked {
        ui.current_page_mut().selected()
    } else {
        None
    };
    let mut data = state.data.write();
    data.user_data.saved_tracks = saved;
    if let Some(shown) = data.context_tracks_mut(&liked_id) {
        let selected = liked_tracks::apply_changes(shown, &liked.tracks, |t| t.id.uri(), selected);
        if let Some(i) = selected {
            ui.current_page_mut().select(i);
        }
    }
    Ok(())
}

/// Remove a track unliked here from the cached liked tracks: the next check compares Spotify's
/// count with the cached one, and a mismatch makes it request every liked track again
fn forget_cached_liked_track(uri: &str) {
    let cache_folder = &config::get_config().cache_folder;
    let Some(mut liked) =
        load_data_from_file_cache::<LikedTracks>(FileCacheKey::LikedTracks, cache_folder)
    else {
        return;
    };
    if liked.remove(uri) {
        if let Err(err) =
            store_data_into_file_cache(FileCacheKey::LikedTracks, cache_folder, &liked)
        {
            tracing::warn!("Failed to update the cached liked tracks: {err:#}");
        }
    }
}

fn move_seed_track_to_front(tracks: &mut Vec<Track>, seed_track: Track) {
    tracks.retain(|track| track.id != seed_track.id);
    tracks.insert(0, seed_track);
}

#[cfg(test)]
mod tests {
    use super::{
        check_liked_tracks, liked_tracks_up_to_date, liked_unchanged, move_seed_track_to_front,
        parse_current_playback_response, LikedPage, LikedTracks, Pace,
    };
    use crate::state::Track;
    use rspotify::model::{Id, PlayableItem, TrackId};

    fn sample_track(id: &'static str, name: &str) -> Track {
        Track {
            id: TrackId::from_id(id).unwrap().into_static(),
            name: name.to_string(),
            artists: vec![],
            album: None,
            duration: std::time::Duration::default(),
            explicit: false,
            added_at: 0,
        }
    }

    #[test]
    fn removing_an_unliked_track_lowers_the_count() {
        let unliked = sample_track("3n3Ppam7vgaVa1iaRUc9Lp", "unliked");
        let kept = sample_track("4uLU6hMCjMI75M1A2tKUQC", "kept");
        let mut liked = LikedTracks {
            total: 2,
            tracks: vec![unliked.clone(), kept.clone()],
            skipped: Vec::new(),
        };

        assert!(liked.remove(&unliked.id.uri()));
        assert_eq!(liked.total, 1);
        assert_eq!(liked.tracks.len(), 1);
        assert_eq!(liked.tracks[0].id, kept.id);

        // a track that isn't cached leaves the count alone
        assert!(!liked.remove(&unliked.id.uri()));
        assert_eq!(liked.total, 1);
    }

    #[test]
    fn removing_a_track_moves_the_items_after_it_that_arent_tracks() {
        let first = sample_track("3n3Ppam7vgaVa1iaRUc9Lp", "first");
        let unliked = sample_track("4uLU6hMCjMI75M1A2tKUQC", "unliked");
        let last = sample_track("1301WleyT98MSxVHPZCA6M", "last");
        // positions: 0 not a track, 1 first, 2 unliked, 3 not a track, 4 last
        let mut liked = LikedTracks {
            total: 5,
            tracks: vec![first, unliked.clone(), last],
            skipped: vec![0, 3],
        };

        assert!(liked.remove(&unliked.id.uri()));
        assert_eq!(liked.total, 4);
        assert_eq!(liked.skipped, vec![0, 2]);
    }

    fn liked_track(i: usize) -> Track {
        sample_track(Box::leak(format!("{i:0>22}").into_boxed_str()), "liked")
    }

    fn ids(items: &[Option<Track>]) -> Vec<Option<TrackId<'static>>> {
        items
            .iter()
            .map(|item| item.as_ref().map(|t| t.id.clone()))
            .collect()
    }

    /// Brings `cached` up to date against Spotify's liked items `spotify`, served in pages of
    /// 50; the result and the number of pages requested
    async fn refresh_against(
        cached: &LikedTracks,
        spotify: &[Option<Track>],
    ) -> (LikedTracks, usize) {
        let requests = std::cell::Cell::new(0);
        let fetch = |offset: usize| {
            requests.set(requests.get() + 1);
            let start = offset.min(spotify.len());
            let end = (offset + 50).min(spotify.len());
            std::future::ready(Ok(LikedPage {
                items: spotify[start..end].to_vec(),
                total: spotify.len(),
                has_next: end < spotify.len(),
            }))
        };
        let refresh = check_liked_tracks(cached, &fetch).await.expect("a check");
        let liked = liked_tracks_up_to_date(refresh, Pace::Now, &fetch)
            .await
            .expect("a refresh")
            .expect("liked tracks");
        (liked, requests.get())
    }

    fn cached(items: &[Option<Track>]) -> LikedTracks {
        LikedTracks::from_slots(items.to_vec())
    }

    #[test]
    fn a_refresh_without_changes_writes_nothing() {
        let library: Vec<_> = (0..3).map(|i| Some(liked_track(i))).collect();
        assert!(liked_unchanged(&cached(&library), &library));
        let mut liked = vec![Some(liked_track(9))];
        liked.extend(library.iter().cloned());
        assert!(!liked_unchanged(&cached(&library), &liked));
    }

    #[tokio::test]
    async fn only_new_likes_take_one_request() {
        let library: Vec<_> = (0..300).map(|i| Some(liked_track(i))).collect();
        let mut spotify = vec![Some(liked_track(1000)), Some(liked_track(1001))];
        spotify.extend(library.iter().cloned());

        let (liked, requests) = refresh_against(&cached(&library), &spotify).await;
        assert_eq!(ids(&liked.slots().expect("slots")), ids(&spotify));
        assert_eq!(requests, 1);
    }

    #[tokio::test]
    async fn tracks_unliked_elsewhere_are_found_with_few_requests() {
        // the newest cached track and one far down were unliked, one other track was liked
        let mut library: Vec<_> = (0..1000).map(|i| Some(liked_track(i))).collect();
        library[400] = None;
        let mut spotify = vec![Some(liked_track(5000))];
        spotify.extend(
            library
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != 0 && *i != 700)
                .map(|(_, item)| item.clone()),
        );

        let (liked, requests) = refresh_against(&cached(&library), &spotify).await;
        assert_eq!(ids(&liked.slots().expect("slots")), ids(&spotify));
        assert!(requests <= 7, "{requests} requests");
    }

    #[tokio::test]
    async fn a_cache_without_positions_requests_everything_once() {
        let library: Vec<_> = (0..120).map(|i| Some(liked_track(i))).collect();
        let mut spotify = library.clone();
        spotify[60] = None;
        // written before the positions were kept: one item too few to line up
        let mut old = cached(&library[1..]);
        old.total += 1;

        let (liked, requests) = refresh_against(&old, &spotify).await;
        assert_eq!(ids(&liked.slots().expect("slots")), ids(&spotify));
        assert_eq!(requests, 3);
    }

    #[tokio::test]
    async fn lists_that_dont_line_up_fall_back_to_requesting_everything() {
        let library: Vec<_> = (0..300).map(|i| Some(liked_track(i))).collect();
        let mut spotify = library.clone();
        spotify.remove(100);
        // a track the cache doesn't have, where the search reads
        spotify[105] = Some(liked_track(9999));

        let (liked, _) = refresh_against(&cached(&library), &spotify).await;
        assert_eq!(ids(&liked.slots().expect("slots")), ids(&spotify));
    }

    #[test]
    fn liked_slots_round_trip() {
        let first = sample_track("3n3Ppam7vgaVa1iaRUc9Lp", "first");
        let second = sample_track("4uLU6hMCjMI75M1A2tKUQC", "second");
        let liked = LikedTracks {
            total: 3,
            tracks: vec![first.clone(), second.clone()],
            skipped: vec![1],
        };

        let slots = liked.slots().expect("slots");
        let ids: Vec<_> = slots
            .iter()
            .map(|s| s.as_ref().map(|t| t.id.clone()))
            .collect();
        assert_eq!(ids, vec![Some(first.id), None, Some(second.id)]);
        assert_eq!(LikedTracks::from_slots(slots).skipped, vec![1]);

        // a cache from before the positions were kept doesn't tell where they are
        let old = LikedTracks {
            total: 3,
            tracks: liked.tracks,
            skipped: Vec::new(),
        };
        assert!(old.slots().is_none());
    }

    #[test]
    fn move_seed_track_to_front_prepends_missing_seed() {
        let seed = sample_track("3n3Ppam7vgaVa1iaRUc9Lp", "seed");
        let second = sample_track("4uLU6hMCjMI75M1A2tKUQC", "second");
        let third = sample_track("1301WleyT98MSxVHPZCA6M", "third");
        let mut tracks = vec![second.clone(), third];

        move_seed_track_to_front(&mut tracks, seed.clone());

        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].id, seed.id);
        assert_eq!(tracks[1].id, second.id);
    }

    #[test]
    fn move_seed_track_to_front_reorders_existing_seed_without_duplication() {
        let seed = sample_track("3n3Ppam7vgaVa1iaRUc9Lp", "seed");
        let second = sample_track("4uLU6hMCjMI75M1A2tKUQC", "second");
        let mut tracks = vec![second.clone(), seed.clone()];

        move_seed_track_to_front(&mut tracks, seed.clone());

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, seed.id);
        assert_eq!(tracks[1].id, second.id);
    }

    #[test]
    fn patches_missing_external_ids_in_playback_track() {
        let response = serde_json::json!({
            "device": {
                "id": null,
                "is_active": true,
                "is_private_session": false,
                "is_restricted": false,
                "name": "Spotify Player",
                "type": "Computer",
                "volume_percent": 50
            },
            "repeat_state": "off",
            "shuffle_state": false,
            "context": null,
            "timestamp": 0,
            "progress_ms": 1000,
            "is_playing": true,
            "item": {
                "album": {
                    "album_type": "album",
                    "artists": [],
                    "external_urls": {},
                    "href": null,
                    "id": null,
                    "images": [],
                    "name": "Skool Luv Affair"
                },
                "artists": [],
                "disc_number": 1,
                "duration_ms": 239_469,
                "explicit": false,
                "external_urls": {},
                "href": "https://api.spotify.com/v1/tracks/6t7WriKgVszATnrdBKSUAf",
                "id": "6t7WriKgVszATnrdBKSUAf",
                "is_local": false,
                "name": "Just One Day",
                "preview_url": null,
                "track_number": 5,
                "type": "track"
            },
            "currently_playing_type": "track",
            "actions": { "disallows": {} }
        });

        let playback = parse_current_playback_response(&response.to_string())
            .unwrap()
            .unwrap();

        assert!(matches!(playback.item, Some(PlayableItem::Track(_))));
    }

    #[test]
    fn parses_queued_tracks_without_external_ids_as_tracks() {
        let track = |id: &str| {
            serde_json::json!({
                "album": {"album_type": "single", "artists": [], "external_urls": {}, "href": null,
                          "id": "6PbCMUiGA9M7BA5tNW9L1H", "images": [], "name": "Deti Techno"},
                "artists": [], "disc_number": 1, "duration_ms": 200_000, "explicit": false,
                "external_urls": {}, "href": null, "id": id, "is_local": false, "name": id,
                "preview_url": null, "track_number": 1, "type": "track"
            })
        };
        let response = serde_json::json!({
            "currently_playing": track("6t7WriKgVszATnrdBKSUAf"),
            "queue": [track("4uLU6hMCjMI75M1A2tKUQC"), track("3n3Ppam7vgaVa1iaRUc9Lp")],
        });

        let queue = super::parse_queue_response(&response.to_string()).unwrap();

        assert!(matches!(
            queue.currently_playing,
            Some(PlayableItem::Track(_))
        ));
        assert!(queue
            .queue
            .iter()
            .all(|item| matches!(item, PlayableItem::Track(_))));
    }
}
