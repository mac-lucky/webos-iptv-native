//! Picture in picture in the guide: where the video goes, the layout without the preview, and
//! what Back and OK do while the video plays in the corner window.

use iptv::model::{ChannelKey, Programme, Timestamp};
use tv_ui::Painter;

use super::geom;
use super::*;
use crate::data::fake::{FakeData, Mono};
use crate::data::{Command, Stream};
use crate::draw::Piece;

const NOW: Timestamp = 1_792_415_460;
const HOUR: i64 = 3600;

/// `count` channels of hour-long programmes, each with a description that wraps over many lines.
fn data(count: usize) -> FakeData {
    let mut data = FakeData::new(NOW, count).with_schedule(|i, from, to| {
        let mut start = from - from.rem_euclid(HOUR);
        let mut out = Vec::new();
        while start < to {
            out.push(Programme {
                start,
                stop: start + HOUR,
                title: format!("Show {i} at {start}"),
                description: Some("A long synthetic description that runs on. ".repeat(20)),
                ..Programme::default()
            });
            start += HOUR;
        }
        out
    });
    data.playback.channel = Some(data.channels[2].key);
    data
}

fn key(data: &FakeData, i: usize) -> ChannelKey {
    data.channels[i].key
}

fn pip(data: &FakeData) -> GuideScreen {
    let mut s = GuideScreen::new(data, Group::All);
    s.set_pip(true);
    s
}

fn press(s: &mut GuideScreen, data: &FakeData, key: Key) -> Output {
    Screen::key(s, key, Press::Short, data, 0)
}

fn draw(s: &mut GuideScreen) {
    Screen::draw(s, Painter::recording(), &Mono, &NoLogos);
}

struct NoLogos;

impl Logos for NoLogos {
    fn logo(&self, _: &iptv::model::Channel) -> Option<(u32, f32, f32)> {
        None
    }
}

/// The details texts as `(x, y, text)`.
fn details(s: &GuideScreen) -> Vec<(f32, f32, String)> {
    s.paint
        .details
        .iter()
        .filter_map(|p| match p {
            Piece::Text { s, x, y, .. } => Some((*x, *y, s.to_string_lossy().into_owned())),
            _ => None,
        })
        .collect()
}

#[test]
fn pip_puts_the_video_in_the_corner_window_the_guide_cuts_for_it() {
    let data = data(12);
    let mut s = GuideScreen::new(&data, Group::All);
    assert_eq!(s.video_rect(), Some(geom::preview_rect()));
    s.set_pip(true);
    let window = s.video_rect().expect("the video stays placed");
    assert_eq!(window, geom::pip_rect(), "the corner window");
    let hole = geom::PIP;
    assert_eq!(
        (window.x, window.y, window.w, window.h),
        (hole.x as i32, hole.y as i32, hole.w as i32, hole.h as i32),
        "the hole is cut where the video goes"
    );
    // The groups panel is on the left: it never covers the window.
    assert!(geom::GROUPS.w < hole.x);
    s.set_pip(false);
    assert_eq!(s.video_rect(), Some(geom::preview_rect()));
}

#[test]
fn without_the_preview_the_details_start_at_the_margin_and_say_more() {
    let data = data(12);
    let mut s = GuideScreen::new(&data, Group::All);
    draw(&mut s);
    let normal = details(&s);
    s.set_pip(true);
    draw(&mut s);
    let wide = details(&s);
    let title = |d: &[(f32, f32, String)]| d.first().cloned().unwrap();
    assert!(title(&normal).2.starts_with("Show 2"), "{normal:?}");
    assert_eq!(title(&normal).0, geom::DETAILS_X);
    assert_eq!(
        title(&wide).0,
        geom::PIP_DETAILS_X,
        "over where the preview was"
    );
    assert_eq!(title(&wide).2, title(&normal).2);
    let lines = |d: &[(f32, f32, String)]| d.iter().filter(|t| t.1 >= geom::DESC_Y).count();
    assert_eq!(lines(&normal), geom::DESC_LINES);
    assert_eq!(lines(&wide), geom::PIP_DESC_LINES);
    let bottom = wide.iter().map(|t| t.1).fold(0.0, f32::max);
    assert!(
        bottom + geom::DESC_LINE_H <= geom::RULER_Y,
        "above the ruler"
    );
    s.set_pip(false);
    draw(&mut s);
    assert_eq!(details(&s), normal, "the preview layout comes back");
}

#[test]
fn back_at_live_leaves_pip_before_it_opens_the_groups_panel() {
    let data = data(12);
    let mut s = pip(&data);
    press(&mut s, &data, Key::Right);
    let out = press(&mut s, &data, Key::Back);
    assert!(out.commands.is_empty() && out.navigate.is_empty());
    assert_eq!(s.video_rect(), Some(geom::pip_rect()), "back to live first");
    assert!(s.grid.at_live());
    press(&mut s, &data, Key::Back);
    assert_eq!(
        s.video_rect(),
        Some(geom::preview_rect()),
        "then out of PiP"
    );
    assert!(!s.panel);
    press(&mut s, &data, Key::Back);
    assert!(s.panel, "then the groups panel");
}

#[test]
fn ok_previews_into_the_window_and_on_its_channel_plays_full_screen() {
    let data = data(12);
    let mut s = pip(&data);
    press(&mut s, &data, Key::Down);
    let other = key(&data, 3);
    let out = press(&mut s, &data, Key::Ok);
    let play = |key| Command::Play {
        stream: Stream::Live(key),
        group: Some(Group::All),
    };
    assert_eq!(out.commands, [play(other)]);
    assert!(out.navigate.is_empty(), "it plays in the window");
    assert_eq!(s.video_rect(), Some(geom::pip_rect()));
    let out = press(&mut s, &data, Key::Ok);
    assert_eq!(out.commands, [play(other)]);
    assert_eq!(out.navigate, [Navigate::Close], "full screen");
}

#[test]
fn back_leaves_pip_then_the_guide_through_the_groups_panel() {
    let data = data(12);
    let mut s = pip(&data);
    press(&mut s, &data, Key::Back);
    assert_eq!(s.video_rect(), Some(geom::preview_rect()), "out of PiP");
    press(&mut s, &data, Key::Back);
    assert!(s.panel);
    let out = press(&mut s, &data, Key::Back);
    assert!(out.navigate.contains(&Navigate::Close));
}
