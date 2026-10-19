//! The refresher's [`Store`] on top of [`Storage`]: the enabled playlists, the guides they serve,
//! and the HTTP validators that let a restart send a conditional GET instead of downloading
//! again. A guide is imported with the channels of every enabled playlist it serves.

use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read};

use rusqlite::{params, ErrorCode, OptionalExtension};

use super::{Result, Storage};
use crate::epg::{self, GuideSource, PlaylistId};
use crate::model::{fnv1a, name_key, unix_now, Channel, Playlist, Timestamp};
use crate::refresh::{Store, StoreError, Stored, Validators};
use crate::xmltv::{self, Options};

pub struct RefreshStore {
    storage: Storage,
    now: fn() -> Timestamp,
    /// The channel filter of each set of enabled playlists a guide serves, computed on first use
    /// and dropped when a new playlist is stored.
    filters: HashMap<Vec<PlaylistId>, Filter>,
}

/// The XMLTV filter the stored playlists give a guide that serves them (no overrides yet).
struct Filter {
    ids: HashSet<String>,
    names: HashSet<String>,
    /// What decides whether a stored guide is current: a changed filter forces a full download.
    fingerprint: i64,
}

impl RefreshStore {
    pub fn new(storage: Storage) -> Self {
        RefreshStore {
            storage,
            now: unix_now,
            filters: HashMap::new(),
        }
    }

    /// Replaces the clock that stamps imports and centres the kept guide window.
    pub fn with_clock(mut self, now: fn() -> Timestamp) -> Self {
        self.now = now;
        self
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// The stored guide source fetched from `url`.
    fn guide(&self, url: &str) -> Result<Option<GuideSource>> {
        let sources = self.storage.guide_sources()?;
        let info = sources.into_iter().find(|s| s.url == url);
        Ok(info.map(|info| info.into_source(Vec::new())))
    }

    /// The enabled playlists `source` serves, in playlist order.
    fn served(&self, source: &GuideSource) -> Result<Vec<PlaylistId>> {
        let playlists = self.storage.playlists()?;
        Ok(playlists
            .iter()
            .filter(|p| p.enabled && source.serves(p.id))
            .map(|p| p.id)
            .collect())
    }

    /// The filter of the `served` playlists, taken out of the cache or made for `source` (one
    /// that serves them); the caller puts it back with [`RefreshStore::keep_filter`].
    fn take_filter(&mut self, served: &[PlaylistId], source: &GuideSource) -> Result<Filter> {
        if let Some(filter) = self.filters.remove(served) {
            return Ok(filter);
        }
        let channels = served
            .iter()
            .map(|&id| Ok((id, self.storage.guide_names(id)?)))
            .collect::<Result<Vec<_>>>()?;
        let playlists: Vec<(PlaylistId, &[Channel])> = channels
            .iter()
            .map(|(id, list)| (*id, list.as_slice()))
            .collect();
        let (ids, names) = epg::filter(source, &playlists, &HashMap::new());
        let fingerprint = fingerprint(&ids, &names);
        Ok(Filter {
            ids,
            names,
            fingerprint,
        })
    }

    fn keep_filter(&mut self, served: Vec<PlaylistId>, filter: Filter) {
        self.filters.insert(served, filter);
    }

    /// The filter fingerprint of the guide at `url`; `None` for a URL that is not a guide.
    fn fingerprint(&mut self, url: &str) -> Result<Option<i64>> {
        let Some(source) = self.guide(url)? else {
            return Ok(None);
        };
        let served = self.served(&source)?;
        if served.is_empty() {
            return Ok(Some(fingerprint(&HashSet::new(), &HashSet::new())));
        }
        let filter = self.take_filter(&served, &source)?;
        let fingerprint = filter.fingerprint;
        self.keep_filter(served, filter);
        Ok(Some(fingerprint))
    }

    fn stored_validators(&mut self, url: &str) -> Result<Validators> {
        let row: Option<(Option<String>, Option<String>, Option<i64>)> = self
            .storage
            .conn
            .prepare_cached(
                "SELECT etag, last_modified, filter FROM http_validator WHERE url = ?1",
            )?
            .query_row([url], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .optional()?;
        let Some((etag, last_modified, stored)) = row else {
            return Ok(Validators::default());
        };
        if stored.is_some() && self.fingerprint(url)? != stored {
            return Ok(Validators::default());
        }
        Ok(Validators {
            etag,
            last_modified,
        })
    }

    fn store_validators(&mut self, url: &str, validators: &Validators) -> Result<()> {
        if validators.etag.is_none() && validators.last_modified.is_none() {
            self.storage
                .conn
                .prepare_cached("DELETE FROM http_validator WHERE url = ?1")?
                .execute([url])?;
            return Ok(());
        }
        let fingerprint = self.fingerprint(url)?;
        self.storage
            .conn
            .prepare_cached(
                "INSERT OR REPLACE INTO http_validator (url, etag, last_modified, filter)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                url,
                validators.etag,
                validators.last_modified,
                fingerprint
            ])?;
        Ok(())
    }

    /// The guides the enabled playlists' headers name, in playlist order, each once.
    fn header_guides(&self) -> Result<Vec<String>> {
        let mut stmt = self.storage.conn.prepare_cached(
            "SELECT s.url FROM guide_source s
             JOIN guide_source_playlist l ON l.source_id = s.id
             WHERE l.playlist_id = ?1 AND s.manual = 0 ORDER BY l.position",
        )?;
        let mut urls: Vec<String> = Vec::new();
        for playlist in self.storage.playlists()?.iter().filter(|p| p.enabled) {
            for url in stmt.query_map([playlist.id], |row| row.get::<_, String>(0))? {
                let url = url?;
                if !urls.contains(&url) {
                    urls.push(url);
                }
            }
        }
        Ok(urls)
    }

    /// Imports the guide of `source` with the filter of the `served` playlists.
    fn import(
        &mut self,
        served: Vec<PlaylistId>,
        source: &GuideSource,
        body: &mut dyn Read,
    ) -> std::result::Result<Stored, StoreError> {
        let filter = self.take_filter(&served, source).map_err(StoreError::new)?;
        if filter.ids.is_empty() && filter.names.is_empty() {
            self.keep_filter(served, filter);
            return Ok(Stored::Same);
        }
        // Guide ids the stored catalog names like a wanted channel: with them a guide that lists
        // programmes before their channel still matches by name from its second import on.
        let named = match self.storage.catalog_ids_named(source.id, &filter.names) {
            Ok(named) => named,
            Err(e) => {
                self.keep_filter(served, filter);
                return Err(StoreError::new(e));
            }
        };
        let Filter {
            mut ids,
            names,
            fingerprint,
        } = filter;
        // Taken out of the playlists' ids again below.
        let added: Vec<String> = named
            .into_iter()
            .filter(|id| ids.insert(id.clone()))
            .collect();
        let options = Options {
            ids,
            names,
            ..Options::new((self.now)())
        };
        let input = BufReader::with_capacity(64 << 10, body);
        let imported = self.storage.import_guide(source.id, input, &options);
        let Options { mut ids, names, .. } = options;
        for id in &added {
            ids.remove(id);
        }
        self.keep_filter(
            served,
            Filter {
                ids,
                names,
                fingerprint,
            },
        );
        let imported = imported.map_err(|e| match e {
            xmltv::Error::Sink(e) if e.sqlite_error_code() == Some(ErrorCode::DiskFull) => {
                StoreError::new("the guide does not fit in storage")
            }
            e => StoreError::new(e),
        })?;
        let stats = &imported.summary.stats;
        Ok(match (imported.replaced, imported.summary.complete) {
            (true, _) => Stored::New {
                cache_validators: !imported.summary.retry,
            },
            // None stored, some over the budget: the other guides hold all the room.
            (false, true) if stats.programmes_kept == 0 && stats.dropped_budget > 0 => {
                Stored::Kept("no room for the guide's programmes")
            }
            (false, true) => Stored::Kept("the guide has no programmes"),
            (false, false) => Stored::Kept("the guide ended early"),
        })
    }
}

impl Storage {
    /// XMLTV ids of the source's catalog channels that carry one of `names` (name keys).
    fn catalog_ids_named(
        &self,
        source: epg::SourceId,
        names: &HashSet<String>,
    ) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT xmltv_id, names FROM epg_channel WHERE source_id = ?1")?;
        let mut ids = Vec::new();
        let mut rows = stmt.query([source])?;
        while let Some(row) = rows.next()? {
            let declared: Vec<String> = super::from_json(row, 1)?;
            if declared.iter().any(|n| names.contains(&name_key(n))) {
                ids.push(row.get(0)?);
            }
        }
        Ok(ids)
    }

    /// The playlist's channels with only what the guide filter reads: name, `tvg-name` and
    /// `tvg-id`.
    fn guide_names(&self, playlist: PlaylistId) -> Result<Vec<Channel>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT name, tvg_name, tvg_id FROM channel WHERE playlist_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map([playlist], |row| {
            Ok(Channel {
                name: row.get(0)?,
                tvg_name: row.get(1)?,
                tvg_id: row.get(2)?,
                ..Channel::default()
            })
        })?;
        rows.collect()
    }
}

impl Store for RefreshStore {
    /// A storage error answers "none": the cost is one full download.
    fn validators(&mut self, url: &str) -> Validators {
        self.stored_validators(url).unwrap_or_default()
    }

    /// Best effort for the same reason.
    fn save_validators(&mut self, url: &str, validators: &Validators) {
        let _ = self.store_validators(url, validators);
    }

    /// The playlist's movies and episodes ([`crate::vod`]) are stored apart from its channels.
    fn save_playlist(
        &mut self,
        id: PlaylistId,
        mut playlist: Playlist,
    ) -> std::result::Result<Stored, StoreError> {
        let vod = crate::vod::split(&mut playlist.channels);
        let changed = self
            .storage
            .save_playlist(id, &playlist, (self.now)())
            .map_err(StoreError::new)?;
        let vod_changed = self.storage.save_vod(id, &vod).map_err(StoreError::new)?;
        if changed {
            self.filters.clear();
        }
        Ok(if changed || vod_changed {
            Stored::New {
                cache_validators: true,
            }
        } else {
            Stored::Same
        })
    }

    /// Asked once per refresh, before any guide is fetched: the moment to delete the guides
    /// that serve nothing any more (`Storage::delete_orphans`), off the UI's thread and before
    /// the imports count what the others hold. Best effort: the next refresh tries again.
    fn guide_urls(&mut self) -> Vec<String> {
        let _ = self.storage.delete_orphans();
        self.header_guides().unwrap_or_default()
    }

    /// A guide storage does not know yet (one the user added) becomes a manual guide for every
    /// playlist. A guide that serves no enabled playlist is not read at all.
    fn import_guide(
        &mut self,
        url: &str,
        body: &mut dyn Read,
    ) -> std::result::Result<Stored, StoreError> {
        let source = match self.guide(url).map_err(StoreError::new)? {
            Some(source) => source,
            None => {
                self.storage
                    .add_manual_guide(url, &[])
                    .map_err(StoreError::new)?;
                self.guide(url)
                    .map_err(StoreError::new)?
                    .ok_or_else(|| StoreError::new("the guide could not be added"))?
            }
        };
        let served = self.served(&source).map_err(StoreError::new)?;
        if served.is_empty() {
            return Ok(Stored::Same);
        }
        self.import(served, &source, body)
    }
}

/// FNV-1a over the sorted ids and names.
fn fingerprint(ids: &HashSet<String>, names: &HashSet<String>) -> i64 {
    let mut ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut names: Vec<&str> = names.iter().map(String::as_str).collect();
    ids.sort_unstable();
    names.sort_unstable();
    let mut bytes = Vec::new();
    for s in ids.iter().chain(&["\u{1}"]).chain(&names) {
        bytes.extend_from_slice(s.as_bytes());
        bytes.push(0);
    }
    fnv1a(&bytes) as i64
}
