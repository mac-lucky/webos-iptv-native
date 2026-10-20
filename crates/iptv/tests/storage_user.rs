//! Favourites, recent channels, catch-up history and settings. The cases marked "ported" come
//! from webOS IPTV Player `src/services/storage-service.test.ts` (commit 7d8fd30, Copyright 2026
//! lennylxx, Apache-2.0; see THIRD-PARTY-NOTICES.md), ported to Rust and changed.

mod common;

use common::TempDir;
use iptv::model::{ChannelKey, Timestamp};
use iptv::storage::{HistoryEntry, Storage, DB_FILE, HISTORY_DAYS, HISTORY_MIN_SECS, RECENT_CAP};

const HOUR: i64 = 3600;
const DAY: i64 = 86_400;
const NOW: Timestamp = 1_700_000_000;

fn storage() -> Storage {
    Storage::open_in_memory().unwrap()
}

fn key(n: i64) -> ChannelKey {
    ChannelKey(n)
}

/// A programme that started an hour ago and ends in an hour, 60 s watched.
fn entry(channel: i64) -> HistoryEntry {
    HistoryEntry {
        key: key(channel),
        start: NOW - HOUR,
        stop: NOW + HOUR,
        title: String::new(),
        icon: None,
        position_secs: 60,
        duration_secs: 3600,
        completed: false,
        updated_at: NOW,
    }
}

// --- favourites --------------------------------------------------------------------------

#[test]
fn toggles_a_favourite_on_and_off_returning_the_new_state() {
    // ported
    let s = storage();
    assert_eq!(s.favourites().unwrap(), []);
    assert!(s.toggle_favourite(key(1)).unwrap());
    assert!(s.is_favourite(key(1)).unwrap());
    assert!(!s.toggle_favourite(key(1)).unwrap());
    assert!(!s.is_favourite(key(1)).unwrap());
    assert_eq!(s.favourites().unwrap(), []);
}

#[test]
fn favourites_keep_the_order_they_were_added_in() {
    let s = storage();
    for n in [3, 1, 2] {
        s.toggle_favourite(key(n)).unwrap();
    }
    s.toggle_favourite(key(1)).unwrap();
    s.toggle_favourite(key(1)).unwrap();
    assert_eq!(s.favourites().unwrap(), [key(3), key(2), key(1)]);
}

#[test]
fn set_favourites_replaces_them_in_the_order_given() {
    let s = storage();
    for n in [1, 2, 3] {
        s.toggle_favourite(key(n)).unwrap();
    }
    s.set_favourites(&[key(5), key(2), key(5), key(4)]).unwrap();
    assert_eq!(s.favourites().unwrap(), [key(5), key(2), key(4)]);
    s.toggle_favourite(key(1)).unwrap();
    assert_eq!(s.favourites().unwrap(), [key(5), key(2), key(4), key(1)]);
    s.set_favourites(&[]).unwrap();
    assert_eq!(s.favourites().unwrap(), []);
}

// --- recent channels ---------------------------------------------------------------------

#[test]
fn stores_newest_first_and_moves_a_repeated_channel_to_the_front() {
    // ported
    let s = storage();
    s.touch_recent(key(1), 1000).unwrap();
    s.touch_recent(key(2), 2000).unwrap();
    s.touch_recent(key(1), 3000).unwrap();
    assert_eq!(s.recent().unwrap(), [(key(1), 3000), (key(2), 2000)]);
}

#[test]
fn caps_the_stored_live_history() {
    // ported, with the web app's cap of 30 set explicitly
    let s = storage();
    s.set_setting(RECENT_CAP, "30").unwrap();
    for i in 0..35 {
        s.touch_recent(key(i), i).unwrap();
    }
    let recent = s.recent().unwrap();
    assert_eq!(recent.len(), 30);
    assert_eq!(recent[0].0, key(34));
    assert_eq!(recent[29].0, key(5));
}

#[test]
fn the_recent_cap_defaults_to_ten_and_ignores_garbage() {
    let s = storage();
    for i in 0..15 {
        s.touch_recent(key(i), NOW).unwrap();
    }
    let recent = s.recent().unwrap();
    assert_eq!(recent.len(), 10);
    assert_eq!(recent[0].0, key(14), "same second, still in touch order");
    s.set_setting(RECENT_CAP, "lots").unwrap();
    assert_eq!(s.recent_cap().unwrap(), 10);
    s.set_setting(RECENT_CAP, "0").unwrap();
    assert_eq!(s.recent_cap().unwrap(), 1);
    assert_eq!(s.recent().unwrap().len(), 1);
}

// --- catch-up history --------------------------------------------------------------------

#[test]
fn returns_none_when_no_entry_exists() {
    // ported
    assert_eq!(storage().progress(key(1), NOW - HOUR, NOW).unwrap(), None);
}

#[test]
fn round_trips_an_entry() {
    // ported
    let s = storage();
    let saved = HistoryEntry {
        title: "Programme Alpha".into(),
        icon: Some("http://host/a".into()),
        ..entry(1)
    };
    s.save_progress(&saved, NOW).unwrap();
    assert_eq!(s.progress(key(1), NOW - HOUR, NOW).unwrap(), Some(saved));
}

#[test]
fn isolates_entries_by_channel_and_by_start() {
    // ported (two cases)
    let s = storage();
    s.save_progress(&entry(1), NOW).unwrap();
    assert_eq!(s.progress(key(2), NOW - HOUR, NOW).unwrap(), None);
    assert_eq!(s.progress(key(1), NOW - 2 * HOUR, NOW).unwrap(), None);
}

#[test]
fn keeps_completed_and_near_end_entries() {
    // ported (two cases)
    let s = storage();
    let done = HistoryEntry {
        position_secs: 3570,
        completed: true,
        ..entry(1)
    };
    let near_end = HistoryEntry {
        position_secs: 3590,
        ..entry(2)
    };
    s.save_progress(&done, NOW).unwrap();
    s.save_progress(&near_end, NOW).unwrap();
    assert!(
        s.progress(key(1), NOW - HOUR, NOW)
            .unwrap()
            .unwrap()
            .completed
    );
    assert!(s.progress(key(2), NOW - HOUR, NOW).unwrap().is_some());
}

#[test]
fn a_later_save_replaces_the_position() {
    let s = storage();
    s.save_progress(&entry(1), NOW).unwrap();
    let later = HistoryEntry {
        position_secs: 900,
        updated_at: NOW + 840,
        ..entry(1)
    };
    s.save_progress(&later, NOW + 840).unwrap();
    assert_eq!(s.history(NOW + 840).unwrap(), [later]);
}

#[test]
fn clears_one_entry_without_touching_others() {
    // ported
    let s = storage();
    s.save_progress(&entry(1), NOW).unwrap();
    s.save_progress(&entry(2), NOW).unwrap();
    s.remove_progress(key(1), NOW - HOUR).unwrap();
    assert_eq!(s.progress(key(1), NOW - HOUR, NOW).unwrap(), None);
    assert!(s.progress(key(2), NOW - HOUR, NOW).unwrap().is_some());
}

#[test]
fn expires_an_entry_when_its_programme_ended_the_set_days_ago() {
    // ported: the web app kept an entry catchupDays past the end; here the history setting
    let s = storage();
    s.set_setting(HISTORY_DAYS, "3").unwrap();
    s.save_progress(&entry(1), NOW).unwrap();
    let stop = NOW + HOUR;
    assert!(s
        .progress(key(1), NOW - HOUR, stop + 3 * DAY - 1)
        .unwrap()
        .is_some());
    assert_eq!(
        s.progress(key(1), NOW - HOUR, stop + 3 * DAY).unwrap(),
        None
    );
}

#[test]
fn keeps_entries_seven_days_by_default() {
    // ported
    let s = storage();
    s.save_progress(&entry(1), NOW).unwrap();
    let stop = NOW + HOUR;
    assert!(s
        .progress(key(1), NOW - HOUR, stop + 7 * DAY - 1)
        .unwrap()
        .is_some());
    assert_eq!(
        s.progress(key(1), NOW - HOUR, stop + 7 * DAY + 1).unwrap(),
        None
    );
}

#[test]
fn lists_one_channels_live_entries_by_start() {
    // ported (getCatchupProgressList: all, empty, expired excluded)
    let s = storage();
    s.set_setting(HISTORY_DAYS, "1").unwrap();
    for (channel, start) in [(1, NOW - 2 * HOUR), (1, NOW - 3 * HOUR), (2, NOW - HOUR)] {
        let e = HistoryEntry {
            start,
            stop: start + HOUR,
            ..entry(channel)
        };
        s.save_progress(&e, NOW).unwrap();
    }
    let starts: Vec<_> = s
        .channel_history(key(1), NOW)
        .unwrap()
        .iter()
        .map(|e| e.start)
        .collect();
    assert_eq!(starts, [NOW - 3 * HOUR, NOW - 2 * HOUR]);
    assert_eq!(s.channel_history(key(3), NOW).unwrap(), []);
    assert_eq!(s.channel_history(key(1), NOW + DAY).unwrap(), []);
}

#[test]
fn lists_every_live_entry_newest_first() {
    // ported (getAllCatchupProgress)
    let s = storage();
    s.set_setting(HISTORY_DAYS, "1").unwrap();
    s.save_progress(&entry(1), NOW).unwrap();
    let long = HistoryEntry {
        start: NOW,
        stop: NOW + 30 * DAY,
        updated_at: NOW + 1,
        ..entry(2)
    };
    s.save_progress(&long, NOW).unwrap();
    let keys = |at| -> Vec<ChannelKey> { s.history(at).unwrap().iter().map(|e| e.key).collect() };
    assert_eq!(keys(NOW), [key(2), key(1)]);
    assert_eq!(keys(NOW + HOUR + DAY + 1), [key(2)]);
}

#[test]
fn clears_history_and_recents_without_touching_favourites() {
    // ported
    let s = storage();
    s.touch_recent(key(1), NOW).unwrap();
    s.save_progress(&entry(1), NOW).unwrap();
    s.toggle_favourite(key(1)).unwrap();
    s.clear_recent().unwrap();
    s.clear_history().unwrap();
    assert_eq!(s.recent().unwrap(), []);
    assert_eq!(s.history(NOW).unwrap(), []);
    assert_eq!(s.favourites().unwrap(), [key(1)]);
}

// --- settings and the file ---------------------------------------------------------------

#[test]
fn settings_are_key_value_and_survive_a_reopen() {
    let dir = TempDir::new("settings");
    {
        let s = Storage::open(&dir.0).unwrap();
        assert_eq!(s.setting("theme").unwrap(), None);
        s.set_setting("theme", "dark").unwrap();
        s.set_setting("theme", "light").unwrap();
        s.set_setting("gone", "x").unwrap();
        s.remove_setting("gone").unwrap();
        s.toggle_favourite(key(7)).unwrap();
    }
    let s = Storage::open(&dir.0).unwrap();
    assert_eq!(s.setting("theme").unwrap().as_deref(), Some("light"));
    assert_eq!(s.setting("gone").unwrap(), None);
    assert_eq!(s.favourites().unwrap(), [key(7)]);
}

#[cfg(unix)]
#[test]
fn the_database_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("mode");
    let s = Storage::open(&dir.0).unwrap();
    s.toggle_favourite(key(1)).unwrap();
    for name in [DB_FILE.to_string(), format!("{DB_FILE}-wal")] {
        let mode = std::fs::metadata(dir.0.join(&name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
    }
}

// --- review regressions -------------------------------------------------------------------

#[test]
fn does_not_keep_a_position_under_the_resume_minimum() {
    // ported (two cases; the web app's minimum was 15 s, ours is the history setting, 10 s)
    let s = storage();
    assert_eq!(s.history_min_secs().unwrap(), 10);
    let early = HistoryEntry {
        position_secs: 9,
        ..entry(1)
    };
    s.save_progress(&early, NOW).unwrap();
    assert_eq!(s.progress(key(1), NOW - HOUR, NOW).unwrap(), None);
    let ten = HistoryEntry {
        position_secs: 10,
        ..entry(1)
    };
    s.save_progress(&ten, NOW).unwrap();
    assert!(s.progress(key(1), NOW - HOUR, NOW).unwrap().is_some());
    let restarted = HistoryEntry {
        position_secs: 5,
        ..entry(1)
    };
    s.save_progress(&restarted, NOW).unwrap();
    assert_eq!(s.progress(key(1), NOW - HOUR, NOW).unwrap(), None);
}

#[test]
fn the_resume_minimum_follows_its_setting() {
    let s = storage();
    s.set_setting(HISTORY_MIN_SECS, "30").unwrap();
    s.save_progress(&entry(1), NOW).unwrap();
    let short = HistoryEntry {
        position_secs: 29,
        ..entry(2)
    };
    s.save_progress(&short, NOW).unwrap();
    let keys: Vec<_> = s.history(NOW).unwrap().iter().map(|e| e.key).collect();
    assert_eq!(keys, [key(1)], "60 s in is kept, 29 s is not");
    // Out of range reads clamp; garbage reads as the default.
    s.set_setting(HISTORY_MIN_SECS, "0").unwrap();
    assert_eq!(s.history_min_secs().unwrap(), 1);
    s.set_setting(HISTORY_MIN_SECS, "lots").unwrap();
    assert_eq!(s.history_min_secs().unwrap(), 10);
}

#[test]
fn a_new_keep_days_redates_the_stored_history_at_once() {
    let s = storage();
    s.save_progress(&entry(1), NOW).unwrap();
    let stop = NOW + HOUR;
    let kept_at = |at| s.progress(key(1), NOW - HOUR, at).unwrap().is_some();
    assert!(kept_at(stop + 6 * DAY));
    s.set_setting(HISTORY_DAYS, "1").unwrap();
    assert!(kept_at(stop + DAY - 1));
    assert!(!kept_at(stop + 2 * DAY), "a shorter keep drops it at once");
    s.set_setting(HISTORY_DAYS, "14").unwrap();
    assert!(kept_at(stop + 13 * DAY), "a longer one brings it back");
    s.remove_setting(HISTORY_DAYS).unwrap();
    assert!(
        kept_at(stop + 6 * DAY) && !kept_at(stop + 7 * DAY),
        "the default again"
    );
}

#[test]
fn several_threads_can_open_a_new_file_at_once() {
    for round in 0..20 {
        let dir = TempDir::new(&format!("race{round}"));
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            let opens: Vec<_> = (0..3)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        Storage::open(&dir.0).map(drop).map_err(|e| e.to_string())
                    })
                })
                .collect();
            for open in opens {
                assert_eq!(open.join().unwrap(), Ok(()), "round {round}");
            }
        });
    }
}
