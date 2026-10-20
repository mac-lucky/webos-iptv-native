//! The Magic Remote's pointer through the whole shell: hover focus, click as OK (held: long OK),
//! the wheel, and the hand-over between pointer and keys, on the real screens.

use std::any::Any;

use tv_screens::data::fake::Mono;
use tv_screens::guide::GuideScreen;
use tv_screens::panels::ContextMenu;
use tv_screens::theme::guide::{GRID_Y, ROW_H};

use super::*;
use crate::input::{PointerEvent, PointerKind};

/// The middle of the guide's channel column on row `row` (the rows start at the top).
fn channel_row(row: usize) -> (f32, f32) {
    (300.0, GRID_Y + (row as f32 + 0.5) * ROW_H)
}

impl Sim {
    /// One frame of every screen on a recording painter: what they lay out is what the pointer
    /// finds.
    fn frame(&mut self) {
        let shell = &mut self.shell;
        shell
            .bottom
            .screen
            .draw(Painter::recording(), &Mono, &shell.logos);
        for layer in &mut shell.stack {
            layer.screen.draw(Painter::recording(), &Mono, &shell.logos);
        }
        shell.dirty = false;
    }

    fn pointer(&mut self, kind: PointerKind, (x, y): (f32, f32)) {
        self.shell.pointer(PointerEvent {
            kind,
            x,
            y,
            at_ms: self.t as u32,
        });
    }

    /// The pointer moved to `at`, as a hand moves it: in steps, drawn after each.
    fn move_to(&mut self, at: (f32, f32)) {
        self.pointer(PointerKind::Move, at);
        self.t += 16;
        self.frame();
    }

    /// A click at `at`: the wheel pressed and released.
    fn click(&mut self, at: (f32, f32)) {
        self.pointer(PointerKind::Down, at);
        self.t += 80;
        self.pointer(PointerKind::Up, at);
        self.run(500);
        self.frame();
    }

    fn wheel(&mut self, up: bool) {
        self.pointer(PointerKind::Wheel { up }, (0.0, 0.0));
    }

    fn top_screen(&self) -> &dyn Any {
        let i = self.shell.top().expect("a screen over the player");
        &*self.shell.stack[i].screen
    }

    /// The channel the guide in front focuses.
    fn guide_channel(&self) -> String {
        let guide: &GuideScreen = self.top_screen().downcast_ref().expect("the guide");
        guide.focus().channel.map(|c| c.name.clone()).unwrap()
    }
}

#[test]
fn pointing_focuses_the_row_and_a_click_is_ok_on_it() {
    let mut s = Sim::new();
    s.frame();
    assert_eq!(s.guide_channel(), "Channel 1");
    s.move_to(channel_row(3));
    assert_eq!(s.guide_channel(), "Channel 4");
    assert!(s.calls.take().is_empty(), "pointing plays nothing");
    // Moving within the same row changes nothing and draws nothing.
    s.pointer(PointerKind::Move, (320.0, channel_row(3).1 + 10.0));
    assert!(!s.shell.needs_frame(), "idle under a still focus");
    // The click previews the channel pointed at (the guide's first OK).
    s.click(channel_row(3));
    assert_eq!(s.guide_channel(), "Channel 4");
    let opened = s.calls.take();
    assert!(
        opened.iter().any(|c| c.starts_with("open live Channel 4")),
        "{opened:?}"
    );
    // A second click on it plays it full screen.
    s.click(channel_row(3));
    assert_eq!(s.open(), 0, "the player in front");
    assert_eq!(s.playing().channel, Some(s.key(3)));
}

#[test]
fn keys_carry_on_from_the_pointer_and_keep_the_focus_until_it_travels() {
    let mut s = Sim::new();
    s.frame();
    s.move_to(channel_row(2));
    assert_eq!(s.guide_channel(), "Channel 3");
    s.press("Down");
    s.frame();
    assert_eq!(s.guide_channel(), "Channel 4", "on from the pointed row");
    // The hand holding the remote drifts: the keys keep the focus.
    let (x, y) = channel_row(2);
    s.move_to((x + 30.0, y));
    s.move_to((x + 40.0, y + 10.0));
    assert_eq!(s.guide_channel(), "Channel 4");
    // A click now is OK on what the keys focused, not on the row under the pointer.
    s.click((x + 40.0, y + 10.0));
    let opened = s.calls.take();
    assert!(
        opened.iter().any(|c| c.starts_with("open live Channel 4")),
        "{opened:?}"
    );
    // Moved on for real, the pointer has the focus again.
    s.move_to(channel_row(5));
    assert_eq!(s.guide_channel(), "Channel 6");
}

#[test]
fn one_press_sent_as_both_a_key_and_a_button_is_one_ok() {
    let mut s = Sim::new();
    let _probe = s.push("probe");
    let ok = |s: &mut Sim, edge| s.edge("OK", edge);
    let at = (960.0, 540.0);
    // Button first, then the key; released in the same order and the other.
    s.pointer(PointerKind::Down, at);
    s.t += 5;
    ok(&mut s, Edge::Down);
    s.t += 60;
    ok(&mut s, Edge::Up);
    s.t += 5;
    s.pointer(PointerKind::Up, at);
    s.run(500);
    assert_eq!(s.seen(), ["probe key Ok"]);
    ok(&mut s, Edge::Down);
    s.t += 5;
    s.pointer(PointerKind::Down, at);
    s.t += 60;
    s.pointer(PointerKind::Up, at);
    s.t += 5;
    ok(&mut s, Edge::Up);
    s.run(500);
    assert_eq!(s.seen(), ["probe key Ok"]);
    // Two presses well apart are two.
    s.pointer(PointerKind::Down, at);
    s.t += 60;
    s.pointer(PointerKind::Up, at);
    s.run(300);
    ok(&mut s, Edge::Down);
    s.t += 60;
    ok(&mut s, Edge::Up);
    s.run(500);
    assert_eq!(s.seen(), ["probe key Ok", "probe key Ok"]);
}

#[test]
fn a_click_on_a_row_drawn_but_not_pointable_does_nothing() {
    let mut s = Sim::new();
    s.frame();
    assert_eq!(s.guide_channel(), "Channel 1");
    // The eighth row shows only its top under the seven whole ones.
    let cut = channel_row(7);
    s.move_to(cut);
    s.click(cut);
    assert!(s.calls.take().is_empty(), "no OK on Channel 1");
    assert_eq!(s.guide_channel(), "Channel 1");
    assert!(s.top_screen().is::<GuideScreen>());
    // The key report of the same press, should the remote send one, goes with it.
    s.pointer(PointerKind::Down, cut);
    s.t += 5;
    s.edge("OK", Edge::Down);
    s.t += 60;
    s.edge("OK", Edge::Up);
    s.t += 5;
    s.pointer(PointerKind::Up, cut);
    s.run(1_500);
    assert!(s.calls.take().is_empty());
    assert!(s.top_screen().is::<GuideScreen>(), "no menu either");
    // The key report first: the OK it holds goes with the click all the same.
    s.edge("OK", Edge::Down);
    s.t += 5;
    s.pointer(PointerKind::Down, cut);
    s.t += 60;
    s.pointer(PointerKind::Up, cut);
    s.t += 5;
    s.edge("OK", Edge::Up);
    s.run(1_500);
    assert!(s.calls.take().is_empty(), "no OK on Channel 1");
    assert!(s.top_screen().is::<GuideScreen>(), "no menu either");
    // A click on nothing at all (the details over the grid) is OK on the focus.
    s.move_to((960.0, 100.0));
    s.click((960.0, 100.0));
    let opened = s.calls.take();
    assert!(
        opened.iter().any(|c| c.starts_with("open live Channel 1")),
        "{opened:?}"
    );
    // After a key the click is OK on what the keys focused, the cut row under the pointer
    // or not.
    s.move_to(cut);
    s.press("Down");
    s.click(cut);
    let opened = s.calls.take();
    assert!(
        opened.iter().any(|c| c.starts_with("open live Channel 2")),
        "{opened:?}"
    );
}

#[test]
fn a_press_released_on_the_other_kind_of_edge_lets_go_once() {
    let mut s = Sim::new();
    s.frame();
    let at = channel_row(1);
    s.move_to(at);
    // The guide's OK acts on release (held, it is the channel menu): pressed as the button,
    // released as the key.
    s.pointer(PointerKind::Down, at);
    s.t += 80;
    s.edge("OK", Edge::Up);
    s.run(1_500);
    let opened = s.calls.take();
    assert_eq!(
        opened
            .iter()
            .filter(|c| c.starts_with("open live Channel 2"))
            .count(),
        1,
        "{opened:?}"
    );
    assert!(s.top_screen().is::<GuideScreen>(), "not held: no menu");
    // A stray button-up afterwards does nothing.
    s.pointer(PointerKind::Up, at);
    s.run(500);
    assert!(s.calls.take().is_empty());
    assert!(s.top_screen().is::<GuideScreen>());
}

#[test]
fn a_held_wheel_is_a_long_ok_and_the_menu_follows_the_pointer() {
    let mut s = Sim::new();
    s.frame();
    let at = channel_row(2);
    s.move_to(at);
    s.pointer(PointerKind::Down, at);
    s.run(1_100);
    s.pointer(PointerKind::Up, at);
    s.run(500);
    assert!(s.top_screen().is::<ContextMenu>(), "the channel menu");
    s.frame();
    // The menu slides in on the right: its second row, then a click on it.
    let x = 1920.0 - 200.0;
    let row = |i: usize| 128.0 + (i as f32 + 0.5) * 76.0;
    s.move_to((x - 100.0, row(1)));
    s.move_to((x, row(1)));
    let menu: &ContextMenu = s.top_screen().downcast_ref().unwrap();
    assert_eq!(menu.panel().page().focus(), 1, "the pointed row");
    // Down carries on from it.
    s.press("Down");
    let menu: &ContextMenu = s.top_screen().downcast_ref().unwrap();
    assert_eq!(menu.panel().page().focus(), 2);
}

#[test]
fn the_wheel_steps_through_lists_but_never_zaps() {
    let mut s = Sim::new();
    s.frame();
    s.wheel(false);
    assert_eq!(s.guide_channel(), "Channel 2");
    s.t += 20;
    s.wheel(false);
    assert_eq!(s.guide_channel(), "Channel 2", "one notch, one step");
    s.t += 100;
    s.wheel(false);
    s.t += 100;
    s.wheel(true);
    assert_eq!(s.guide_channel(), "Channel 2");
    // The wheel moved the focus from under the pointer: a drift does not take it back.
    s.move_to(channel_row(5));
    assert_eq!(s.guide_channel(), "Channel 2");
    // Full screen, Up and Down zap: the wheel does nothing there.
    s.live(4);
    s.shell.navigate(Navigate::Player, None);
    s.shell.settle(s.t);
    s.run(5_000);
    s.calls.take();
    s.t += 200;
    s.wheel(false);
    s.run(500);
    assert!(s.calls.take().is_empty(), "no zap");
    assert_eq!(s.playing().channel, Some(s.key(4)));
}

#[test]
fn the_pointer_is_left_to_the_tv_keyboard_while_it_types() {
    let mut s = Sim::new();
    let field = s.push("field");
    field.0.editing.set(true);
    s.press("Down");
    assert!(s.shell.typing);
    s.seen();
    s.pointer(PointerKind::Down, (960.0, 540.0));
    s.t += 80;
    s.pointer(PointerKind::Up, (960.0, 540.0));
    s.wheel(true);
    s.run(500);
    assert!(s.seen().is_empty());
}
