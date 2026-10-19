//! VOD on [`Storage`]: each playlist's movies and episodes, written apart from its
//! channels when a refresh stores the playlist, and how far into each item the viewer got.

use rusqlite::{params, OptionalExtension, Row};

use super::{from_json, json, to_sql, Result, Storage};
use crate::epg::PlaylistId;
use crate::model::{fnv1a, ChannelKey};
use crate::vod::{VodItem, VodKind, VodProgress};

/// The most positions kept; saving one past it drops the least recently watched.
const MAX_PROGRESS: i64 = 2_000;

const ITEM_COLUMNS: &str = "key, kind, title, series, season, episode, year, poster, category, \
                            url, http_headers";

impl Storage {
    /// Replaces the playlist's VOD items. False when they are what is stored already (the same
    /// download again): nothing is rewritten then.
    pub fn save_vod(&self, playlist: PlaylistId, items: &[VodItem]) -> Result<bool> {
        let hash = content_hash(items)?;
        let tx = self.write()?;
        let stored: Option<i64> = tx
            .prepare_cached("SELECT content_hash FROM vod_source WHERE playlist_id = ?1")?
            .query_row([playlist], |row| row.get(0))
            .optional()?;
        if stored == Some(hash) {
            return Ok(false);
        }
        tx.execute("DELETE FROM vod_item WHERE playlist_id = ?1", [playlist])?;
        {
            let mut insert = tx.prepare_cached(&format!(
                "INSERT INTO vod_item (playlist_id, position, {ITEM_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
            ))?;
            for (position, v) in items.iter().enumerate() {
                insert.execute(params![
                    playlist,
                    position as i64,
                    v.key.0,
                    v.kind.as_str(),
                    v.title,
                    v.series,
                    v.season,
                    v.episode,
                    v.year,
                    v.poster,
                    category(&v.groups),
                    v.url,
                    json(&v.http_headers)?,
                ])?;
            }
        }
        tx.execute(
            "INSERT INTO vod_source (playlist_id, content_hash) VALUES (?1, ?2)
             ON CONFLICT (playlist_id) DO UPDATE SET content_hash = excluded.content_hash",
            params![playlist, hash],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// In playlist order.
    pub fn vod_items(&self, playlist: PlaylistId) -> Result<Vec<VodItem>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {ITEM_COLUMNS} FROM vod_item WHERE playlist_id = ?1 ORDER BY position"
        ))?;
        let rows = stmt.query_map([playlist], item)?;
        rows.collect()
    }

    /// Keeps where the viewer got in an item, or forgets it when [`VodProgress::worth_keeping`]
    /// says there is nothing to resume or mark.
    pub fn save_vod_progress(&self, p: &VodProgress) -> Result<()> {
        let tx = self.write()?;
        if p.worth_keeping() {
            tx.prepare_cached(
                "INSERT OR REPLACE INTO vod_progress
                 (key, position_secs, duration_secs, watched, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                p.key.0,
                p.position_secs,
                p.duration_secs,
                p.watched,
                p.updated_at
            ])?;
            tx.prepare_cached(
                "DELETE FROM vod_progress WHERE key IN (SELECT key FROM vod_progress
                 ORDER BY updated_at DESC, key LIMIT -1 OFFSET ?1)",
            )?
            .execute([MAX_PROGRESS])?;
        } else {
            tx.prepare_cached("DELETE FROM vod_progress WHERE key = ?1")?
                .execute([p.key.0])?;
        }
        tx.commit()
    }

    /// Every kept position, most recently watched first.
    pub fn vod_progress(&self) -> Result<Vec<VodProgress>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT key, position_secs, duration_secs, watched, updated_at FROM vod_progress
             ORDER BY updated_at DESC, key",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(VodProgress {
                key: ChannelKey(row.get(0)?),
                position_secs: row.get(1)?,
                duration_secs: row.get(2)?,
                watched: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        rows.collect()
    }
}

fn item(row: &Row<'_>) -> Result<VodItem> {
    let kind = row.get_ref(1)?.as_str()?;
    Ok(VodItem {
        key: ChannelKey(row.get(0)?),
        kind: VodKind::parse(kind).unwrap_or_default(),
        title: row.get(2)?,
        series: row.get(3)?,
        season: row.get(4)?,
        episode: row.get(5)?,
        year: row.get(6)?,
        poster: row.get(7)?,
        groups: groups(row.get(8)?),
        url: row.get(9)?,
        http_headers: from_json(row, 10)?,
    })
}

/// The `category` column: the groups as a playlist's `group-title` lists them, `;` between (the
/// parser splits names on it, so none holds one); `NULL` for none.
fn category(groups: &[String]) -> Option<String> {
    (!groups.is_empty()).then(|| groups.join(";"))
}

/// [`category`] read back.
fn groups(category: Option<String>) -> Vec<String> {
    let names = category.iter().flat_map(|c| c.split(';'));
    names.filter(|g| !g.is_empty()).map(str::to_owned).collect()
}

/// FNV-1a over a hash of each item's stored fields.
fn content_hash(items: &[VodItem]) -> Result<i64> {
    let mut buf = Vec::new();
    let mut hashes = Vec::with_capacity(8 * items.len());
    for v in items {
        buf.clear();
        let fields = (
            v.key.0,
            v.kind.as_str(),
            &v.title,
            &v.series,
            v.season,
            v.episode,
            v.year,
            &v.poster,
            &v.groups,
            &v.url,
            &v.http_headers,
        );
        serde_json::to_writer(&mut buf, &fields).map_err(to_sql)?;
        hashes.extend(fnv1a(&buf).to_le_bytes());
    }
    Ok(fnv1a(&hashes) as i64)
}

#[cfg(test)]
mod tests {
    use super::super::MIGRATIONS;
    use super::*;
    use rusqlite::Connection;

    fn movie(n: u32) -> VodItem {
        let url = format!("http://h.example/movie/{n}.mkv?token=t");
        VodItem {
            key: ChannelKey::of_url(&url),
            kind: VodKind::Movie,
            title: format!("Film {n}"),
            year: Some(1990 + n as u16),
            poster: Some("http://h.example/p.jpg".into()),
            groups: vec!["Filmy".into()],
            url,
            http_headers: vec![("User-Agent".into(), "x".into())],
            ..VodItem::default()
        }
    }

    fn episode(s: u32, e: u32) -> VodItem {
        let url = format!("http://h.example/series/{s}/{e}.mp4");
        VodItem {
            key: ChannelKey::of_url(&url),
            kind: VodKind::Episode,
            title: String::new(),
            series: Some("Ranczo".into()),
            season: Some(s),
            episode: Some(e),
            url,
            ..VodItem::default()
        }
    }

    #[test]
    fn items_round_trip_and_the_same_list_is_not_written_again() {
        let s = Storage::open_in_memory().unwrap();
        let id = s.add_playlist("p", "http://h.example/list.m3u").unwrap();
        let mut two_groups = movie(2);
        two_groups.groups.push("Adult".into());
        let items = vec![movie(1), episode(1, 2), two_groups];
        assert!(s.save_vod(id, &items).unwrap());
        assert_eq!(s.vod_items(id).unwrap(), items);
        assert!(!s.save_vod(id, &items).unwrap());
        assert!(s.save_vod(id, &items[..1]).unwrap());
        assert_eq!(s.vod_items(id).unwrap(), &items[..1]);
        // Removing the playlist takes its items with it.
        s.remove_playlist(id).unwrap();
        assert!(s.vod_items(id).unwrap().is_empty());
        let rows: i64 = s
            .conn
            .query_row("SELECT count(*) FROM vod_source", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn positions_are_kept_forgotten_and_capped() {
        let s = Storage::open_in_memory().unwrap();
        let key = ChannelKey(7);
        s.save_vod_progress(&VodProgress::new(key, 600, 6_000, 10))
            .unwrap();
        s.save_vod_progress(&VodProgress::new(ChannelKey(8), 5_990, 6_000, 20))
            .unwrap();
        let kept = s.vod_progress().unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].key, ChannelKey(8));
        assert!(kept[0].watched);
        assert_eq!(kept[1], VodProgress::new(key, 600, 6_000, 10));
        // Back to the first seconds: nothing to resume, the position goes.
        s.save_vod_progress(&VodProgress::new(key, 5, 6_000, 30))
            .unwrap();
        assert_eq!(s.vod_progress().unwrap().len(), 1);
        for k in 0..MAX_PROGRESS + 5 {
            s.save_vod_progress(&VodProgress::new(ChannelKey(100 + k), 60, 0, 100 + k))
                .unwrap();
        }
        let kept = s.vod_progress().unwrap();
        assert_eq!(kept.len() as i64, MAX_PROGRESS);
        assert_eq!(kept[0].key, ChannelKey(100 + MAX_PROGRESS + 4));
    }

    #[test]
    fn a_database_from_before_vod_moves_its_playlists_to_a_full_download() {
        let at = MIGRATIONS
            .iter()
            .position(|m| m.contains("CREATE TABLE vod_item"))
            .unwrap();
        let conn = Connection::open_in_memory().unwrap();
        for step in &MIGRATIONS[..at] {
            conn.execute_batch(step).unwrap();
        }
        conn.pragma_update(None, "user_version", at as i64).unwrap();
        conn.execute_batch(
            "INSERT INTO playlist (id, name, url, content_hash) VALUES (5, 'p', 'http://h/l', 42);
             INSERT INTO http_validator (url, etag) VALUES ('http://h/l', 'e');",
        )
        .unwrap();
        let s = Storage::init(conn, "memory").unwrap();
        let version: i64 = s
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        let (hash, validators): (Option<i64>, i64) = s
            .conn
            .query_row(
                "SELECT (SELECT content_hash FROM playlist WHERE id = 5),
                 (SELECT count(*) FROM http_validator)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((hash, validators), (None, 0));
        assert!(s.save_vod(5, &[movie(3)]).unwrap());
        assert_eq!(s.vod_items(5).unwrap(), [movie(3)]);
    }
}
