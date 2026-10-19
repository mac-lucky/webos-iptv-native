//! The screens' [`TvData`] and the writes their commands make, over `iptv::storage` on the
//! television and over [`FakeData`] in the simulator's `--fake` mode.
//!
//! [`StoreData`] holds a [`Snapshot`] of the channels of every enabled playlist and their guide
//! matches, plus favourites, recents and history, and answers from memory; only programme
//! windows are read from SQLite, per query, through the snapshot's `ChannelKey -> Match` map.
//! After boot the refresher's thread reads each new snapshot once it stored new data, so a
//! reload on the UI thread only swaps it in. The group settings (`groups.show_*`,
//! `groups.hidden`, `groups.channel_sort`) shape [`TvData::groups`] and [`TvData::channels`],
//! whose sorted lists are kept per group until the data or a setting changes. A storage error is
//! logged and reads as empty: a broken database shows an empty guide, never a crash.

mod search;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iptv::epg::{Match, Matcher};
use iptv::model::{Channel, ChannelKey, Playlist, Programme, Timestamp};
use iptv::refresh::{self, Refresher, StoreError, Stored, Transport, Validators};
use iptv::storage::{RefreshStore, Storage};
use tv_screens::data::fake::FakeData;
use tv_screens::data::{GridProgramme, Group, HistoryEntry, Playback, SearchResult, TvData};
use tv_screens::lists::history::FINISHED_SLACK_SECS;
use tv_screens::panels::settings::{is_http_url, SPECS};
use tv_screens::panels::{keys, Settings};

use crate::logs::log;

/// What the shell reads and writes. Writes take effect in the reads at once.
pub trait Store: TvData {
    /// The stored settings.
    fn settings(&self) -> Settings;
    /// Store one setting; `settings` holds every current value.
    fn save_setting(&mut self, key: &str, settings: &Settings);
    fn toggle_favorite(&mut self, key: ChannelKey);
    fn remove_history(&mut self, key: ChannelKey, start: Timestamp);
    /// The channel has played long enough to count as watched.
    fn touch_recent(&mut self, key: ChannelKey);
    /// What [`TvData::playback`] answers; the shell keeps it current.
    fn playback_mut(&mut self) -> &mut Playback;
    /// Keep how far into `programme` on `channel` the viewer got, for History.
    fn save_progress(&mut self, channel: ChannelKey, programme: &Programme, position_secs: i64);
    /// The playlist or a guide changed on disk.
    fn reload(&mut self) {}
    /// The app's private data dir (flash, owner-only), where the database lives; `None` for data
    /// kept in memory.
    fn data_dir(&self) -> Option<&Path> {
        None
    }
    /// A background refresh of the playlist and its guides through `transport`, or `None`
    /// while no playlist URL is set (idle) or the data cannot be refreshed. `at_boot`: the
    /// app is starting, so `playlist.update_on_start` decides whether it refreshes at once.
    fn refresher(&self, _transport: Box<dyn Transport>, _at_boot: bool) -> Option<Refresher> {
        None
    }
}

pub use iptv::model::unix_now;

/// The TV's offset from UTC at `t`, from its time zone (`localtime_r`).
pub fn utc_offset_at(t: Timestamp) -> i32 {
    // The television's 32-bit time_t ends in 2038.
    let t = t as libc::time_t;
    // SAFETY: `tm` is a plain C struct for localtime_r to fill; both pointers outlive the call.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i32
    }
}

/// Storage in the app's data dir, or an empty database in memory when there is none.
pub fn open() -> StoreData {
    match data_dir() {
        Some(dir) => open_at(&dir),
        None => open_in_memory(),
    }
}

/// Storage in `dir`, or in memory when it cannot be opened there.
pub fn open_at(dir: &Path) -> StoreData {
    match Storage::open(dir) {
        Ok(storage) => {
            log(&format!("storage: {}", dir.display()));
            let mut data = StoreData::new(storage);
            data.dir = Some(dir.to_owned());
            data.search.set_dir(dir.to_owned());
            data.provision(dir);
            data
        }
        Err(e) => {
            log(&format!("storage: {}: {e}", dir.display()));
            open_in_memory()
        }
    }
}

/// What `cargo xtask provision` leaves in the data dir for the next start: one line per setting,
/// `playlist <url>` and/or `epg <url>`.
const PROVISION_FILE: &str = "provision.url";
/// The most a provisioned file may hold.
const PROVISION_MAX: u64 = 8192;

/// The settings a provisioned file sets: every non-empty line `playlist <url>` or `epg <url>`, at
/// most one of each, every URL http(s). `None` when any line is not that, or no line is there.
fn provisioned_urls(text: &str) -> Option<Vec<(&'static str, &str)>> {
    let mut urls = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (name, url) = line.split_once(' ')?;
        let key = match name {
            "playlist" => keys::PLAYLIST_URL,
            "epg" => keys::EPG_URL,
            _ => return None,
        };
        let url = url.trim();
        if !is_http_url(url) || urls.iter().any(|&(k, _)| k == key) {
            return None;
        }
        urls.push((key, url));
    }
    (!urls.is_empty()).then_some(urls)
}

fn open_in_memory() -> StoreData {
    log("storage: no writable data dir, nothing is kept");
    StoreData::new(Storage::open_in_memory().expect("SQLite opens in memory"))
}

/// What the refresher fetches, from the Settings. No `Debug`: the URLs carry tokens (their
/// secrets were registered for masking when the Settings took them).
pub fn refresh_config(s: &Settings) -> refresh::Config {
    let playlist = s.get(keys::PLAYLIST_URL);
    let guide = s.get(keys::EPG_URL);
    refresh::Config {
        playlist_url: playlist.to_owned(),
        extra_guides: (!guide.is_empty())
            .then(|| guide.to_owned())
            .into_iter()
            .collect(),
        interval: Some(Duration::from_secs(
            u64::from(s.number(keys::EPG_UPDATE_HOURS)) * 3600,
        )),
    }
}

/// The first data dir that exists or can be made owner-only: the user's Application Support
/// dir on the simulator, the private data candidates on the television.
fn data_dir() -> Option<PathBuf> {
    #[cfg(feature = "hostsim")]
    let candidates = std::env::var_os("HOME")
        .map(|home| {
            Path::new(&home)
                .join("Library/Application Support")
                .join(crate::NAME)
        })
        .into_iter();
    #[cfg(not(feature = "hostsim"))]
    let candidates = tv_base::paths::private_data_candidates().into_iter();
    candidates.into_iter().find(|dir| make_dir(dir))
}

/// `dir` as a [`tv_base::paths::private_dir`]: a real directory this uid owns, 0700. An existing
/// one that is a link or someone else's is refused, never chmod'ed or used.
fn make_dir(dir: &Path) -> bool {
    tv_base::paths::private_dir(dir, tv_base::paths::euid())
}

/// A storage result, or its default with the error logged.
fn or_log<T: Default>(what: &str, r: iptv::storage::Result<T>) -> T {
    r.unwrap_or_else(|e| {
        log(&format!("storage: {what}: {e}"));
        T::default()
    })
}

/// What a reload reads: every enabled playlist's channels, their guide matches and what sorting
/// and search need of them. `Send`, so the refresher's thread can read it ([`Snapshotting`]).
struct Snapshot {
    /// In playlist order.
    channels: Vec<Channel>,
    index: HashMap<ChannelKey, usize>,
    guides: HashMap<ChannelKey, Match>,
    /// Each channel's place in name order (case-insensitive, ties in playlist order).
    name_rank: Vec<u32>,
    search: search::Index,
}

impl Snapshot {
    fn read(storage: &Storage) -> Snapshot {
        let playlists = or_log("playlists", storage.playlists());
        let sources = or_log("guides", storage.epg_sources());
        let matcher = Matcher::new(&sources);
        let no_overrides = HashMap::new();
        let mut channels = Vec::new();
        let mut guides = HashMap::new();
        for p in playlists.iter().filter(|p| p.enabled) {
            let list = or_log("channels", storage.channels(p.id));
            let matches = matcher.match_playlist(p.id, &list, &no_overrides);
            for (c, m) in list.iter().zip(matches.channels) {
                if let Some(m) = m {
                    guides.insert(c.key, m);
                }
            }
            channels.extend(list);
        }
        let index = channels
            .iter()
            .enumerate()
            .map(|(i, c)| (c.key, i))
            .collect();
        let mut by_name: Vec<usize> = (0..channels.len()).collect();
        by_name.sort_by_cached_key(|&i| channels[i].name.to_lowercase());
        let mut name_rank = vec![0; channels.len()];
        for (rank, i) in by_name.into_iter().enumerate() {
            name_rank[i] = rank as u32;
        }
        let search = search::Index::new(&channels, &guides);
        Snapshot {
            channels,
            index,
            guides,
            name_rank,
            search,
        }
    }
}

/// Channels shared between the lists that show them.
pub type Channels = Rc<[Rc<Channel>]>;

/// A snapshot the refresher's thread read for the UI thread to take.
type NextSnapshot = Arc<Mutex<Option<Snapshot>>>;

/// The refresher's store: after a refresh that stored new data it reads the next [`Snapshot`]
/// too, on the refresher's thread, before the refresher announces the change, so the reload that
/// follows on the UI thread only swaps it in.
struct Snapshotting {
    store: RefreshStore,
    next: NextSnapshot,
}

impl refresh::Store for Snapshotting {
    fn validators(&mut self, url: &str) -> Validators {
        self.store.validators(url)
    }

    fn save_validators(&mut self, url: &str, validators: &Validators) {
        self.store.save_validators(url, validators);
    }

    fn save_playlist(&mut self, playlist: Playlist) -> Result<Stored, StoreError> {
        self.store.save_playlist(playlist)
    }

    fn guide_urls(&mut self) -> Vec<String> {
        self.store.guide_urls()
    }

    fn import_guide(&mut self, url: &str, body: &mut dyn Read) -> Result<Stored, StoreError> {
        self.store.import_guide(url, body)
    }

    fn refreshed(&mut self) {
        let snapshot = Snapshot::read(self.store.storage());
        *self.next.lock().unwrap_or_else(|e| e.into_inner()) = Some(snapshot);
    }
}

pub struct StoreData {
    storage: Storage,
    /// Where the database file is; `None` for one in memory.
    dir: Option<PathBuf>,
    /// The playlist URL was provisioned at this start: refresh at once.
    provisioned: bool,
    settings: Settings,
    /// Every enabled playlist's channels, in order: [`Group::All`] unsorted.
    channels: Channels,
    index: HashMap<ChannelKey, usize>,
    guides: HashMap<ChannelKey, Match>,
    name_rank: Vec<u32>,
    /// The groups' channel lists as [`TvData::channels`] made them, until something they show
    /// changes.
    lists: RefCell<Vec<(Group, Channels)>>,
    /// Filled by the refresher's thread, taken by [`Store::reload`].
    next: NextSnapshot,
    search: search::Search,
    favorites: Vec<ChannelKey>,
    recents: Vec<ChannelKey>,
    history: Vec<HistoryEntry>,
    playback: Playback,
}

impl StoreData {
    pub fn new(storage: Storage) -> Self {
        let settings = Settings::from_stored(
            SPECS
                .iter()
                .filter_map(|s| Some((s.key, storage.setting(s.key).ok()??))),
        );
        let mut data = StoreData {
            storage,
            dir: None,
            provisioned: false,
            settings,
            channels: Rc::new([]),
            index: HashMap::new(),
            guides: HashMap::new(),
            name_rank: Vec::new(),
            lists: RefCell::default(),
            next: NextSnapshot::default(),
            search: search::Search::default(),
            favorites: Vec::new(),
            recents: Vec::new(),
            history: Vec::new(),
            playback: Playback::default(),
        };
        data.reload();
        data
    }

    /// Take the URLs `cargo xtask provision` left in `dir`: store each as its setting with its
    /// secrets registered for masking, and delete the file whatever it held. A file with any bad
    /// line is refused whole. Only the fact is logged, never the value.
    fn provision(&mut self, dir: &Path) {
        let path = dir.join(PROVISION_FILE);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            return;
        };
        let text = (meta.is_file() && meta.len() <= PROVISION_MAX)
            .then(|| std::fs::read_to_string(&path).ok())
            .flatten();
        if let Err(e) = std::fs::remove_file(&path) {
            log(&format!("storage: {PROVISION_FILE} stays: {}", e.kind()));
        }
        let Some(urls) = text.as_deref().and_then(provisioned_urls) else {
            log(&format!(
                "storage: {PROVISION_FILE} refused: not `playlist|epg <http(s) URL>` lines"
            ));
            return;
        };
        let mut settings = self.settings.clone();
        for (key, url) in urls {
            if settings.set(key, url.to_owned()) || settings.get(key) == url {
                self.save_setting(key, &settings);
                self.provisioned = true;
                log(&format!("storage: provisioned {key}"));
            } else {
                log(&format!(
                    "storage: provisioned {key} refused by the settings"
                ));
            }
        }
    }

    fn load_recents(&mut self) {
        let recents = or_log("recents", self.storage.recent());
        self.recents = recents.into_iter().map(|(k, _)| k).collect();
    }

    fn load_history(&mut self) {
        let now = self.now();
        self.history = or_log("history", self.storage.history(now))
            .into_iter()
            .map(|e| HistoryEntry {
                channel: e.key,
                programme: Programme {
                    start: e.start,
                    stop: e.stop,
                    title: e.title,
                    description: None,
                    category: None,
                    icon: e.icon,
                },
                position_secs: i64::from(e.position_secs),
                watched_at: e.updated_at,
            })
            .collect();
        self.forget(&Group::History);
    }

    /// Take `snapshot` as the data every read answers from.
    fn install(&mut self, snapshot: Snapshot) {
        let Snapshot {
            channels,
            index,
            guides,
            name_rank,
            search,
        } = snapshot;
        self.channels = channels.into_iter().map(Rc::new).collect();
        self.index = index;
        self.guides = guides;
        self.name_rank = name_rank;
        self.search.set_index(search);
        self.lists.get_mut().clear();
    }

    /// `group`'s channels as [`TvData::channels`] lists them, made on first ask.
    pub fn list(&self, group: &Group) -> Channels {
        if let Some((_, list)) = self.lists.borrow().iter().find(|(g, _)| g == group) {
            return Rc::clone(list);
        }
        let list = self.make_list(group);
        self.lists
            .borrow_mut()
            .push((group.clone(), Rc::clone(&list)));
        list
    }

    /// A group's channels, ordered as `groups.channel_sort` asks.
    fn make_list(&self, group: &Group) -> Channels {
        let sort = self.settings.get(keys::GROUPS_CHANNEL_SORT);
        let all = || 0..self.channels.len();
        let mut picked: Vec<usize> = match group {
            Group::All if !matches!(sort, "number" | "name") => {
                return Rc::clone(&self.channels);
            }
            Group::All => all().collect(),
            Group::Favorites => self
                .favorites
                .iter()
                .filter_map(|k| self.index.get(k).copied())
                .collect(),
            Group::History => {
                let watched: HashSet<ChannelKey> = self.history.iter().map(|h| h.channel).collect();
                all()
                    .filter(|&i| watched.contains(&self.channels[i].key))
                    .collect()
            }
            Group::Playlist(name) => all()
                .filter(|&i| self.channels[i].groups.contains(name))
                .collect(),
        };
        match sort {
            "number" => picked.sort_by_key(|&i| self.channels[i].number.unwrap_or(u32::MAX)),
            "name" => picked.sort_by_key(|&i| self.name_rank[i]),
            _ => {}
        }
        picked
            .into_iter()
            .map(|i| Rc::clone(&self.channels[i]))
            .collect()
    }

    /// The kept list of `group` is out of date.
    fn forget(&mut self, group: &Group) {
        self.lists.get_mut().retain(|(g, _)| g != group);
    }
}

impl TvData for StoreData {
    fn now(&self) -> Timestamp {
        unix_now()
    }

    fn groups(&self) -> Vec<Group> {
        let s = &self.settings;
        let mut out: Vec<Group> = [
            (keys::GROUPS_SHOW_FAVORITES, Group::Favorites),
            (keys::GROUPS_SHOW_ALL, Group::All),
            (keys::GROUPS_SHOW_HISTORY, Group::History),
        ]
        .into_iter()
        .filter(|(key, _)| s.flag(key))
        .map(|(_, g)| g)
        .collect();
        let hidden: HashSet<&str> = s.hidden_groups().collect();
        let mut seen = HashSet::new();
        for name in self.channels.iter().flat_map(|c| &c.groups) {
            if !hidden.contains(name.as_str()) && seen.insert(name.as_str()) {
                out.push(Group::Playlist(name.clone()));
            }
        }
        out
    }

    fn programmes(&self, channel: ChannelKey, from: Timestamp, to: Timestamp) -> Vec<Programme> {
        match self.guides.get(&channel) {
            Some(m) => or_log("programmes", self.storage.programmes(m, from, to)),
            None => Vec::new(),
        }
    }

    fn is_favorite(&self, channel: ChannelKey) -> bool {
        self.favorites.contains(&channel)
    }

    fn recent_channels(&self) -> Vec<ChannelKey> {
        self.recents.clone()
    }

    fn history(&self) -> Vec<HistoryEntry> {
        self.history.clone()
    }

    fn playback(&self) -> Playback {
        self.playback.clone()
    }

    fn utc_offset_secs(&self) -> i32 {
        utc_offset_at(self.now())
    }

    fn past_days(&self) -> u32 {
        self.settings.number(keys::EPG_PAST_DAYS)
    }

    fn channels(&self, group: &Group) -> Channels {
        self.list(group)
    }

    fn channel(&self, key: ChannelKey) -> Option<Rc<Channel>> {
        self.index.get(&key).map(|&i| Rc::clone(&self.channels[i]))
    }

    fn grid_programmes(
        &self,
        channel: ChannelKey,
        from: Timestamp,
        to: Timestamp,
    ) -> Vec<GridProgramme> {
        match self.guides.get(&channel) {
            Some(m) => grid(or_log("grid", self.storage.grid_programmes(m, from, to))),
            None => Vec::new(),
        }
    }

    fn now_next(
        &self,
        channel: ChannelKey,
        now: Timestamp,
    ) -> (Option<Programme>, Option<Programme>) {
        match self.guides.get(&channel) {
            Some(m) => or_log("now/next", self.storage.now_next(m, now)),
            None => (None, None),
        }
    }

    fn programme_at(&self, channel: ChannelKey, start: Timestamp) -> Option<Programme> {
        let m = self.guides.get(&channel)?;
        or_log("programme", self.storage.programme_at(m, start))
    }

    /// The matched channels' rows in one read snapshot; an unmatched channel's row is empty.
    fn guide_window(
        &self,
        channels: &[ChannelKey],
        from: Timestamp,
        to: Timestamp,
    ) -> Vec<Vec<GridProgramme>> {
        let matched: Vec<(usize, &Match)> = channels
            .iter()
            .enumerate()
            .filter_map(|(i, k)| Some((i, self.guides.get(k)?)))
            .collect();
        let guides: Vec<&Match> = matched.iter().map(|&(_, m)| m).collect();
        let mut rows = vec![Vec::new(); channels.len()];
        let read = or_log("window", self.storage.guide_window(&guides, from, to));
        for ((i, _), row) in matched.into_iter().zip(read) {
            rows[i] = grid(row);
        }
        rows
    }

    /// On the search worker ([`search`]), with `epg.past_days` bounding the archive too.
    fn search(&self, query: &str) -> Option<SearchResult> {
        self.search
            .ask(query, self.now(), self.past_days(), &self.storage)
    }
}

/// Storage's grid rows (start, stop and title read) as the screens take them.
fn grid(programmes: Vec<Programme>) -> Vec<GridProgramme> {
    programmes.into_iter().map(GridProgramme::from).collect()
}

impl Store for StoreData {
    fn settings(&self) -> Settings {
        self.settings.clone()
    }

    fn save_setting(&mut self, key: &str, settings: &Settings) {
        or_log("setting", self.storage.set_setting(key, settings.get(key)));
        // The playlist row and the manual guides follow the URLs, so an old token is not kept
        // (a cleared URL is stored as "" for the same reason).
        if matches!(key, keys::PLAYLIST_URL | keys::EPG_URL) {
            let config = refresh_config(settings);
            if let Some(p) = or_log("playlists", self.storage.playlists()).first() {
                let (url, guides) = (&config.playlist_url, &config.extra_guides);
                or_log("sources", self.storage.set_sources(p.id, url, guides));
            }
        }
        self.settings = settings.clone();
        if key.starts_with("groups.") {
            self.lists.get_mut().clear();
        }
    }

    fn toggle_favorite(&mut self, key: ChannelKey) {
        or_log("favourite", self.storage.toggle_favourite(key));
        self.favorites = or_log("favourites", self.storage.favourites());
        self.forget(&Group::Favorites);
    }

    fn remove_history(&mut self, key: ChannelKey, start: Timestamp) {
        or_log("history", self.storage.remove_progress(key, start));
        self.load_history();
    }

    fn touch_recent(&mut self, key: ChannelKey) {
        or_log("recent", self.storage.touch_recent(key, unix_now()));
        self.load_recents();
    }

    fn playback_mut(&mut self) -> &mut Playback {
        &mut self.playback
    }

    fn save_progress(&mut self, key: ChannelKey, p: &Programme, position_secs: i64) {
        let duration = p.duration_secs().max(0);
        let position = position_secs.clamp(0, duration);
        let now = unix_now();
        let entry = iptv::storage::HistoryEntry {
            key,
            start: p.start,
            stop: p.stop,
            title: p.title.clone(),
            icon: p.icon.clone(),
            position_secs: u32::try_from(position).unwrap_or(u32::MAX),
            duration_secs: u32::try_from(duration).unwrap_or(u32::MAX),
            completed: position >= duration - FINISHED_SLACK_SECS,
            updated_at: now,
        };
        or_log("progress", self.storage.save_progress(&entry, now));
        self.load_history();
    }

    /// Swaps in the snapshot the refresher's thread read, or reads one here (at boot, or after a
    /// change the refresher did not make).
    fn reload(&mut self) {
        let next = self.next.lock().unwrap_or_else(|e| e.into_inner()).take();
        let snapshot = next.unwrap_or_else(|| Snapshot::read(&self.storage));
        self.install(snapshot);
        self.favorites = or_log("favourites", self.storage.favourites());
        self.load_recents();
        self.load_history();
    }

    fn data_dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Into this database over a connection of the refresher's own. The Settings URL fills the
    /// first playlist, which is made on first use; a database in memory is never refreshed. At
    /// boot with `playlist.update_on_start` off it waits for a request or its interval, unless
    /// nothing was ever fetched.
    fn refresher(&self, transport: Box<dyn Transport>, at_boot: bool) -> Option<Refresher> {
        let config = refresh_config(&self.settings);
        if config.playlist_url.is_empty() {
            return None;
        }
        let storage = match Storage::open(self.dir.as_deref()?) {
            Ok(s) => s,
            Err(e) => {
                log(&format!("refresh: storage: {e}"));
                return None;
            }
        };
        let playlist = match or_log("playlists", self.storage.playlists()).first() {
            Some(p) => p.id,
            None => match self.storage.add_playlist("Playlist", &config.playlist_url) {
                Ok(id) => id,
                Err(e) => {
                    log(&format!("refresh: playlist: {e}"));
                    return None;
                }
            },
        };
        // A row a build before the URL sync wrote keeps the URL it was made with.
        let (url, guides) = (&config.playlist_url, &config.extra_guides);
        or_log("sources", self.storage.set_sources(playlist, url, guides));
        let store = Box::new(Snapshotting {
            store: RefreshStore::new(storage, playlist),
            next: Arc::clone(&self.next),
        });
        let now = !at_boot
            || self.provisioned
            || self.settings.flag(keys::PLAYLIST_UPDATE_ON_START)
            || self.channels.is_empty();
        if now {
            Refresher::start(transport, store, config)
        } else {
            Refresher::start_idle(transport, store, config)
        }
    }
}

/// The simulator's synthetic guide; writes stay in memory and settings are not kept.
impl Store for FakeData {
    fn settings(&self) -> Settings {
        Settings::new()
    }

    fn save_setting(&mut self, _key: &str, _settings: &Settings) {}

    fn toggle_favorite(&mut self, key: ChannelKey) {
        match self.favorites.iter().position(|&k| k == key) {
            Some(i) => {
                self.favorites.remove(i);
            }
            None => self.favorites.push(key),
        }
    }

    fn remove_history(&mut self, key: ChannelKey, start: Timestamp) {
        self.history
            .retain(|h| h.channel != key || h.programme.start != start);
    }

    fn touch_recent(&mut self, key: ChannelKey) {
        self.recents.retain(|&k| k != key);
        self.recents.insert(0, key);
        self.recents.truncate(10);
    }

    fn playback_mut(&mut self) -> &mut Playback {
        &mut self.playback
    }

    /// Storage's rule without its expiry: under 15 s in and not finished starts over.
    fn save_progress(&mut self, channel: ChannelKey, p: &Programme, position: i64) {
        self.history
            .retain(|h| h.channel != channel || h.programme.start != p.start);
        if position >= 15 || position >= p.duration_secs() - FINISHED_SLACK_SECS {
            self.history.insert(
                0,
                HistoryEntry {
                    channel,
                    programme: p.clone(),
                    position_secs: position,
                    watched_at: self.now,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iptv::model::Playlist;

    fn channel(name: &str, group: &str, number: u32) -> Channel {
        let url = format!("http://host/{name}/index.m3u8");
        Channel {
            key: ChannelKey::of_url(&url),
            name: name.into(),
            number: Some(number),
            logo: None,
            groups: vec![group.into()],
            url,
            tvg_id: None,
            tvg_name: None,
            tvg_shift_minutes: 0,
            catchup: Default::default(),
            radio: false,
            http_headers: vec![],
            extra: vec![],
        }
    }

    fn store() -> StoreData {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage.add_playlist("p", "http://host/list.m3u").unwrap();
        let playlist = Playlist {
            channels: vec![
                channel("Beta", "News", 3),
                channel("alpha", "Sport", 1),
                channel("Gamma", "News", 2),
            ],
            ..Playlist::default()
        };
        assert!(storage.save_playlist(id, &playlist, 1_700_000_000).unwrap());
        StoreData::new(storage)
    }

    fn names(list: Channels) -> Vec<String> {
        list.iter().map(|c| c.name.clone()).collect()
    }

    #[test]
    fn groups_follow_the_playlist_and_the_group_settings() {
        let mut s = store();
        let news = Group::Playlist("News".into());
        assert_eq!(
            s.groups(),
            [
                Group::Favorites,
                Group::All,
                Group::History,
                news.clone(),
                Group::Playlist("Sport".into())
            ]
        );
        assert_eq!(names(s.channels(&news)), ["Beta", "Gamma"]);
        let mut settings = s.settings();
        settings.set(keys::GROUPS_HIDDEN, "Sport".into());
        settings.set(keys::GROUPS_SHOW_HISTORY, "false".into());
        settings.set(keys::GROUPS_CHANNEL_SORT, "name".into());
        for key in [
            keys::GROUPS_HIDDEN,
            keys::GROUPS_SHOW_HISTORY,
            keys::GROUPS_CHANNEL_SORT,
        ] {
            s.save_setting(key, &settings);
        }
        assert_eq!(s.groups(), [Group::Favorites, Group::All, news]);
        assert_eq!(names(s.channels(&Group::All)), ["alpha", "Beta", "Gamma"]);
        let stored = s.storage.setting(keys::GROUPS_HIDDEN).unwrap();
        assert_eq!(stored.as_deref(), Some("Sport"), "settings are stored");
        settings.set(keys::GROUPS_CHANNEL_SORT, "number".into());
        s.save_setting(keys::GROUPS_CHANNEL_SORT, &settings);
        assert_eq!(names(s.channels(&Group::All)), ["alpha", "Gamma", "Beta"]);
    }

    #[test]
    fn writes_show_in_the_reads() {
        let mut s = store();
        let beta = s.channels[0].key;
        let gamma = s.channels[2].key;
        s.toggle_favorite(gamma);
        s.toggle_favorite(beta);
        assert!(s.is_favorite(beta));
        assert_eq!(names(s.channels(&Group::Favorites)), ["Gamma", "Beta"]);
        s.toggle_favorite(gamma);
        assert_eq!(names(s.channels(&Group::Favorites)), ["Beta"]);
        s.touch_recent(beta);
        s.touch_recent(gamma);
        assert_eq!(s.recent_channels(), [gamma, beta]);
        assert_eq!(s.channel(beta).map(|c| c.name.clone()), Some("Beta".into()));
        assert_eq!(s.programmes(beta, 0, i64::MAX), [], "no guide matched");
    }

    /// Serves a two-channel playlist at any URL, counting the requests.
    #[derive(Clone, Default)]
    struct Playlist2(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl Playlist2 {
        fn gets(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Transport for Playlist2 {
        fn get(
            &self,
            _url: &str,
            _headers: &[String],
            _budget: std::time::Duration,
            _cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) -> Result<refresh::Response, refresh::NetError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let m3u = "#EXTM3U\n#EXTINF:-1 group-title=\"News\",One\nhttp://host/1.m3u8\n\
                       #EXTINF:-1 group-title=\"News\",Two\nhttp://host/2.m3u8\n";
            Ok(refresh::Response {
                status: 200,
                validators: Default::default(),
                location: None,
                body: Box::new(std::io::Cursor::new(m3u.as_bytes().to_vec())),
            })
        }
    }

    /// Wait up to 10 s for `done`.
    fn until(done: impl Fn() -> bool) {
        let start = std::time::Instant::now();
        while !done() && start.elapsed().as_secs() < 10 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn the_refresher_fills_the_playlist_the_settings_name() {
        let dir = std::env::temp_dir().join(format!("webosiptv-store-{}", std::process::id()));
        assert!(make_dir(&dir));
        let net = Playlist2::default();
        let mut idle = StoreData::new(Storage::open(&dir).unwrap());
        idle.dir = Some(dir.clone());
        let no_url = idle.refresher(Box::new(net.clone()), true);
        assert!(no_url.is_none(), "no URL: idle");
        drop(idle);
        let storage = Storage::open(&dir).unwrap();
        storage
            .set_setting(keys::PLAYLIST_URL, "http://host/list.m3u")
            .unwrap();
        storage
            .set_setting(keys::PLAYLIST_UPDATE_ON_START, "false")
            .unwrap();
        let mut data = StoreData::new(storage);
        let in_memory = data.refresher(Box::new(net.clone()), true);
        assert!(in_memory.is_none(), "in memory: never");
        data.dir = Some(dir.clone());
        // Nothing fetched yet: it refreshes at once even with update on start off.
        let refresher = data.refresher(Box::new(net.clone()), true).unwrap();
        until(|| refresher.generation() > 0);
        drop(refresher);
        let next = data.next.lock().unwrap().as_ref().map(|s| s.channels.len());
        assert_eq!(next, Some(2), "the refresher's thread read the snapshot");
        data.reload();
        assert!(
            data.next.lock().unwrap().is_none(),
            "and the reload took it"
        );
        assert_eq!(names(data.channels(&Group::All)), ["One", "Two"]);
        assert_eq!(data.storage.playlists().unwrap().len(), 1);
        // A new URL replaces the old one (and its token) in the playlist row at once.
        let mut settings = data.settings();
        let new_url = "http://b.example/y.m3u";
        assert!(settings.set(keys::PLAYLIST_URL, new_url.into()));
        data.save_setting(keys::PLAYLIST_URL, &settings);
        assert_eq!(data.storage.playlists().unwrap()[0].url, new_url);
        assert!(settings.set(keys::PLAYLIST_URL, String::new()));
        data.save_setting(keys::PLAYLIST_URL, &settings);
        assert_eq!(
            data.storage.playlists().unwrap()[0].url,
            "",
            "cleared, not kept"
        );
        assert!(settings.set(keys::PLAYLIST_URL, "http://host/list.m3u".into()));
        data.save_setting(keys::PLAYLIST_URL, &settings);
        // Now with channels stored, a boot waits for a request.
        let gets = net.gets();
        let refresher = data.refresher(Box::new(net.clone()), true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(net.gets(), gets, "update on start is off");
        refresher.refresh_now();
        until(|| net.gets() > gets);
        assert!(net.gets() > gets);
        drop(refresher);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provisioned_urls_become_their_settings_and_the_file_goes() {
        let dir = std::env::temp_dir().join(format!("webosiptv-prov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(make_dir(&dir));
        let file = dir.join(PROVISION_FILE);
        let url = format!("https://iptv.example/{}/list.m3u", "k".repeat(13));
        std::fs::write(&file, format!("playlist {url}\n")).unwrap();
        let data = open_at(&dir);
        assert_eq!(data.settings().get(keys::PLAYLIST_URL), url);
        assert_eq!(
            data.settings().get(keys::EPG_URL),
            "",
            "a line not sent changes nothing"
        );
        assert!(data.provisioned && !file.exists());
        drop(data);
        let reopened = open_at(&dir);
        assert_eq!(reopened.settings().get(keys::PLAYLIST_URL), url, "stored");
        assert!(!reopened.provisioned, "only the start that took it");
        drop(reopened);
        let guide = "https://guide.example/epg.xml.gz";
        std::fs::write(&file, format!("\nepg {guide}\n")).unwrap();
        let data = open_at(&dir);
        assert_eq!(data.settings().get(keys::EPG_URL), guide);
        assert_eq!(
            data.settings().get(keys::PLAYLIST_URL),
            url,
            "the playlist stays"
        );
        assert!(data.provisioned && !file.exists());
        drop(data);
        for bad in [
            "playlist ftp://iptv.example/x",
            "",
            "https://a.example/x",
            "playlist https://a.example/x\nplaylist https://b.example/y",
            "playlist https://a.example/x\nepg https://b.example/y z",
            "playlist https://a.example/x\nlogo https://b.example/y",
        ] {
            std::fs::write(&file, bad).unwrap();
            let data = open_at(&dir);
            assert_eq!(
                data.settings().get(keys::PLAYLIST_URL),
                url,
                "{bad:?} refused"
            );
            assert_eq!(
                data.settings().get(keys::EPG_URL),
                guide,
                "{bad:?} refused whole"
            );
            assert!(!file.exists(), "a refused file is deleted too");
        }
        std::os::unix::fs::symlink(dir.join("elsewhere"), &file).unwrap();
        std::fs::write(dir.join("elsewhere"), "playlist https://other.example/x").unwrap();
        let data = open_at(&dir);
        assert_eq!(
            data.settings().get(keys::PLAYLIST_URL),
            url,
            "a link is not followed"
        );
        assert!(std::fs::symlink_metadata(&file).is_err());
        drop(data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_is_kept_past_fifteen_seconds_and_shows_in_history() {
        let mut s = store();
        let key = s.channels[0].key;
        let now = unix_now();
        let programme = Programme {
            start: now - 7_200,
            stop: now - 3_600,
            title: "Earlier".into(),
            ..Programme::default()
        };
        s.save_progress(key, &programme, 10);
        assert_eq!(s.history(), [], "under the minimum: start over");
        s.save_progress(key, &programme, 600);
        let h = s.history();
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].channel, h[0].position_secs), (key, 600));
        assert_eq!(h[0].programme.title, programme.title);
        assert_eq!(names(s.channels(&Group::History)), ["Beta"]);
    }

    #[test]
    fn a_group_list_is_kept_until_what_it_shows_changes() {
        let mut s = store();
        let all = s.list(&Group::All);
        assert!(Rc::ptr_eq(&all, &s.list(&Group::All)), "kept");
        assert!(
            Rc::ptr_eq(&all, &s.channels),
            "unsorted All is the channels"
        );
        let beta = s.channels[0].key;
        let favourites = s.list(&Group::Favorites);
        assert!(favourites.is_empty());
        s.toggle_favorite(beta);
        assert_eq!(
            s.list(&Group::Favorites).len(),
            1,
            "a favourite change shows"
        );
        let mut settings = s.settings();
        settings.set(keys::GROUPS_CHANNEL_SORT, "name".into());
        s.save_setting(keys::GROUPS_CHANNEL_SORT, &settings);
        let sorted = s.list(&Group::All);
        assert_eq!(names(s.channels(&Group::All)), ["alpha", "Beta", "Gamma"]);
        assert!(Rc::ptr_eq(&sorted[1], &s.channels[0]), "shared, not copied");
        s.reload();
        assert!(
            !Rc::ptr_eq(&sorted, &s.list(&Group::All)),
            "a reload drops it"
        );
    }

    /// XMLTV's `YYYYMMDDHHMMSS +0000`.
    fn stamp(t: Timestamp) -> String {
        let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
        let (year, month, day) = tv_base::time::civil_from_days(days);
        let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
        format!("{year:04}{month:02}{day:02}{h:02}{m:02}{s:02} +0000")
    }

    const NEWS: &str = "Wiadomo\u{15b}ci";

    /// Three channels on two guide channels: `News` and `Sport` on `a` (News with a 3-day
    /// archive), `Other` on `b`, with programmes around `now`.
    fn guided(storage: Storage, now: Timestamp) -> StoreData {
        let id = storage.add_playlist("p", "http://host/list.m3u").unwrap();
        let mut news = channel("News", "News", 1);
        news.tvg_id = Some("a".into());
        news.catchup = iptv::model::Catchup {
            mode: Some(iptv::model::CatchupMode::Shift),
            days: 3,
            source: Some(String::new()),
        };
        let mut sport = channel("Sport", "Sport", 2);
        sport.tvg_id = Some("a".into());
        let mut other = channel("Other", "News", 3);
        other.tvg_id = Some("b".into());
        let playlist = Playlist {
            channels: vec![news, sport, other],
            ..Playlist::default()
        };
        storage.save_playlist(id, &playlist, now).unwrap();
        let source = storage
            .add_manual_guide("http://g.example/x.xml", &[])
            .unwrap();
        let programmes = [
            ("a", now - 7200, now - 3600, NEWS),
            ("a", now - 600, now + 1200, NEWS),
            ("a", now + 1200, now + 3000, "Pogoda"),
            ("a", now + 3 * 86_400, now + 3 * 86_400 + 1800, NEWS),
            ("b", now - 7200, now - 3600, NEWS),
            ("b", now + 3600, now + 5400, "wiadomosci"),
        ];
        let mut xml = String::from(
            r#"<tv><channel id="a"><display-name>A</display-name></channel><channel id="b"><display-name>B</display-name></channel>"#,
        );
        for (c, start, stop, title) in programmes {
            xml += &format!(
                r#"<programme channel="{c}" start="{}" stop="{}"><title>{title}</title></programme>"#,
                stamp(start),
                stamp(stop)
            );
        }
        xml += "</tv>";
        let options = iptv::xmltv::Options::new(now);
        storage
            .import_guide(source, xml.as_bytes(), &options)
            .unwrap();
        StoreData::new(storage)
    }

    fn hits(found: &SearchResult) -> Vec<(String, Timestamp)> {
        found
            .hits
            .iter()
            .map(|(_, p)| (p.title.clone(), p.start))
            .collect()
    }

    #[test]
    fn guide_reads_go_through_the_matches() {
        let now = unix_now();
        let s = guided(Storage::open_in_memory().unwrap(), now);
        let [news, sport, other] = [0, 1, 2].map(|i| s.channels[i].key);
        let grid = s.grid_programmes(news, now - 60, now + 3600);
        let titles: Vec<&str> = grid.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, [NEWS, "Pogoda"]);
        let (on, next) = s.now_next(sport, now);
        assert_eq!(on.map(|p| p.start), Some(now - 600));
        assert_eq!(next.map(|p| p.title), Some("Pogoda".into()));
        let at = s.programme_at(other, now + 3600).unwrap();
        assert_eq!(at.title, "wiadomosci");
        let rows = s.guide_window(&[other, ChannelKey(7), news], now, now + 3700);
        assert_eq!(rows.iter().map(Vec::len).collect::<Vec<_>>(), [1, 0, 2]);
    }

    #[test]
    fn search_lists_on_air_then_the_archive_then_what_is_next() {
        let now = unix_now();
        let s = guided(Storage::open_in_memory().unwrap(), now);
        assert_eq!(s.search("  ").map(|f| f.hits.len()), Some(0));
        // In memory there is no second connection: the answer is at once.
        let found = s.search("WIADOMOSCI").unwrap();
        assert_eq!(found.query, "wiadomosci");
        let keys: Vec<ChannelKey> = found.hits.iter().map(|(k, _)| *k).collect();
        let [news, sport, other] = [0, 1, 2].map(|i| s.channels[i].key);
        // On air on both channels of `a` in playlist order; the past one only where an archive
        // keeps it; the one three days ahead is past the search window.
        assert_eq!(keys, [news, sport, news, other]);
        assert_eq!(
            hits(&found)[2..],
            [
                (NEWS.to_owned(), now - 7200),
                ("wiadomosci".into(), now + 3600)
            ]
        );
        assert!(!found.capped);
    }

    #[test]
    fn search_runs_on_a_worker_for_a_database_on_disk() {
        let dir = std::env::temp_dir().join(format!("webosiptv-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(make_dir(&dir));
        let now = unix_now();
        let mut s = guided(Storage::open(&dir).unwrap(), now);
        s.search.set_dir(dir.clone());
        assert_eq!(s.search("pogoda"), None, "the worker looks");
        // Asked again on each tick until the answer is in.
        until(|| s.search("pogoda").is_some());
        let found = s.search("pogoda").unwrap();
        let pogoda = ("Pogoda".to_owned(), now + 1200);
        assert_eq!(
            hits(&found),
            [pogoda.clone(), pogoda],
            "on both channels of `a`"
        );
        // A new query replaces the answer; one typed over is never shown.
        assert_eq!(s.search("pog"), None);
        assert_eq!(s.search("wiad"), None);
        until(|| s.search("wiad").is_some());
        assert_eq!(s.search("wiad").unwrap().hits.len(), 4);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_offset_is_the_time_zones() {
        // Whatever zone the host runs in, it is whole quarter hours within a day.
        let off = utc_offset_at(1_700_000_000);
        assert!(off.abs() < 86_400 && off % 900 == 0, "{off}");
    }
}
