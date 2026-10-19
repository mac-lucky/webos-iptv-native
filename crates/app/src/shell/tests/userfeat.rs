//! Parental controls, the channel name editor and programme reminders in the running app: every
//! way to play passes the PIN gate, the PIN dialog opens what it was asked for, name rules reach
//! the lists at once, and a reminder prompts at its programme's start.

use iptv::model::{Channel, Playlist};
use iptv::storage::Storage;
use tv_screens::data::PinTask;
use tv_screens::panels::reminder::PROMPT_MS;
use tv_screens::parental::{pin_matches, Parental};
use tv_screens::reminders::{self, Toggled};

use super::*;
use crate::store::StoreData;

/// What the Settings panel, a menu or the PIN dialog sends for a change.
fn put(s: &mut Sim, key: &str, value: &str) {
    let (key, value) = (key.to_owned(), value.to_owned());
    s.shell.run(Command::SetSetting { key, value }, s.t);
    s.shell.settle(s.t);
}

fn digits(s: &mut Sim, pin: &str) {
    for d in pin.chars() {
        s.press(&d.to_string());
    }
}

fn lock_channel(s: &mut Sim, i: usize) {
    let hex = s.key(i).hex();
    put(s, keys::PARENTAL_CHANNELS, &hex);
}

/// The channels the player was asked to open, by name.
fn opened(s: &Sim) -> Vec<String> {
    s.calls
        .take()
        .into_iter()
        .filter(|c| c.starts_with("open "))
        .collect()
}

#[test]
pub(super) fn a_locked_channel_plays_only_after_the_pin() {
    let mut s = Sim::new();
    lock_channel(&mut s, 0);
    assert!(s
        .shell
        .store
        .locked(&s.shell.store.channels(&Group::All)[0]));
    // OK in the guide previews the focused channel: held back for the PIN.
    s.press("OK");
    assert_eq!(opened(&s), Vec::<String>::new(), "no video before the PIN");
    assert_eq!(s.open(), 2, "the PIN dialog over the guide");
    digits(&mut s, "1234");
    assert_eq!(
        (s.open(), opened(&s).len()),
        (2, 0),
        "a wrong PIN keeps it shut"
    );
    digits(&mut s, "0000");
    let opened_now = opened(&s);
    assert!(
        opened_now.len() == 1 && opened_now[0].starts_with("open live Channel 1 "),
        "{opened_now:?}"
    );
    assert_eq!(s.open(), 1, "the dialog closed, the guide previews it");
    // Open for the session now: OK again plays it full screen without the PIN.
    s.press("OK");
    assert_eq!(s.open(), 0);
}

#[test]
fn back_in_the_pin_dialog_plays_nothing() {
    let mut s = Sim::new();
    lock_channel(&mut s, 0);
    s.press("OK");
    s.press("Back");
    assert_eq!(
        (s.open(), opened(&s).len()),
        (1, 0),
        "back to the guide, nothing plays"
    );
    assert_eq!(s.playing().channel, None);
}

#[test]
fn zapping_and_number_entry_pass_the_gate() {
    let mut s = Sim::new();
    s.live(0);
    s.shell.navigate(Navigate::Player, None);
    s.run(100);
    s.calls.take();
    lock_channel(&mut s, 1);
    // Up zaps to the next channel: channel 2 is locked, so channel 3.
    s.press("Up");
    let zapped = opened(&s);
    assert!(
        zapped.len() == 1 && zapped[0].starts_with("open live Channel 3 "),
        "{zapped:?}"
    );
    // Typing 2 asks for channel 2 itself: the PIN first.
    s.press("2");
    s.run(5_000);
    assert_eq!(s.open(), 1, "the PIN dialog over the player");
    assert_eq!(opened(&s).len(), 0);
    digits(&mut s, "0000");
    let typed = opened(&s);
    assert!(
        typed.len() == 1 && typed[0].starts_with("open live Channel 2 "),
        "{typed:?}"
    );
}

#[test]
fn search_history_and_recents_plays_pass_the_gate() {
    // Each of these screens answers a pick with `Command::Play` (their own tests): the gate is
    // the shell's, whichever screen sent it, and the whole output waits with it.
    let mut s = Sim::new();
    lock_channel(&mut s, 5);
    let play = Output::command(Command::Play {
        stream: Stream::Live(s.key(5)),
        group: None,
    })
    .and(Output::navigate(Navigate::Player));
    let guide = Some(0);
    s.shell.apply(play, guide, s.t);
    s.shell.settle(s.t);
    assert_eq!(
        (s.open(), opened(&s).len()),
        (2, 0),
        "held, the guide still up"
    );
    digits(&mut s, "0000");
    assert_eq!(opened(&s).len(), 1);
    assert_eq!(s.open(), 0, "the held navigation ran too");
}

#[test]
fn a_list_pick_the_pin_held_back_is_not_taken_as_playing() {
    for preview in [true, false] {
        let mut s = Sim::new();
        s.live(0);
        s.calls.take();
        lock_channel(&mut s, 1);
        s.shell.navigate(Navigate::ChannelList { preview }, None);
        s.shell.settle(s.t);
        s.press("Down");
        s.press("OK");
        assert_eq!(s.open(), 2, "preview {preview}: the PIN dialog");
        s.press("Back");
        assert_eq!(s.open(), 1, "the list again");
        if preview {
            // Back leaves the list: there is nothing to commit, so no PIN dialog again.
            s.press("Back");
            assert_eq!(s.open(), 0, "the list closed");
        } else {
            // OK on the same channel asks again rather than closing the list silently.
            s.press("OK");
            assert_eq!(s.open(), 2, "asked again");
        }
        assert_eq!(opened(&s).len(), 0);
        assert_eq!(s.playing().channel, Some(s.key(0)), "channel 1 plays on");
    }
}

#[test]
fn a_number_the_pin_held_back_leaves_the_player_on_what_plays() {
    let mut s = Sim::new();
    s.live(0);
    s.shell.navigate(Navigate::Player, None);
    s.run(100);
    lock_channel(&mut s, 1);
    s.press("2");
    s.run(5_000);
    assert_eq!(s.open(), 1, "the PIN dialog");
    s.press("Back");
    let shown = s
        .shell
        .bottom
        .screen
        .header()
        .channel
        .as_ref()
        .map(|c| c.key);
    assert_eq!(
        shown,
        Some(s.key(0)),
        "the banner names the channel on screen"
    );
    // Long OK opens the menu of the channel on screen, not of the one held back.
    s.long("OK");
    let top = s.shell.top().unwrap();
    let any: &dyn std::any::Any = &*s.shell.stack[top].screen;
    let menu = any.downcast_ref::<ContextMenu>().expect("the channel menu");
    let rows: Vec<&str> = menu
        .panel()
        .page()
        .rows()
        .iter()
        .map(|r| r.label())
        .collect();
    // Channel 2 is locked: its menu would offer Unlock channel.
    assert!(rows.contains(&"Lock channel"), "{rows:?}");
    assert_eq!(s.playing().channel, Some(s.key(0)));
}

#[test]
fn going_to_the_background_locks_again_what_the_pin_opened() {
    let mut s = Sim::new();
    lock_channel(&mut s, 0);
    s.press("OK");
    digits(&mut s, "0000");
    s.press("OK");
    assert_eq!((s.open(), s.playing().channel), (0, Some(s.key(0))));
    s.calls.take();
    s.shell.suspend();
    assert_eq!(
        s.calls.take(),
        ["suspend", "stop"],
        "the locked channel stops"
    );
    assert_eq!(s.playing().channel, None);
    assert_eq!(s.open(), 1, "the guide is up");
    s.shell.resume();
    s.run(100);
    s.calls.take();
    s.press("OK");
    assert_eq!(s.open(), 2, "the PIN again");
    assert_eq!(opened(&s).len(), 0);

    // A channel that is not locked plays on.
    let mut s = Sim::new();
    lock_channel(&mut s, 0);
    s.live(1);
    s.calls.take();
    s.shell.suspend();
    s.shell.resume();
    assert_eq!(s.calls.take(), ["suspend", "resume"]);
    assert_eq!(s.playing().channel, Some(s.key(1)));
}

#[test]
fn a_renamed_or_deleted_zap_group_gives_way_to_every_channel() {
    use tv_screens::library::LibraryEdit;
    for delete in [false, true] {
        let mut s = Sim::new();
        let kids = Group::Custom("Kids".into());
        let channels = vec![s.key(2), s.key(3)];
        s.shell.run(
            Command::Library(LibraryEdit::CreateGroup("Kids".into())),
            s.t,
        );
        let add = LibraryEdit::AddToGroup {
            to: "Kids".into(),
            channels,
            from: None,
        };
        s.shell.run(Command::Library(add), s.t);
        let play = Command::Play {
            stream: Stream::Live(s.key(2)),
            group: Some(kids.clone()),
        };
        s.shell.run(play, s.t);
        assert_eq!(s.shell.bottom.screen.group(), &kids);
        let edit = if delete {
            LibraryEdit::DeleteGroup("Kids".into())
        } else {
            LibraryEdit::RenameGroup {
                from: "Kids".into(),
                to: "Children".into(),
            }
        };
        s.shell.run(Command::Library(edit), s.t);
        assert_eq!(
            s.shell.bottom.screen.group(),
            &Group::All,
            "delete {delete}"
        );
        assert_eq!(s.shell.shown_group(), Group::All);
        // Zapping walks every channel again.
        s.shell.navigate(Navigate::Player, None);
        s.run(100);
        s.calls.take();
        s.press("Up");
        assert_eq!(opened(&s).len(), 1, "zapped");
    }
}

#[test]
fn a_locked_last_channel_does_not_start_by_itself() {
    locale();
    for locked in [false, true] {
        let mut data = FakeData::new(NOW, 12);
        let key = data.channels[2].key;
        data.recents = vec![key];
        if locked {
            let blocked = Settings::from_stored([(keys::PARENTAL_CHANNELS, key.hex())]);
            data.parental = Parental::from_settings(&blocked);
        }
        let calls = Calls::default();
        let mut shell = Shell::new(
            Box::new(data),
            Box::new(calls.clone()),
            None,
            LogoCache::new(None),
        );
        shell.start(0);
        let started = calls.take().iter().any(|c| c.starts_with("open live"));
        assert_eq!(started, !locked, "locked: {locked}");
        assert_eq!(
            shell.top().is_some(),
            locked,
            "the guide when it did not start"
        );
    }
}

#[test]
pub(super) fn a_locked_group_opens_with_the_pin() {
    let mut s = Sim::new();
    put(&mut s, keys::PARENTAL_GROUPS, "Sport");
    let sport = Group::Playlist("Sport".into());
    assert!(s.shell.store.channels(&sport).is_empty());
    assert_eq!(
        s.shell.store.channels(&Group::All).len(),
        12,
        "listed in All"
    );
    // The guide's groups panel: Back opens it on All channels; Sport is three rows down.
    s.press("Back");
    for _ in 0..3 {
        s.press("Down");
    }
    s.press("OK");
    assert_eq!(s.open(), 2, "OK on the locked group asks for the PIN");
    digits(&mut s, "0000");
    assert_eq!(s.open(), 1);
    assert_eq!(
        s.shell.store.channels(&sport).len(),
        4,
        "open for the session"
    );
}

#[test]
fn lock_changes_take_the_pin() {
    let mut s = Sim::new();
    let hex = s.key(0).hex();
    // What the channel menu's Lock channel row returns.
    let ask = Output::close().and(Output::navigate(Navigate::Pin(PinTask::Set {
        key: keys::PARENTAL_CHANNELS,
        value: hex.clone(),
        toast: "Channel locked".into(),
    })));
    s.shell.apply(ask, None, s.t);
    s.shell.settle(s.t);
    assert_eq!(
        s.shell.settings.get(keys::PARENTAL_CHANNELS),
        "",
        "not before the PIN"
    );
    digits(&mut s, "0000");
    assert_eq!(s.shell.settings.get(keys::PARENTAL_CHANNELS), hex);
    assert_eq!(s.toast().as_deref(), Some("Channel locked"));
    assert!(s
        .shell
        .store
        .locked(&s.shell.store.channels(&Group::All)[0]));
}

#[test]
pub(super) fn the_pin_can_be_changed_and_is_kept_as_a_verifier() {
    let mut s = Sim::new();
    s.shell.navigate(Navigate::Pin(PinTask::Change), None);
    s.shell.settle(s.t);
    digits(&mut s, "0000");
    digits(&mut s, "4321");
    digits(&mut s, "4321");
    assert_eq!(s.toast().as_deref(), Some("PIN changed"));
    let stored = s.shell.settings.get(keys::PARENTAL_PIN).to_owned();
    assert!(!stored.contains("4321") && pin_matches(&stored, "4321"));
    assert!(
        !format!("{:?}", s.shell.settings).contains(&stored),
        "never printed"
    );
    lock_channel(&mut s, 0);
    s.press("OK");
    digits(&mut s, "0000");
    assert_eq!(opened(&s).len(), 0, "the old PIN no longer opens");
    digits(&mut s, "4321");
    assert_eq!(opened(&s).len(), 1);
}

fn remind(s: &mut Sim, i: usize, start: i64, title: &str) {
    let settings = s.shell.settings.clone();
    let Toggled::Added(value) = reminders::toggled(&settings, s.key(i), start, title) else {
        panic!("added");
    };
    put(s, keys::REMINDERS, &value);
}

#[test]
pub(super) fn a_reminder_prompts_at_its_start_and_ok_switches() {
    let mut s = Sim::new();
    // The synthetic store's clock stands at NOW; the loop's carries it from the start (0 ms).
    remind(&mut s, 3, NOW + 60, "Film");
    assert_eq!(
        s.shell.store.reminders().len(),
        1,
        "the guide's bell reads it"
    );
    assert_eq!(s.shell.reminders.next_wake(s.t), Some(60_000));
    s.run(58_000);
    assert_eq!(s.open(), 1, "not yet");
    s.run(2_500);
    assert_eq!(s.open(), 2, "the prompt over the guide");
    assert_eq!(
        s.shell.settings.get(keys::REMINDERS),
        "",
        "fired: off the list"
    );
    assert!(s.shell.store.reminders().is_empty());
    s.calls.take();
    s.press("OK");
    let switched = opened(&s);
    assert!(
        switched.len() == 1 && switched[0].starts_with("open live Channel 4 "),
        "{switched:?}"
    );
    assert_eq!(s.open(), 0, "full screen");
}

#[test]
fn an_unanswered_prompt_goes_and_a_long_past_reminder_is_dropped() {
    let mut s = Sim::new();
    remind(&mut s, 1, NOW + 2, "News");
    s.run(3_000);
    assert_eq!(s.open(), 2);
    s.run(PROMPT_MS + 500);
    assert_eq!(
        (s.open(), opened(&s).len()),
        (1, 0),
        "gone by itself, nothing played"
    );
    remind(&mut s, 1, NOW - 3_600, "Old");
    s.run(100);
    assert_eq!(s.open(), 1, "no prompt for an hour-old start");
    assert_eq!(s.shell.settings.get(keys::REMINDERS), "", "dropped");
    assert_eq!(
        s.shell.reminders.next_wake(s.t),
        None,
        "nothing to wake for"
    );
}

#[test]
pub(super) fn name_rules_clean_the_names_at_once() {
    let mut s = Sim::new();
    put(&mut s, keys::NAMES_PREFIXES, "Channel");
    let names = |s: &Sim| -> Vec<String> {
        let list = s.shell.store.channels(&Group::All);
        list.iter().take(3).map(|c| c.name.clone()).collect()
    };
    assert_eq!(names(&s), ["1", "2", "3"]);
    put(&mut s, keys::NAMES_PREFIXES, "");
    put(&mut s, keys::NAMES_SUFFIXES, "1\n2");
    assert_eq!(names(&s), ["Channel", "Channel", "Channel 3"]);
}

fn named(name: &str, number: u32) -> Channel {
    let mut c = FakeData::new(NOW, 1).channels.remove(0);
    c.url = format!("http://names.test/{number}.m3u8");
    c.key = ChannelKey::of_url(&c.url);
    c.name = name.into();
    c.number = Some(number);
    c
}

#[test]
fn the_store_cleans_names_once_and_sorts_by_the_clean_ones() {
    let storage = Storage::open_in_memory().unwrap();
    let id = storage
        .add_playlist("p", "http://names.test/list.m3u")
        .unwrap();
    let playlist = Playlist {
        channels: vec![named("PL: Zeta HD", 1), named("UK: Alpha FHD", 2)],
        ..Playlist::default()
    };
    assert!(storage.save_playlist(id, &playlist, NOW).unwrap());
    let mut store = StoreData::new(storage);
    let mut settings = store.settings();
    for (key, value) in [
        (keys::NAMES_PREFIXES, "PL:\nUK:"),
        (keys::NAMES_SUFFIXES, "HD\nFHD"),
        (keys::GROUPS_CHANNEL_SORT, "name"),
    ] {
        assert!(settings.set(key, value.into()));
        store.save_setting(key, &settings);
    }
    let names: Vec<String> = store
        .channels(&Group::All)
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(
        names,
        ["Alpha", "Zeta"],
        "cleaned, then sorted by the clean names"
    );
}
