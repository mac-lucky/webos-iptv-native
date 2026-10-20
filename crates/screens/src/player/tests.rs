use super::transport::{Button, Focus, COMMIT_MS, FAST};
use super::*;
use crate::data::fake::{FakeData, Mono};
use crate::data::{MediaInfo, Playback};
use crate::draw::HAlign;
use iptv::model::Timestamp;
use std::cell::Cell;
use tv_platform::i18n::msg;

// 2026-08-27 15:12:00 UTC, a Thursday.
const T: Timestamp = 1_787_843_520;
const SLOT: i64 = 1800;
// The half-hour slot on air at T: 15:00 - 15:30.
const ON_AIR: Timestamp = T - 720;

/// The half-hour programme starting at `start`, titled by its start so tests can name it.
fn slot(start: Timestamp) -> Programme {
    Programme {
        start,
        stop: start + SLOT,
        title: format!("Show {}", time::hhmm(start, 0)),
        description: Some("About the show.".into()),
        category: Some("News".into()),
        icon: None,
    }
}

/// Every channel's guide: half-hour slots overlapping `[from, to)`.
fn slots(from: Timestamp, to: Timestamp) -> Vec<Programme> {
    let first = from - from.rem_euclid(SLOT);
    (0..)
        .map(|i| first + i * SLOT)
        .take_while(|s| *s < to)
        .map(slot)
        .collect()
}

/// Six channels playing the first one live in full HD, the guide counted in `reads`.
fn counted(reads: Rc<Cell<usize>>) -> FakeData {
    let mut w = FakeData::new(T, 6).with_schedule(move |_, from, to| {
        reads.set(reads.get() + 1);
        slots(from, to)
    });
    w.playback = Playback {
        channel: Some(w.channels[0].key),
        media: MediaInfo {
            width: 1920,
            height: 1080,
            fps: 25,
            audio_channels: 2,
        },
        ..Playback::default()
    };
    w
}

fn world() -> FakeData {
    counted(Rc::default())
}

trait World {
    fn key(&self, i: usize) -> ChannelKey;
    /// Catch-up of the slot starting at `start`, `pos` seconds in.
    fn catchup(&mut self, start: Timestamp, pos: i64);
}

impl World for FakeData {
    fn key(&self, i: usize) -> ChannelKey {
        self.channels[i].key
    }

    fn catchup(&mut self, start: Timestamp, pos: i64) {
        self.playback.catchup = Some(slot(start));
        self.playback.position_secs = pos;
    }
}

fn live(key: ChannelKey) -> Command {
    Command::Play {
        stream: Stream::Live(key),
        group: None,
    }
}

fn archive(channel: ChannelKey, programme: Programme, offset_secs: i64) -> Command {
    Command::Play {
        stream: Stream::Catchup {
            channel,
            programme,
            offset_secs,
        },
        group: None,
    }
}

fn screen() -> PlayerScreen {
    PlayerScreen::new(PlayerSettings::default())
}

fn short(s: &mut PlayerScreen, w: &FakeData, k: Key, ms: u64) -> Output {
    s.key(k, Press::Short, w, ms)
}

// ---- info panel (task 26) ----

#[test]
fn ok_opens_the_info_panel_with_the_programme_and_the_channel() {
    let w = world();
    let mut s = screen();
    let out = short(&mut s, &w, Key::Ok, 0);
    assert!(out.redraw && out.commands.is_empty());
    assert_eq!(s.overlay(), Overlay::Info { tile: 0 });
    let h = s.header();
    assert_eq!(h.title, "Show 15:00");
    assert_eq!(h.times, "15:00 \u{2014} 15:30");
    assert_eq!(h.progress, Some(0.4));
    assert_eq!(h.remaining, "18 min");
    assert_eq!((h.number.as_str(), h.name.as_str()), ("1", "Channel 1"));
    assert_eq!(h.chips, ["FULL HD", "25 FPS", "STEREO"]);
    assert_eq!(h.description.as_deref(), Some("About the show."));
    assert_eq!(h.next_times, "15:30 \u{2014} 16:00");
    assert_eq!(h.next_title, "Show 15:30");
    assert_eq!(h.group, "All channels");
    assert_eq!(h.clock, "Thu, Aug 27 15:12");
    assert!(h.archive && !h.catchup);
}

#[test]
fn the_header_follows_the_offset_and_the_group() {
    let mut w = world();
    w.utc_offset = 7200;
    let mut s = screen();
    s.zap_through(Some(Group::Playlist("News".into())), Some(w.key(0)), &w);
    short(&mut s, &w, Key::Ok, 0);
    assert_eq!(s.header().times, "17:00 \u{2014} 17:30");
    assert_eq!(s.header().clock, "Thu, Aug 27 17:12");
    assert_eq!(s.header().group, "News");
}

#[test]
fn chips_name_resolution_rate_and_sound() {
    let m = |height, fps, audio_channels| MediaInfo {
        width: 0,
        height,
        fps,
        audio_channels,
    };
    assert_eq!(model::chips(&m(720, 50, 6)), ["HD", "50 FPS", "5.1"]);
    assert_eq!(model::chips(&m(2160, 0, 1)), ["4K", "MONO"]);
    assert_eq!(model::chips(&m(576, 25, 3)), ["SD", "25 FPS", "3 CH"]);
    assert!(model::chips(&MediaInfo::default()).is_empty());
}

#[test]
fn tile_row_lists_guide_history_and_recent_channels_but_not_the_one_playing() {
    let mut w = world();
    w.recents = vec![w.key(0), w.key(3), w.key(1), w.key(4)];
    let mut s = screen();
    short(&mut s, &w, Key::Ok, 0);
    let kinds: Vec<_> = s.tiles().iter().map(|t| t.kind.clone()).collect();
    assert_eq!(
        kinds,
        [
            TileKind::Guide,
            TileKind::History,
            TileKind::Channel(w.key(3)),
            TileKind::Channel(w.key(1)),
            TileKind::Channel(w.key(4)),
        ]
    );
    let t = &s.tiles()[2];
    assert_eq!(t.label, "Show 15:00");
    assert_eq!(t.progress, Some(0.4));
    assert_eq!(t.channel.as_ref().unwrap().name, "Channel 4");
    assert_eq!(s.tiles()[0].label, "TV guide");
}

#[test]
fn the_tile_row_keeps_the_most_recent_channels() {
    let mut w = FakeData::new(T, RECENT_TILES + 3).with_schedule(|_, from, to| slots(from, to));
    w.playback.channel = Some(w.key(0));
    w.recents = w.channels.iter().map(|c| c.key).rev().collect();
    let mut s = screen();
    short(&mut s, &w, Key::Ok, 0);
    assert_eq!(s.tiles().len(), 2 + RECENT_TILES);
    assert_eq!(
        s.tiles()[2].kind,
        TileKind::Channel(w.key(RECENT_TILES + 2))
    );
}

#[test]
fn left_and_right_move_along_the_tiles_and_ok_switches() {
    let mut w = world();
    w.recents = vec![w.key(2), w.key(1)];
    let mut s = screen();
    short(&mut s, &w, Key::Ok, 0);
    short(&mut s, &w, Key::Left, 10);
    assert_eq!(s.overlay(), Overlay::Info { tile: 0 });
    for _ in 0..5 {
        short(&mut s, &w, Key::Right, 20);
    }
    assert_eq!(s.overlay(), Overlay::Info { tile: 3 });
    let out = short(&mut s, &w, Key::Ok, 30);
    assert_eq!(out.commands, [live(w.key(1))]);
    assert_eq!(s.overlay(), Overlay::Banner);
    assert_eq!(s.header().name, "Channel 2");
}

#[test]
fn guide_and_history_tiles_navigate() {
    let w = world();
    let mut s = screen();
    short(&mut s, &w, Key::Ok, 0);
    let out = short(&mut s, &w, Key::Ok, 10);
    assert_eq!(out.navigate, [Navigate::Guide]);
    assert_eq!(s.overlay(), Overlay::Hidden);
    short(&mut s, &w, Key::Ok, 20);
    short(&mut s, &w, Key::Right, 30);
    let out = short(&mut s, &w, Key::Ok, 40);
    assert_eq!(out.navigate, [Navigate::History]);
}

#[test]
fn the_panel_hides_after_the_timeout_and_keys_restart_it() {
    let w = world();
    let mut s = screen();
    short(&mut s, &w, Key::Ok, 1_000);
    assert_eq!(s.next_wake(1_000), Some(6_000));
    short(&mut s, &w, Key::Right, 4_000);
    let out = s.tick(&w, 6_000);
    assert!(!out.redraw);
    assert_eq!(s.overlay(), Overlay::Info { tile: 1 });
    let out = s.tick(&w, 9_000);
    assert!(out.redraw);
    assert_eq!(s.overlay(), Overlay::Hidden);
    assert_eq!(s.next_wake(9_000), None);
}

#[test]
fn settings_come_from_the_shared_keys() {
    use crate::panels::keys;
    let p = PlayerSettings::default();
    assert_eq!(
        (
            p.panel_timeout_ms,
            p.info_on_zap,
            p.skip_back_secs,
            p.skip_forward_secs
        ),
        (5_000, true, 10, 30)
    );
    let stored = crate::panels::Settings::from_stored([
        (keys::PLAYER_INFO_SECS, "8"),
        (keys::PLAYER_INFO_ON_ZAP, "false"),
        (keys::SKIP_BACK_SECS, "15"),
        (keys::SKIP_FORWARD_SECS, "120"),
    ]);
    let p = PlayerSettings::from_settings(&stored);
    assert_eq!(
        (
            p.panel_timeout_ms,
            p.info_on_zap,
            p.skip_back_secs,
            p.skip_forward_secs
        ),
        (8_000, false, 15, 120)
    );
}

#[test]
fn without_info_on_zap_a_zap_shows_nothing() {
    let mut w = world();
    let mut s = PlayerScreen::new(PlayerSettings {
        info_on_zap: false,
        ..PlayerSettings::default()
    });
    s.tick(&w, 0);
    assert_eq!(s.overlay(), Overlay::Hidden);
    short(&mut s, &w, Key::Ok, 0);
    assert_eq!(
        short(&mut s, &w, Key::ChannelUp, 10).commands,
        [live(w.key(1))]
    );
    assert_eq!(s.overlay(), Overlay::Hidden);
    assert_eq!(s.next_wake(10), None);
    w.playback.channel = Some(w.key(1));
    assert!(!s.tick(&w, 20).redraw);
    // a channel started elsewhere is not announced either, but the panels follow it
    w.playback.channel = Some(w.key(4));
    assert!(!s.tick(&w, 30).redraw);
    assert_eq!(s.overlay(), Overlay::Hidden);
    assert_eq!(s.header().name, "Channel 5");
}

#[test]
fn an_open_panel_refreshes_on_the_minute_only() {
    let mut w = world();
    let mut s = screen();
    short(&mut s, &w, Key::Ok, 0);
    w.now = T + 30;
    assert!(!s.tick(&w, 100).redraw);
    w.now = T + 60;
    assert!(s.tick(&w, 200).redraw);
    assert_eq!(s.header().clock, "Thu, Aug 27 15:13");
    assert_eq!(s.header().remaining, "17 min");
}

#[test]
fn a_hidden_player_is_idle() {
    let w = world();
    let mut s = screen();
    s.tick(&w, 0);
    s.tick(&w, 6_000);
    assert_eq!(s.overlay(), Overlay::Hidden);
    assert_eq!(s.next_wake(6_000), None);
    assert_eq!(s.tick(&w, 7_000), Output::default());
}

#[test]
fn a_channel_started_elsewhere_shows_the_banner() {
    let mut w = world();
    let mut s = screen();
    s.tick(&w, 0);
    s.tick(&w, 10_000);
    w.playback.channel = Some(w.key(4));
    let out = s.tick(&w, 11_000);
    assert!(out.redraw);
    assert_eq!(s.overlay(), Overlay::Banner);
    assert_eq!(s.header().name, "Channel 5");
}

// ---- playback panel (task 27) ----

fn playback_panel(w: &FakeData, s: &mut PlayerScreen) {
    short(s, w, Key::Ok, 0);
    short(s, w, Key::Up, 0);
    assert_eq!(s.overlay(), Overlay::Playback);
}

#[test]
fn up_opens_the_playback_panel_only_where_it_can_seek() {
    let mut w = world();
    w.playback.channel = Some(w.key(2)); // no archive
    let mut s = screen();
    short(&mut s, &w, Key::Ok, 0);
    short(&mut s, &w, Key::Up, 10);
    assert_eq!(s.overlay(), Overlay::Info { tile: 0 });
}

#[test]
fn taps_seek_by_the_skip_step_as_one_command() {
    let mut w = world();
    w.catchup(ON_AIR - 2 * SLOT, 600);
    let mut s = screen();
    playback_panel(&w, &mut s);
    assert_eq!(s.transport_focus(), Focus::Seek);
    let r = s.readout();
    assert_eq!((r.elapsed.as_str(), r.total.as_str()), ("10:00", "30:00"));
    // 14:10 plays at 15:12: one hour two minutes behind live.
    assert_eq!(r.offset, "[-1:02:00]");
    for i in 0..4 {
        let out = short(&mut s, &w, Key::Right, 100 * i);
        assert!(out.commands.is_empty());
    }
    let r = s.readout();
    assert_eq!(
        (r.elapsed.as_str(), r.offset.as_str()),
        ("12:00", "[+02:00]")
    );
    assert_eq!(r.knob, 0.4);
    assert_eq!(s.next_wake(300), Some(300 + COMMIT_MS));
    assert!(s.tick(&w, 300 + COMMIT_MS - 1).commands.is_empty());
    let out = s.tick(&w, 300 + COMMIT_MS);
    assert_eq!(out.commands, [Command::SeekTo { position_secs: 720 }]);
}

#[test]
fn a_held_key_seeks_faster_and_stays_inside_the_programme() {
    let mut w = world();
    w.catchup(ON_AIR - 2 * SLOT, 600);
    let mut s = screen();
    playback_panel(&w, &mut s);
    s.key(Key::Left, Press::Long, &w, 0);
    let back = PlayerSettings::default().skip_back_secs;
    assert_eq!(s.readout().elapsed, time::clock_secs(600 - FAST * back));
    for _ in 0..20 {
        s.key(Key::Left, Press::Repeat, &w, 0);
    }
    assert_eq!(s.readout().elapsed, "00:00");
    for _ in 0..30 {
        s.key(Key::Right, Press::Repeat, &w, 0);
    }
    let out = s.tick(&w, COMMIT_MS);
    // a finished programme stops short of its end
    let last = SLOT - transport::END_GUARD_SECS;
    assert_eq!(
        out.commands,
        [Command::SeekTo {
            position_secs: last
        }]
    );
}

#[test]
fn seeking_past_the_live_edge_returns_to_live() {
    let mut w = world();
    w.catchup(ON_AIR, 300);
    let mut s = screen();
    playback_panel(&w, &mut s);
    for _ in 0..30 {
        short(&mut s, &w, Key::Right, 0);
    }
    // the bar stops at the live edge: 12 minutes into the programme on air
    assert_eq!(s.readout().elapsed, "12:00");
    assert_eq!(s.tick(&w, COMMIT_MS).commands, [Command::ReturnToLive]);
}

#[test]
fn rewinding_live_starts_catch_up_and_forward_does_nothing() {
    let w = world();
    let mut s = screen();
    playback_panel(&w, &mut s);
    assert_eq!(s.readout().offset, "");
    assert_eq!(s.readout().elapsed, "12:00");
    short(&mut s, &w, Key::Right, 0);
    assert!(s.tick(&w, COMMIT_MS).commands.is_empty());
    short(&mut s, &w, Key::Left, 1_000);
    let out = s.tick(&w, 1_000 + COMMIT_MS);
    assert_eq!(out.commands, [archive(w.key(0), slot(ON_AIR), 720 - 10)]);
}

#[test]
fn transport_buttons_on_catch_up() {
    let mut w = world();
    w.catchup(ON_AIR - 2 * SLOT, 600);
    let mut s = screen();
    playback_panel(&w, &mut s);
    short(&mut s, &w, Key::Down, 0);
    assert_eq!(s.transport_focus(), Focus::Button(Button::PlayPause));
    assert_eq!(
        short(&mut s, &w, Key::Ok, 0).commands,
        [Command::TogglePause]
    );
    short(&mut s, &w, Key::Right, 0);
    short(&mut s, &w, Key::Right, 0);
    assert_eq!(s.transport_focus(), Focus::Button(Button::Next));
    assert_eq!(
        short(&mut s, &w, Key::Ok, 0).commands,
        [archive(w.key(0), slot(ON_AIR - SLOT), 0)]
    );
    short(&mut s, &w, Key::Right, 0);
    assert_eq!(
        short(&mut s, &w, Key::Ok, 0).commands,
        [Command::SeekTo { position_secs: 0 }]
    );
    short(&mut s, &w, Key::Right, 0);
    assert_eq!(s.transport_focus(), Focus::Button(Button::Live));
    assert_eq!(
        short(&mut s, &w, Key::Ok, 0).commands,
        [Command::ReturnToLive]
    );
    // the row ends there
    short(&mut s, &w, Key::Right, 0);
    assert_eq!(s.transport_focus(), Focus::Button(Button::Live));
    for _ in 0..6 {
        short(&mut s, &w, Key::Left, 0);
    }
    assert_eq!(s.transport_focus(), Focus::Button(Button::Prev));
    assert_eq!(
        short(&mut s, &w, Key::Ok, 0).commands,
        [archive(w.key(0), slot(ON_AIR - 3 * SLOT), 0)]
    );
    short(&mut s, &w, Key::Up, 0);
    assert_eq!(s.transport_focus(), Focus::Seek);
}

#[test]
fn live_with_an_archive_offers_restart_but_not_live_or_pause() {
    let w = world();
    let mut s = screen();
    playback_panel(&w, &mut s);
    for b in [
        Button::PlayPause,
        Button::FastForward,
        Button::Next,
        Button::Live,
    ] {
        assert!(!s.button_enabled(b), "{b:?}");
    }
    for b in [Button::Prev, Button::Rewind, Button::Restart] {
        assert!(s.button_enabled(b), "{b:?}");
    }
    short(&mut s, &w, Key::Down, 0);
    assert_eq!(s.transport_focus(), Focus::Button(Button::Prev));
    short(&mut s, &w, Key::Right, 0);
    short(&mut s, &w, Key::Right, 0);
    assert_eq!(s.transport_focus(), Focus::Button(Button::Restart));
    assert_eq!(
        short(&mut s, &w, Key::Ok, 0).commands,
        [archive(w.key(0), slot(ON_AIR), 0)]
    );
}

#[test]
fn back_drops_a_pending_seek_then_closes() {
    let mut w = world();
    w.catchup(ON_AIR - 2 * SLOT, 600);
    let mut s = screen();
    playback_panel(&w, &mut s);
    short(&mut s, &w, Key::Right, 0);
    short(&mut s, &w, Key::Back, 10);
    assert_eq!(s.overlay(), Overlay::Playback);
    assert_eq!(s.readout().elapsed, "10:00");
    assert!(s.tick(&w, 10 + COMMIT_MS).commands.is_empty());
    short(&mut s, &w, Key::Back, 20);
    assert_eq!(s.overlay(), Overlay::Hidden);
}

#[test]
fn media_keys_seek_and_pause_catch_up() {
    let mut w = world();
    w.catchup(ON_AIR - 2 * SLOT, 600);
    let mut s = screen();
    assert_eq!(
        short(&mut s, &w, Key::PlayPause, 0).commands,
        [Command::TogglePause]
    );
    short(&mut s, &w, Key::Rewind, 0);
    assert_eq!(s.overlay(), Overlay::Playback);
    assert_eq!(s.readout().offset, "[-00:10]");
    assert_eq!(
        s.tick(&w, COMMIT_MS).commands,
        [Command::SeekTo { position_secs: 590 }]
    );
}

#[test]
fn the_playback_panel_ticks_every_second() {
    let mut w = world();
    w.catchup(ON_AIR - 2 * SLOT, 600);
    let mut s = screen();
    playback_panel(&w, &mut s);
    assert_eq!(s.next_wake(0), Some(1_000));
    w.now += 1;
    w.playback.position_secs += 1;
    assert!(s.tick(&w, 1_000).redraw);
    assert_eq!(s.readout().elapsed, "10:01");
}

#[test]
fn ticks_read_the_guide_only_when_the_programme_changes() {
    let reads = Rc::new(Cell::new(0));
    let mut w = counted(Rc::clone(&reads));
    w.catchup(ON_AIR - 2 * SLOT, 600);
    let mut s = screen();
    playback_panel(&w, &mut s);
    let opened = reads.get();
    assert!(opened > 0);
    for i in 1..60 {
        w.now = T + i;
        w.playback.position_secs = 600 + i;
        assert!(s.tick(&w, i as u64).redraw);
    }
    assert_eq!(reads.get(), opened, "a seconds tick reads no guide");
    assert_eq!(s.readout().elapsed, "10:59");
    // Next: another catch-up programme plays
    w.catchup(ON_AIR - SLOT, 0);
    s.tick(&w, 60);
    assert!(reads.get() > opened);
    assert_eq!(s.header().title, "Show 14:30");

    // live: read again once the programme on air ends
    let reads = Rc::new(Cell::new(0));
    let mut w = counted(Rc::clone(&reads));
    let mut s = screen();
    short(&mut s, &w, Key::Ok, 0);
    let opened = reads.get();
    w.now = ON_AIR + SLOT - 1;
    assert!(s.tick(&w, 1_000).redraw);
    assert_eq!(reads.get(), opened);
    assert_eq!(s.header().remaining, "1 min");
    w.now = ON_AIR + SLOT;
    s.tick(&w, 2_000);
    assert!(reads.get() > opened);
    assert_eq!(s.header().title, "Show 15:30");
}

#[test]
fn next_wake_is_the_soonest_deadline() {
    let mut w = world();
    w.now = T + 25;
    let mut s = PlayerScreen::new(PlayerSettings {
        panel_timeout_ms: 60_000,
        ..PlayerSettings::default()
    });
    assert_eq!(s.next_wake(0), None);
    // 15:12:25: the clock turns over in 35 s, before the panel times out
    short(&mut s, &w, Key::Ok, 1_000);
    assert_eq!(s.next_wake(1_000), Some(36_000));
    short(&mut s, &w, Key::Back, 2_000);
    assert_eq!(s.next_wake(2_000), None);
    // a zap banner, then a channel number typed over it
    short(&mut s, &w, Key::Up, 3_000);
    assert_eq!(s.overlay(), Overlay::Banner);
    assert_eq!(s.next_wake(3_000), Some(38_000));
    short(&mut s, &w, Key::Digit(4), 4_000);
    assert_eq!(s.next_wake(4_000), Some(4_000 + crate::number::TIMEOUT_MS));
    s.tick(&w, 4_000 + crate::number::TIMEOUT_MS);
    assert_eq!(s.next_wake(6_000), Some(6_000 + 35_000));
}

// ---- Left and Right skip in full screen (task 71) ----

#[test]
fn left_and_right_skip_catch_up_as_one_seek() {
    let mut w = world();
    w.catchup(ON_AIR - 2 * SLOT, 600);
    let mut s = screen();
    for i in 0..3 {
        let out = short(&mut s, &w, Key::Left, 100 * i);
        assert!(out.commands.is_empty() && out.toast.is_none());
    }
    assert_eq!(s.overlay(), Overlay::Playback);
    assert_eq!(s.transport_focus(), Focus::Seek);
    assert_eq!(s.readout().offset, "[-00:30]");
    assert!(s.tick(&w, 200 + COMMIT_MS - 1).commands.is_empty());
    assert_eq!(
        s.tick(&w, 200 + COMMIT_MS).commands,
        [Command::SeekTo { position_secs: 570 }]
    );
    // Right by the forward step; held on the seek bar, faster.
    short(&mut s, &w, Key::Right, 1_000);
    assert_eq!(
        s.tick(&w, 1_000 + COMMIT_MS).commands,
        [Command::SeekTo { position_secs: 630 }]
    );
    short(&mut s, &w, Key::Right, 2_000);
    s.key(Key::Right, Press::Repeat, &w, 2_100);
    let forward = PlayerSettings::default().skip_forward_secs;
    assert_eq!(
        s.tick(&w, 2_100 + COMMIT_MS).commands,
        [Command::SeekTo {
            position_secs: 600 + forward + FAST * forward
        }]
    );
}

#[test]
fn left_on_live_plays_the_archive_from_that_far_behind_now() {
    let mut w = world();
    let mut s = screen();
    short(&mut s, &w, Key::Left, 0);
    assert_eq!(s.overlay(), Overlay::Playback);
    assert_eq!(s.readout().offset, "[-00:10]");
    // 15:12 is 12 minutes into the slot on air: the archive from 15:11:50.
    assert_eq!(
        s.tick(&w, COMMIT_MS).commands,
        [archive(w.key(0), slot(ON_AIR), 720 - 10)]
    );
    // Playing it, the panel says how far behind live it is.
    w.catchup(ON_AIR, 710);
    s.tick(&w, COMMIT_MS + 100);
    assert_eq!(s.overlay(), Overlay::Playback);
    assert_eq!(s.readout().offset, "[-00:10]");
    assert_eq!(s.readout().elapsed, "11:50");
}

#[test]
fn right_on_live_takes_back_a_pending_left_and_stays_live_at_the_edge() {
    let w = world();
    let mut s = screen();
    // Nothing ahead of live: the panel shows where live is, nothing is asked.
    short(&mut s, &w, Key::Right, 0);
    assert_eq!(s.overlay(), Overlay::Playback);
    assert_eq!(s.readout().elapsed, "12:00");
    assert!(s.tick(&w, COMMIT_MS).commands.is_empty());
    // Back four steps, forward one (30 s): ten seconds behind.
    for i in 0..4 {
        short(&mut s, &w, Key::Left, 1_000 + i);
    }
    short(&mut s, &w, Key::Right, 1_010);
    assert_eq!(
        s.tick(&w, 1_010 + COMMIT_MS).commands,
        [archive(w.key(0), slot(ON_AIR), 720 - 10)]
    );
    // Back and forward again before it is sent: still live.
    short(&mut s, &w, Key::Left, 3_000);
    short(&mut s, &w, Key::Right, 3_010);
    assert_eq!(s.readout().offset, "[+00:00]");
    assert!(s.tick(&w, 3_010 + COMMIT_MS).commands.is_empty());
}

#[test]
fn right_on_a_time_shift_returns_to_live_at_the_live_edge() {
    let mut w = world();
    let mut s = screen();
    // 10 s behind: the forward step reaches the edge.
    w.catchup(ON_AIR, 710);
    short(&mut s, &w, Key::Right, 0);
    assert_eq!(s.readout().elapsed, "12:00");
    assert_eq!(s.tick(&w, COMMIT_MS).commands, [Command::ReturnToLive]);
    // 30 s behind with a 25 s step: within the slack of the edge, live too. (The panel stays
    // up, following playback every second.)
    s.settings.skip_forward_secs = 25;
    w.catchup(ON_AIR, 690);
    s.tick(&w, 900);
    short(&mut s, &w, Key::Right, 1_000);
    assert_eq!(
        s.tick(&w, 1_000 + COMMIT_MS).commands,
        [Command::ReturnToLive]
    );
    // Further behind: a seek inside the time shift.
    w.catchup(ON_AIR, 600);
    s.tick(&w, 1_900);
    short(&mut s, &w, Key::Right, 2_000);
    assert_eq!(
        s.tick(&w, 2_000 + COMMIT_MS).commands,
        [Command::SeekTo { position_secs: 625 }]
    );
    // Back from the time shift never goes live.
    w.catchup(ON_AIR, 715);
    s.tick(&w, 2_900);
    short(&mut s, &w, Key::Left, 3_000);
    assert_eq!(
        s.tick(&w, 3_000 + COMMIT_MS).commands,
        [Command::SeekTo { position_secs: 705 }]
    );
}

#[test]
fn a_time_shift_past_its_programmes_end_still_reaches_live_and_plays_on() {
    // The programme ended seven seconds ago and the shift plays its last three seconds: Right
    // returns to live, not two seconds back.
    let mut w = world();
    w.now = ON_AIR + 7;
    w.catchup(ON_AIR - SLOT, SLOT - 3);
    let mut s = screen();
    short(&mut s, &w, Key::Right, 0);
    assert_eq!(s.readout().offset, "[+00:10]");
    assert_eq!(s.tick(&w, COMMIT_MS).commands, [Command::ReturnToLive]);
    // Five minutes behind: Right goes on into the programme on air, from the archive.
    let mut w = world();
    w.now = ON_AIR + 300;
    w.catchup(ON_AIR - SLOT, SLOT - 20);
    let mut s = screen();
    short(&mut s, &w, Key::Right, 0);
    assert_eq!(s.readout().elapsed, "30:00", "the knob at the end");
    assert_eq!(
        s.tick(&w, COMMIT_MS).commands,
        [archive(w.key(0), slot(ON_AIR), 10)]
    );
    // A programme the one on air does not follow keeps its end guard.
    let mut w = world();
    w.catchup(ON_AIR - 2 * SLOT, SLOT - 3);
    let mut s = screen();
    short(&mut s, &w, Key::Right, 0);
    assert_eq!(
        s.tick(&w, COMMIT_MS).commands,
        [Command::SeekTo {
            position_secs: SLOT - transport::END_GUARD_SECS
        }]
    );
}

#[test]
fn left_under_the_zap_banner_counts_from_now() {
    let mut w = world();
    let mut s = screen();
    short(&mut s, &w, Key::ChannelUp, 0);
    assert_eq!(s.overlay(), Overlay::Banner);
    w.playback.channel = Some(w.key(1));
    s.tick(&w, 100);
    // Four seconds later, the banner still up: ten seconds behind that moment.
    w.now = T + 4;
    short(&mut s, &w, Key::Left, 4_000);
    assert_eq!(
        s.tick(&w, 4_000 + COMMIT_MS).commands,
        [archive(w.key(1), slot(ON_AIR), 720 + 4 - 10)]
    );
}

#[test]
fn a_burst_is_sent_against_the_programme_it_started_in() {
    // Left in the last second of the programme on air, sent once the next one is on: its
    // archive, not live.
    let mut w = world();
    w.now = ON_AIR + SLOT - 1;
    let mut s = screen();
    short(&mut s, &w, Key::Left, 0);
    w.now = ON_AIR + SLOT;
    s.tick(&w, 500);
    assert_eq!(
        s.tick(&w, COMMIT_MS).commands,
        [archive(w.key(0), slot(ON_AIR), SLOT - 1 - 10)]
    );
    // Rewind in an archive programme, the next one playing by the time it is sent: the seek
    // lands in the one it was counted in.
    let mut w = world();
    w.catchup(ON_AIR - SLOT, 600);
    let mut s = screen();
    short(&mut s, &w, Key::Rewind, 0);
    w.catchup(ON_AIR, 0);
    s.tick(&w, 500);
    assert_eq!(
        s.tick(&w, COMMIT_MS).commands,
        [archive(w.key(0), slot(ON_AIR - SLOT), 590)]
    );
}

#[test]
fn left_and_right_without_catch_up_say_so_and_seek_nothing() {
    let mut w = world();
    w.playback.channel = Some(w.key(2)); // no archive
    let mut s = screen();
    for k in [Key::Left, Key::Right, Key::Rewind, Key::FastForward] {
        let out = short(&mut s, &w, k, 0);
        assert_eq!(
            out.toast.as_deref(),
            Some(msg::player_no_catchup()),
            "{k:?}"
        );
        assert!(out.commands.is_empty() && !out.redraw);
        assert_eq!(s.overlay(), Overlay::Hidden);
        assert!(s.tick(&w, COMMIT_MS).commands.is_empty());
    }
    // Held, they still open the channel list.
    assert_eq!(
        s.key(Key::Left, Press::Long, &w, 0).navigate,
        [Navigate::ChannelList { preview: false }]
    );
    // An archive, but no guide to seek in.
    let mut w = FakeData::new(T, 6).with_schedule(|_, _, _| Vec::new());
    w.playback.channel = Some(w.key(0));
    let out = short(&mut screen(), &w, Key::Left, 0);
    assert_eq!(out.toast.as_deref(), Some(msg::epg_catchup_unavailable()));
    // Nothing playing: nothing to say.
    w.playback.channel = None;
    assert_eq!(short(&mut screen(), &w, Key::Left, 0).toast, None);
}

#[test]
fn held_left_right_and_channel_keys_keep_the_moved_actions() {
    use crate::input::Hold;
    let mut w = world();
    w.recents = vec![w.key(0), w.key(4)];
    let mut s = screen();
    for k in [Key::Left, Key::Right, Key::ChannelUp, Key::ChannelDown] {
        assert_eq!(s.hold(k), Hold::Long, "{k:?}");
    }
    let held = |s: &mut PlayerScreen, k| s.key(k, Press::Long, &w, 0);
    assert_eq!(
        held(&mut s, Key::Left).navigate,
        [Navigate::ChannelList { preview: false }]
    );
    assert_eq!(
        held(&mut s, Key::ChannelDown).navigate,
        [Navigate::ChannelList { preview: true }]
    );
    assert_eq!(held(&mut s, Key::Right).commands, [live(w.key(4))]);
    let mut s = screen();
    assert!(held(&mut s, Key::ChannelUp).navigate.is_empty());
    assert_eq!(s.overlay(), Overlay::Card { index: 0 });
    // Short CH+ and CH- still zap.
    let mut s = screen();
    assert_eq!(
        short(&mut s, &w, Key::ChannelUp, 0).commands,
        [live(w.key(1))]
    );
    // Over the zap banner too: Left skips.
    assert_eq!(s.overlay(), Overlay::Banner);
    short(&mut s, &w, Key::Left, 10);
    assert_eq!(s.overlay(), Overlay::Playback);
}

// ---- card, zap, last channel, number entry (task 29) ----

#[test]
fn long_ch_plus_opens_the_programme_card_and_right_steps_on() {
    let w = world();
    let mut s = screen();
    s.key(Key::ChannelUp, Press::Long, &w, 0);
    assert_eq!(s.overlay(), Overlay::Card { index: 0 });
    let c = s.card();
    assert_eq!(c.channel, "1  Channel 1");
    assert_eq!(c.title, "Show 15:00");
    assert_eq!(c.times, "15:00 \u{2014} 15:30");
    assert_eq!(c.duration, "30 min");
    assert_eq!(c.progress, Some(0.4));
    assert_eq!(c.category.as_deref(), Some("News"));
    short(&mut s, &w, Key::Right, 0);
    s.key(Key::Right, Press::Long, &w, 0);
    assert_eq!(s.overlay(), Overlay::Card { index: 2 });
    assert_eq!(s.card().title, "Show 16:00");
    assert_eq!(s.card().progress, None);
    short(&mut s, &w, Key::Left, 0);
    assert_eq!(s.card().title, "Show 15:30");
    // the card stays until Back
    assert_eq!(s.next_wake(0), Some(60_000));
    s.tick(&w, 60_000);
    assert_eq!(s.overlay(), Overlay::Card { index: 1 });
    short(&mut s, &w, Key::Back, 0);
    assert_eq!(s.overlay(), Overlay::Hidden);
}

#[test]
fn the_card_starts_at_the_catch_up_programme() {
    let mut w = world();
    w.catchup(ON_AIR - 4 * SLOT, 60);
    let mut s = screen();
    s.key(Key::ChannelUp, Press::Long, &w, 0);
    assert_eq!(s.card().title, "Show 13:00");
}

#[test]
fn up_down_and_channel_keys_zap_through_the_group() {
    let mut w = world();
    let mut s = screen();
    let news = Some(Group::Playlist("News".into())); // channels 1 and 4
    s.zap_through(news, Some(w.key(0)), &w);
    assert_eq!(short(&mut s, &w, Key::Up, 0).commands, [live(w.key(3))]);
    assert_eq!(s.overlay(), Overlay::Banner);
    assert_eq!(s.header().name, "Channel 4");
    // the app has not switched yet: the next zap starts from the pending channel and wraps
    assert_eq!(
        short(&mut s, &w, Key::ChannelUp, 10).commands,
        [live(w.key(0))]
    );
    w.playback.channel = Some(w.key(0));
    s.tick(&w, 20);
    assert_eq!(short(&mut s, &w, Key::Down, 30).commands, [live(w.key(3))]);
    assert_eq!(
        short(&mut s, &w, Key::ChannelDown, 40).commands,
        [live(w.key(0))]
    );
}

#[test]
fn a_channel_from_outside_the_group_zaps_through_every_channel() {
    let mut w = world();
    let mut s = screen();
    let news = Some(Group::Playlist("News".into())); // channels 1 and 4
    s.zap_through(news, Some(w.key(0)), &w);
    w.recents = vec![w.key(0), w.key(3)];
    // the last channel is in the group: zapping stays in it
    let last = s.key(Key::Right, Press::Long, &w, 0);
    assert_eq!(last.commands, [live(w.key(3))]);
    assert_eq!(s.group(), &Group::Playlist("News".into()));
    // channel 3 is not: zapping goes on through every channel, from it
    short(&mut s, &w, Key::Digit(3), 10);
    assert_eq!(short(&mut s, &w, Key::Ok, 20).commands, [live(w.key(2))]);
    assert_eq!(s.group(), &Group::All);
    assert_eq!(s.header().group, "All channels");
    assert_eq!(short(&mut s, &w, Key::Up, 30).commands, [live(w.key(3))]);
}

#[test]
fn a_group_picked_over_the_player_is_zapped_through_while_it_lists_the_channel() {
    let w = world();
    let mut s = screen();
    let news = Group::Playlist("News".into()); // channels 1 and 4; channel 1 plays
    s.message(ScreenMessage::SetGroup(news.clone()), &w);
    assert_eq!(s.group(), &news);
    s.message(ScreenMessage::SetGroup(Group::Playlist("Sport".into())), &w);
    assert_eq!(s.group(), &Group::All, "Sport has no channel 1");
}

#[test]
fn long_right_toggles_the_last_channel() {
    let mut w = world();
    let mut s = screen();
    let long_right = |s: &mut PlayerScreen, w: &FakeData, ms| s.key(Key::Right, Press::Long, w, ms);
    assert!(long_right(&mut s, &w, 0).commands.is_empty());
    w.recents = vec![w.key(0), w.key(4)];
    assert_eq!(long_right(&mut s, &w, 0).commands, [live(w.key(4))]);
    w.playback.channel = Some(w.key(4));
    w.recents = vec![w.key(4), w.key(0)];
    s.tick(&w, 10);
    assert_eq!(long_right(&mut s, &w, 20).commands, [live(w.key(0))]);
}

#[test]
fn back_opens_the_guide_and_other_keys_navigate() {
    let w = world();
    let mut s = screen();
    let nav = |s: &mut PlayerScreen, k, p| s.key(k, p, &w, 0).navigate;
    assert_eq!(nav(&mut s, Key::Back, Press::Short), [Navigate::Guide]);
    assert_eq!(
        nav(&mut s, Key::Left, Press::Long),
        [Navigate::ChannelList { preview: false }]
    );
    assert_eq!(
        nav(&mut s, Key::ChannelDown, Press::Long),
        [Navigate::ChannelList { preview: true }]
    );
    assert_eq!(nav(&mut s, Key::Up, Press::Long), [Navigate::History]);
    assert_eq!(
        nav(&mut s, Key::Ok, Press::Long),
        [Navigate::ContextMenu {
            channel: w.key(0),
            programme: None,
        }]
    );
    short(&mut s, &w, Key::Ok, 0);
    assert_eq!(nav(&mut s, Key::Down, Press::Short), [Navigate::PlayerMenu]);
    assert_eq!(s.overlay(), Overlay::Hidden);
    assert_eq!(nav(&mut s, Key::Down, Press::Long), [Navigate::Settings]);
}

#[test]
fn long_down_opens_settings_and_back_is_never_held() {
    use crate::input::Hold;
    let w = world();
    let mut s = screen();
    // Down acts on release, as Up does: short zaps, long opens Settings. Back acts at once.
    assert_eq!(s.hold(Key::Down), Hold::Long);
    assert_eq!(s.hold(Key::Back), Hold::Once);
    assert_eq!(short(&mut s, &w, Key::Down, 0).commands, [live(w.key(5))]);
    let out = s.key(Key::Down, Press::Long, &w, 0);
    assert!(out.commands.is_empty());
    assert_eq!(out.navigate, [Navigate::Settings]);
    // With a panel up Down keeps its own meaning.
    short(&mut s, &w, Key::Ok, 0);
    assert_eq!(s.hold(Key::Down), Hold::Once);
    assert_eq!(
        short(&mut s, &w, Key::Down, 0).navigate,
        [Navigate::PlayerMenu]
    );
}

#[test]
fn digits_type_a_channel_number_that_plays_after_the_timeout() {
    let w = world();
    let mut s = screen();
    short(&mut s, &w, Key::Ok, 0);
    short(&mut s, &w, Key::Digit(0), 100);
    short(&mut s, &w, Key::Digit(5), 200);
    assert_eq!(s.overlay(), Overlay::Hidden);
    let n = s.number().unwrap();
    assert_eq!(n.digits, "05");
    assert_eq!(n.preview.as_deref(), Some("Channel 5"));
    assert_eq!(s.next_wake(200), Some(2_200));
    assert!(s.tick(&w, 2_199).commands.is_empty());
    let out = s.tick(&w, 2_200);
    assert_eq!(out.commands, [live(w.key(4))]);
    assert!(s.number().is_none());
    assert_eq!(s.overlay(), Overlay::Banner);
}

#[test]
fn ok_commits_a_number_back_cancels_and_unknown_numbers_do_nothing() {
    let w = world();
    let mut s = screen();
    short(&mut s, &w, Key::Digit(3), 0);
    let out = short(&mut s, &w, Key::Ok, 10);
    assert_eq!(out.commands, [live(w.key(2))]);
    assert_eq!(s.overlay(), Overlay::Banner);

    short(&mut s, &w, Key::Digit(4), 20);
    let out = short(&mut s, &w, Key::Back, 30);
    assert!(out.commands.is_empty() && out.navigate.is_empty());
    assert!(s.number().is_none());

    for d in [9, 9, 9, 9, 9] {
        short(&mut s, &w, Key::Digit(d), 40);
    }
    assert_eq!(s.number().unwrap().digits, "9999");
    assert_eq!(s.number().unwrap().preview, None);
    assert!(s.tick(&w, 10_000).commands.is_empty());
}

#[test]
fn declared_numbers_win_over_playlist_positions() {
    let mut w = world();
    w.channels[3].number = Some(101);
    let mut s = screen();
    for d in [1, 0, 1] {
        short(&mut s, &w, Key::Digit(d), 0);
    }
    assert_eq!(s.tick(&w, 5_000).commands, [live(w.key(3))]);
    // position 4 is taken by nothing now
    short(&mut s, &w, Key::Digit(4), 6_000);
    assert_eq!(s.number().unwrap().preview, None);
}

#[test]
fn up_on_the_seek_bar_stays_in_the_panel() {
    let w = world();
    let mut s = screen();
    playback_panel(&w, &mut s);
    let out = short(&mut s, &w, Key::Up, 0);
    assert!(out.commands.is_empty());
    assert_eq!(s.overlay(), Overlay::Playback);
}

#[test]
fn keys_ask_the_input_layer_for_long_press_or_repeat() {
    use crate::input::Hold;
    let w = world();
    let mut s = screen();
    assert_eq!(s.hold(Key::Right), Hold::Long);
    assert_eq!(s.hold(Key::Down), Hold::Long);
    assert_eq!(s.hold(Key::Back), Hold::Once);
    short(&mut s, &w, Key::Ok, 0);
    assert_eq!(s.hold(Key::Right), Hold::Repeat);
    assert_eq!(s.hold(Key::ChannelUp), Hold::Once);
    assert_eq!(s.hold(Key::Ok), Hold::Long);
    short(&mut s, &w, Key::Up, 0);
    assert_eq!(s.hold(Key::Left), Hold::Repeat);
    short(&mut s, &w, Key::Down, 0);
    assert_eq!(s.hold(Key::Left), Hold::Once);
}

// ---- view ----

fn texts(pieces: &[Piece]) -> Vec<String> {
    pieces
        .iter()
        .filter_map(|p| match p {
            Piece::Text { s, .. } => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// Every piece lies wholly inside the 1920 x 1080 frame, its bottom edge included.
fn on_screen(pieces: &[Piece]) {
    use tv_ui::consts::{SCR_H, SCR_W};
    for p in pieces {
        let r = match p {
            Piece::Fill { r, .. } | Piece::Icon { r, .. } | Piece::Logo { r, .. } => *r,
            Piece::Text {
                s, x, y, size, h, ..
            } => {
                let w = Mono.width(s, *size, false);
                let left = match h {
                    HAlign::Left => *x,
                    HAlign::Center => x - w * 0.5,
                    HAlign::Right => x - w,
                };
                tv_ui::Rect::new(left, *y, w, Mono.line_h(*size))
            }
        };
        assert!(
            r.x >= 0.0 && r.y >= 0.0 && r.x + r.w <= SCR_W && r.y + r.h <= SCR_H,
            "{p:?}"
        );
    }
}

fn logos(pieces: &[Piece]) -> usize {
    pieces
        .iter()
        .filter(|p| matches!(p, Piece::Logo { .. }))
        .count()
}

#[test]
fn the_playback_panel_hides_the_tile_row_and_fits_the_screen() {
    let mut w = world();
    w.recents = vec![w.key(1), w.key(2), w.key(3)];
    w.catchup(ON_AIR - 2 * SLOT, 600);
    let mut s = screen();
    short(&mut s, &w, Key::Ok, 0);
    let info = s.layout(&Mono);
    assert!(texts(info).iter().any(|x| x == "TV guide"));
    assert_eq!(
        logos(info),
        1 + 3,
        "the header logo and the recent channels"
    );
    on_screen(info);

    short(&mut s, &w, Key::Up, 0);
    short(&mut s, &w, Key::Right, 0);
    assert_eq!(s.overlay(), Overlay::Playback);
    let playback = s.layout(&Mono);
    let t = texts(playback);
    for gone in ["TV guide", "History"] {
        assert!(!t.iter().any(|x| x == gone), "{gone} is drawn: {t:?}");
    }
    assert_eq!(logos(playback), 1, "only the header logo");
    assert!(t.iter().any(|x| x == "[+00:30]"), "{t:?}");
    on_screen(playback);

    // Down to the buttons, Down again: the info panel and its row are back.
    short(&mut s, &w, Key::Down, 0);
    short(&mut s, &w, Key::Down, 0);
    assert_eq!(s.overlay(), Overlay::Info { tile: 0 });
    let info = s.layout(&Mono);
    assert!(texts(info).iter().any(|x| x == "TV guide"));
    assert_eq!(logos(info), 1 + 3);
}

#[test]
fn the_info_panel_lays_out_on_screen_with_every_fact() {
    let mut w = world();
    w.recents = vec![w.key(1), w.key(2), w.key(3), w.key(4), w.key(5)];
    let mut s = screen();
    assert!(s.layout(&Mono).is_empty());
    short(&mut s, &w, Key::Ok, 0);
    let pieces = s.layout(&Mono);
    let t = texts(pieces);
    for want in [
        "All channels",
        "Thu, Aug 27 15:12",
        "Show 15:00",
        "15:00 \u{2014} 15:30",
        "18 min",
        "1",
        "Channel 1",
        "FULL HD",
        "25 FPS",
        "STEREO",
        "15:30 \u{2014} 16:00",
        "Show 15:30",
        "TV guide",
        "History",
    ] {
        assert!(t.iter().any(|x| x == want), "{want} missing from {t:?}");
    }
    let logos: Vec<_> = pieces
        .iter()
        .filter_map(|p| match p {
            Piece::Logo {
                channel, fallback, ..
            } => Some((
                channel.as_ref().map(|c| c.name.clone()),
                fallback.to_string_lossy().into_owned(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        logos.len(),
        1 + 5,
        "the header logo and one per recent channel"
    );
    assert_eq!(logos[0], (Some("Channel 1".into()), "C1".into()));
    assert_eq!(logos[1].1, "C2");
    // the banner's description is not part of the info panel
    assert!(!t.iter().any(|x| x.contains("About")));
    on_screen(pieces);
}

#[test]
fn the_view_lays_out_again_only_when_the_screen_changed() {
    let w = world();
    let mut s = screen();
    short(&mut s, &w, Key::Ok, 0);
    let first = s.layout(&Mono).to_vec();
    let generation = s.generation();
    assert!(!s.tick(&w, 100).redraw);
    assert_eq!(s.generation(), generation);
    assert_eq!(s.layout(&Mono), first.as_slice());
    short(&mut s, &w, Key::Right, 200);
    assert_ne!(s.generation(), generation);
    assert_ne!(s.layout(&Mono), first.as_slice());
    assert!(matches!(&s.layout(&Mono)[0], Piece::Fill { .. }));
}

#[test]
fn banner_playback_card_and_number_lay_out_on_screen() {
    let mut w = world();
    w.catchup(ON_AIR - 2 * SLOT, 600);
    let mut s = screen();

    s.tick(&w, 0);
    assert_eq!(s.overlay(), Overlay::Banner);
    assert!(texts(s.layout(&Mono))
        .iter()
        .any(|x| x == "About the show."));
    on_screen(s.layout(&Mono));

    short(&mut s, &w, Key::Ok, 0);
    short(&mut s, &w, Key::Up, 0);
    short(&mut s, &w, Key::Right, 0);
    let t = texts(s.layout(&Mono));
    for want in ["10:30", " / ", "30:00", "[+00:30]"] {
        assert!(t.iter().any(|x| x == want), "{want} missing from {t:?}");
    }
    on_screen(s.layout(&Mono));

    short(&mut s, &w, Key::Back, 0);
    short(&mut s, &w, Key::Back, 0);
    s.key(Key::ChannelUp, Press::Long, &w, 0);
    assert!(texts(s.layout(&Mono)).iter().any(|x| x == "Show 14:00"));
    on_screen(s.layout(&Mono));

    short(&mut s, &w, Key::Back, 0);
    short(&mut s, &w, Key::Digit(4), 0);
    assert_eq!(texts(s.layout(&Mono)), ["4", "Channel 4"]);
    on_screen(s.layout(&Mono));
}

#[test]
fn long_text_is_fitted_to_the_panel() {
    let mut w = world();
    w.channels[0].name = "A very long channel name ".repeat(8);
    let mut s = screen();
    s.key(Key::Right, Press::Long, &w, 0);
    on_screen(s.layout(&Mono));
    short(&mut s, &w, Key::Back, 0);
    short(&mut s, &w, Key::Ok, 0);
    on_screen(s.layout(&Mono));
}

/// `s` and `w` at `secs` after T: the world's clock and the shell's milliseconds together.
fn at(s: &mut PlayerScreen, w: &mut FakeData, secs: i64) -> u64 {
    w.now = T + secs;
    let ms = secs as u64 * 1000;
    s.tick(w, ms);
    ms
}

/// The app opening a seek of the slot on air at `target`: the archive starts `early` seconds
/// before it, shows its first frame two seconds after the press at `pressed`, and plays on until
/// `until`, where the panel reads it.
fn opens(
    s: &mut PlayerScreen,
    w: &mut FakeData,
    target: i64,
    early: i64,
    pressed: i64,
    until: i64,
) {
    w.catchup(ON_AIR, target - early);
    at(s, w, pressed + 1);
    at(s, w, pressed + 2);
    w.catchup(ON_AIR, target - early + (until - pressed - 2));
    at(s, w, until);
}

#[test]
fn back_taps_one_at_a_time_and_one_forward_return_to_live_past_the_archives_lag() {
    // The C1 on 2026-10-19: three Lefts a few seconds apart, each its own seek, then one Right.
    // Every seek opened on a segment start before the moment it asked for and showed its first
    // frame two seconds after the press, so the time shift trailed live by more than the 30 s
    // asked, and the Right (30 s) landed outside LIVE_SLACK_SECS of the edge: it seeked instead
    // of going live.
    let mut w = world();
    let mut s = screen();
    let ms = at(&mut s, &mut w, 0);
    short(&mut s, &w, Key::Left, ms);
    let out = s.tick(&w, ms + COMMIT_MS);
    assert_eq!(out.commands, [archive(w.key(0), slot(ON_AIR), 710)]);
    opens(&mut s, &mut w, 710, 4, 0, 4);
    assert_eq!(
        s.transport.lag.secs(),
        6,
        "4 s early, 2 s to the first frame"
    );

    for (pressed, target, early) in [(4, 698, 5), (8, 685, 1)] {
        let ms = pressed as u64 * 1000;
        short(&mut s, &w, Key::Left, ms);
        let out = s.tick(&w, ms + COMMIT_MS);
        assert_eq!(
            out.commands,
            [Command::SeekTo {
                position_secs: target
            }]
        );
        opens(&mut s, &mut w, target, early, pressed, pressed + 4);
    }
    assert_eq!(s.transport.lag.secs(), 6 + 7 + 3);
    let edge = w.now - ON_AIR;
    assert_eq!(
        edge - w.playback.position_secs,
        30 + 16,
        "30 s asked, 16 s of lag"
    );

    // Right lands 16 s short of the edge, as far as the archive put playback back: live.
    let ms = 12_000;
    short(&mut s, &w, Key::Right, ms);
    assert!(edge - (w.playback.position_secs + 30) > transport::LIVE_SLACK_SECS);
    assert_eq!(s.tick(&w, ms + COMMIT_MS).commands, [Command::ReturnToLive]);
    w.playback.catchup = None;
    at(&mut s, &mut w, 14);
    assert_eq!(s.transport.lag.secs(), 0, "live ends the time shift");
}

#[test]
fn a_pause_is_the_viewers_and_does_not_count_as_lag() {
    let mut w = world();
    let mut s = screen();
    let ms = at(&mut s, &mut w, 0);
    short(&mut s, &w, Key::Left, ms);
    s.tick(&w, ms + COMMIT_MS);
    opens(&mut s, &mut w, 710, 4, 0, 4);
    assert_eq!(s.transport.lag.secs(), 6);
    // A minute paused: 76 s behind live, 6 of them the archive's.
    w.playback.position_secs = 709;
    w.playback.paused = true;
    at(&mut s, &mut w, 5);
    at(&mut s, &mut w, 64);
    w.playback.paused = false;
    at(&mut s, &mut w, 65);
    assert_eq!(s.transport.lag.secs(), 6);
    assert_eq!(w.now - ON_AIR - w.playback.position_secs, 10 + 6 + 60);
    // Right (30 s) still leaves 46 s behind: a seek, not live.
    short(&mut s, &w, Key::Right, 65_000);
    assert_eq!(
        s.tick(&w, 65_000 + COMMIT_MS).commands,
        [Command::SeekTo {
            position_secs: w.playback.position_secs + 30
        }]
    );
}

#[test]
fn a_clock_set_back_before_the_bursts_programme_keeps_the_seek_inside_it() {
    // Left on live starts a burst in the programme on air, and Prev plays the one before it
    // while the burst waits. Then the clock is set back (an NTP correction after boot) into the
    // earlier programme: the burst's own programme has not started by the new clock.
    let mut w = world();
    let mut s = screen();
    let ms = at(&mut s, &mut w, 0);
    short(&mut s, &w, Key::Left, ms);
    short(&mut s, &w, Key::Down, ms + 100);
    assert_eq!(s.transport_focus(), Focus::Button(Button::Prev));
    let out = short(&mut s, &w, Key::Ok, ms + 200);
    assert_eq!(out.commands, [archive(w.key(0), slot(ON_AIR - SLOT), 0)]);
    w.catchup(ON_AIR - SLOT, 0);
    w.now = ON_AIR - 60;
    s.tick(&w, ms + 300);
    short(&mut s, &w, Key::Up, ms + 400);
    short(&mut s, &w, Key::Left, ms + 500);
    assert_eq!(s.transport.shown_position(&s.ctx), 0);
}

#[test]
fn time_away_from_the_app_never_counts_as_the_archives_lag() {
    // Left on live and the panel hidden at once, then the app in the background before the
    // archive showed its first frame. Five minutes later it plays on where it was kept, and
    // Right asks for 30 s on: 300 s behind live is the viewer's, not the archive's.
    let mut w = world();
    let mut s = screen();
    let ms = at(&mut s, &mut w, 0);
    short(&mut s, &w, Key::Left, ms);
    let out = s.tick(&w, ms + COMMIT_MS);
    assert_eq!(out.commands, [archive(w.key(0), slot(ON_AIR), 710)]);
    short(&mut s, &w, Key::Back, ms + COMMIT_MS + 100);
    assert_eq!(s.overlay(), Overlay::Hidden);
    for (secs, pos) in [(300, 706), (301, 707), (302, 708)] {
        w.catchup(ON_AIR, pos);
        at(&mut s, &mut w, secs);
    }
    short(&mut s, &w, Key::Right, 302_000);
    assert_eq!(
        s.tick(&w, 302_000 + COMMIT_MS).commands,
        [Command::SeekTo { position_secs: 738 }]
    );
}

#[test]
fn the_archives_lag_is_read_behind_a_hidden_panel() {
    // Four Lefts on live (40 s) and Back at once: the archive opens 4 s early with its first
    // frame 2 s after the press, behind the hidden panel. A minute later Right (30 s) lands
    // 16 s behind live, all of it past what was asked the archive's: live.
    let mut w = world();
    let mut s = screen();
    let ms = at(&mut s, &mut w, 0);
    for i in 0..4 {
        short(&mut s, &w, Key::Left, ms + i * 100);
    }
    let out = s.tick(&w, ms + 300 + COMMIT_MS);
    assert_eq!(out.commands, [archive(w.key(0), slot(ON_AIR), 680)]);
    short(&mut s, &w, Key::Back, ms + 1_100);
    assert_eq!(s.overlay(), Overlay::Hidden);
    opens(&mut s, &mut w, 680, 4, 0, 4);
    w.playback.position_secs += 1;
    at(&mut s, &mut w, 5);
    assert_eq!(s.transport.lag.secs(), 6);
    w.playback.position_secs += 55;
    at(&mut s, &mut w, 60);
    short(&mut s, &w, Key::Right, 60_000);
    assert_eq!(
        s.tick(&w, 60_000 + COMMIT_MS).commands,
        [Command::ReturnToLive]
    );
}
