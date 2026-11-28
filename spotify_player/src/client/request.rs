use crate::state::{
    AlbumId, Category, ContextId, Item, ItemId, PlayableId, Playback, PlaylistId, TrackId,
};

#[derive(Clone, Debug)]
/// A request that modifies the player's playback
pub enum PlayerRequest {
    NextTrack,
    PreviousTrack,
    Resume,
    Pause,
    ResumePause,
    SeekTrack(chrono::Duration),
    Repeat,
    Shuffle,
    Volume(u8),
    ToggleMute,
    TransferPlayback(String, bool),
    StartPlayback(Playback, Option<bool>),
}

#[derive(Clone, Debug)]
/// A request to the client
pub enum ClientRequest {
    GetCurrentUser,
    GetDevices,
    GetBrowseCategories,
    GetBrowseCategoryPlaylists(Category),
    GetUserPlaylists,
    GetUserSavedAlbums,
    GetUserSavedShows,
    GetUserFollowedArtists,
    GetContext(ContextId),
    GetCurrentPlayback,
    /// Move another device's playback to this machine's device, playing
    TakePlayback,
    /// The app's Spotify logins and the Soloist on this machine, for the `g A` page
    GetLogins,
    /// Sign in to the `n`-th login again, in the browser
    StartSignIn(usize),
    /// Finish the sign-in with the pasted redirect address or code
    FinishSignIn(Pasted),
    CancelSignIn,
    /// Load the cover and details of a track shown before Spotify reported it
    NewPlaybackShown,
    Search(String),
    AddPlayableToQueue(PlayableId<'static>),
    AddAlbumToQueue(AlbumId<'static>),
    AddPlayableToPlaylist(PlaylistId<'static>, PlayableId<'static>),
    DeleteTrackFromPlaylist(PlaylistId<'static>, TrackId<'static>),
    ReorderPlaylistItems {
        playlist_id: PlaylistId<'static>,
        insert_index: usize,
        range_start: usize,
        range_length: Option<usize>,
        snapshot_id: Option<String>,
    },
    AddToLibrary(Item),
    DeleteFromLibrary(ItemId),
    Player(PlayerRequest),
    GetCurrentUserQueue,
    GetLyrics {
        track_id: TrackId<'static>,
    },
    #[cfg(feature = "streaming")]
    RestartIntegratedClient,
    CreatePlaylist {
        playlist_name: String,
        public: bool,
        collab: bool,
        desc: String,
    },
}

/// Pasted text holding an auth code, kept out of the logs that show requests
#[derive(Clone)]
pub struct Pasted(pub String);

impl std::fmt::Debug for Pasted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pasted(..)")
    }
}
