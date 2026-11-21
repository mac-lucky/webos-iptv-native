use super::*;
use tv_screens::data::VodSection;

fn enable(s: &mut Sim, mode: &str) {
    s.shell.set_setting(keys::LEFT_MENU, mode.to_owned());
    s.shell.navigation.focused = false;
    s.shell.settle(s.t);
}

#[test]
pub(super) fn the_left_menu_mode_applies_at_once() {
    let mut s = Sim::new();
    enable(&mut s, "persistent");
    assert!(s.shell.navigation_visible());
    s.press("Back");
    assert!(s.shell.navigation.focused, "one Back opens the main menu");
    enable(&mut s, "startup");
    assert!(
        !s.shell.navigation_visible(),
        "the startup mode leaves the whole list visible"
    );
    s.press("Back");
    assert!(s.shell.navigation_visible());
    s.press("Right");
    assert!(!s.shell.navigation_visible());
    enable(&mut s, "classic");
    assert!(!s.shell.navigation_available());
}

#[test]
fn both_accessible_modes_open_the_menu_on_start() {
    for mode in ["persistent", "startup"] {
        let mut s = Sim::new();
        enable(&mut s, mode);
        s.shell.start(s.t);
        assert!(s.shell.navigation_visible());
        assert!(s.shell.navigation.focused);
    }
}

#[test]
fn playing_a_movie_hides_the_sidebar_and_keeps_the_picture_full_screen() {
    locale();
    let calls = Calls::default();
    let mut data = FakeData::new(NOW, 12);
    data.vod = tv_screens::vod::fake::items("");
    let mut shell = Shell::new(
        Box::new(data),
        Box::new(calls.clone()),
        None,
        LogoCache::new(None),
    );
    shell.start(0);
    let mut s = Sim {
        shell,
        calls,
        seen: Rc::default(),
        t: 1_000,
    };
    for mode in ["persistent", "startup"] {
        enable(&mut s, mode);
        s.shell.navigate(Navigate::Vod(VodSection::Movies), None);
        s.run(20);
        s.press("Right");
        s.press("OK");
        assert!(!s.shell.navigation_available());
        assert!(s.shell.rect.is_none());
        s.press("Back");
    }
}

#[test]
fn the_menu_keeps_modal_dialogs_and_pin_controls() {
    let mut s = Sim::new();
    enable(&mut s, "persistent");
    s.shell.navigate(Navigate::Settings, None);
    assert!(!s.shell.navigation_available());
    s.press("Back");
    s.run(100);
    assert!(s.shell.navigation_available());
}

#[test]
fn pointer_coordinates_follow_the_scaled_content() {
    let mut s = Sim::new();
    enable(&mut s, "persistent");
    let mut ev = crate::input::PointerEvent {
        kind: crate::input::PointerKind::Move,
        x: 1120.0,
        y: 540.0,
        at_ms: 1_000,
    };
    assert!(!s.shell.navigation_pointer(&mut ev));
    assert!((ev.x - 960.0).abs() < 0.1);
    assert!((ev.y - 540.0).abs() < 0.1);
    ev.x = 100.0;
    ev.y = 280.0;
    assert!(s.shell.navigation_pointer(&mut ev));
    assert!(s.shell.navigation.focused);
}

#[test]
fn the_magic_remote_button_and_key_open_settings_only_once() {
    use crate::input::{PointerEvent, PointerKind};
    for button_first in [false, true] {
        let mut s = Sim::new();
        enable(&mut s, "persistent");
        let event = |kind, t| PointerEvent {
            kind,
            x: 100.0,
            y: 576.0,
            at_ms: t,
        };
        s.shell.pointer(event(PointerKind::Move, s.t as u32));
        if button_first {
            s.shell.pointer(event(PointerKind::Down, s.t as u32));
        }
        s.edge("OK", Edge::Down);
        if !button_first {
            s.shell.pointer(event(PointerKind::Down, s.t as u32));
        }
        s.t += 50;
        s.edge("OK", Edge::Up);
        s.shell.pointer(event(PointerKind::Up, s.t as u32));
        s.run(500);
        assert_eq!(s.open(), 2, "one guide and one settings panel");
        assert!(!s.shell.navigation_available());
    }
}
