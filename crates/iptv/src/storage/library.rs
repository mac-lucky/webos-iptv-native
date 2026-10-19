//! The user's library on top of the stored playlists: which playlists
//! are fetched and in what order, the user's own groups, and the order the user gave the groups
//! and their channels.
//!
//! Custom groups hold channel keys, as favourites do: a channel that drops out of its playlist
//! keeps its place and is back when the playlist lists it again. A channel *moved* into a custom
//! group, rather than copied, leaves its playlist groups (it stays in All channels). The order
//! tables list only what the user placed; whatever they do not list follows the listed ones in
//! playlist order.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use super::{prune_validators, Result, Storage};
use crate::epg::PlaylistId;
use crate::model::ChannelKey;

/// The playlist the Settings URL names (`playlist.url`: provisioning, the Settings URL row).
pub const MAIN_PLAYLIST: PlaylistId = 1;

/// The most playlists the library panel adds: each is fetched on every refresh.
pub const MAX_PLAYLISTS: usize = 16;
/// The most custom groups kept.
pub const MAX_CUSTOM_GROUPS: usize = 200;
/// The longest custom group name, in characters.
pub const MAX_GROUP_NAME: usize = 64;
/// The most rows a [`UserLayout`] may bring in (memberships plus ordered groups and channels):
/// a restored layout is untrusted input.
const MAX_LAYOUT_ROWS: usize = 200_000;

/// A playlist or custom group, as the order tables name it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GroupRef {
    /// A group the playlists declare, one for every playlist that has it.
    Playlist(String),
    Custom(String),
}

impl GroupRef {
    fn kind(&self) -> i64 {
        match self {
            GroupRef::Playlist(_) => 0,
            GroupRef::Custom(_) => 1,
        }
    }

    fn name(&self) -> &str {
        match self {
            GroupRef::Playlist(name) | GroupRef::Custom(name) => name,
        }
    }
}

/// A custom group and its channels in the group's order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomGroup {
    pub name: String,
    /// Each channel's key ([`ChannelKey`]'s bits) and whether it was moved here, which takes it
    /// out of its playlist groups.
    pub channels: Vec<(i64, bool)>,
}

/// What the user arranged, for backup and restore: custom groups with their channels, the group
/// order, and the channel order of playlist groups. Hidden groups are settings
/// (`groups.hidden`, `groups.show_*`) and travel with the settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLayout {
    pub custom_groups: Vec<CustomGroup>,
    pub group_order: Vec<GroupRef>,
    /// A playlist group's name and its channel keys in the user's order.
    pub channel_order: Vec<(String, Vec<i64>)>,
}

/// A name a custom group may have: trimmed, not empty, at most [`MAX_GROUP_NAME`] characters,
/// no control characters.
pub fn group_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && name.chars().count() <= MAX_GROUP_NAME
        && !name.chars().any(char::is_control)
}

impl Storage {
    // --- playlists --------------------------------------------------------------------------

    /// Points [`MAIN_PLAYLIST`] at `url`, making its row (first in order) when there is none and
    /// `url` is not empty. A cleared URL is stored as "", so its token is not kept.
    pub fn set_main_playlist(&self, url: &str) -> Result<()> {
        let tx = self.write()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM playlist WHERE id = ?1)",
            [MAIN_PLAYLIST],
            |row| row.get(0),
        )?;
        if exists {
            tx.execute(
                "UPDATE playlist SET url = ?2 WHERE id = ?1 AND url <> ?2",
                params![MAIN_PLAYLIST, url],
            )?;
        } else if !url.is_empty() {
            tx.execute(
                "INSERT INTO playlist (id, name, url, position) VALUES (?1, 'Playlist', ?2,
                 (SELECT COALESCE(MIN(position), 1) - 1 FROM playlist))",
                params![MAIN_PLAYLIST, url],
            )?;
        }
        prune_validators(&tx)?;
        tx.commit()
    }

    /// The playlist is fetched from `url` from now on; the old URL's validators go with it.
    pub fn set_playlist_url(&self, id: PlaylistId, url: &str) -> Result<()> {
        let tx = self.write()?;
        tx.execute(
            "UPDATE playlist SET url = ?2 WHERE id = ?1 AND url <> ?2",
            params![id, url],
        )?;
        prune_validators(&tx)?;
        tx.commit()
    }

    /// A disabled playlist keeps its channels but is neither fetched nor shown.
    pub fn set_playlist_enabled(&self, id: PlaylistId, enabled: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE playlist SET enabled = ?2 WHERE id = ?1",
            params![id, enabled],
        )?;
        Ok(())
    }

    /// Moves the playlist one place earlier (`up`) or later. False when it is already at that
    /// end or not there.
    pub fn move_playlist(&self, id: PlaylistId, up: bool) -> Result<bool> {
        let tx = self.write()?;
        let mut ids: Vec<PlaylistId> = tx
            .prepare_cached("SELECT id FROM playlist ORDER BY position, id")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_>>()?;
        let Some(at) = ids.iter().position(|&p| p == id) else {
            return Ok(false);
        };
        let to = if up { at.checked_sub(1) } else { Some(at + 1) };
        let Some(to) = to.filter(|&to| to < ids.len()) else {
            return Ok(false);
        };
        ids.swap(at, to);
        let mut place = tx.prepare_cached("UPDATE playlist SET position = ?2 WHERE id = ?1")?;
        for (position, id) in ids.iter().enumerate() {
            place.execute(params![id, position as i64])?;
        }
        drop(place);
        tx.commit()?;
        Ok(true)
    }

    /// How many channels each playlist has stored.
    pub fn channel_counts(&self) -> Result<HashMap<PlaylistId, usize>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT playlist_id, COUNT(*) FROM channel GROUP BY playlist_id")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get::<_, i64>(1)? as usize)))?;
        rows.collect()
    }

    // --- custom groups ----------------------------------------------------------------------

    /// In the order they were made.
    pub fn custom_groups(&self) -> Result<Vec<CustomGroup>> {
        let groups = self
            .conn
            .prepare_cached("SELECT id, name FROM custom_group ORDER BY id")?
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get(1)?)))?
            .collect::<Result<Vec<(i64, String)>>>()?;
        let mut members = self.conn.prepare_cached(
            "SELECT key, moved FROM custom_group_channel WHERE group_id = ?1 ORDER BY position",
        )?;
        groups
            .into_iter()
            .map(|(id, name)| {
                let channels = members
                    .query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<_>>()?;
                Ok(CustomGroup { name, channels })
            })
            .collect()
    }

    /// Makes an empty custom group. False when the name is not one a group may have
    /// ([`group_name_ok`]), is taken, or [`MAX_CUSTOM_GROUPS`] exist.
    pub fn create_group(&self, name: &str) -> Result<bool> {
        if !group_name_ok(name) {
            return Ok(false);
        }
        let tx = self.write()?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM custom_group", [], |row| row.get(0))?;
        if count as usize >= MAX_CUSTOM_GROUPS {
            return Ok(false);
        }
        let made = tx.execute(
            "INSERT INTO custom_group (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
            [name],
        )?;
        tx.commit()?;
        Ok(made == 1)
    }

    /// False when `from` is not a custom group or `to` is not a free name it may have.
    pub fn rename_group(&self, from: &str, to: &str) -> Result<bool> {
        if !group_name_ok(to) {
            return Ok(false);
        }
        if from == to {
            return Ok(group_id(&self.conn, from)?.is_some());
        }
        let tx = self.write()?;
        let taken = group_id(&tx, to)?.is_some();
        let renamed = !taken
            && tx.execute(
                "UPDATE custom_group SET name = ?2 WHERE name = ?1",
                [from, to],
            )? == 1;
        if renamed {
            tx.execute(
                "UPDATE group_order SET name = ?2 WHERE kind = 1 AND name = ?1",
                [from, to],
            )?;
        }
        tx.commit()?;
        Ok(renamed)
    }

    /// Deletes the custom group; the channels moved into it are back in their playlist groups.
    pub fn delete_group(&self, name: &str) -> Result<()> {
        let tx = self.write()?;
        tx.execute("DELETE FROM custom_group WHERE name = ?1", [name])?;
        tx.execute(
            "DELETE FROM group_order WHERE kind = 1 AND name = ?1",
            [name],
        )?;
        tx.commit()
    }

    /// Puts `keys` at the end of custom group `to`; a channel already in it stays where it is.
    /// With `from` they are moved rather than copied: out of a custom group they leave it,
    /// keeping whether they had left their playlist groups; out of a playlist group they leave
    /// the playlist groups. False when `to` is not a custom group.
    pub fn add_to_group(
        &self,
        to: &str,
        keys: &[ChannelKey],
        from: Option<&GroupRef>,
    ) -> Result<bool> {
        let tx = self.write()?;
        let Some(target) = group_id(&tx, to)? else {
            return Ok(false);
        };
        let source = match from {
            Some(GroupRef::Custom(name)) if name == to => return Ok(true),
            Some(GroupRef::Custom(name)) => group_id(&tx, name)?,
            _ => None,
        };
        {
            let mut flag = tx.prepare_cached(
                "SELECT moved FROM custom_group_channel WHERE group_id = ?1 AND key = ?2",
            )?;
            let mut leave = tx.prepare_cached(
                "DELETE FROM custom_group_channel WHERE group_id = ?1 AND key = ?2",
            )?;
            let mut put = tx.prepare_cached(
                "INSERT INTO custom_group_channel (group_id, key, position, moved)
                 VALUES (?1, ?2, (SELECT COALESCE(MAX(position), -1) + 1
                 FROM custom_group_channel WHERE group_id = ?1), ?3)
                 ON CONFLICT (group_id, key) DO UPDATE SET moved = MAX(moved, excluded.moved)",
            )?;
            for key in keys {
                let moved = match (from, source) {
                    (Some(GroupRef::Playlist(_)), _) => true,
                    (_, Some(source)) => {
                        let moved: Option<bool> = flag
                            .query_row(params![source, key.0], |row| row.get(0))
                            .optional()?;
                        leave.execute(params![source, key.0])?;
                        moved.unwrap_or(false)
                    }
                    _ => false,
                };
                put.execute(params![target, key.0, moved])?;
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// Takes `keys` out of custom group `name`.
    pub fn remove_from_group(&self, name: &str, keys: &[ChannelKey]) -> Result<()> {
        let tx = self.write()?;
        if let Some(id) = group_id(&tx, name)? {
            let mut leave = tx.prepare_cached(
                "DELETE FROM custom_group_channel WHERE group_id = ?1 AND key = ?2",
            )?;
            for key in keys {
                leave.execute(params![id, key.0])?;
            }
        }
        tx.commit()
    }

    // --- order ------------------------------------------------------------------------------

    /// The groups the user placed, in that order.
    pub fn group_order(&self) -> Result<Vec<GroupRef>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT kind, name FROM group_order ORDER BY position")?;
        let rows = stmt.query_map([], |row| {
            let name = row.get(1)?;
            Ok(if row.get::<_, i64>(0)? == 1 {
                GroupRef::Custom(name)
            } else {
                GroupRef::Playlist(name)
            })
        })?;
        rows.collect()
    }

    /// Replaces the group order with `order`.
    pub fn set_group_order(&self, order: &[GroupRef]) -> Result<()> {
        let tx = self.write()?;
        write_group_order(&tx, order)?;
        tx.commit()
    }

    /// Every playlist group's channels in the user's order.
    pub fn channel_order(&self) -> Result<HashMap<String, Vec<ChannelKey>>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT group_name, key FROM channel_order ORDER BY group_name, position",
        )?;
        let mut out: HashMap<String, Vec<ChannelKey>> = HashMap::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            out.entry(row.get(0)?)
                .or_default()
                .push(ChannelKey(row.get(1)?));
        }
        Ok(out)
    }

    /// Puts the group's channels in the order of `keys`: a playlist group's channels not listed
    /// follow in playlist order, a custom group's follow in the order they had.
    pub fn set_channel_order(&self, group: &GroupRef, keys: &[ChannelKey]) -> Result<()> {
        let tx = self.write()?;
        match group {
            GroupRef::Playlist(name) => {
                write_channel_order(&tx, name, keys.iter().map(|k| k.0))?;
            }
            GroupRef::Custom(name) => {
                if let Some(id) = group_id(&tx, name)? {
                    let members: Vec<i64> = tx
                        .prepare_cached(
                            "SELECT key FROM custom_group_channel WHERE group_id = ?1
                             ORDER BY position",
                        )?
                        .query_map([id], |row| row.get(0))?
                        .collect::<Result<_>>()?;
                    // Sets, so a group of thousands of channels is not compared pair by pair.
                    let member: HashSet<i64> = members.iter().copied().collect();
                    let listed: Vec<i64> = keys
                        .iter()
                        .map(|k| k.0)
                        .filter(|k| member.contains(k))
                        .collect();
                    let placed: HashSet<i64> = listed.iter().copied().collect();
                    let rest = members.into_iter().filter(|k| !placed.contains(k));
                    let mut place = tx.prepare_cached(
                        "UPDATE custom_group_channel SET position = ?3
                         WHERE group_id = ?1 AND key = ?2",
                    )?;
                    for (position, key) in listed.iter().copied().chain(rest).enumerate() {
                        place.execute(params![id, key, position as i64])?;
                    }
                }
            }
        }
        tx.commit()
    }

    // --- backup -----------------------------------------------------------------------------

    /// Everything the user arranged ([`UserLayout`]).
    pub fn user_layout(&self) -> Result<UserLayout> {
        let mut order: Vec<(String, Vec<i64>)> = self
            .channel_order()?
            .into_iter()
            .map(|(name, keys)| (name, keys.into_iter().map(|k| k.0).collect()))
            .collect();
        order.sort();
        Ok(UserLayout {
            custom_groups: self.custom_groups()?,
            group_order: self.group_order()?,
            channel_order: order,
        })
    }

    /// Replaces everything the user arranged with `layout`, at once. False, with nothing
    /// changed, when it is not one this app could have made: a name a group may not have, two
    /// groups of one name, more than [`MAX_CUSTOM_GROUPS`] groups, or more rows than a layout
    /// holds.
    pub fn set_user_layout(&self, layout: &UserLayout) -> Result<bool> {
        let rows = layout
            .custom_groups
            .iter()
            .map(|g| g.channels.len() + 1)
            .chain(layout.channel_order.iter().map(|(_, keys)| keys.len() + 1))
            .fold(layout.group_order.len(), usize::saturating_add);
        let mut names: Vec<&str> = layout
            .custom_groups
            .iter()
            .map(|g| g.name.as_str())
            .collect();
        names.sort_unstable();
        let unique = names.windows(2).all(|w| w[0] != w[1]);
        if rows > MAX_LAYOUT_ROWS
            || names.len() > MAX_CUSTOM_GROUPS
            || !unique
            || !names.iter().all(|n| group_name_ok(n))
        {
            return Ok(false);
        }
        let tx = self.write()?;
        tx.execute_batch(
            "DELETE FROM custom_group; DELETE FROM group_order; DELETE FROM channel_order;",
        )?;
        for group in &layout.custom_groups {
            let id: i64 = tx.query_row(
                "INSERT INTO custom_group (name) VALUES (?1) RETURNING id",
                [&group.name],
                |row| row.get(0),
            )?;
            let mut put = tx.prepare_cached(
                "INSERT OR IGNORE INTO custom_group_channel (group_id, key, position, moved)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (position, (key, moved)) in group.channels.iter().enumerate() {
                put.execute(params![id, key, position as i64, moved])?;
            }
        }
        write_group_order(&tx, &layout.group_order)?;
        for (name, keys) in &layout.channel_order {
            write_channel_order(&tx, name, keys.iter().copied())?;
        }
        tx.commit()?;
        Ok(true)
    }
}

/// The row id of custom group `name`.
fn group_id(conn: &rusqlite::Connection, name: &str) -> Result<Option<i64>> {
    conn.prepare_cached("SELECT id FROM custom_group WHERE name = ?1")?
        .query_row([name], |row| row.get(0))
        .optional()
}

fn write_group_order(tx: &Transaction<'_>, order: &[GroupRef]) -> Result<()> {
    tx.execute("DELETE FROM group_order", [])?;
    let mut put = tx.prepare_cached(
        "INSERT OR IGNORE INTO group_order (kind, name, position) VALUES (?1, ?2, ?3)",
    )?;
    for (position, group) in order.iter().enumerate() {
        put.execute(params![group.kind(), group.name(), position as i64])?;
    }
    Ok(())
}

fn write_channel_order(
    tx: &Transaction<'_>,
    group: &str,
    keys: impl Iterator<Item = i64>,
) -> Result<()> {
    tx.execute("DELETE FROM channel_order WHERE group_name = ?1", [group])?;
    let mut put = tx.prepare_cached(
        "INSERT OR IGNORE INTO channel_order (group_name, key, position) VALUES (?1, ?2, ?3)",
    )?;
    for (position, key) in keys.enumerate() {
        put.execute(params![group, key, position as i64])?;
    }
    Ok(())
}

/// Inside the write that removes playlist `id`: drop what the user kept for its channels
/// (favourites, recents, history, custom group places, manual order). A key another playlist
/// also stores is kept.
pub(super) fn forget_channels_of(tx: &Transaction<'_>, id: PlaylistId) -> Result<()> {
    for table in [
        "favourite",
        "recent",
        "history",
        "custom_group_channel",
        "channel_order",
    ] {
        tx.execute(
            &format!(
                "DELETE FROM {table} WHERE key IN (SELECT key FROM channel WHERE playlist_id = ?1)
                 AND key NOT IN (SELECT key FROM channel WHERE playlist_id <> ?1)"
            ),
            [id],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
