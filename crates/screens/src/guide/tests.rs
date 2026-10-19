//! Host tests of the guide with a fixed clock: remote behaviour, the window, the groups panel,
//! Manage favorites, where the video goes, and the bounds the virtualized cache keeps with a
//! full-size playlist.

use std::cell::Cell;
use std::rc::Rc;

use iptv::model::{Channel, ChannelKey, Programme, Timestamp};
use tv_machine::machine::Measure;
use tv_ui::Painter;

use super::clock::{self, HALF_HOUR};
use super::geom;
use super::grid::Anchor;
use super::*;
use crate::data::fake::{FakeData, Mono};
use crate::data::{Command, Stream};
use crate::draw::Piece;
use crate::theme::size;
use crate::time::{DAY, MINUTE};

/// 2026-10-19 13:11:00 UTC; 15:11 local at `OFFSET`.
const NOW: Timestamp = 1_792_415_460;
const OFFSET: i32 = 7200;
/// A day boundary well before any archive, so the schedule is the same whatever is asked.
const ORIGIN: Timestamp = NOW - 13 * 3600 - 11 * 60 - 10 * DAY;
/// Programme lengths in minutes; each channel starts the cycle at its own phase.
const LENGTHS: [i64; 5] = [30, 60, 15, 90, 45];
const CYCLE: i64 = 240 * MINUTE;

/// The schedule of channel `phase` overlapping `[from, to)`.
fn schedule(phase: usize, from: Timestamp, to: Timestamp) -> Vec<Programme> {
    let mut start = ORIGIN + (from - ORIGIN).div_euclid(CYCLE) * CYCLE;
    let mut i = phase;
    let mut out = Vec::new();
    while start < to {
        let stop = start + LENGTHS[i % LENGTHS.len()] * MINUTE;
        if stop > from {
            out.push(Programme {
                start,
                stop,
                title: format!("P{phase} {}", time::hhmm(start, OFFSET)),
                description: Some(
                    "A synthetic programme with a description long enough to wrap over more \
                     than three lines of the details panel so the last line is cut short. "
                        .repeat(3),
                ),
                category: None,
                icon: None,
            });
        }
        start = stop;
        i += 1;
    }
    out
}

/// `count` channels at `OFFSET`, each on the absolute [`schedule`] (the same programmes whatever
/// window is asked) except those in `no_epg`; every guide read counts in `reads`.
fn guide_data(count: usize, no_epg: &[usize], reads: Rc<Cell<usize>>) -> FakeData {
    let no_epg = no_epg.to_vec();
    let mut data = FakeData::new(NOW, count).with_schedule(move |i, from, to| {
        reads.set(reads.get() + 1);
        if no_epg.contains(&i) {
            vec![]
        } else {
            schedule(i, from, to)
        }
    });
    data.utc_offset = OFFSET;
    data
}

fn data(count: usize) -> FakeData {
    guide_data(count, &[], Rc::default())
}

fn key(data: &FakeData, i: usize) -> ChannelKey {
    data.channels[i].key
}

/// A guide opened on channel `playing` (row index in All).
fn opened(data: &mut FakeData, playing: usize) -> GuideScreen {
    data.playback.channel = Some(key(data, playing));
    let s = GuideScreen::new(data, Group::All);
    assert_eq!(s.video_rect(), Some(geom::preview_rect()));
    s
}

fn press(s: &mut GuideScreen, data: &FakeData, key: Key) -> Output {
    Screen::key(s, key, Press::Short, data, 0)
}

fn long(s: &mut GuideScreen, data: &FakeData, key: Key) -> Output {
    Screen::key(s, key, Press::Long, data, 0)
}

/// Whether a tick asks for a frame.
fn tick(s: &mut GuideScreen, data: &FakeData) -> bool {
    Screen::tick(s, data, 0).redraw
}

/// Neither a command nor another screen.
fn quiet(o: &Output) -> bool {
    o.commands.is_empty() && o.navigate.is_empty()
}

/// `stream` from the guide's group, All.
fn play(stream: Stream) -> Command {
    Command::Play {
        stream,
        group: Some(Group::All),
    }
}

fn focus(s: &GuideScreen) -> Programme {
    s.grid.focused_programme().expect("a focused programme")
}

fn on_air(p: &Programme) -> bool {
    p.is_on_air(NOW)
}

/// Logos for every other channel, so drawing takes both the texture and the fallback.
struct SomeLogos;

impl Logos for SomeLogos {
    fn logo(&self, c: &Channel) -> Option<(u32, f32, f32)> {
        c.number.filter(|n| n % 2 == 0).map(|_| (7, 160.0, 90.0))
    }
}

fn draw(s: &mut GuideScreen) {
    Screen::draw(s, Painter::recording(), &Mono, &SomeLogos);
}

fn texts(pieces: &[Piece]) -> Vec<String> {
    pieces
        .iter()
        .filter_map(|p| match p {
            Piece::Text { s, .. } => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

#[test]
fn opens_on_the_playing_channel_at_live() {
    let mut data = data(12);
    let s = opened(&mut data, 4);
    assert_eq!(s.grid.channel().unwrap().key, key(&data, 4));
    assert!(on_air(&focus(&s)));
    assert_eq!(s.grid.window, clock::floor_half_hour(NOW, OFFSET));
    assert_eq!(time::hhmm(s.grid.window, OFFSET), "15:00");
    assert!(!s.panel);
}

#[test]
fn up_and_down_wrap_and_keep_live() {
    let mut data = data(12);
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Up);
    assert_eq!(s.grid.channel().unwrap().key, key(&data, 11));
    assert!(on_air(&focus(&s)));
    press(&mut s, &data, Key::Down);
    assert_eq!(s.grid.channel().unwrap().key, key(&data, 0));
    for i in 1..12 {
        press(&mut s, &data, Key::Down);
        let c = s.grid.cursor;
        assert_eq!(c.index, i);
        assert!(on_air(&focus(&s)), "row {i} stays on the live column");
        assert!(c.index >= c.top && c.index < c.top + geom::VISIBLE_ROWS - 1);
    }
}

#[test]
fn up_and_down_keep_the_time_column() {
    let mut data = data(12);
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Right);
    press(&mut s, &data, Key::Right);
    let anchor = focus(&s).start;
    assert_eq!(s.grid.anchor, Anchor::At(anchor));
    for _ in 0..5 {
        press(&mut s, &data, Key::Down);
        let p = focus(&s);
        assert!(p.start <= anchor && anchor < p.stop, "{p:?} holds {anchor}");
    }
    assert_eq!(
        s.grid.anchor,
        Anchor::At(anchor),
        "vertical moves keep the anchor"
    );
}

#[test]
fn right_walks_the_future_and_pages_the_window() {
    let mut data = data(6);
    let mut s = opened(&mut data, 0);
    let mut last = focus(&s);
    for _ in 0..40 {
        press(&mut s, &data, Key::Right);
        let p = focus(&s);
        assert_eq!(p.start, last.stop, "contiguous schedule: the next one");
        let w = s.grid.window;
        assert_eq!(
            clock::floor_half_hour(w, OFFSET),
            w,
            "window on a half hour"
        );
        assert!(p.start >= w && p.start <= w + geom::window_span() - geom::WINDOW_TAIL);
        last = p;
    }
    assert!(last.start > NOW + DAY / 2);
}

#[test]
fn left_steps_back_through_live_into_the_past() {
    let mut data = data(6);
    let mut s = opened(&mut data, 1);
    let live = focus(&s);
    press(&mut s, &data, Key::Right);
    press(&mut s, &data, Key::Right);
    press(&mut s, &data, Key::Left);
    press(&mut s, &data, Key::Left);
    assert_eq!(focus(&s), live);
    assert_eq!(s.grid.anchor, Anchor::Live);
    let out = press(&mut s, &data, Key::Left);
    assert!(!s.panel, "Left never opens the groups panel");
    assert!(quiet(&out) && out.redraw);
    let p = focus(&s);
    assert_eq!(p.stop, live.start, "the programme before the live one");
    assert_eq!(s.grid.anchor, Anchor::At(p.start));
    assert!(s.grid.window <= p.start);
}

/// Held, Left repeats like Right (the grid scrolls); in the groups panel it keeps its long press.
#[test]
fn held_left_repeats_in_the_grid() {
    let mut data = data(6);
    let mut s = opened(&mut data, 0);
    assert_eq!(s.hold(Key::Left), Hold::Repeat);
    assert_eq!(s.hold(Key::Right), Hold::Repeat);
    let live = focus(&s);
    Screen::key(&mut s, Key::Left, Press::Repeat, &data, 0);
    assert_eq!(focus(&s).stop, live.start);
    press(&mut s, &data, Key::Back);
    press(&mut s, &data, Key::Back);
    assert!(s.panel);
    assert_eq!(s.hold(Key::Left), Hold::Long);
}

#[test]
fn the_video_stays_in_the_preview_under_the_groups_panel() {
    let mut data = data(9);
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Back);
    assert!(s.panel);
    assert_eq!(s.video_rect(), Some(geom::preview_rect()));
    // Moving through the groups and back to the grid never moves it.
    press(&mut s, &data, Key::Down);
    press(&mut s, &data, Key::Down);
    assert_eq!(s.group, Group::Playlist("News".into()));
    assert_eq!(s.video_rect(), Some(geom::preview_rect()));
    press(&mut s, &data, Key::Right);
    assert!(!s.panel);
    assert_eq!(s.video_rect(), Some(geom::preview_rect()));
    // The preview plays without moving it; full screen is the guide closing.
    press(&mut s, &data, Key::Down);
    let out = press(&mut s, &data, Key::Ok);
    assert_eq!(out.commands.len(), 1);
    assert!(out.navigate.is_empty());
    assert_eq!(s.video_rect(), Some(geom::preview_rect()));
    assert_eq!(press(&mut s, &data, Key::Ok).navigate, [Navigate::Close]);
}

/// A channel without catch-up shows its past like any other: details on focus, but OK only says
/// there is no archive.
#[test]
fn the_past_shows_everywhere_and_plays_only_from_the_archive() {
    let mut data = data(6);
    // Every third channel has no catch-up.
    assert!(!data.channels[2].catchup.available());
    let mut s = opened(&mut data, 2);
    let live = focus(&s);
    press(&mut s, &data, Key::Left);
    let p = focus(&s);
    assert_eq!(p.stop, live.start);
    assert!(!s.panel);
    assert!(p.description.is_some(), "the details are read");
    let out = press(&mut s, &data, Key::Ok);
    assert!(quiet(&out), "nothing plays");
    assert_eq!(out.toast.as_deref(), Some(msg::epg_catchup_unavailable()));
    assert_eq!(focus(&s), p);
}

/// Left walks back to the start of the day `past_days` before today, on a catch-up channel past
/// its archive too; OK plays only what the archive still reaches.
#[test]
fn left_walks_back_to_the_past_days_limit() {
    let mut data = data(6);
    data.past_days = 1;
    data.channels[0].catchup.days = 1;
    let limit = time::day_start(NOW, OFFSET) - DAY;
    let mut s = opened(&mut data, 0);
    let mut steps = 0;
    loop {
        let before = focus(&s);
        let out = press(&mut s, &data, Key::Left);
        assert!(quiet(&out) && !s.panel);
        if focus(&s) == before {
            break;
        }
        steps += 1;
        assert!(focus(&s).stop > limit, "inside the past days");
        assert!(steps < 200);
    }
    let first = focus(&s);
    assert!(
        first.start <= limit,
        "{first:?} is the last one reaching {limit}"
    );
    assert!(first.start < NOW - DAY, "past the one-day archive");
    let out = press(&mut s, &data, Key::Ok);
    assert!(out.commands.is_empty() && out.toast.is_some());
    // Right walks forward into the archive, where OK plays, and on to live.
    while focus(&s).start < NOW - DAY + HALF_HOUR {
        press(&mut s, &data, Key::Right);
    }
    let out = press(&mut s, &data, Key::Ok);
    assert!(matches!(
        &out.commands[..],
        [Command::Play {
            stream: Stream::Catchup { .. },
            ..
        }]
    ));
    while !on_air(&focus(&s)) {
        press(&mut s, &data, Key::Right);
    }
    assert_eq!(s.grid.anchor, Anchor::Live);
}

/// The archive's edge moves with the clock: the programme the preview already plays from it
/// still goes full screen after it slipped out.
#[test]
fn ok_on_the_previewed_programme_survives_the_archive_edge() {
    let mut data = data(6);
    data.channels[0].catchup.days = 1;
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Left);
    let p = focus(&s);
    let out = press(&mut s, &data, Key::Ok);
    assert_eq!(out.commands.len(), 1, "previewed from the archive");
    data.playback.catchup = Some(p.clone());
    data.now = p.start + DAY + HALF_HOUR;
    tick(&mut s, &data);
    let out = press(&mut s, &data, Key::Ok);
    assert!(out.toast.is_none() && out.commands.is_empty());
    assert_eq!(out.navigate, [Navigate::Close]);
}

/// Browsing the past, a row whose programme at that time is still on air keeps walking back.
#[test]
fn left_in_the_archive_passes_a_long_running_programme() {
    let mut data = data(6);
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Left);
    let anchor = focus(&s).start;
    for _ in 0..3 {
        press(&mut s, &data, Key::Down);
    }
    let p = focus(&s);
    assert!(
        on_air(&p) && p.start <= anchor,
        "phase 3 runs 90 minutes from the cycle start"
    );
    press(&mut s, &data, Key::Left);
    assert!(!s.panel);
    assert_eq!(focus(&s).stop, p.start);
}

#[test]
fn ok_previews_then_plays() {
    let mut data = data(6);
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Down);
    let key = key(&data, 1);
    let out = press(&mut s, &data, Key::Ok);
    assert_eq!(out.commands, [play(Stream::Live(key))]);
    assert!(out.navigate.is_empty());
    let out = press(&mut s, &data, Key::Ok);
    assert_eq!(out.commands, [play(Stream::Live(key))]);
    assert_eq!(out.navigate, [Navigate::Close]);
    assert!(!out.redraw, "the guide is gone");
}

#[test]
fn ok_on_the_playing_channel_plays_at_once() {
    let mut data = data(6);
    let mut s = opened(&mut data, 3);
    let out = press(&mut s, &data, Key::Ok);
    assert_eq!(out.commands, [play(Stream::Live(key(&data, 3)))]);
    assert_eq!(out.navigate, [Navigate::Close]);
}

#[test]
fn ok_on_an_archived_programme_plays_catch_up() {
    let mut data = data(6);
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Left);
    let p = focus(&s);
    assert!(p.description.is_some(), "every field of the programme");
    let out = press(&mut s, &data, Key::Ok);
    let catchup = Stream::Catchup {
        channel: key(&data, 0),
        programme: p,
        offset_secs: 0,
    };
    assert_eq!(out.commands, [play(catchup)]);
    assert!(
        out.navigate.is_empty(),
        "the archive plays in the preview first"
    );
    // Already playing from the archive: full screen, where it is.
    let out = press(&mut s, &data, Key::Ok);
    assert!(out.commands.is_empty());
    assert_eq!(out.navigate, [Navigate::Close]);
}

#[test]
fn ok_on_a_future_programme_previews_live() {
    let mut data = data(6);
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Down);
    press(&mut s, &data, Key::Right);
    let out = press(&mut s, &data, Key::Ok);
    assert_eq!(out.commands, [play(Stream::Live(key(&data, 1)))]);
}

#[test]
fn long_ok_asks_for_the_channel_menu() {
    let mut data = data(6);
    let mut s = opened(&mut data, 0);
    let out = long(&mut s, &data, Key::Ok);
    assert_eq!(
        out.navigate,
        [Navigate::ContextMenu {
            channel: key(&data, 0),
            programme: Some(focus(&s)),
        }]
    );
    press(&mut s, &data, Key::Left);
    let out = long(&mut s, &data, Key::Ok);
    assert_eq!(
        out.navigate,
        [Navigate::ContextMenu {
            channel: key(&data, 0),
            programme: Some(focus(&s)),
        }]
    );
    assert!(focus(&s).stop <= NOW, "the archived programme");
    assert!(out.commands.is_empty());
}

#[test]
fn back_returns_to_live_before_opening_the_groups() {
    let mut data = data(6);
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Down);
    press(&mut s, &data, Key::Left);
    press(&mut s, &data, Key::Back);
    assert!(!s.panel);
    assert!(on_air(&focus(&s)));
    press(&mut s, &data, Key::Right);
    press(&mut s, &data, Key::Back);
    assert!(!s.panel);
    assert_eq!(s.grid.anchor, Anchor::Live);
    let (row, live) = (s.grid.cursor.index, focus(&s));
    press(&mut s, &data, Key::Back);
    assert!(s.panel);
    // Back in the panel: the grid as it was.
    press(&mut s, &data, Key::Back);
    assert!(!s.panel);
    assert_eq!(s.grid.cursor.index, row);
    assert_eq!(focus(&s), live);
    assert_eq!(s.grid.anchor, Anchor::Live);
}

#[test]
fn long_back_returns_to_what_played() {
    let mut data = data(6);
    // Still playing: asked for again only to give the zap group back.
    let mut s = opened(&mut data, 0);
    let out = long(&mut s, &data, Key::Back);
    assert_eq!(out.commands, [play(Stream::Live(key(&data, 0)))]);
    assert_eq!(out.navigate, [Navigate::Close]);

    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Down);
    press(&mut s, &data, Key::Ok);
    let out = long(&mut s, &data, Key::Back);
    assert_eq!(out.commands, [play(Stream::Live(key(&data, 0)))]);
    assert_eq!(out.navigate, [Navigate::Close]);
}

#[test]
fn long_back_gives_back_the_group_the_guide_opened_on() {
    let mut data = data(9);
    data.playback.channel = Some(key(&data, 1));
    let sport = Group::Playlist("Sport".into());
    let mut s = GuideScreen::new(&data, sport.clone());
    // Over to News, where OK previews a channel and the player zaps through News.
    s.message(
        ScreenMessage::SetGroup(Group::Playlist("News".into())),
        &data,
    );
    let out = press(&mut s, &data, Key::Ok);
    let news = Some(Group::Playlist("News".into()));
    assert!(matches!(&out.commands[..], [Command::Play { group, .. }] if *group == news));
    let out = long(&mut s, &data, Key::Back);
    let back = Command::Play {
        stream: Stream::Live(key(&data, 1)),
        group: Some(sport),
    };
    assert_eq!(out.commands, [back]);
}

/// A screen over the guide can start something (the channel menu's Watch from start): the
/// preview follows playback, so OK and long Back act on what really plays.
#[test]
fn the_preview_follows_what_plays() {
    let mut data = data(6);
    let mut s = opened(&mut data, 0);
    let p = focus(&s);
    assert!(on_air(&p));
    data.playback.catchup = Some(p.clone());
    assert!(tick(&mut s, &data), "the play mark may move");
    // OK on the programme watched from its start: full screen, where it is.
    let out = press(&mut s, &data, Key::Ok);
    assert!(out.commands.is_empty());
    assert_eq!(out.navigate, [Navigate::Close]);
    // Long Back undoes it: live again, as when the guide opened.
    data.playback.catchup = None;
    let mut s = opened(&mut data, 0);
    data.playback.channel = Some(key(&data, 2));
    tick(&mut s, &data);
    let out = long(&mut s, &data, Key::Back);
    assert_eq!(out.commands, [play(Stream::Live(key(&data, 0)))]);
    // Opened during the archived programme and it still plays: nothing to undo.
    let archived = schedule(1, NOW - 3 * 3600, NOW - 3 * 3600 + 1).remove(0);
    data.playback.catchup = Some(archived.clone());
    data.playback.position_secs = 900;
    let mut s = opened(&mut data, 1);
    assert!(long(&mut s, &data, Key::Back).commands.is_empty());
    // Something else played from outside meanwhile: it is played again from where it was.
    let mut s = opened(&mut data, 1);
    data.playback.catchup = None;
    tick(&mut s, &data);
    let out = long(&mut s, &data, Key::Back);
    let resume = Stream::Catchup {
        channel: key(&data, 1),
        programme: archived,
        offset_secs: 900,
    };
    assert_eq!(out.commands, [play(resume)]);
}

#[test]
fn long_back_resumes_an_archived_programme() {
    let mut data = data(6);
    let programme = schedule(0, NOW - 3 * 3600, NOW - 3 * 3600 + 1).remove(0);
    data.playback.catchup = Some(programme.clone());
    data.playback.position_secs = 600;
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Down);
    press(&mut s, &data, Key::Ok);
    let out = long(&mut s, &data, Key::Back);
    let catchup = Stream::Catchup {
        channel: key(&data, 0),
        programme,
        offset_secs: 600,
    };
    assert_eq!(out.commands, [play(catchup)]);
    assert_eq!(out.navigate, [Navigate::Close]);
}

#[test]
fn opening_during_catch_up_focuses_the_archived_programme() {
    let mut data = data(6);
    let programme = schedule(1, NOW - 3 * 3600, NOW - 3 * 3600 + 1).remove(0);
    data.playback.catchup = Some(programme.clone());
    let mut s = opened(&mut data, 1);
    assert_eq!(focus(&s), programme);
    assert!(s.grid.window <= programme.start);
    // OK on what already plays in the preview: full screen.
    let out = press(&mut s, &data, Key::Ok);
    assert!(out.commands.is_empty());
    assert_eq!(out.navigate, [Navigate::Close]);
    // Back goes to live, a second Back to the groups.
    let mut s = opened(&mut data, 1);
    press(&mut s, &data, Key::Back);
    assert!(on_air(&focus(&s)));
    assert!(!s.panel);
    let out = press(&mut s, &data, Key::Ok);
    assert_eq!(out.commands, [play(Stream::Live(key(&data, 1)))]);
}

#[test]
fn a_group_chosen_elsewhere_and_the_scroll_settling() {
    let mut data = data(30);
    let mut s = opened(&mut data, 0);
    let sport = Group::Playlist("Sport".into());
    s.message(ScreenMessage::SetGroup(sport.clone()), &data);
    assert_eq!(s.group, sport);
    assert_eq!(s.grid.channels.len(), 10);
    assert!(s.grid.channels.iter().all(|c| c.groups[0] == "Sport"));
    for _ in 0..8 {
        assert!(press(&mut s, &data, Key::Down).redraw);
    }
    assert!(s.paint.animating(), "the rows scroll towards the focus");
    assert_eq!(s.next_wake(0), Some(FRAME_MS));
    for _ in 0..240 {
        s.update(1.0 / 60.0);
    }
    assert!(!s.paint.animating());
    assert_eq!(s.paint.scroll.pos, s.grid.cursor.top as f32 * geom::ROW_H);
}

#[test]
fn a_row_without_guide_data_is_one_cut_cell() {
    let mut data = guide_data(6, &[1], Rc::default());
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Down);
    assert_eq!(s.grid.focused_programme(), None);
    draw(&mut s);
    let i = s.grid.rows.iter().position(|r| r.index == 1).unwrap();
    let cells = &s.paint.rows[i].cells;
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].start, None);
    assert!(cells[0].geom.cut_left && cells[0].geom.cut_right);
    assert!(quiet(&press(&mut s, &data, Key::Right)));
    let out = press(&mut s, &data, Key::Ok);
    assert_eq!(out.commands, [play(Stream::Live(key(&data, 1)))]);
    assert!(quiet(&press(&mut s, &data, Key::Left)));
    assert!(!s.panel);
}

#[test]
fn groups_panel_filters_as_it_moves() {
    let mut data = data(9);
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Back);
    assert!(s.panel);
    let list = s.groups.list.clone();
    assert_eq!(&list[..3], &[Group::Favorites, Group::All, Group::History]);
    assert_eq!(s.group, Group::All);

    // History is a screen of its own: focusing it keeps the guide, OK opens it.
    press(&mut s, &data, Key::Down);
    assert_eq!(s.group, Group::All);
    assert_eq!(s.grid.channels.len(), 9);
    let out = press(&mut s, &data, Key::Ok);
    assert_eq!(out.navigate, [Navigate::History]);
    assert!(out.commands.is_empty() && s.panel);
    // Then the first playlist group.
    press(&mut s, &data, Key::Down);
    assert_eq!(s.group, Group::Playlist("News".into()));
    let names: Vec<_> = s
        .grid
        .channels
        .iter()
        .map(|c| c.groups[0].clone())
        .collect();
    assert_eq!(names, vec!["News"; 3]);
    assert!(s.panel, "moving does not leave the panel");

    let out = press(&mut s, &data, Key::Right);
    assert!(!s.panel);
    assert!(quiet(&out));
    assert_eq!(s.group, Group::Playlist("News".into()));
}

#[test]
fn groups_panel_keys() {
    let mut data = data(9);
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Back);
    for _ in 0..3 {
        press(&mut s, &data, Key::Down);
    }
    long(&mut s, &data, Key::Left);
    assert_eq!(s.group, Group::Favorites, "long Left jumps to the top");
    assert!(s.grid.channels.is_empty());
    press(&mut s, &data, Key::Ok);
    assert!(s.panel, "an empty group keeps the focus in the panel");
    assert_eq!(
        press(&mut s, &data, Key::Left).navigate,
        [Navigate::MainMenu]
    );
    assert_eq!(
        long(&mut s, &data, Key::Ok).navigate,
        [Navigate::GroupMenu(Group::Favorites)]
    );
    press(&mut s, &data, Key::Down);
    press(&mut s, &data, Key::Back);
    assert!(!s.panel, "Back returns to the grid");
    assert_eq!(s.group, Group::All);
    press(&mut s, &data, Key::Back);
    let out = long(&mut s, &data, Key::Back);
    assert_eq!(out.navigate, [Navigate::Close]);
    assert!(!s.panel);
}

#[test]
fn favorites_follow_the_data() {
    let mut data = data(9);
    data.favorites = vec![key(&data, 5), key(&data, 2)];
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Back);
    long(&mut s, &data, Key::Left);
    let keys: Vec<_> = s.grid.channels.iter().map(|c| c.key).collect();
    assert_eq!(keys, vec![key(&data, 2), key(&data, 5)]);
}

#[test]
fn manage_favorites_marks_many_channels_with_ok() {
    let reads = Rc::new(Cell::new(0));
    let mut data = guide_data(9, &[], Rc::clone(&reads));
    let mut s = opened(&mut data, 0);
    s.message(ScreenMessage::ManageFavorites, &data);
    assert_eq!(s.grid.mode, Mode::ManageFavorites);
    reads.take();
    let out = press(&mut s, &data, Key::Ok);
    assert_eq!(out.commands, [Command::ToggleFavorite(key(&data, 0))]);
    assert!(out.navigate.is_empty(), "OK does not play in this mode");
    assert_eq!(reads.take(), 0, "a toggle reads no guide");
    press(&mut s, &data, Key::Down);
    press(&mut s, &data, Key::Down);
    press(&mut s, &data, Key::Ok);
    let star =
        |s: &GuideScreen, i: usize| s.grid.rows.iter().find(|r| r.index == i).unwrap().favorite;
    assert!(
        star(&s, 0) && star(&s, 2) && !star(&s, 1),
        "stars flip before the data catches up"
    );
    // Toggling again unstars.
    press(&mut s, &data, Key::Ok);
    assert!(!star(&s, 2));
    // Horizontal keys and long OK do nothing here.
    let before = s.grid.focused_programme();
    assert!(quiet(&press(&mut s, &data, Key::Right)));
    assert!(quiet(&long(&mut s, &data, Key::Ok)));
    assert_eq!(s.grid.focused_programme(), before);
    // The app stores the toggles; the data now agrees.
    data.favorites = vec![key(&data, 0)];
    s.refresh(&data);
    assert!(star(&s, 0) && !star(&s, 2));
    press(&mut s, &data, Key::Back);
    assert_eq!(s.grid.mode, Mode::Browse);
    assert!(!s.panel, "Back only leaves the mode");
}

#[test]
fn ticks_redraw_only_when_something_moves() {
    let mut data = data(6);
    let mut s = opened(&mut data, 0);
    let live = focus(&s);
    assert!(!tick(&mut s, &data), "same instant");
    data.now = NOW + MINUTE;
    assert!(tick(&mut s, &data), "the clock and the now-line moved");
    assert!(!tick(&mut s, &data));
    // The live programme ends: focus follows live to the next one.
    data.now = live.stop;
    assert!(tick(&mut s, &data));
    assert_eq!(focus(&s).start, live.stop);
    // The window follows the clock at live.
    data.now = clock::ceil_half_hour(NOW, OFFSET) + 1;
    tick(&mut s, &data);
    assert_eq!(s.grid.window, clock::ceil_half_hour(NOW, OFFSET));
}

#[test]
fn wakes_follow_what_moves_on_screen() {
    let mut data = data(30);
    data.channels[0].catchup.days = 7;
    let mut s = opened(&mut data, 0);
    // At live the now-line moves a pixel every 5.6 s, before the minute turns at 15:12.
    let ms = s.next_wake(10_000).expect("live is never idle");
    assert!((11_000..=16_000).contains(&ms), "{ms}");
    let step = geom::now_line_step(s.grid.now, s.grid.window).unwrap();
    data.now = step;
    assert!(tick(&mut s, &data), "the now-line moved at its step");
    // A second before the live programme ends, its end is the next wake.
    data.now = focus(&s).stop - 1;
    tick(&mut s, &data);
    assert_eq!(s.next_wake(500), Some(1_500));
    // Scrolling rows want the next frame.
    for _ in 0..8 {
        press(&mut s, &data, Key::Down);
    }
    assert_eq!(s.next_wake(0), Some(FRAME_MS));
    // Back in the archive on an earlier day nothing follows the clock.
    data.now = NOW;
    let mut s = opened(&mut data, 0);
    press(&mut s, &data, Key::Left);
    for _ in 0..40 {
        if time::local_day(s.grid.window, OFFSET) != time::local_day(NOW, OFFSET) {
            break;
        }
        press(&mut s, &data, Key::Left);
    }
    assert!(time::local_day(s.grid.window, OFFSET) < time::local_day(NOW, OFFSET));
    assert_eq!(s.next_wake(0), None);
    // Later today, away from live: the ruler clock still turns each minute.
    let mut s = opened(&mut data, 0);
    for _ in 0..8 {
        press(&mut s, &data, Key::Right);
    }
    assert!(geom::now_line_step(s.grid.now, s.grid.window).is_none());
    assert_eq!(s.next_wake(0), Some(1_000 * (60 - (data.now % 60)) as u64));
}

#[test]
fn ticks_leave_a_moved_window_alone() {
    let mut data = data(6);
    let mut s = opened(&mut data, 0);
    for _ in 0..6 {
        press(&mut s, &data, Key::Right);
    }
    let window = s.grid.window;
    let p = focus(&s);
    data.now = NOW + HALF_HOUR;
    tick(&mut s, &data);
    assert_eq!(s.grid.window, window);
    assert_eq!(focus(&s), p);
}

/// 191 channels over seven days: a key press reads a handful of rows, never the playlist.
#[test]
fn the_cache_stays_small_with_a_full_playlist() {
    let reads = Rc::new(Cell::new(0));
    let mut data = guide_data(191, &[], Rc::clone(&reads));
    let mut s = opened(&mut data, 0);
    let bound = geom::VISIBLE_ROWS + 4;
    // Opening reads the rows and the focused programme once.
    assert!(reads.take() <= bound + 1);
    assert!(s.grid.rows.len() <= bound);
    for _ in 0..191 {
        press(&mut s, &data, Key::Down);
        // One for the row scrolling in, one for the focused programme; the wrap reads the
        // screen again.
        let q = reads.take();
        let wrapped = s.grid.cursor.index == 0;
        assert!(q <= if wrapped { 1 + bound } else { 2 }, "{q}");
        assert!(s.grid.rows.len() <= bound);
        assert!(s.grid.row().is_some());
    }
    assert_eq!(s.grid.cursor.index, 0, "wrapped all the way round");
    // A horizontal move inside the window reads only the newly focused programme.
    press(&mut s, &data, Key::Right);
    let q = reads.take();
    assert!(q <= 1, "{q}");
    // Paging the window reads the next programme, the rows on screen and no others.
    for _ in 0..12 {
        press(&mut s, &data, Key::Right);
        let q = reads.take();
        assert!(q <= 2 + bound, "{q}");
    }
    draw(&mut s);
    assert_eq!(s.paint.rows.len(), s.grid.rows.len());
    for row in &s.paint.rows {
        for c in &row.cells {
            assert!(c.geom.x0 >= geom::GRID_X && c.geom.x1 <= geom::SCREEN_W);
        }
        assert!(row.cells.len() <= 20);
    }
    // A tick within the minute does no work.
    assert!(!tick(&mut s, &data));
    assert_eq!(reads.take(), 0);
}

#[test]
fn details_and_ruler() {
    let mut data = data(6);
    let mut s = opened(&mut data, 0);
    draw(&mut s);
    let p = focus(&s);
    assert!(on_air(&p));
    let ruler = texts(&s.paint.ruler);
    assert_eq!(ruler[0], "Mon, Oct 19 15:11", "the clock while on today");
    assert_eq!(ruler[1..6], ["15:00", "15:30", "16:00", "16:30", "17:00"]);
    let details = texts(&s.paint.details);
    assert_eq!(details[0], p.title);
    // The description wraps to three lines, the last one cut short.
    let lines = &details[details.len() - geom::DESC_LINES..];
    assert!(lines[2].ends_with('\u{2026}'), "{lines:?}");
    let w = geom::DETAILS_RIGHT - geom::DETAILS_X;
    assert!(lines
        .iter()
        .all(|l| Mono.width_str(l, size::LABEL, false) <= w));
}

/// Drawing walks the cache with a recording painter: every state draws without panicking.
#[test]
fn every_state_draws() {
    let mut data = guide_data(30, &[3], Rc::default());
    let mut s = opened(&mut data, 0);
    draw(&mut s);
    press(&mut s, &data, Key::Left);
    s.update(1.0 / 60.0);
    draw(&mut s);
    for _ in 0..4 {
        press(&mut s, &data, Key::Down);
    }
    draw(&mut s);
    press(&mut s, &data, Key::Back);
    press(&mut s, &data, Key::Back);
    assert!(s.panel);
    draw(&mut s);
    press(&mut s, &data, Key::Right);
    s.message(ScreenMessage::ManageFavorites, &data);
    draw(&mut s);
    s.refresh(&data);
    draw(&mut s);
}
