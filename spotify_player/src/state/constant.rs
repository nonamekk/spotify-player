pub use super::*;
use std::sync::LazyLock;

pub const USER_TOP_TRACKS_URI: &str = "tracks:user-top-tracks";
pub const USER_RECENTLY_PLAYED_TRACKS_URI: &str = "tracks:user-recently-played-tracks";
pub const USER_LIKED_TRACKS_URI: &str = "tracks:user-liked-tracks";
/// The tracks the account's devices played (`g D`)
pub const OTHER_DEVICES_URI: &str = "tracks:other-devices";
/// The list another device plays or played last (`g d`)
pub const OTHER_DEVICE_LIST_URI: &str = "tracks:other-device-list";

pub static USER_TOP_TRACKS_ID: LazyLock<TracksId> =
    LazyLock::new(|| TracksId::new(USER_TOP_TRACKS_URI, "Top Tracks"));

pub static USER_RECENTLY_PLAYED_TRACKS_ID: LazyLock<TracksId> =
    LazyLock::new(|| TracksId::new(USER_RECENTLY_PLAYED_TRACKS_URI, "Recently Played Tracks"));

pub static USER_LIKED_TRACKS_ID: LazyLock<TracksId> =
    LazyLock::new(|| TracksId::new(USER_LIKED_TRACKS_URI, "Liked Tracks"));

pub static OTHER_DEVICES_ID: LazyLock<TracksId> =
    LazyLock::new(|| TracksId::new(OTHER_DEVICES_URI, "Played on Devices"));

pub static OTHER_DEVICE_LIST_ID: LazyLock<TracksId> =
    LazyLock::new(|| TracksId::new(OTHER_DEVICE_LIST_URI, "Other Device's List"));
