//! SQLite storage: playlists and their channels, the guide, favourites, recent
//! channels, catch-up history and settings, in one file under a data dir the caller passes in.
//!
//! The journal is WAL with `synchronous = NORMAL`: a commit appends to the log without an fsync,
//! which spares the TV's flash, readers never wait for a writer, and a power cut can lose the
//! last commits but not corrupt the file. Use one `Storage` per thread; connections on the same
//! file share it safely.
//!
//! A guide import writes programmes under a new generation in short batched transactions and
//! switches the source to it once the guide was read to its `</tv>`, then deletes the old one in
//! small steps: the UI keeps the previous guide until the switch, its own writes never wait for
//! the network, and a failed, cut or empty download leaves the old guide. Flash is bounded: the
//! file by `MAX_DB_BYTES` (a guide that does not fit fails its import and the old one stays),
//! the programmes of all guides together by `MAX_PROGRAMMES`.
//!
//! Favourites, recents and the history rules follow webOS IPTV Player
//! `src/services/storage-service.ts` (Apache-2.0).

use std::collections::HashMap;
use std::fmt;
use std::io::BufRead;
use std::path::Path;
use std::time::Duration;

use rusqlite::{ffi, params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};

use crate::epg::{GuideChannel, GuideChannelId, GuideSource, Match, PlaylistId, SourceId};
use crate::model::{
    fnv1a, fold, Catchup, CatchupMode, Channel, ChannelKey, EpgChannel, Playlist, Programme,
    Timestamp,
};
use crate::xmltv::{self, Sink};
use crate::{m3u, redact};

mod library;
#[cfg(all(test, unix))]
mod limits_tests;
mod refresh_store;
mod vod_store;
pub use library::{
    group_name_ok, CustomGroup, GroupRef, UserLayout, MAIN_PLAYLIST, MAX_CUSTOM_GROUPS,
    MAX_GROUP_NAME, MAX_PLAYLISTS,
};
pub use refresh_store::RefreshStore;

pub type Result<T> = rusqlite::Result<T>;

/// The database file inside the data dir.
pub const DB_FILE: &str = "iptv.sqlite3";
/// Setting: how many recent channels are kept (default 10).
pub const RECENT_CAP: &str = "recent_cap";
/// Setting: days a history entry is kept after its programme ends (default 7). Changing it
/// re-dates every stored entry at once.
pub const HISTORY_DAYS: &str = "history.keep_days";
/// Setting: seconds into a catch-up programme before it is kept in History (default 10). Below
/// it a position means "start over" and is not kept, unless the programme was watched to the end.
pub const HISTORY_MIN_SECS: &str = "history.min_secs";

const DAY_SECS: i64 = 86_400;
/// Programmes per guide write transaction.
const BATCH: usize = 500;
/// Old programmes deleted per transaction after a guide switch.
const DELETE_BATCH: i64 = 2000;
/// Channel rows one guide import may make: one per channel of the largest playlist the M3U
/// parser accepts (the wanted ones), plus the catalog the XMLTV parser sends.
const MAX_ROWS: usize = m3u::Limits::DEFAULT.max_channels + xmltv::MAX_CHANNELS;
/// The most the database file may grow to (`PRAGMA max_page_count`). Nearly all of it is guide
/// programmes, on flash the TV shares with its installed apps; a write past it fails with
/// SQLITE_FULL, which a guide import answers by keeping the guide it had. An import holds the
/// old and the new generation at once, so the largest guide may take half.
const MAX_DB_BYTES: i64 = 256 << 20;
/// Programmes kept across the current generation of every guide; an import gets what the others
/// leave. Twice this (a guide holding all of it, re-imported) fits in `MAX_DB_BYTES` at up to
/// ~640 bytes a programme, page slack included: measured, a 250-byte description and an icon
/// URL take ~390, a 600-byte one ~840 (a 2 KB one fills a page alone), each plus the folded
/// title search matches (about the title's length again). Guides that run longer
/// descriptions throughout reach the file budget first, and keep their old guide. A 191-channel
/// playlist over the kept two weeks is ~130k.
const MAX_PROGRAMMES: usize = 200_000;
/// How far back from a time the programme on air then is looked for.
const LOOKBACK_SECS: i64 = DAY_SECS;

/// A history query: the columns [`history_entry`] reads, then `$rest`.
macro_rules! history_sql {
    ($rest:literal) => {
        concat!(
            "SELECT key, start, stop, title, icon, position_secs, duration_secs, completed,
             updated_at FROM history ",
            $rest
        )
    };
}

/// A query of `$columns` (by default every one, as [`programme`] reads them) of channel `?1`'s
/// programmes in its source's current generation, narrowed by `$rest`.
macro_rules! programme_sql {
    ($columns:literal, $rest:expr) => {
        concat!(
            "SELECT ",
            $columns,
            " FROM programme WHERE channel_id = ?1 AND generation = (SELECT s.generation
             FROM epg_channel c JOIN guide_source s ON s.id = c.source_id WHERE c.id = ?1) AND ",
            $rest
        )
    };
    ($rest:expr) => {
        programme_sql!("start, stop, title, description, category, icon", $rest)
    };
}

/// The latest-starting programme on air at `?3`, searched from `?2` (`?3` minus
/// [`LOOKBACK_SECS`]).
macro_rules! on_air {
    () => {
        "start >= ?2 AND start <= ?3 AND stop > ?3 ORDER BY start DESC LIMIT 1"
    };
}

/// Programmes overlapping `?3..?4` from the one on air at `?3` on: one short index range, where
/// a fixed lookback would scan a day of programmes per channel. A programme that started before
/// the one on air and overlaps it (a guide error) is left out, as [`Storage::now_next`] leaves it.
macro_rules! window_sql {
    ($columns:literal) => {
        programme_sql!(
            $columns,
            concat!(
                "start >= COALESCE((",
                programme_sql!("start", on_air!()),
                "), ?3) AND start < ?4 AND stop > ?3 ORDER BY start"
            )
        )
    };
}

const WINDOW_SQL: &str = window_sql!("start, stop, title, description, category, icon");
/// `WINDOW_SQL` with only what the grid paints, as [`grid_programme`] reads it.
const GRID_SQL: &str = window_sql!("start, stop, title");
const CURRENT_SQL: &str = programme_sql!(on_air!());
/// The first programme starting after `?2`.
const NEXT_SQL: &str = programme_sql!("start > ?2 ORDER BY start LIMIT 1");
/// The programme starting at `?2`.
const AT_SQL: &str = programme_sql!("start = ?2");
/// Current-generation programmes overlapping `?1..?2` whose folded title contains `?3`: on air at
/// `?4` first, then past ones newest first, then upcoming ones soonest first. A row from before
/// the `folded` column matches on its ASCII-lowercased title until its guide is imported again.
const SEARCH_SQL: &str = "SELECT p.channel_id, p.start, p.stop, p.title FROM programme p
     JOIN epg_channel c ON c.id = p.channel_id JOIN guide_source s ON s.id = c.source_id
     WHERE p.generation = s.generation AND p.start < ?2 AND p.stop > ?1
     AND instr(COALESCE(p.folded, lower(p.title)), ?3) > 0
     ORDER BY CASE WHEN p.stop <= ?4 THEN 1 WHEN p.start > ?4 THEN 2 ELSE 0 END,
     CASE WHEN p.stop <= ?4 THEN -p.start ELSE p.start END";

/// Each entry upgrades the schema by one version (`PRAGMA user_version`). Append, never edit
/// (the first entry is frozen from the first install on a TV; until then it is still edited).
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE setting (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE playlist (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    position INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER,
    -- Fingerprint of the stored channels and guide URLs: the same download is not written again.
    content_hash INTEGER
);

CREATE TABLE channel (
    playlist_id INTEGER NOT NULL REFERENCES playlist (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    key INTEGER NOT NULL,
    name TEXT NOT NULL,
    number INTEGER,
    logo TEXT,
    groups TEXT NOT NULL,
    url TEXT NOT NULL,
    tvg_id TEXT,
    tvg_name TEXT,
    tvg_shift_minutes INTEGER NOT NULL,
    catchup_mode TEXT,
    catchup_days INTEGER NOT NULL,
    catchup_source TEXT,
    radio INTEGER NOT NULL,
    http_headers TEXT NOT NULL,
    extra TEXT NOT NULL,
    PRIMARY KEY (playlist_id, position)
) WITHOUT ROWID;

CREATE TABLE guide_source (
    id INTEGER PRIMARY KEY,
    url TEXT NOT NULL UNIQUE,
    manual INTEGER NOT NULL DEFAULT 0,
    offset_minutes INTEGER NOT NULL DEFAULT 0,
    generation INTEGER NOT NULL DEFAULT 0,
    name TEXT,
    tz_offset_minutes INTEGER,
    updated_at INTEGER
);

CREATE TABLE guide_source_playlist (
    source_id INTEGER NOT NULL REFERENCES guide_source (id) ON DELETE CASCADE,
    playlist_id INTEGER NOT NULL REFERENCES playlist (id) ON DELETE CASCADE,
    -- Order in the playlist header.
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (source_id, playlist_id)
) WITHOUT ROWID;
CREATE INDEX guide_source_playlist_by_playlist ON guide_source_playlist (playlist_id);

CREATE TABLE epg_channel (
    id INTEGER PRIMARY KEY,
    source_id INTEGER NOT NULL REFERENCES guide_source (id) ON DELETE CASCADE,
    xmltv_id TEXT NOT NULL,
    names TEXT NOT NULL,
    icon TEXT,
    generation INTEGER NOT NULL,
    UNIQUE (source_id, xmltv_id)
);

CREATE TABLE programme (
    channel_id INTEGER NOT NULL REFERENCES epg_channel (id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,
    start INTEGER NOT NULL,
    stop INTEGER NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    category TEXT,
    icon TEXT
);
CREATE UNIQUE INDEX programme_slot ON programme (channel_id, generation, start);

-- HTTP validators per fetched URL; `filter` fingerprints the channel filter a guide was
-- stored with, so a changed filter forces a full download.
CREATE TABLE http_validator (
    url TEXT PRIMARY KEY,
    etag TEXT,
    last_modified TEXT,
    filter INTEGER
) WITHOUT ROWID;

CREATE TABLE favourite (key INTEGER NOT NULL UNIQUE);

CREATE TABLE recent (
    key INTEGER NOT NULL UNIQUE,
    watched_at INTEGER NOT NULL
);

CREATE TABLE history (
    key INTEGER NOT NULL,
    start INTEGER NOT NULL,
    stop INTEGER NOT NULL,
    title TEXT NOT NULL,
    icon TEXT,
    position_secs INTEGER NOT NULL,
    duration_secs INTEGER NOT NULL,
    completed INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (key, start)
) WITHOUT ROWID;
"#,
    r#"
-- The title in `model::fold` form, what search matches. Rows an older build wrote have none
-- until their guide is imported again: forgetting the validators makes the next fetch of each
-- guide a full download.
ALTER TABLE programme ADD COLUMN folded TEXT;
DELETE FROM http_validator;
"#,
    r#"
-- Kodi's catchup-correction, in seconds. Rows an older build wrote kept the attribute in `extra`
-- and read 0 here until their playlist is stored again: forgetting the playlists' validators and
-- content hashes makes the next refresh of each a full download that rewrites its channels.
ALTER TABLE channel ADD COLUMN catchup_correction_secs INTEGER NOT NULL DEFAULT 0;
DELETE FROM http_validator WHERE url IN (SELECT url FROM playlist);
UPDATE playlist SET content_hash = NULL;
"#,
    r#"
-- Several playlists and the user's own groups and order. Playlist 1
-- is the one the Settings URL (`playlist.url`) names; an install that set it but never fetched
-- it gets its row here, so it is playlist 1 like every install that did.
INSERT INTO playlist (id, name, url, position)
SELECT 1, 'Playlist', value, COALESCE((SELECT MIN(position) FROM playlist), 1) - 1 FROM setting
WHERE key = 'playlist.url' AND value <> '' AND NOT EXISTS (SELECT 1 FROM playlist WHERE id = 1);

CREATE TABLE custom_group (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE custom_group_channel (
    group_id INTEGER NOT NULL REFERENCES custom_group (id) ON DELETE CASCADE,
    key INTEGER NOT NULL,
    position INTEGER NOT NULL,
    -- Moved here rather than copied: the channel leaves its playlist groups.
    moved INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (group_id, key)
) WITHOUT ROWID;

-- The user's order of the playlist (kind 0) and custom (kind 1) groups.
CREATE TABLE group_order (
    kind INTEGER NOT NULL,
    name TEXT NOT NULL,
    position INTEGER NOT NULL,
    PRIMARY KEY (kind, name)
) WITHOUT ROWID;

-- The user's order of the channels of a playlist group.
CREATE TABLE channel_order (
    group_name TEXT NOT NULL,
    key INTEGER NOT NULL,
    position INTEGER NOT NULL,
    PRIMARY KEY (group_name, key)
) WITHOUT ROWID;
"#,
    r#"
-- VOD: the movies and episodes a playlist lists, kept apart from its channels, and how
-- far into each the viewer got. Rows an older build wrote kept them as channels: forgetting the
-- playlists' validators and content hashes makes the next refresh of each a full download that
-- moves them here.
CREATE TABLE vod_item (
    playlist_id INTEGER NOT NULL REFERENCES playlist (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    key INTEGER NOT NULL,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    series TEXT,
    season INTEGER,
    episode INTEGER,
    year INTEGER,
    poster TEXT,
    category TEXT,
    url TEXT NOT NULL,
    http_headers TEXT NOT NULL,
    PRIMARY KEY (playlist_id, position)
) WITHOUT ROWID;

-- Fingerprint of a playlist's stored VOD items: the same download is not written again.
CREATE TABLE vod_source (
    playlist_id INTEGER PRIMARY KEY REFERENCES playlist (id) ON DELETE CASCADE,
    content_hash INTEGER NOT NULL
);

CREATE TABLE vod_progress (
    key INTEGER PRIMARY KEY,
    position_secs INTEGER NOT NULL,
    duration_secs INTEGER NOT NULL,
    watched INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
DELETE FROM http_validator WHERE url IN (SELECT url FROM playlist);
UPDATE playlist SET content_hash = NULL;
"#,
];

/// `Debug` masks the URL.
#[derive(Clone, PartialEq, Eq)]
pub struct PlaylistInfo {
    pub id: PlaylistId,
    pub name: String,
    /// Carries the account token: never log it.
    pub url: String,
    pub enabled: bool,
    pub updated_at: Option<Timestamp>,
}

/// `Debug` masks the URL.
#[derive(Clone, PartialEq, Eq)]
pub struct GuideSourceInfo {
    pub id: SourceId,
    /// May carry a token: never log it.
    pub url: String,
    pub manual: bool,
    pub playlists: Vec<PlaylistId>,
    pub offset_minutes: i32,
    /// The guide's own name, from its last import.
    pub name: Option<String>,
    pub updated_at: Option<Timestamp>,
}

/// A watched catch-up programme and where playback stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub key: ChannelKey,
    /// Programme start and stop in guide time (before the channel's shift, see
    /// [`Match::shift_secs`]), so a changed shift keeps the entry; the start identifies it within
    /// the channel.
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
    pub icon: Option<String>,
    pub position_secs: u32,
    pub duration_secs: u32,
    pub completed: bool,
    pub updated_at: Timestamp,
}

/// A programme [`Storage::search`] found, in guide time (before any channel's shift).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TitleHit {
    pub guide_channel: GuideChannelId,
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Imported {
    pub summary: xmltv::Summary,
    /// False when the guide ended before `</tv>`, or kept no programmes while one is stored:
    /// the previous guide was left in place.
    pub replaced: bool,
    /// Programmes dropped because their channel already had one at that start.
    pub duplicates: usize,
}

impl fmt::Debug for PlaylistInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlaylistInfo")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("url", &redact::url(&self.url))
            .field("enabled", &self.enabled)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl fmt::Debug for GuideSourceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuideSourceInfo")
            .field("id", &self.id)
            .field("url", &redact::url(&self.url))
            .field("manual", &self.manual)
            .field("playlists", &self.playlists)
            .field("offset_minutes", &self.offset_minutes)
            .field("name", &self.name)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl GuideSourceInfo {
    /// The source as the matcher takes it, with its channel catalog.
    pub(crate) fn into_source(self, channels: Vec<GuideChannel>) -> GuideSource {
        GuideSource {
            id: self.id,
            manual: self.manual,
            playlists: self.playlists,
            offset_minutes: self.offset_minutes,
            channels,
        }
    }
}

pub struct Storage {
    conn: Connection,
    /// `MAX_PROGRAMMES`; a test sets a smaller one.
    max_programmes: usize,
}

impl Storage {
    /// Opens or creates [`DB_FILE`] in `dir` and migrates it. The file holds tokenised URLs, so
    /// `dir` must be a directory this uid owns that nobody else can write, and the database and
    /// its `-wal` and `-shm` files regular files this uid owns, 0600 (a looser mode is tightened;
    /// SQLite gives the files it creates the database's mode). Anything else is refused before
    /// SQLite opens a file: a link planted at a name, or a file someone else can read, would hand
    /// the tokens over.
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join(DB_FILE);
        #[cfg(unix)]
        private_files(dir, &path, tv_base::paths::euid()).map_err(|e| {
            rusqlite::Error::SqliteFailure(
                ffi::Error::new(ffi::SQLITE_CANTOPEN),
                Some(format!("the database is not private: {e}")),
            )
        })?;
        Self::init(Connection::open(&path)?, "wal")
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, "memory")
    }

    fn init(mut conn: Connection, journal: &str) -> Result<Self> {
        conn.busy_handler(Some(wait_for_lock))?;
        conn.set_prepared_statement_cache_capacity(64);
        let mode = wal(&conn)?;
        if !mode.eq_ignore_ascii_case(journal) {
            return Err(rusqlite::Error::SqliteFailure(
                ffi::Error::new(ffi::SQLITE_CANTOPEN),
                Some(format!("journal mode is {mode}, not {journal}")),
            ));
        }
        let page_size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
        conn.execute_batch(&format!(
            "PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA journal_size_limit = 4194304;
             PRAGMA max_page_count = {};",
            MAX_DB_BYTES / page_size.max(512)
        ))?;
        migrate(&mut conn)?;
        Ok(Storage {
            conn,
            max_programmes: MAX_PROGRAMMES,
        })
    }

    fn write(&self) -> Result<Transaction<'_>> {
        Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
    }

    // --- settings ---------------------------------------------------------------------------

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .prepare_cached("SELECT value FROM setting WHERE key = ?1")?
            .query_row([key], |row| row.get(0))
            .optional()
    }

    /// A new [`HISTORY_DAYS`] re-dates the stored history in the same transaction.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let tx = self.write()?;
        tx.prepare_cached(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )?
        .execute([key, value])?;
        self.follow_setting(key)?;
        tx.commit()
    }

    pub fn remove_setting(&self, key: &str) -> Result<()> {
        let tx = self.write()?;
        tx.prepare_cached("DELETE FROM setting WHERE key = ?1")?
            .execute([key])?;
        self.follow_setting(key)?;
        tx.commit()
    }

    /// Inside the write that changed `key`: keep what the stored rows derive from it true.
    fn follow_setting(&self, key: &str) -> Result<()> {
        if key == HISTORY_DAYS {
            let keep = i64::from(self.history_days()?) * DAY_SECS;
            self.conn
                .prepare_cached("UPDATE history SET expires_at = stop + ?1")?
                .execute([keep])?;
        }
        Ok(())
    }

    /// [`RECENT_CAP`], 1 to 100.
    pub fn recent_cap(&self) -> Result<u32> {
        self.number_setting(RECENT_CAP, 10, 100)
    }

    /// [`HISTORY_DAYS`], 1 to 365.
    pub fn history_days(&self) -> Result<u32> {
        self.number_setting(HISTORY_DAYS, 7, 365)
    }

    /// [`HISTORY_MIN_SECS`], 1 to 3600.
    pub fn history_min_secs(&self) -> Result<u32> {
        self.number_setting(HISTORY_MIN_SECS, 10, 3600)
    }

    fn number_setting(&self, key: &str, default: u32, max: u32) -> Result<u32> {
        let value = self
            .setting(key)?
            .and_then(|v| v.trim().parse::<u32>().ok());
        Ok(value.map_or(default, |v| v.clamp(1, max)))
    }

    // --- favourites -------------------------------------------------------------------------

    /// Adds or removes the channel; returns whether it is now a favourite.
    pub fn toggle_favourite(&self, key: ChannelKey) -> Result<bool> {
        let tx = self.write()?;
        let removed = tx
            .prepare_cached("DELETE FROM favourite WHERE key = ?1")?
            .execute([key.0])?;
        if removed == 0 {
            tx.prepare_cached("INSERT INTO favourite (key) VALUES (?1)")?
                .execute([key.0])?;
        }
        tx.commit()?;
        Ok(removed == 0)
    }

    pub fn is_favourite(&self, key: ChannelKey) -> Result<bool> {
        self.conn
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM favourite WHERE key = ?1)")?
            .query_row([key.0], |row| row.get(0))
    }

    /// In the order they were added.
    pub fn favourites(&self) -> Result<Vec<ChannelKey>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT key FROM favourite ORDER BY rowid")?;
        let keys = stmt.query_map([], |row| Ok(ChannelKey(row.get(0)?)))?;
        keys.collect()
    }

    /// Replaces every favourite with `keys`, in their order, in one transaction (a restored
    /// backup). A key given twice is kept once, where it first appears.
    pub fn set_favourites(&self, keys: &[ChannelKey]) -> Result<()> {
        let tx = self.write()?;
        tx.prepare_cached("DELETE FROM favourite")?.execute([])?;
        let mut insert = tx.prepare_cached("INSERT OR IGNORE INTO favourite (key) VALUES (?1)")?;
        for key in keys {
            insert.execute([key.0])?;
        }
        drop(insert);
        tx.commit()
    }

    // --- recent channels --------------------------------------------------------------------

    /// Moves the channel to the front and drops the oldest past [`RECENT_CAP`]. The player calls
    /// it once a zap has lasted a few seconds.
    pub fn touch_recent(&self, key: ChannelKey, now: Timestamp) -> Result<()> {
        let cap = self.recent_cap()?;
        let tx = self.write()?;
        // A fresh rowid keeps touch order even within one second.
        tx.prepare_cached("DELETE FROM recent WHERE key = ?1")?
            .execute([key.0])?;
        tx.prepare_cached("INSERT INTO recent (key, watched_at) VALUES (?1, ?2)")?
            .execute(params![key.0, now])?;
        tx.prepare_cached(
            "DELETE FROM recent WHERE rowid NOT IN
             (SELECT rowid FROM recent ORDER BY rowid DESC LIMIT ?1)",
        )?
        .execute([cap])?;
        tx.commit()
    }

    /// Newest first, with the time each was last watched.
    pub fn recent(&self) -> Result<Vec<(ChannelKey, Timestamp)>> {
        let cap = self.recent_cap()?;
        let mut stmt = self
            .conn
            .prepare_cached("SELECT key, watched_at FROM recent ORDER BY rowid DESC LIMIT ?1")?;
        let rows = stmt.query_map([cap], |row| Ok((ChannelKey(row.get(0)?), row.get(1)?)))?;
        rows.collect()
    }

    pub fn clear_recent(&self) -> Result<()> {
        self.conn.execute("DELETE FROM recent", [])?;
        Ok(())
    }

    // --- catch-up history -------------------------------------------------------------------

    /// Stores the entry until its programme ended [`HISTORY_DAYS`] ago. An entry already past
    /// that, or not completed and under [`HISTORY_MIN_SECS`] in (start over), is removed instead.
    /// Expired entries are pruned on every save.
    pub fn save_progress(&self, entry: &HistoryEntry, now: Timestamp) -> Result<()> {
        let expires_at = entry.stop + i64::from(self.history_days()?) * DAY_SECS;
        let resumable = entry.completed || entry.position_secs >= self.history_min_secs()?;
        let tx = self.write()?;
        tx.prepare_cached("DELETE FROM history WHERE expires_at <= ?1")?
            .execute([now])?;
        if resumable && expires_at > now {
            tx.prepare_cached(
                "INSERT OR REPLACE INTO history (key, start, stop, title, icon, position_secs,
                 duration_secs, completed, updated_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?
            .execute(params![
                entry.key.0,
                entry.start,
                entry.stop,
                entry.title,
                entry.icon,
                entry.position_secs,
                entry.duration_secs,
                entry.completed,
                entry.updated_at,
                expires_at,
            ])?;
        } else {
            tx.prepare_cached("DELETE FROM history WHERE key = ?1 AND start = ?2")?
                .execute(params![entry.key.0, entry.start])?;
        }
        tx.commit()
    }

    pub fn progress(
        &self,
        key: ChannelKey,
        start: Timestamp,
        now: Timestamp,
    ) -> Result<Option<HistoryEntry>> {
        self.conn
            .prepare_cached(history_sql!(
                "WHERE key = ?1 AND start = ?2 AND expires_at > ?3"
            ))?
            .query_row(params![key.0, start, now], history_entry)
            .optional()
    }

    /// One channel's live entries by programme start, to mark watched programmes in the guide.
    pub fn channel_history(&self, key: ChannelKey, now: Timestamp) -> Result<Vec<HistoryEntry>> {
        let mut stmt = self.conn.prepare_cached(history_sql!(
            "WHERE key = ?1 AND expires_at > ?2 ORDER BY start"
        ))?;
        let rows = stmt.query_map(params![key.0, now], history_entry)?;
        rows.collect()
    }

    /// Every live entry, most recently watched first.
    pub fn history(&self, now: Timestamp) -> Result<Vec<HistoryEntry>> {
        let mut stmt = self.conn.prepare_cached(history_sql!(
            "WHERE expires_at > ?1 ORDER BY updated_at DESC, start DESC"
        ))?;
        let rows = stmt.query_map([now], history_entry)?;
        rows.collect()
    }

    pub fn remove_progress(&self, key: ChannelKey, start: Timestamp) -> Result<()> {
        self.conn
            .prepare_cached("DELETE FROM history WHERE key = ?1 AND start = ?2")?
            .execute(params![key.0, start])?;
        Ok(())
    }

    pub fn clear_history(&self) -> Result<()> {
        self.conn.execute("DELETE FROM history", [])?;
        Ok(())
    }

    // --- playlists --------------------------------------------------------------------------

    /// Adds a playlist last in order. Into an empty table it is [`MAIN_PLAYLIST`], the one the
    /// Settings URL names: a caller adding another one makes sure that row exists first.
    pub fn add_playlist(&self, name: &str, url: &str) -> Result<PlaylistId> {
        self.conn
            .prepare_cached(
                "INSERT INTO playlist (name, url, position)
                 VALUES (?1, ?2, (SELECT COALESCE(MAX(position), -1) + 1 FROM playlist))
                 RETURNING id",
            )?
            .query_row([name, url], |row| row.get(0))
    }

    pub fn playlists(&self) -> Result<Vec<PlaylistInfo>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, name, url, enabled, updated_at FROM playlist ORDER BY position, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PlaylistInfo {
                id: row.get(0)?,
                name: row.get(1)?,
                url: row.get(2)?,
                enabled: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// Removes the playlist, its channels, what the user kept for them (favourites, recents,
    /// history, places in custom groups and manual order) and the guides only it named.
    pub fn remove_playlist(&self, id: PlaylistId) -> Result<()> {
        let tx = self.write()?;
        library::forget_channels_of(&tx, id)?;
        tx.execute("DELETE FROM playlist WHERE id = ?1", [id])?;
        delete_orphan_guides(&tx)?;
        tx.commit()
    }

    /// Brings the stored sources in line with the user's settings: playlist `id` is fetched from
    /// `url`, and the manual guides are the ones in `guides` (added when first fetched). Any
    /// other manual guide is detached at once: it serves nothing and its URL is gone, and the
    /// refresher deletes its programmes later in small steps (`Storage::delete_orphans`), so
    /// the settings write that calls this never waits on a whole guide. Validators of URLs
    /// nothing is fetched from any more go too. Each of these URLs may carry a token that must
    /// not outlive its setting.
    pub fn set_sources(&self, id: PlaylistId, url: &str, guides: &[String]) -> Result<()> {
        let tx = self.write()?;
        tx.execute(
            "UPDATE playlist SET url = ?2 WHERE id = ?1 AND url <> ?2",
            params![id, url],
        )?;
        let stale: Vec<SourceId> = tx
            .prepare_cached("SELECT id, url FROM guide_source WHERE manual = 1")?
            .query_map([], |row| Ok((row.get(0)?, row.get::<_, String>(1)?)))?
            .filter_map(|row| match row {
                Ok((id, url)) => (!guides.contains(&url)).then_some(Ok(id)),
                Err(e) => Some(Err(e)),
            })
            .collect::<Result<_>>()?;
        for source in &stale {
            tx.execute(
                "DELETE FROM guide_source_playlist WHERE source_id = ?1",
                [source],
            )?;
            // Never an http URL, so never fetched, and unique as the column must be.
            tx.execute(
                "UPDATE guide_source SET manual = 0, url = 'removed:' || id WHERE id = ?1",
                [source],
            )?;
        }
        if !stale.is_empty() {
            // A header guide the user had made manual lost its playlist links: the next download
            // of each playlist links it again.
            tx.execute("UPDATE playlist SET content_hash = NULL", [])?;
        }
        prune_validators(&tx)?;
        tx.commit()
    }

    /// Deletes the guides that serve nothing (no playlist names them, or [`Storage::set_sources`]
    /// detached them), their programmes first in small steps, as an import's switch does: one
    /// cascade would hold the write lock for the whole guide. The refresher calls it once per
    /// refresh, on its own thread.
    fn delete_orphans(&self) -> Result<()> {
        let orphans: Vec<(SourceId, i64)> = self
            .conn
            .prepare_cached(
                "SELECT id, generation FROM guide_source WHERE manual = 0
                 AND id NOT IN (SELECT source_id FROM guide_source_playlist)",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_>>()?;
        if orphans.is_empty() {
            return Ok(());
        }
        for &(source, generation) in &orphans {
            self.discard_pending(source, generation)?;
            self.delete_generation(source, generation)?;
        }
        let tx = self.write()?;
        delete_orphan_guides(&tx)?;
        tx.commit()
    }

    /// Replaces the playlist's channels and links the guides its header names. False when both
    /// are what is stored already (the same download again): nothing is rewritten then.
    pub fn save_playlist(
        &self,
        id: PlaylistId,
        playlist: &Playlist,
        now: Timestamp,
    ) -> Result<bool> {
        let mut buf = Vec::new();
        let hash = content_hash(playlist, &mut buf)?;
        let tx = self.write()?;
        let stored: Option<i64> = tx
            .query_row(
                "SELECT content_hash FROM playlist WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let changed = stored != Some(hash);
        if changed {
            replace_channels(&tx, id, playlist, &mut buf)?;
            link_guides(&tx, id, &playlist.epg_urls)?;
        }
        tx.execute(
            "UPDATE playlist SET updated_at = ?2, content_hash = ?3 WHERE id = ?1",
            params![id, now, hash],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    /// In playlist order.
    pub fn channels(&self, playlist: PlaylistId) -> Result<Vec<Channel>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT key, name, number, logo, groups, url, tvg_id, tvg_name, tvg_shift_minutes,
             catchup_mode, catchup_days, catchup_source, radio, http_headers, extra,
             catchup_correction_secs
             FROM channel WHERE playlist_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map([playlist], |row| {
            let mode = row.get_ref(9)?.as_str_or_null()?;
            Ok(Channel {
                key: ChannelKey(row.get(0)?),
                name: row.get(1)?,
                number: row.get(2)?,
                logo: row.get(3)?,
                groups: from_json(row, 4)?,
                url: row.get(5)?,
                tvg_id: row.get(6)?,
                tvg_name: row.get(7)?,
                tvg_shift_minutes: row.get(8)?,
                catchup: Catchup {
                    mode: mode.and_then(CatchupMode::parse),
                    days: row.get(10)?,
                    source: row.get(11)?,
                    correction_secs: row.get(15)?,
                },
                radio: row.get(12)?,
                http_headers: from_json(row, 13)?,
                extra: from_json(row, 14)?,
            })
        })?;
        rows.collect()
    }

    // --- guide sources ----------------------------------------------------------------------

    /// Adds a guide by hand, or makes an existing one manual, serving `playlists` (none: all).
    pub fn add_manual_guide(&self, url: &str, playlists: &[PlaylistId]) -> Result<SourceId> {
        let tx = self.write()?;
        let id: SourceId = tx.query_row(
            "INSERT INTO guide_source (url, manual) VALUES (?1, 1)
             ON CONFLICT (url) DO UPDATE SET manual = 1 RETURNING id",
            [url],
            |row| row.get(0),
        )?;
        tx.execute(
            "DELETE FROM guide_source_playlist WHERE source_id = ?1",
            [id],
        )?;
        for playlist in playlists {
            tx.execute(
                "INSERT OR IGNORE INTO guide_source_playlist (source_id, playlist_id) VALUES (?1, ?2)",
                [id, *playlist],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// The user's correction for a guide whose times are off.
    pub fn set_guide_offset(&self, id: SourceId, minutes: i32) -> Result<()> {
        self.conn.execute(
            "UPDATE guide_source SET offset_minutes = ?2 WHERE id = ?1",
            params![id, minutes],
        )?;
        Ok(())
    }

    /// Manual sources first.
    pub fn guide_sources(&self) -> Result<Vec<GuideSourceInfo>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, url, manual, offset_minutes, name, updated_at FROM guide_source
             ORDER BY manual DESC, id",
        )?;
        let mut sources = stmt
            .query_map([], |row| {
                Ok(GuideSourceInfo {
                    id: row.get(0)?,
                    url: row.get(1)?,
                    manual: row.get(2)?,
                    playlists: Vec::new(),
                    offset_minutes: row.get(3)?,
                    name: row.get(4)?,
                    updated_at: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        let mut links = self.conn.prepare_cached(
            "SELECT playlist_id FROM guide_source_playlist WHERE source_id = ?1 ORDER BY playlist_id",
        )?;
        for source in &mut sources {
            source.playlists = links
                .query_map([source.id], |row| row.get(0))?
                .collect::<Result<_>>()?;
        }
        Ok(sources)
    }

    /// Every source with its channel catalog, as the matcher takes them.
    pub fn epg_sources(&self) -> Result<Vec<GuideSource>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT c.id, c.xmltv_id, c.names, c.icon, EXISTS (SELECT 1 FROM programme p
             WHERE p.channel_id = c.id AND p.generation = s.generation)
             FROM epg_channel c JOIN guide_source s ON s.id = c.source_id
             WHERE c.source_id = ?1 ORDER BY c.id",
        )?;
        let mut sources = Vec::new();
        for info in self.guide_sources()? {
            let channels = stmt
                .query_map([info.id], |row| {
                    Ok(GuideChannel {
                        row: row.get(0)?,
                        epg: EpgChannel {
                            id: row.get(1)?,
                            names: from_json(row, 2)?,
                            icon: row.get(3)?,
                        },
                        has_programmes: row.get(4)?,
                    })
                })?
                .collect::<Result<_>>()?;
            sources.push(info.into_source(channels));
        }
        Ok(sources)
    }

    // --- guide import -----------------------------------------------------------------------

    /// Streams one XMLTV download into the source. Channel row ids stay stable across imports,
    /// so matches stay valid. Programmes go in under a new generation in short transactions and
    /// the source switches to it only once the guide was read to its `</tv>`: until then the
    /// UI reads the previous guide, and a failed, cut or empty download leaves it in place.
    pub fn import_guide<R: BufRead>(
        &self,
        source: SourceId,
        input: R,
        options: &xmltv::Options,
    ) -> std::result::Result<Imported, xmltv::Error<rusqlite::Error>> {
        let sql = xmltv::Error::Sink;
        let current: i64 = self
            .conn
            .query_row(
                "SELECT generation FROM guide_source WHERE id = ?1",
                [source],
                |row| row.get(0),
            )
            .map_err(sql)?;
        self.discard_pending(source, current).map_err(sql)?;
        let next = current + 1;
        let room = self
            .max_programmes
            .saturating_sub(self.programmes_besides(source).map_err(sql)?);
        let mut writer = GuideWriter {
            conn: &self.conn,
            source,
            generation: next,
            rows: HashMap::new(),
            duplicates: 0,
            room,
            over_budget: 0,
            channels: Vec::new(),
            programmes: Vec::new(),
        };
        let parsed = xmltv::parse(input, options, &mut writer).and_then(|summary| {
            writer.flush().map_err(sql)?;
            Ok(summary)
        });
        let (duplicates, over_budget) = (writer.duplicates, writer.over_budget);
        let mut summary = match parsed {
            Ok(summary) => summary,
            Err(e) => {
                // Best effort: the next import discards these rows anyway.
                let _ = self.discard_pending(source, current);
                return Err(e);
            }
        };
        // Programmes past the room the other guides left were not stored.
        summary.stats.programmes_kept -= over_budget;
        summary.stats.dropped_budget += over_budget;
        let tx = self.write().map_err(sql)?;
        let had_programmes: bool = tx
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM epg_channel c JOIN programme p
                 ON p.channel_id = c.id AND p.generation = ?2 WHERE c.source_id = ?1)",
                [source, current],
                |row| row.get(0),
            )
            .map_err(sql)?;
        // An empty guide is usually a transient upstream answer: keep the one we have.
        let replaced = summary.complete && (summary.stats.programmes_kept > 0 || !had_programmes);
        if replaced {
            tx.execute(
                "UPDATE guide_source SET generation = ?2, name = ?3, tz_offset_minutes = ?4,
                 updated_at = ?5 WHERE id = ?1",
                params![
                    source,
                    next,
                    summary.source_name,
                    summary.tz_offset_minutes,
                    options.now
                ],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
        // The switch is done; what is left is garbage, deleted in small steps.
        if replaced {
            self.delete_generation(source, current).map_err(sql)?;
            self.conn
                .execute(
                    "DELETE FROM epg_channel WHERE source_id = ?1 AND generation <> ?2",
                    [source, next],
                )
                .map_err(sql)?;
        } else {
            self.discard_pending(source, current).map_err(sql)?;
        }
        Ok(Imported {
            summary,
            replaced,
            duplicates,
        })
    }

    /// Programmes in the current generation of every guide but `source`.
    fn programmes_besides(&self, source: SourceId) -> Result<usize> {
        self.conn
            .prepare_cached(
                "SELECT COUNT(*) FROM guide_source s JOIN epg_channel c ON c.source_id = s.id
                 JOIN programme p ON p.channel_id = c.id AND p.generation = s.generation
                 WHERE s.id <> ?1",
            )?
            .query_row([source], |row| row.get::<_, i64>(0))
            .map(|n| n as usize)
    }

    /// Drops what an unfinished import or switch left: programmes of the generations next to
    /// `current`, and the claim an import put on channels.
    fn discard_pending(&self, source: SourceId, current: i64) -> Result<()> {
        self.delete_generation(source, current + 1)?;
        self.delete_generation(source, current - 1)?;
        self.conn.execute(
            "UPDATE epg_channel SET generation = ?2 WHERE source_id = ?1 AND generation <> ?2",
            [source, current],
        )?;
        Ok(())
    }

    /// Deletes one generation of the source's programmes, [`DELETE_BATCH`] rows per
    /// transaction, so the write lock and the WAL stay small. Each channel's rows are one index
    /// range, deleted channel by channel.
    fn delete_generation(&self, source: SourceId, generation: i64) -> Result<()> {
        let channels: Vec<GuideChannelId> = self
            .conn
            .prepare_cached(
                "SELECT c.id FROM epg_channel c WHERE c.source_id = ?1 AND EXISTS
                 (SELECT 1 FROM programme p WHERE p.channel_id = c.id AND p.generation = ?2)",
            )?
            .query_map([source, generation], |row| row.get(0))?
            .collect::<Result<_>>()?;
        let mut channels = channels.into_iter().peekable();
        while channels.peek().is_some() {
            let tx = self.write()?;
            let mut room = DELETE_BATCH;
            {
                let mut delete = tx.prepare_cached(
                    "DELETE FROM programme WHERE rowid IN (SELECT rowid FROM programme
                     WHERE channel_id = ?1 AND generation = ?2 LIMIT ?3)",
                )?;
                while let Some(&channel) = channels.peek() {
                    let deleted = delete.execute(params![channel, generation, room])? as i64;
                    if deleted < room {
                        channels.next();
                    }
                    room -= deleted;
                    if room == 0 {
                        break;
                    }
                }
            }
            tx.commit()?;
            if channels.peek().is_some() {
                // A gap for a user write waiting on the lock.
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    // --- guide queries ----------------------------------------------------------------------

    /// Programmes of the matched channel that overlap `from..to`, in the channel's time (shift
    /// applied), by start, from the one on air at `from` on (`WINDOW_SQL`).
    pub fn programmes(
        &self,
        guide: &Match,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<Programme>> {
        self.window(WINDOW_SQL, programme, guide, from, to)
    }

    /// [`Storage::programmes`] with only start, stop and title, what the guide grid paints;
    /// [`Storage::programme_at`] reads the rest of the focused one.
    pub fn grid_programmes(
        &self,
        guide: &Match,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<Programme>> {
        self.window(GRID_SQL, grid_programme, guide, from, to)
    }

    /// [`Storage::grid_programmes`] for many channels in one read snapshot (the visible guide
    /// rows).
    pub fn guide_window(
        &self,
        guides: &[&Match],
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<Vec<Programme>>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let rows = guides
            .iter()
            .map(|guide| self.grid_programmes(guide, from, to))
            .collect();
        tx.finish()?;
        rows
    }

    /// The matched channel's programme starting at `start` (channel time), every field read.
    pub fn programme_at(&self, guide: &Match, start: Timestamp) -> Result<Option<Programme>> {
        let shift = guide.shift_secs();
        self.conn
            .prepare_cached(AT_SQL)?
            .query_row(params![guide.guide_channel, start - shift], |row| {
                programme(row, shift)
            })
            .optional()
    }

    fn window(
        &self,
        sql: &str,
        read: fn(&Row<'_>, i64) -> Result<Programme>,
        guide: &Match,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<Programme>> {
        let shift = guide.shift_secs();
        let (from, to) = (from - shift, to - shift);
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(
            params![guide.guide_channel, from - LOOKBACK_SECS, from, to],
            |row| read(row, shift),
        )?;
        rows.collect()
    }

    /// The programme on air at `now` (the latest-starting one, when they overlap) and the next
    /// one to start, from one read snapshot.
    pub fn now_next(
        &self,
        guide: &Match,
        now: Timestamp,
    ) -> Result<(Option<Programme>, Option<Programme>)> {
        let shift = guide.shift_secs();
        let at = now - shift;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let current = tx
            .prepare_cached(CURRENT_SQL)?
            .query_row(
                params![guide.guide_channel, at - LOOKBACK_SECS, at],
                |row| programme(row, shift),
            )
            .optional()?;
        let next = tx
            .prepare_cached(NEXT_SQL)?
            .query_row(params![guide.guide_channel, at], |row| {
                programme(row, shift)
            })
            .optional()?;
        tx.finish()?;
        Ok((current, next))
    }

    /// Programmes of every guide's current generation that overlap `from..to` and whose title
    /// contains `query` (a non-empty [`fold`]ed string), in `SEARCH_SQL`'s order around `now`,
    /// all in guide time. Each goes to `visit` in turn until it returns false. One scan of the
    /// programme table inside SQLite: ~10 ms for 100k programmes on an M-series Mac, too long for
    /// the UI thread on a TV.
    pub fn search(
        &self,
        query: &str,
        from: Timestamp,
        to: Timestamp,
        now: Timestamp,
        mut visit: impl FnMut(TitleHit) -> bool,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(SEARCH_SQL)?;
        let mut rows = stmt.query(params![from, to, query, now])?;
        while let Some(row) = rows.next()? {
            let hit = TitleHit {
                guide_channel: row.get(0)?,
                start: row.get(1)?,
                stop: row.get(2)?,
                title: row.get(3)?,
            };
            if !visit(hit) {
                break;
            }
        }
        Ok(())
    }
}

/// Busy handler: polls every millisecond for five seconds. SQLite's own timeout backs off to
/// 100 ms naps and keeps missing the short gaps between a refresh's write transactions.
fn wait_for_lock(attempt: i32) -> bool {
    std::thread::sleep(Duration::from_millis(1));
    attempt < 5000
}

/// Switches the file to WAL and returns the journal mode now in force. The switch needs an
/// exclusive lock and SQLite does not wait for that one, so a second process opening the same
/// new file retries for up to five seconds.
fn wal(conn: &Connection) -> Result<String> {
    let mut tries = 0;
    loop {
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0)) {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy && tries < 250 =>
            {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            other => return other,
        }
    }
}

/// Brings the schema up to date. Each step runs in a write transaction that reads the version
/// itself, so two processes opening a new file at once cannot both apply it.
fn migrate(conn: &mut Connection) -> Result<()> {
    let known = MIGRATIONS.len() as i64;
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > known {
            return Err(rusqlite::Error::SqliteFailure(
                ffi::Error::new(ffi::SQLITE_MISMATCH),
                Some(format!(
                    "database schema {version} is newer than this app ({known})"
                )),
            ));
        }
        if version == known {
            return tx.commit();
        }
        tx.execute_batch(MIGRATIONS[version as usize])?;
        tx.pragma_update(None, "user_version", version + 1)?;
        tx.commit()?;
    }
}

/// Removes the header guides no playlist names, and the validators of URLs nothing is fetched
/// from any more.
fn delete_orphan_guides(tx: &Transaction<'_>) -> Result<()> {
    tx.execute(
        "DELETE FROM guide_source WHERE manual = 0
         AND id NOT IN (SELECT source_id FROM guide_source_playlist)",
        [],
    )?;
    prune_validators(tx)
}

/// Validators are kept per URL, tokens included: only for URLs a playlist or a guide still has.
fn prune_validators(tx: &Transaction<'_>) -> Result<()> {
    tx.execute(
        "DELETE FROM http_validator WHERE url NOT IN
         (SELECT url FROM playlist UNION ALL SELECT url FROM guide_source)",
        [],
    )?;
    Ok(())
}

/// What [`Storage::open`] requires of the data dir and the database files, for `owner`.
#[cfg(unix)]
fn private_files(dir: &Path, db: &Path, owner: u32) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir)?;
    // Whoever can write the directory can swap a file in it between this check and SQLite's
    // open of it.
    if !meta.is_dir() || meta.uid() != owner || meta.mode() & 0o022 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the data dir is not this uid's alone",
        ));
    }
    tv_base::paths::private_file(db, owner, true)?;
    for suffix in ["-wal", "-shm"] {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        tv_base::paths::private_file(Path::new(&name), owner, false)?;
    }
    Ok(())
}

/// Writes the playlist's channel rows in place of the stored ones, each under its key in this
/// playlist ([`ChannelKey::in_playlist`]).
fn replace_channels(
    tx: &Transaction<'_>,
    id: PlaylistId,
    playlist: &Playlist,
    buf: &mut Vec<u8>,
) -> Result<()> {
    tx.execute("DELETE FROM channel WHERE playlist_id = ?1", [id])?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO channel (playlist_id, position, key, name, number, logo, groups, url, tvg_id,
         tvg_name, tvg_shift_minutes, catchup_mode, catchup_days, catchup_source, radio,
         http_headers, extra, catchup_correction_secs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
         ?18)",
    )?;
    for (position, c) in playlist.channels.iter().enumerate() {
        let [groups, http_headers, extra] = json_columns(buf, c)?;
        insert.execute(params![
            id,
            position as i64,
            c.key.in_playlist(id).0,
            c.name,
            c.number,
            c.logo,
            groups,
            c.url,
            c.tvg_id,
            c.tvg_name,
            c.tvg_shift_minutes,
            c.catchup.mode.map(CatchupMode::as_str),
            c.catchup.days,
            c.catchup.source,
            c.radio,
            http_headers,
            extra,
            c.catchup.correction_secs,
        ])?;
    }
    Ok(())
}

/// Links the guides the playlist header names, in order, and drops the guides no playlist names
/// any more. A manual source keeps the scope the user gave it.
fn link_guides(tx: &Transaction<'_>, id: PlaylistId, urls: &[String]) -> Result<()> {
    tx.execute(
        "DELETE FROM guide_source_playlist WHERE playlist_id = ?1
         AND source_id IN (SELECT id FROM guide_source WHERE manual = 0)",
        [id],
    )?;
    for (position, url) in urls.iter().enumerate() {
        let source: SourceId = tx
            .prepare_cached(
                "INSERT INTO guide_source (url) VALUES (?1)
                 ON CONFLICT (url) DO UPDATE SET url = url RETURNING id",
            )?
            .query_row([url], |row| row.get(0))?;
        tx.prepare_cached(
            "INSERT OR IGNORE INTO guide_source_playlist (source_id, playlist_id, position)
             SELECT ?1, ?2, ?3 FROM guide_source WHERE id = ?1 AND manual = 0",
        )?
        .execute([source, id, position as i64])?;
    }
    delete_orphan_guides(tx)
}

/// FNV-1a over a hash of each channel's stored fields, then of each guide URL.
fn content_hash(playlist: &Playlist, buf: &mut Vec<u8>) -> Result<i64> {
    let mut hashes = Vec::with_capacity(8 * (playlist.channels.len() + playlist.epg_urls.len()));
    for c in &playlist.channels {
        buf.clear();
        let fields = (
            c.key.0,
            &c.name,
            c.number,
            &c.logo,
            &c.groups,
            &c.url,
            &c.tvg_id,
            &c.tvg_name,
            c.tvg_shift_minutes,
            c.catchup.mode.map(CatchupMode::as_str),
            c.catchup.days,
            &c.catchup.source,
            c.radio,
            &c.http_headers,
            &c.extra,
            c.catchup.correction_secs,
        );
        serde_json::to_writer(&mut *buf, &fields).map_err(to_sql)?;
        hashes.extend(fnv1a(buf).to_le_bytes());
    }
    for url in &playlist.epg_urls {
        hashes.extend(fnv1a(url.as_bytes()).to_le_bytes());
    }
    Ok(fnv1a(&hashes) as i64)
}

/// A channel's JSON columns (groups, headers, extra), written into one reused buffer.
fn json_columns<'b>(buf: &'b mut Vec<u8>, c: &Channel) -> Result<[&'b str; 3]> {
    buf.clear();
    serde_json::to_writer(&mut *buf, &c.groups).map_err(to_sql)?;
    let groups = buf.len();
    serde_json::to_writer(&mut *buf, &c.http_headers).map_err(to_sql)?;
    let headers = buf.len();
    serde_json::to_writer(&mut *buf, &c.extra).map_err(to_sql)?;
    let text = std::str::from_utf8(buf).map_err(to_sql)?;
    Ok([&text[..groups], &text[groups..headers], &text[headers..]])
}

fn to_sql(e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

fn json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(to_sql)
}

fn from_json<T: serde::de::DeserializeOwned>(row: &Row<'_>, column: usize) -> Result<T> {
    serde_json::from_str(row.get_ref(column)?.as_str()?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn history_entry(row: &Row<'_>) -> Result<HistoryEntry> {
    Ok(HistoryEntry {
        key: ChannelKey(row.get(0)?),
        start: row.get(1)?,
        stop: row.get(2)?,
        title: row.get(3)?,
        icon: row.get(4)?,
        position_secs: row.get(5)?,
        duration_secs: row.get(6)?,
        completed: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn programme(row: &Row<'_>, shift: i64) -> Result<Programme> {
    Ok(Programme {
        description: row.get(3)?,
        category: row.get(4)?,
        icon: row.get(5)?,
        ..grid_programme(row, shift)?
    })
}

fn grid_programme(row: &Row<'_>, shift: i64) -> Result<Programme> {
    Ok(Programme {
        start: row.get::<_, i64>(0)? + shift,
        stop: row.get::<_, i64>(1)? + shift,
        title: row.get(2)?,
        ..Programme::default()
    })
}

/// The import sink: buffers up to [`BATCH`] rows and writes them in one short transaction, so
/// the write lock is never held while the download stalls.
struct GuideWriter<'c> {
    conn: &'c Connection,
    source: SourceId,
    generation: i64,
    /// XMLTV id to channel row, for this import; at most [`MAX_ROWS`].
    rows: HashMap<String, GuideChannelId>,
    /// Programmes ignored for a start their channel already had.
    duplicates: usize,
    /// Programmes this import may still store (`MAX_PROGRAMMES` less the other guides').
    room: usize,
    /// Programmes not stored for want of room.
    over_budget: usize,
    channels: Vec<EpgChannel>,
    programmes: Vec<(Owner, Programme)>,
}

/// A buffered programme's channel: its row, or its XMLTV id while it has none yet.
enum Owner {
    Row(GuideChannelId),
    Id(String),
}

/// Whether channel `id` has a row or may get one.
fn has_room(rows: &HashMap<String, GuideChannelId>, id: &str) -> bool {
    rows.len() < MAX_ROWS || rows.contains_key(id)
}

impl GuideWriter<'_> {
    fn flush(&mut self) -> Result<()> {
        if self.channels.is_empty() && self.programmes.is_empty() {
            return Ok(());
        }
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        {
            let mut declared = tx.prepare_cached(
                "INSERT INTO epg_channel (source_id, xmltv_id, names, icon, generation)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (source_id, xmltv_id) DO UPDATE SET names = excluded.names,
                 icon = excluded.icon, generation = excluded.generation RETURNING id",
            )?;
            for channel in self.channels.drain(..) {
                if !has_room(&self.rows, &channel.id) {
                    continue;
                }
                let row = declared.query_row(
                    params![
                        self.source,
                        channel.id,
                        json(&channel.names)?,
                        channel.icon,
                        self.generation
                    ],
                    |row| row.get(0),
                )?;
                self.rows.insert(channel.id, row);
            }
            // A channel known only from its programmes so far; its declaration may follow.
            let mut implied = tx.prepare_cached(
                "INSERT INTO epg_channel (source_id, xmltv_id, names, generation)
                 VALUES (?1, ?2, json_array(?2), ?3)
                 ON CONFLICT (source_id, xmltv_id) DO UPDATE SET generation = excluded.generation
                 RETURNING id",
            )?;
            let mut insert = tx.prepare_cached(
                "INSERT OR IGNORE INTO programme (channel_id, generation, start, stop, title,
                 description, category, icon, folded) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for (owner, p) in self.programmes.drain(..) {
                let row = match owner {
                    Owner::Row(row) => row,
                    Owner::Id(id) => match self.rows.get(&id) {
                        Some(&row) => row,
                        None if has_room(&self.rows, &id) => {
                            let row = implied
                                .query_row(params![self.source, id, self.generation], |row| {
                                    row.get(0)
                                })?;
                            self.rows.insert(id, row);
                            row
                        }
                        None => continue,
                    },
                };
                let inserted = insert.execute(params![
                    row,
                    self.generation,
                    p.start,
                    p.stop,
                    p.title,
                    p.description,
                    p.category,
                    p.icon,
                    fold(&p.title)
                ])?;
                self.duplicates += usize::from(inserted == 0);
            }
        }
        tx.commit()
    }
}

impl Sink for GuideWriter<'_> {
    type Error = rusqlite::Error;

    // Channels outside the filter are stored too: the catalog is what a manual mapping picks
    // from. The XMLTV parser bounds how many it sends.
    fn channel(&mut self, channel: EpgChannel, _wanted: bool) -> Result<()> {
        self.channels.push(channel);
        if self.channels.len() >= BATCH {
            self.flush()?;
        }
        Ok(())
    }

    fn programme(&mut self, channel_id: &str, programme: Programme) -> Result<()> {
        if self.room == 0 {
            self.over_budget += 1;
            return Ok(());
        }
        self.room -= 1;
        let owner = match self.rows.get(channel_id) {
            Some(&row) => Owner::Row(row),
            None => Owner::Id(channel_id.to_owned()),
        };
        self.programmes.push((owner, programme));
        if self.programmes.len() >= BATCH {
            self.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(storage: &Storage, table: &str) -> i64 {
        storage
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn entry(key: i64, start: Timestamp, stop: Timestamp) -> HistoryEntry {
        HistoryEntry {
            key: ChannelKey(key),
            start,
            stop,
            title: String::new(),
            icon: None,
            position_secs: 60,
            duration_secs: 3600,
            completed: false,
            updated_at: start,
        }
    }

    #[test]
    fn migrations_set_the_version_once() {
        let storage = Storage::open_in_memory().unwrap();
        let version: i64 = storage
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        let mut conn = storage.conn;
        migrate(&mut conn).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        assert!(migrate(&mut conn).is_err());
    }

    #[test]
    fn the_correction_column_makes_each_playlist_download_again() {
        let conn = Connection::open_in_memory().unwrap();
        for step in &MIGRATIONS[..2] {
            conn.execute_batch(step).unwrap();
        }
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute_batch(
            "INSERT INTO playlist (id, name, url, content_hash) VALUES (1, 'p', 'http://h/p', 7);
             INSERT INTO guide_source (id, url) VALUES (1, 'http://h/g');
             INSERT INTO channel (playlist_id, position, key, name, groups, url,
             tvg_shift_minutes, catchup_mode, catchup_days, radio, http_headers, extra)
             VALUES (1, 0, 1, 'a', '[]', 'http://h/a', 0, 'shift', 3, 0, '[]',
             '[[\"catchup-correction\",\"2\"]]');
             INSERT INTO http_validator (url, etag) VALUES ('http://h/p', 'e'), ('http://h/g', 'e');",
        )
        .unwrap();
        let storage = Storage::init(conn, "memory").unwrap();
        let channel = storage.channels(1).unwrap().remove(0);
        assert_eq!(channel.catchup.correction_secs, 0);
        let hash: Option<i64> = storage
            .conn
            .query_row("SELECT content_hash FROM playlist", [], |row| row.get(0))
            .unwrap();
        assert_eq!(hash, None);
        let validated: Vec<String> = storage
            .conn
            .prepare("SELECT url FROM http_validator")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(validated, ["http://h/g"], "a guide keeps its validators");
    }

    #[test]
    fn guide_queries_seek_the_slot_index() {
        let storage = Storage::open_in_memory().unwrap();
        let plan = |sql: &str| -> String {
            let mut stmt = storage
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let params = vec![1i64; stmt.parameter_count()];
            let steps = stmt
                .query_map(rusqlite::params_from_iter(params), |row| {
                    row.get::<_, String>(3)
                })
                .unwrap();
            steps.collect::<Result<Vec<_>>>().unwrap().join(" | ")
        };
        let range = "programme_slot (channel_id=? AND generation=? AND start>? AND start<?)";
        for sql in [WINDOW_SQL, GRID_SQL] {
            // The window and the on-air lookup inside it.
            assert_eq!(plan(sql).matches(range).count(), 2, "{}", plan(sql));
        }
        assert!(plan(CURRENT_SQL).contains(range), "{}", plan(CURRENT_SQL));
        let next = "programme_slot (channel_id=? AND generation=? AND start>?)";
        assert!(plan(NEXT_SQL).contains(next), "{}", plan(NEXT_SQL));
        let at = "programme_slot (channel_id=? AND generation=? AND start=?)";
        assert!(plan(AT_SQL).contains(at), "{}", plan(AT_SQL));
    }

    #[test]
    fn a_save_prunes_expired_history() {
        let storage = Storage::open_in_memory().unwrap();
        storage.set_setting(HISTORY_DAYS, "1").unwrap();
        storage.save_progress(&entry(1, 0, 3600), 0).unwrap();
        assert_eq!(count(&storage, "history"), 1);
        let after = 3600 + DAY_SECS + 1;
        storage
            .save_progress(&entry(2, after, after + 3600), after)
            .unwrap();
        assert_eq!(count(&storage, "history"), 1);
        assert!(storage.progress(ChannelKey(1), 0, 0).unwrap().is_none());
    }

    /// XMLTV's `YYYYMMDDHHMMSS +0000`.
    pub(super) fn stamp(t: Timestamp) -> String {
        let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
        let (year, month, day) = tv_base::time::civil_from_days(days);
        let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
        format!("{year:04}{month:02}{day:02}{h:02}{m:02}{s:02} +0000")
    }

    /// A guide of `(channel, start, stop, title)` programmes.
    fn guide_of(programmes: &[(&str, Timestamp, Timestamp, &str)]) -> String {
        let mut xml = String::from("<tv>");
        for (channel, start, stop, title) in programmes {
            xml += &format!(
                r#"<programme channel="{channel}" start="{}" stop="{}"><title>{title}</title></programme>"#,
                stamp(*start),
                stamp(*stop)
            );
        }
        xml + "</tv>"
    }

    #[test]
    fn search_matches_folded_titles_of_the_current_guide_in_list_order() {
        const NOW: Timestamp = 1_780_000_000;
        let storage = Storage::open_in_memory().unwrap();
        let source = storage
            .add_manual_guide("http://g.example/x.xml", &[])
            .unwrap();
        let options = xmltv::Options::new(NOW);
        let news = "Wiadomo\u{15b}ci";
        let xml = guide_of(&[
            ("a", NOW - 7200, NOW - 3600, news),
            ("a", NOW - 600, NOW + 1200, news),
            ("a", NOW + 1200, NOW + 3000, "Sport"),
            ("b", NOW - 9000, NOW - 7200, "WIADOMO\u{15a}CI wieczorne"),
            ("b", NOW + 3600, NOW + 5400, "wiadomosci"),
        ]);
        storage
            .import_guide(source, xml.as_bytes(), &options)
            .unwrap();
        let search = |query: &str, max: usize| {
            let mut hits = Vec::new();
            let (from, to) = (NOW - 86_400, NOW + 86_400);
            storage
                .search(query, from, to, NOW, |h| {
                    hits.push((h.start, h.title));
                    hits.len() < max
                })
                .unwrap();
            hits
        };
        let all = search("wiadomosci", 10);
        assert_eq!(
            all,
            [
                (NOW - 600, news.to_owned()),
                (NOW - 7200, news.to_owned()),
                (NOW - 9000, "WIADOMO\u{15a}CI wieczorne".to_owned()),
                (NOW + 3600, "wiadomosci".to_owned()),
            ],
            "on air, past newest first, upcoming"
        );
        assert_eq!(search("wiadomosci", 1).len(), 1, "visit stops the scan");
        assert_eq!(search("cnn", 10), []);
        // A new import replaces the generation searched.
        let xml = guide_of(&[("a", NOW - 600, NOW + 1200, "Sport")]);
        storage
            .import_guide(source, xml.as_bytes(), &options)
            .unwrap();
        assert_eq!(search("wiadomosci", 10), []);
        assert_eq!(search("sport", 10), [(NOW - 600, "Sport".to_owned())]);
        // A row from before the folded column matches on its lowercased title.
        storage
            .conn
            .execute("UPDATE programme SET folded = NULL", [])
            .unwrap();
        assert_eq!(search("spo", 10).len(), 1);
    }

    #[test]
    fn an_already_expired_entry_is_not_written() {
        let storage = Storage::open_in_memory().unwrap();
        let now = 100 * DAY_SECS;
        storage.save_progress(&entry(1, 0, 3600), now).unwrap();
        assert_eq!(count(&storage, "history"), 0);
    }
}
