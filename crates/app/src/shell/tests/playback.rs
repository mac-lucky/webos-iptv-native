//! The viewer's playback options in the running app, and its lifecycle: the display mode and
//! the audio offsets reach the player when they change and before each channel opens, the sleep
//! timer warns and then stops playback, and the background, foreground and exit edges suspend,
//! resume and release the player with the archive position kept.

use tv_screens::playback::with_channel_offset;

use super::*;

/// What a settings row or the player menu sends for a change.
fn change(s: &mut Sim, key: &str, value: &str) {
    let (key, value) = (key.to_owned(), value.to_owned());
    s.shell.run(Command::SetSetting { key, value }, s.t);
}

/// The player calls since the last look, without the rectangles: an open as
/// `open live <channel>`.
fn told(s: &Sim) -> Vec<String> {
    let calls = s
        .calls
        .take()
        .into_iter()
        .filter(|c| !c.starts_with("rect"));
    calls
        .map(|c| match c.strip_prefix("open ") {
            Some(rest) => format!(
                "open {}",
                rest.splitn(4, ' ').take(3).collect::<Vec<_>>().join(" ")
            ),
            None => c,
        })
        .collect()
}

/// A programme that ended an hour ago, from the archive of every synthetic channel.
fn aired() -> Programme {
    Programme {
        start: NOW - 2 * 3600,
        stop: NOW - 3600,
        title: "Film".into(),
        ..Programme::default()
    }
}

/// Where History has `programme` on `channel`, if anywhere.
fn kept_at(s: &Sim, channel: ChannelKey, programme: &Programme) -> Option<i64> {
    let history = s.shell.store.history();
    let entry = history
        .iter()
        .find(|h| h.channel == channel && h.programme.start == programme.start)?;
    Some(entry.position_secs)
}

#[test]
pub(super) fn the_display_mode_and_the_audio_offsets_reach_the_player_at_once() {
    let mut s = Sim::new();
    s.live(0);
    s.calls.take();
    change(&mut s, keys::PLAYER_DISPLAY_MODE, "4:3");
    assert_eq!(told(&s), ["display 4:3"]);
    change(&mut s, keys::AUDIO_OFFSET_MS, "-100");
    assert_eq!(
        told(&s),
        ["audio offset -100"],
        "the default, the channel has none"
    );
    let own = with_channel_offset(&s.shell.settings, s.key(0), 250);
    change(&mut s, keys::CHANNEL_AUDIO_OFFSETS, &own);
    assert_eq!(told(&s), ["audio offset 250"], "its own, at once");
    // every channel opens with its offset already set, and only a change is told
    s.live(1);
    assert_eq!(told(&s), ["audio offset -100", "open live Channel 2"]);
    s.live(2);
    assert_eq!(told(&s), ["open live Channel 3"]);
    s.live(0);
    assert_eq!(told(&s), ["audio offset 250", "open live Channel 1"]);
    change(&mut s, keys::PLAYER_DISPLAY_MODE, "normal");
    assert_eq!(told(&s), ["display normal"]);
}

#[test]
fn defaults_tell_the_player_nothing() {
    let mut s = Sim::new();
    s.live(3);
    assert_eq!(told(&s), ["open live Channel 4"]);
}

#[test]
fn the_sleep_timer_warns_a_minute_before_then_stops_and_opens_the_guide() {
    let mut s = Sim::new();
    let channel = s.key(0);
    s.catchup(channel, &aired(), 0);
    s.shell.navigate(Navigate::Player, None);
    s.shell.settle(s.t);
    s.shell.store.playback_mut().position_secs = 600;
    s.calls.take();
    s.shell.run(Command::SleepTimer { minutes: 15 }, s.t);
    assert_eq!(s.toast().as_deref(), Some("Playback stops in 15 min"));
    s.run(1_000);
    assert_eq!(s.playing().sleep_left_secs, Some(15 * 60 - 1));
    s.run(14 * 60_000 - 1_000);
    assert_eq!(s.toast().as_deref(), Some("Playback stops in a minute"));
    assert!(!told(&s).contains(&"stop".to_owned()));
    s.run(60_000);
    assert_eq!(told(&s), ["stop"], "the decoder is released");
    assert_eq!(s.playing(), Playback::default(), "nothing plays, no timer");
    assert_eq!(s.open(), 1, "the guide");
    assert_eq!(s.toast().as_deref(), Some("Sleep timer stopped playback"));
    assert_eq!(kept_at(&s, channel, &aired()), Some(600));
    assert!(s.shell.sleep.is_none());
}

#[test]
fn the_sleep_timer_is_replaced_or_turned_off_and_outlives_a_zap() {
    let mut s = Sim::new();
    s.live(0);
    s.shell.run(Command::SleepTimer { minutes: 30 }, s.t);
    s.run(1_000);
    s.shell.run(Command::SleepTimer { minutes: 15 }, s.t);
    s.live(1);
    s.run(1_000);
    assert_eq!(
        s.playing().sleep_left_secs,
        Some(15 * 60 - 1),
        "the new one, past a zap"
    );
    s.shell.run(Command::SleepTimer { minutes: 0 }, s.t);
    assert_eq!(s.toast().as_deref(), Some("Sleep timer off"));
    assert_eq!(s.playing().sleep_left_secs, None);
    s.calls.take();
    s.run(31 * 60_000);
    assert!(
        !told(&s).contains(&"stop".to_owned()),
        "off: playback goes on"
    );
    assert_eq!(s.playing().channel, Some(s.key(1)));
}

/// The background edge (Home, another app) keeps the archive position and releases
/// the decoder; the foreground edge plays on and draws again; the exit keeps the position and
/// releases the decoder before the process ends.
#[test]
fn background_suspends_foreground_resumes_and_exit_releases() {
    let mut s = Sim::new();
    let channel = s.key(0);
    s.catchup(channel, &aired(), 0);
    s.shell.store.playback_mut().position_secs = 300;
    s.calls.take();
    s.shell.suspend();
    assert_eq!(told(&s), ["suspend"]);
    assert_eq!(kept_at(&s, channel, &aired()), Some(300));
    s.shell.dirty = false;
    s.shell.resume();
    assert_eq!(told(&s), ["resume"]);
    assert!(s.shell.needs_frame(), "the frame is drawn again");
    assert_eq!(s.playing().channel, Some(channel), "the same programme");
    s.shell.store.playback_mut().position_secs = 420;
    s.shell.shutdown();
    assert_eq!(told(&s), ["shutdown"]);
    assert_eq!(kept_at(&s, channel, &aired()), Some(420));
}

/// A relaunch while the app runs is the system's to answer: with `handlesRelaunch` off, webOS
/// brings the running instance to the front (the foreground edge above) instead of starting
/// another, and the native lifecycle interface delivers the background and foreground edges.
#[test]
fn a_relaunch_is_the_system_bringing_the_running_instance_back() {
    let appinfo: serde_json::Value =
        serde_json::from_str(include_str!("../../../../../pkg/appinfo.json")).unwrap();
    assert_eq!(appinfo["handlesRelaunch"], false);
    assert_eq!(appinfo["nativeLifeCycleInterfaceVersion"], 2);
}
