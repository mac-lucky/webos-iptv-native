//! The playlists and the viewer's groups in the store: what the
//! refresher fetches, the order and the members of the groups the lists show, and the
//! [`LibraryEdit`]s the library panel sends, over storage on the television and in memory over
//! [`FakeData`] in the simulator.
//!
//! Groups show as: the built-in ones the settings show, then the playlist and custom groups the
//! viewer placed, in that order, then the other custom groups as made, then the other playlist
//! groups in playlist order (merged by name across the playlists). A playlist group lists its
//! channels in the viewer's order where one was saved (under the playlist sorting), the rest in
//! playlist order; channels moved into a custom group are not in it, and a playlist group left
//! with none is not listed.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use iptv::epg::PlaylistId;
use iptv::model::ChannelKey;
use iptv::redact;
use iptv::refresh;
use iptv::storage::{GroupRef, Storage, MAIN_PLAYLIST, MAX_PLAYLISTS};
use tv_screens::data::fake::FakeData;
use tv_screens::data::Group;
use tv_screens::library::{LibraryEdit, PlaylistEntry, SecretUrl};
use tv_screens::panels::{keys, Settings};

use super::{or_log, Store, StoreData};
use crate::logs::log;

/// The viewer's arrangement as the lists use it.
#[derive(Default)]
pub(super) struct Layout {
    /// Custom groups as made, each with its channels in order.
    custom: Vec<(String, Vec<ChannelKey>)>,
    /// Channels moved into a custom group: they left their playlist groups.
    moved: HashSet<ChannelKey>,
    group_order: HashMap<GroupRef, usize>,
    /// Each playlist group's placed channels and their places.
    channel_order: HashMap<String, HashMap<ChannelKey, usize>>,
}

impl Layout {
    pub(super) fn read(storage: &Storage) -> Layout {
        let mut moved = HashSet::new();
        let custom = or_log("custom groups", storage.custom_groups())
            .into_iter()
            .map(|g| {
                let keys = g.channels.iter().map(|&(key, _)| ChannelKey(key)).collect();
                moved.extend(g.channels.iter().filter(|c| c.1).map(|c| ChannelKey(c.0)));
                (g.name, keys)
            })
            .collect();
        let group_order = or_log("group order", storage.group_order())
            .into_iter()
            .enumerate()
            .map(|(i, g)| (g, i))
            .collect();
        let channel_order = or_log("channel order", storage.channel_order())
            .into_iter()
            .map(|(group, keys)| {
                (
                    group,
                    keys.into_iter().enumerate().map(|(i, k)| (k, i)).collect(),
                )
            })
            .collect();
        Layout {
            custom,
            moved,
            group_order,
            channel_order,
        }
    }

    pub(super) fn moved(&self, key: ChannelKey) -> bool {
        self.moved.contains(&key)
    }

    /// The channels of custom group `name`, in its order.
    pub(super) fn members(&self, name: &str) -> &[ChannelKey] {
        self.custom
            .iter()
            .find(|(n, _)| n == name)
            .map_or(&[], |(_, keys)| keys.as_slice())
    }

    /// Where the viewer put `key` among the channels of playlist group `group`.
    pub(super) fn channel_place(&self, group: &str, key: ChannelKey) -> Option<usize> {
        self.channel_order.get(group)?.get(&key).copied()
    }

    /// The custom groups and `playlist` groups in the order the lists show them.
    pub(super) fn arrange(&self, playlist: impl Iterator<Item = String>) -> Vec<Group> {
        let mut groups: Vec<Group> = self
            .custom
            .iter()
            .map(|(name, _)| Group::Custom(name.clone()))
            .chain(playlist.map(Group::Playlist))
            .collect();
        groups.sort_by_cached_key(|g| match self.group_order.get(&group_ref(g)) {
            Some(&at) => (0, at),
            None if matches!(g, Group::Custom(_)) => (1, 0),
            None => (2, 0),
        });
        groups
    }
}

/// The order tables' name of a playlist or custom group; the built-in ones are never placed.
fn group_ref(group: &Group) -> GroupRef {
    match group {
        Group::Custom(name) => GroupRef::Custom(name.clone()),
        other => GroupRef::Playlist(other.name().to_owned()),
    }
}

/// What the refresher fetches: every enabled playlist that has a URL, by row id, and the Settings
/// guide. `None` while there is none to fetch. No `Debug`: the URLs carry tokens. The display
/// order stays out of it: moving a playlist up or down changes nothing fetched, so it must not
/// cancel a refresh under way and start another ([`refresh::Refresher::set_config`]).
pub(super) fn refresh_config(storage: &Storage, settings: &Settings) -> Option<refresh::Config> {
    let mut playlists: Vec<(PlaylistId, String)> = or_log("playlists", storage.playlists())
        .into_iter()
        .filter(|p| p.enabled && !p.url.is_empty())
        .map(|p| (p.id, p.url))
        .collect();
    playlists.sort_unstable_by_key(|(id, _)| *id);
    if playlists.is_empty() {
        return None;
    }
    Some(refresh::Config {
        playlists,
        extra_guides: extra_guides(settings),
        interval: Some(Duration::from_secs(
            u64::from(settings.number(keys::EPG_UPDATE_HOURS)) * 3600,
        )),
    })
}

/// The guide the Settings URL names, fetched besides the ones the playlists declare.
pub(super) fn extra_guides(settings: &Settings) -> Vec<String> {
    let guide = settings.get(keys::EPG_URL);
    (!guide.is_empty())
        .then(|| guide.to_owned())
        .into_iter()
        .collect()
}

/// A new playlist's name: its server's host, without credentials or port.
fn name_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = host.split(':').next().unwrap_or_default();
    if host.is_empty() {
        "Playlist".to_owned()
    } else {
        host.to_owned()
    }
}

impl StoreData {
    /// Read the playlists and the viewer's arrangement again; every stored playlist URL is
    /// registered for masking.
    pub(super) fn load_library(&mut self) {
        let counts = or_log("channel counts", self.storage.channel_counts());
        self.playlists = or_log("playlists", self.storage.playlists())
            .into_iter()
            .map(|p| {
                if !p.url.is_empty() {
                    redact::register_secrets(&p.url);
                }
                PlaylistEntry {
                    channels: counts.get(&p.id).copied().unwrap_or(0),
                    id: p.id,
                    name: p.name,
                    url: SecretUrl(p.url),
                    enabled: p.enabled,
                }
            })
            .collect();
        self.layout = Layout::read(&self.storage);
        self.lists.get_mut().clear();
    }

    /// Make the change; a playlist change reads the channels again.
    pub(super) fn edit(&mut self, edit: LibraryEdit) {
        let s = &self.storage;
        let channels_change = match edit {
            LibraryEdit::AddPlaylist(SecretUrl(url)) => {
                redact::register_secrets(&url);
                let stored = or_log("playlists", s.playlists());
                if stored.len() >= MAX_PLAYLISTS {
                    log("storage: playlist not added: the list is full");
                } else if stored.iter().any(|p| p.id == MAIN_PLAYLIST) {
                    or_log("playlist", s.add_playlist(&name_of(&url), &url).map(|_| ()));
                } else {
                    // Without playlist 1 the new one is it: the Settings URL names it.
                    let mut settings = self.settings.clone();
                    if settings.set(keys::PLAYLIST_URL, url) {
                        self.save_setting(keys::PLAYLIST_URL, &settings);
                    }
                }
                false
            }
            LibraryEdit::SetPlaylistUrl { playlist, url } => {
                redact::register_secrets(&url.0);
                or_log("playlist", s.set_playlist_url(playlist, &url.0));
                false
            }
            LibraryEdit::SetPlaylistEnabled { playlist, enabled } => {
                or_log("playlist", s.set_playlist_enabled(playlist, enabled));
                true
            }
            LibraryEdit::MovePlaylist { playlist, up } => {
                or_log("playlist", s.move_playlist(playlist, up));
                true
            }
            LibraryEdit::RemovePlaylist(playlist) => {
                or_log("playlist", s.remove_playlist(playlist));
                true
            }
            LibraryEdit::CreateGroup(name) => {
                refused("create", or_log("group", s.create_group(&name)));
                false
            }
            LibraryEdit::RenameGroup { from, to } => {
                refused("rename", or_log("group", s.rename_group(&from, &to)));
                false
            }
            LibraryEdit::DeleteGroup(name) => {
                or_log("group", s.delete_group(&name));
                false
            }
            LibraryEdit::AddToGroup { to, channels, from } => {
                let from = from.as_ref().map(group_ref);
                refused(
                    "add to",
                    or_log("group", s.add_to_group(&to, &channels, from.as_ref())),
                );
                false
            }
            LibraryEdit::RemoveFromGroup { group, channels } => {
                or_log("group", s.remove_from_group(&group, &channels));
                false
            }
            LibraryEdit::SetGroupOrder(groups) => {
                let order: Vec<GroupRef> = groups.iter().map(group_ref).collect();
                or_log("group order", s.set_group_order(&order));
                false
            }
            LibraryEdit::SetChannelOrder { group, channels } => {
                or_log(
                    "channel order",
                    s.set_channel_order(&group_ref(&group), &channels),
                );
                false
            }
        };
        if channels_change {
            self.reload();
        } else {
            self.load_library();
        }
    }
}

/// Storage said no (a name taken or not allowed, a group gone): the panel had checked, so only
/// a log line.
fn refused(what: &str, done: bool) {
    if !done {
        log(&format!("storage: could not {what} the group"));
    }
}

/// The simulator's playlists and custom groups change in memory.
pub(super) fn edit_fake(data: &mut FakeData, edit: LibraryEdit) {
    let playlists = &mut data.playlists;
    let groups = &mut data.custom_groups;
    let position = |playlists: &[PlaylistEntry], id| playlists.iter().position(|p| p.id == id);
    match edit {
        LibraryEdit::AddPlaylist(url) => {
            let id = playlists.iter().map(|p| p.id).max().unwrap_or(0) + 1;
            playlists.push(PlaylistEntry {
                id,
                name: name_of(&url.0),
                url,
                enabled: true,
                channels: 0,
            });
        }
        LibraryEdit::SetPlaylistUrl { playlist, url } => {
            if let Some(i) = position(playlists, playlist) {
                playlists[i].url = url;
            }
        }
        LibraryEdit::SetPlaylistEnabled { playlist, enabled } => {
            if let Some(i) = position(playlists, playlist) {
                playlists[i].enabled = enabled;
            }
        }
        LibraryEdit::MovePlaylist { playlist, up } => {
            if let Some(at) = position(playlists, playlist) {
                let to = if up { at.checked_sub(1) } else { Some(at + 1) };
                if let Some(to) = to.filter(|&to| to < playlists.len()) {
                    playlists.swap(at, to);
                }
            }
        }
        LibraryEdit::RemovePlaylist(playlist) => playlists.retain(|p| p.id != playlist),
        LibraryEdit::CreateGroup(name) => {
            if !groups.iter().any(|(n, _)| *n == name) {
                groups.push((name, Vec::new()));
            }
        }
        LibraryEdit::RenameGroup { from, to } => {
            for (name, _) in groups.iter_mut().filter(|(n, _)| *n == from) {
                *name = to.clone();
            }
        }
        LibraryEdit::DeleteGroup(name) => groups.retain(|(n, _)| *n != name),
        LibraryEdit::AddToGroup { to, channels, from } => {
            if let Some(Group::Custom(source)) = &from {
                for (_, keys) in groups.iter_mut().filter(|(n, _)| n == source && *n != to) {
                    keys.retain(|k| !channels.contains(k));
                }
            }
            if let Some((_, keys)) = groups.iter_mut().find(|(n, _)| *n == to) {
                let new: Vec<ChannelKey> = channels
                    .iter()
                    .copied()
                    .filter(|k| !keys.contains(k))
                    .collect();
                keys.extend(new);
            }
        }
        LibraryEdit::RemoveFromGroup { group, channels } => {
            for (_, keys) in groups.iter_mut().filter(|(n, _)| *n == group) {
                keys.retain(|k| !channels.contains(k));
            }
        }
        // Only the custom groups take an order here; the fake lists them first.
        LibraryEdit::SetGroupOrder(order) => {
            groups.sort_by_key(|(name, _)| {
                order
                    .iter()
                    .position(|g| matches!(g, Group::Custom(n) if n == name))
                    .unwrap_or(usize::MAX)
            });
        }
        LibraryEdit::SetChannelOrder { group, channels } => {
            if let Group::Custom(name) = group {
                for (_, keys) in groups.iter_mut().filter(|(n, _)| *n == name) {
                    keys.sort_by_key(|k| {
                        channels.iter().position(|c| c == k).unwrap_or(usize::MAX)
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
