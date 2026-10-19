//! Settings in the running app: where the panel opens, that every value is stored and read back
//! after a restart, and that each one changes what it is for without a restart (the two start
//! rows change the next start, as their labels say).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iptv::model::Playlist;
use iptv::storage::Storage;
use tv_screens::panels::settings::{Kind, Spec, SPECS};
use tv_screens::player::PlayerSettings;

use super::*;
use crate::store::unix_now;

/// Real storage in a temp dir of its own, holding the 12 synthetic channels as its playlist;
/// removed when dropped.
struct Disk(std::path::PathBuf);

impl Disk {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("webosiptv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let storage = Storage::open(&dir).unwrap();
        let id = storage.add_playlist("p", PLAYLIST).unwrap();
        let playlist = Playlist {
            channels: FakeData::new(NOW, 12).channels,
            ..Playlist::default()
        };
        assert!(storage.save_playlist(id, &playlist, NOW).unwrap());
        Disk(dir)
    }

    /// A started shell over what is on disk now, without a network.
    fn sim(&self) -> Sim {
        self.sim_with(None)
    }

    fn sim_with(&self, net: Option<NetMaker>) -> Sim {
        locale();
        let calls = Calls::default();
        let mut shell = Shell::new(
            Box::new(crate::store::open_at(&self.0)),
            Box::new(calls.clone()),
            net,
            LogoCache::new(None),
        );
        shell.start(0);
        Sim {
            shell,
            calls,
            seen: Rc::default(),
            t: 1_000,
        }
    }

    fn storage(&self) -> Storage {
        Storage::open(&self.0).unwrap()
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Sim {
    /// What the Settings panel sends for a change.
    fn set(&mut self, key: &str, value: &str) {
        let (key, value) = (key.to_owned(), value.to_owned());
        self.shell.run(Command::SetSetting { key, value }, self.t);
    }

    /// The player reports `secs` into the archive programme playing, and the viewer goes back
    /// to live: what History keeps of it.
    fn leave_archive_at(&mut self, secs: i64) {
        self.shell.store.playback_mut().position_secs = secs;
        self.shell.run(Command::ReturnToLive, self.t);
    }
}

/// Serves the one-channel playlist at any URL and notes every URL asked for.
#[derive(Clone, Default)]
struct Asked(Arc<Mutex<Vec<String>>>);

impl Asked {
    fn net(&self) -> NetMaker {
        let asked = self.clone();
        Box::new(move || Box::new(asked.clone()))
    }

    fn urls(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    /// Wait up to 10 s for `url` to be asked for.
    fn wait_for(&self, url: &str) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if self.urls().iter().any(|u| u == url) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

impl Transport for Asked {
    fn get(
        &self,
        url: &str,
        headers: &[String],
        budget: Duration,
        cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<iptv::refresh::Response, iptv::refresh::NetError> {
        self.0.lock().unwrap().push(url.to_owned());
        OneChannel.get(url, headers, budget, cancel)
    }
}

const PLAYLIST: &str = "http://playlist.test/list.m3u";

/// A value `spec` accepts other than its default.
fn other_value(spec: &Spec) -> String {
    match spec.kind {
        Kind::Toggle if spec.default == "true" => "false".into(),
        Kind::Toggle => "true".into(),
        Kind::Choice(options) => options
            .iter()
            .map(|(v, _)| *v)
            .find(|v| *v != spec.default)
            .unwrap()
            .into(),
        Kind::Url { .. } => format!("http://{}.test/source", spec.key.replace('.', "-")),
        Kind::List => "News\nSport".into(),
    }
}

/// A programme of `title` that ended `days_ago` days before now.
fn aired(title: &str, days_ago: i64) -> Programme {
    let stop = unix_now() - days_ago * 86_400 - 600;
    Programme {
        start: stop - 1_800,
        stop,
        title: title.into(),
        ..Programme::default()
    }
}

/// What History holds, by title (saves within one second have no order of their own).
fn history_titles(s: &Sim) -> Vec<String> {
    let history = s.shell.store.history();
    let mut titles: Vec<String> = history.into_iter().map(|h| h.programme.title).collect();
    titles.sort();
    titles
}

#[test]
fn every_setting_survives_a_restart() {
    let disk = Disk::new("settings-restart");
    let mut s = disk.sim();
    let mut changed = Vec::new();
    for spec in SPECS {
        let key = spec.key;
        assert_eq!(s.shell.settings.get(key), spec.default, "{key}: default");
        let value = other_value(spec);
        s.set(key, &value);
        assert_eq!(s.shell.settings.get(key), value, "{key}: changed");
        assert_eq!(
            s.shell.store.settings().get(key),
            value,
            "{key}: in the store"
        );
        changed.push((key, value));
    }
    drop(s);
    let storage = disk.storage();
    for (key, value) in &changed {
        let stored = storage.setting(key).unwrap();
        assert_eq!(stored.as_deref(), Some(value.as_str()), "{key}: on disk");
    }
    drop(storage);
    let s = disk.sim();
    for (key, value) in &changed {
        assert_eq!(s.shell.settings.get(key), value, "{key}: read back");
    }
    assert_eq!(
        s.shell.remote.long_ms,
        s.shell.settings.number(keys::LONG_PRESS_MS)
    );
    assert_eq!(
        s.shell.bottom.screen.settings,
        PlayerSettings::from_settings(&s.shell.settings),
        "the restarted player follows the stored values"
    );
}

/// The test of each setting's effect: a new setting must come with one.
fn effect_test(key: &str) -> Option<fn()> {
    Some(match key {
        keys::LAST_CHANNEL_ON_START => the_last_channel_plays_on_start_when_the_setting_asks,
        keys::HISTORY_MIN_SECS => history_keeps_a_programme_from_the_minimum_asked_for,
        keys::HISTORY_KEEP_DAYS => a_shorter_history_keep_drops_old_entries_at_once,
        keys::PLAYLIST_URL => {
            super::the_refresher_waits_for_a_playlist_url_and_stops_when_it_is_cleared
        }
        keys::PLAYLIST_UPDATE_ON_START => update_on_start_decides_whether_a_start_fetches,
        keys::EPG_URL | keys::EPG_UPDATE_HOURS => {
            the_epg_source_and_interval_reach_the_running_refresher
        }
        keys::EPG_PAST_DAYS => past_days_bound_the_guide_at_once,
        keys::PLAYER_INFO_ON_ZAP
        | keys::PLAYER_INFO_SECS
        | keys::SKIP_BACK_SECS
        | keys::SKIP_FORWARD_SECS => the_player_settings_reach_the_player_at_once,
        keys::GROUPS_SHOW_FAVORITES
        | keys::GROUPS_SHOW_ALL
        | keys::GROUPS_SHOW_HISTORY
        | keys::GROUPS_HIDDEN
        | keys::GROUPS_CHANNEL_SORT => the_group_settings_reshape_the_groups_at_once,
        keys::LONG_PRESS_MS => the_long_press_threshold_follows_its_setting,
        keys::SUBTITLES_SHOW | keys::SUBTITLES_SIZE => {
            the_subtitle_settings_reach_the_overlay_at_once
        }
        keys::ACCENT_COLOR => super::accent::the_accent_colour_applies_at_once,
        keys::PLAYER_DISPLAY_MODE | keys::AUDIO_OFFSET_MS | keys::CHANNEL_AUDIO_OFFSETS => {
            super::playback::the_display_mode_and_the_audio_offsets_reach_the_player_at_once
        }
        keys::PARENTAL_PIN => super::userfeat::the_pin_can_be_changed_and_is_kept_as_a_verifier,
        keys::PARENTAL_CHANNELS => super::userfeat::a_locked_channel_plays_only_after_the_pin,
        keys::PARENTAL_GROUPS => super::userfeat::a_locked_group_opens_with_the_pin,
        keys::NAMES_PREFIXES | keys::NAMES_SUFFIXES => {
            super::userfeat::name_rules_clean_the_names_at_once
        }
        keys::REMINDERS => super::userfeat::a_reminder_prompts_at_its_start_and_ok_switches,
        _ => return None,
    })
}

#[test]
fn every_setting_has_a_test_of_its_effect() {
    for spec in SPECS {
        assert!(
            effect_test(spec.key).is_some(),
            "{}: no effect test",
            spec.key
        );
    }
}

#[test]
fn the_last_channel_plays_on_start_when_the_setting_asks() {
    let disk = Disk::new("last-channel");
    let mut s = disk.sim();
    assert_eq!(
        (s.open(), s.playing().channel),
        (1, None),
        "nothing watched yet: the guide"
    );
    let watched = s.key(4);
    s.live(4);
    s.shell.navigate(Navigate::Player, None);
    s.run(6_000);
    assert_eq!(
        s.shell.store.recent_channels(),
        [watched],
        "watched long enough"
    );
    s.set(keys::LAST_CHANNEL_ON_START, "false");
    drop(s);
    let mut s = disk.sim();
    assert_eq!(
        (s.open(), s.playing().channel),
        (1, None),
        "off: the guide, nothing plays"
    );
    s.set(keys::LAST_CHANNEL_ON_START, "true");
    drop(s);
    let mut s = disk.sim();
    assert_eq!(s.open(), 0, "on: full screen");
    assert_eq!(s.playing().channel, Some(watched));
    assert_eq!(s.calls.take(), ["open live Channel 5 None"]);
}

#[test]
fn history_keeps_a_programme_from_the_minimum_asked_for() {
    let disk = Disk::new("history-min");
    let mut s = disk.sim();
    let channel = s.key(0);
    s.catchup(channel, &aired("Ten seconds", 0), 0);
    s.leave_archive_at(10);
    assert_eq!(history_titles(&s), ["Ten seconds"], "10 s by default");
    s.set(keys::HISTORY_MIN_SECS, "30");
    s.catchup(channel, &aired("Twenty seconds", 1), 0);
    s.leave_archive_at(20);
    s.catchup(channel, &aired("Thirty seconds", 2), 0);
    s.leave_archive_at(30);
    assert_eq!(history_titles(&s), ["Ten seconds", "Thirty seconds"]);
}

#[test]
fn a_shorter_history_keep_drops_old_entries_at_once() {
    let disk = Disk::new("history-keep");
    let mut s = disk.sim();
    let channel = s.key(0);
    for (title, days_ago) in [("Three days ago", 3), ("Today", 0)] {
        s.catchup(channel, &aired(title, days_ago), 0);
        s.leave_archive_at(600);
    }
    assert_eq!(history_titles(&s), ["Three days ago", "Today"]);
    s.shell.navigate(Navigate::History, None);
    s.set(keys::HISTORY_KEEP_DAYS, "1");
    assert_eq!(history_titles(&s), ["Today"]);
    s.set(keys::HISTORY_KEEP_DAYS, "14");
    assert_eq!(
        history_titles(&s),
        ["Three days ago", "Today"],
        "kept rows come back"
    );
}

#[test]
fn update_on_start_decides_whether_a_start_fetches() {
    let disk = Disk::new("update-on-start");
    let storage = disk.storage();
    storage.set_setting(keys::PLAYLIST_URL, PLAYLIST).unwrap();
    storage
        .set_setting(keys::PLAYLIST_UPDATE_ON_START, "false")
        .unwrap();
    drop(storage);
    let asked = Asked::default();
    let mut s = disk.sim_with(Some(asked.net()));
    assert!(s.shell.refresher.is_some());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(asked.urls(), Vec::<String>::new(), "off: the start waits");
    // The row says "on start": changing it now fetches nothing; the next start does.
    s.set(keys::PLAYLIST_UPDATE_ON_START, "true");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(asked.urls(), Vec::<String>::new());
    drop(s);
    let _s = disk.sim_with(Some(asked.net()));
    assert!(asked.wait_for(PLAYLIST), "on: the start fetches");
}

#[test]
fn the_epg_source_and_interval_reach_the_running_refresher() {
    let disk = Disk::new("epg");
    let asked = Asked::default();
    let mut s = disk.sim_with(Some(asked.net()));
    s.set(keys::PLAYLIST_URL, PLAYLIST);
    assert!(asked.wait_for(PLAYLIST));
    let guide = "http://guide.test/epg.xml";
    s.set(keys::EPG_URL, guide);
    let config = s.shell.store.refresh_config().unwrap();
    assert_eq!(config.extra_guides, [guide]);
    assert!(asked.wait_for(guide), "fetched without a restart");
    let before = asked.urls().len();
    s.set(keys::EPG_UPDATE_HOURS, "6");
    let interval = s.shell.store.refresh_config().unwrap().interval;
    assert_eq!(interval, Some(Duration::from_secs(6 * 3600)));
    let start = Instant::now();
    while asked.urls().len() == before && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        asked.urls().len() > before,
        "a new config refreshes with it"
    );
}

#[test]
fn past_days_bound_the_guide_at_once() {
    // The guide asks TvData::past_days on every Left (guide tests: the limit it walks to).
    let disk = Disk::new("past-days");
    let mut s = disk.sim();
    assert_eq!(s.shell.store.past_days(), 7);
    s.set(keys::EPG_PAST_DAYS, "1");
    assert_eq!(s.shell.store.past_days(), 1);
}

#[test]
fn the_player_settings_reach_the_player_at_once() {
    // What the player does with them: player tests (banner, panel timeout, seek steps).
    let disk = Disk::new("player");
    let mut s = disk.sim();
    for (key, value) in [
        (keys::PLAYER_INFO_ON_ZAP, "false"),
        (keys::PLAYER_INFO_SECS, "10"),
        (keys::SKIP_BACK_SECS, "30"),
        (keys::SKIP_FORWARD_SECS, "120"),
    ] {
        s.set(key, value);
    }
    assert_eq!(
        s.shell.bottom.screen.settings,
        PlayerSettings {
            panel_timeout_ms: 10_000,
            info_on_zap: false,
            skip_back_secs: 30,
            skip_forward_secs: 120,
        }
    );
}

#[test]
fn the_subtitle_settings_reach_the_overlay_at_once() {
    // What the overlay does with them: crates/app/src/subtitles.rs and tv_media's subtitle tests.
    let disk = Disk::new("subtitles");
    let mut s = disk.sim();
    assert!(!s.shell.subtitles.shows());
    assert_eq!(s.shell.subtitles.scale(), 1.0);
    s.set(keys::SUBTITLES_SHOW, "true");
    s.set(keys::SUBTITLES_SIZE, "large");
    assert!(s.shell.subtitles.shows());
    assert_eq!(s.shell.subtitles.scale(), 1.3);
    s.set(keys::SUBTITLES_SHOW, "false");
    assert!(!s.shell.subtitles.shows());
}

#[test]
fn the_group_settings_reshape_the_groups_at_once() {
    let disk = Disk::new("groups");
    let mut s = disk.sim();
    let news = Group::Playlist("News".into());
    let sport = Group::Playlist("Sport".into());
    let movies = Group::Playlist("Movies".into());
    let all = [Group::Favorites, Group::All, Group::History];
    let groups = |s: &Sim| s.shell.store.groups();
    assert_eq!(groups(&s)[..3], all);
    for (key, gone) in [
        (keys::GROUPS_SHOW_FAVORITES, Group::Favorites),
        (keys::GROUPS_SHOW_ALL, Group::All),
        (keys::GROUPS_SHOW_HISTORY, Group::History),
    ] {
        s.set(key, "false");
        assert!(!groups(&s).contains(&gone), "{key}");
    }
    s.set(keys::GROUPS_HIDDEN, "Sport");
    assert_eq!(groups(&s), [news, movies]);
    s.set(keys::GROUPS_HIDDEN, "");
    assert!(groups(&s).contains(&sport));
    let names = |s: &Sim| -> Vec<String> {
        let list = s.shell.store.channels(&Group::All);
        list.iter().take(4).map(|c| c.name.clone()).collect()
    };
    assert_eq!(
        names(&s),
        ["Channel 1", "Channel 2", "Channel 3", "Channel 4"]
    );
    s.set(keys::GROUPS_CHANNEL_SORT, "name");
    assert_eq!(
        names(&s),
        ["Channel 1", "Channel 10", "Channel 11", "Channel 12"]
    );
}

#[test]
fn the_long_press_threshold_follows_its_setting() {
    let disk = Disk::new("long-press");
    let mut s = disk.sim();
    s.live(0);
    s.shell.navigate(Navigate::Player, None);
    s.shell.settle(s.t);
    let hold_ok = |s: &mut Sim, ms: u64| {
        s.edge("OK", Edge::Down);
        s.run(ms);
        s.edge("OK", Edge::Up);
        s.run(500);
    };
    hold_ok(&mut s, 600);
    assert_eq!(s.open(), 1, "past 500 ms: long, the channel menu");
    s.press("Back");
    assert_eq!(s.open(), 0);
    s.set(keys::LONG_PRESS_MS, "800");
    assert_eq!(s.shell.remote.long_ms, 800);
    hold_ok(&mut s, 600);
    assert_eq!(
        s.open(),
        0,
        "under 800 ms: short, the info panel over the player"
    );
    hold_ok(&mut s, 900);
    assert_eq!(s.open(), 1, "past 800 ms: long again");
}

#[test]
fn settings_open_from_the_main_menu_and_long_back_and_back_walks_out_a_level_at_a_time() {
    let mut s = Sim::new();
    assert_eq!(s.open(), 1, "the guide");
    s.shell.navigate(Navigate::MainMenu, None);
    s.shell.settle(s.t);
    // Search, TV guide, Channels, History, Settings.
    for _ in 0..4 {
        s.press("Down");
    }
    s.press("OK");
    // The Settings tree: General, then its first row (Start with the last channel) flips.
    s.press("OK");
    s.press("OK");
    assert!(!s.shell.settings.flag(keys::LAST_CHANNEL_ON_START));
    // Into Add to History after, then Back to General, where Up and OK flip the first row.
    s.press("Down");
    s.press("OK");
    s.press("Back");
    s.press("Up");
    s.press("OK");
    assert!(
        s.shell.settings.flag(keys::LAST_CHANNEL_ON_START),
        "Back left one page"
    );
    s.press("Back");
    assert_eq!(
        s.open(),
        2,
        "the second Back leaves Settings open on its first page"
    );
    s.press("Back");
    assert_eq!(s.open(), 1, "the third closes it: the guide again");

    s.live(2);
    s.shell.navigate(Navigate::Player, None);
    s.shell.settle(s.t);
    assert_eq!(s.open(), 0);
    s.long("Back");
    assert_eq!(s.open(), 1, "long Back in the player: Settings");
    s.press("OK");
    s.press("OK");
    assert!(!s.shell.settings.flag(keys::LAST_CHANNEL_ON_START));
    s.press("Back");
    assert_eq!(s.open(), 1);
    s.press("Back");
    assert_eq!(s.open(), 0, "back to the player");
    assert_eq!(s.playing().channel, Some(s.key(2)), "which played on");
    // A short Back still opens the guide.
    s.press("Back");
    assert_eq!(s.open(), 1);
}
