//! The playlists and the user's own groups as the screens see them,
//! and the changes to them a screen asks for ([`Command::Library`](crate::data::Command)). The
//! store makes the changes; [`crate::panels::library`] is where the viewer asks for them.

use std::fmt;

use iptv::model::ChannelKey;
use iptv::redact;

use crate::data::Group;

/// A URL that carries an account token. Its `Debug` is masked ([`redact::url`]), so a command or
/// a playlist row in a log line or a test failure never shows the token.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretUrl(pub String);

impl fmt::Debug for SecretUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&redact::url(&self.0))
    }
}

/// One stored playlist, as Settings lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistEntry {
    /// Its storage row; [`iptv::storage::MAIN_PLAYLIST`] is the one the Settings URL names.
    pub id: i64,
    pub name: String,
    pub url: SecretUrl,
    /// A disabled playlist is neither fetched nor shown, and keeps its channels.
    pub enabled: bool,
    /// Channels stored for it.
    pub channels: usize,
}

/// A change to the playlists or the groups. The groups are named as the screens name them:
/// [`Group::Playlist`] and [`Group::Custom`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LibraryEdit {
    /// Add a playlist fetched from this URL, last in order. With no playlist 1 the panel sets
    /// the Settings URL instead, which makes it.
    AddPlaylist(SecretUrl),
    SetPlaylistUrl {
        playlist: i64,
        url: SecretUrl,
    },
    SetPlaylistEnabled {
        playlist: i64,
        enabled: bool,
    },
    /// One place earlier (`up`) or later.
    MovePlaylist {
        playlist: i64,
        up: bool,
    },
    RemovePlaylist(i64),
    CreateGroup(String),
    RenameGroup {
        from: String,
        to: String,
    },
    DeleteGroup(String),
    /// Copy `channels` into custom group `to`, or move them there out of `from`.
    AddToGroup {
        to: String,
        channels: Vec<ChannelKey>,
        from: Option<Group>,
    },
    RemoveFromGroup {
        group: String,
        channels: Vec<ChannelKey>,
    },
    /// The playlist and custom groups in the viewer's order; the built-in groups stay first.
    SetGroupOrder(Vec<Group>),
    /// A playlist or custom group's channels in the viewer's order.
    SetChannelOrder {
        group: Group,
        channels: Vec<ChannelKey>,
    },
}

/// Where the library panel opens ([`Navigate::Library`](crate::data::Navigate)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LibraryPage {
    /// Settings' playlists: add, enable, sort and remove them.
    Playlists,
    /// Manage groups: show or hide each group, reorder them, make a new one.
    Groups,
    /// A group's channels: select them to copy or move into a custom group, reorder them.
    Channels(Group),
    /// A custom group's options: rename, delete, its channels.
    CustomGroup(String),
}
