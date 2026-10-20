//! Remote keys as raw edges: what the television reported, when the app saw it, nothing decided.
//!
//! **LG's SDL fork does not lay `SDL_KeyboardEvent` out the way SDL2's headers do**: it writes the
//! state (u32: low byte 1 pressed, bit `0x100` auto-repeat) at +16, the webOS key code at +20 and
//! the SDL keysym at +24. Stock SDL2, which the simulator links, keeps `state: u8` at +12,
//! `repeat: u8` at +13, the scancode at +16 and the keysym at +20. Reading one layout out of the
//! other yields plausible garbage rather than a crash, so each build reads only its own.
//!
//! The simulator translates desktop keys into the pairs the television sends ([`REMOTE`]), so
//! everything above this module sees television codes on both builds.
//!
//! While the Magic Remote's cursor shows, webOS sends the remote as mouse events instead: motion,
//! its OK (the wheel pressed) as a button, the wheel turned ([`decode_pointer`]). Both SDLs lay
//! those out alike: x at +20 and y at +24, the wheel's y at +20.

use crate::sdl::{self, Event};
use tv_ui::consts::{
    SCANCODE_0, SCANCODE_1, SCANCODE_9, SDLK_BACKSPACE, SDLK_CLEAR, SDLK_DOWN, SDLK_LEFT,
    SDLK_RETURN, SDLK_RIGHT, SDLK_UP, WCODE_BACK, WCODE_BLUE, WCODE_CH_DOWN_ALT, WCODE_CH_DOWN_KEY,
    WCODE_CH_UP_ALT, WCODE_CH_UP_KEY, WCODE_EXIT, WCODE_GREEN, WCODE_RED, WCODE_YELLOW,
};

/// One key edge from the remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// The SDL keysym; 0 for the buttons only a remote has.
    pub sym: u32,
    /// The webOS key code: the SDL scancode for keyboard-like keys, LG's own range above 300 for
    /// remote buttons. Every remote key has a distinct one.
    pub wcode: u32,
    pub edge: Edge,
    /// Milliseconds since boot when the app took the event off SDL's queue.
    pub at_ms: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Down,
    /// The remote's (or keyboard's) own auto-repeat while held. A long press is a timing the
    /// action layer reads from Down and Up, not this.
    Repeat,
    Up,
}

/// The remote's keys as the television reports them: `(name, sym, wcode)`. The D-pad, OK and the
/// digits arrive as SDL scancodes, BACK as 482, the channel rocker as 300/301 and the colour keys
/// as 486-489 with no keysym (PlxNative's Magic Remote measurements). The last two are the
/// on-screen keyboard's delete and clear-all keys, which arrive as key events while it is up.
pub const REMOTE: [(&str, u32, u32); 25] = [
    ("Up", SDLK_UP, 82),
    ("Down", SDLK_DOWN, 81),
    ("Left", SDLK_LEFT, 80),
    ("Right", SDLK_RIGHT, 79),
    ("OK", SDLK_RETURN, 40),
    ("Back", 0, WCODE_BACK),
    ("Exit", 0, WCODE_EXIT),
    ("CH+", 0, WCODE_CH_UP_KEY),
    ("CH-", 0, WCODE_CH_DOWN_KEY),
    ("Red", 0, WCODE_RED),
    ("Green", 0, WCODE_GREEN),
    ("Yellow", 0, WCODE_YELLOW),
    ("Blue", 0, WCODE_BLUE),
    ("1", b'1' as u32, SCANCODE_1),
    ("2", b'2' as u32, SCANCODE_1 + 1),
    ("3", b'3' as u32, SCANCODE_1 + 2),
    ("4", b'4' as u32, SCANCODE_1 + 3),
    ("5", b'5' as u32, SCANCODE_1 + 4),
    ("6", b'6' as u32, SCANCODE_1 + 5),
    ("7", b'7' as u32, SCANCODE_1 + 6),
    ("8", b'8' as u32, SCANCODE_1 + 7),
    ("9", b'9' as u32, SCANCODE_9),
    ("0", b'0' as u32, SCANCODE_0),
    ("Del", SDLK_BACKSPACE, 42),
    ("Clear", SDLK_CLEAR, 156),
];

/// The name of a remote key, by its webOS code. webOS 25 spells the channel rocker 480/481.
pub fn name(wcode: u32) -> Option<&'static str> {
    let wcode = match wcode {
        WCODE_CH_UP_ALT => WCODE_CH_UP_KEY,
        WCODE_CH_DOWN_ALT => WCODE_CH_DOWN_KEY,
        other => other,
    };
    REMOTE.iter().find(|k| k.2 == wcode).map(|k| k.0)
}

/// Whether a key that arrives while the TV keyboard types may go into the log: its control keys
/// (Enter, delete, clear, arrows and the like), never one that types a character. SDL gives a key
/// with a character its code point and any other key its scancode with bit 30 set, except the
/// keypad, whose digits and signs also carry a scancode, so those stay out (a USB keyboard's
/// keypad would otherwise log a typed token digit by digit).
pub fn loggable_while_typing(sym: u32) -> bool {
    const SCANCODE_KEY: u32 = 1 << 30;
    match sym {
        0..=0x1f | 0x7f => true,
        // SDL's keypad scancodes, all but its Enter (88).
        SCANCODE_KEY.. => !matches!(
            sym - SCANCODE_KEY,
            84..=87 | 89..=99 | 103 | 133 | 134 | 176..=221
        ),
        _ => false,
    }
}

/// An `SDL_KEYDOWN`/`SDL_KEYUP` event in LG's layout.
#[cfg(not(feature = "hostsim"))]
pub fn decode(ev: &Event, at_ms: u32) -> KeyEvent {
    let state = ev.u32_at(16);
    let edge = match (state & 0xff, state & 0x100) {
        (1, 0) => Edge::Down,
        (1, _) => Edge::Repeat,
        _ => Edge::Up,
    };
    KeyEvent {
        sym: ev.u32_at(24),
        wcode: ev.u32_at(20),
        edge,
        at_ms,
    }
}

/// An `SDL_KEYDOWN`/`SDL_KEYUP` event in stock SDL2's layout, translated to the television's codes.
#[cfg(feature = "hostsim")]
pub fn decode(ev: &Event, at_ms: u32) -> KeyEvent {
    let edge = match (ev.0[12], ev.0[13]) {
        (0, _) => Edge::Up,
        (_, 0) => Edge::Down,
        _ => Edge::Repeat,
    };
    let (sym, wcode) = desktop_to_tv(ev.u32_at(16), ev.u32_at(20));
    KeyEvent {
        sym,
        wcode,
        edge,
        at_ms,
    }
}

/// A desktop key as the remote key it stands for: Backspace is BACK, Page Up/Down the channel
/// rocker, F1-F4 the colour keys, Delete and the keypad's Clear the TV keyboard's delete and
/// clear-all. Arrows, Enter and the digits already carry the television's pair (keysym,
/// scancode), so they and every other key pass through.
#[cfg(feature = "hostsim")]
fn desktop_to_tv(scancode: u32, sym: u32) -> (u32, u32) {
    const BACKSPACE: u32 = 42;
    const F1: u32 = 58;
    const F4: u32 = 61;
    const PAGE_UP: u32 = 75;
    const DELETE: u32 = 76;
    const PAGE_DOWN: u32 = 78;
    const KEYPAD_CLEAR: u32 = 83;
    match scancode {
        BACKSPACE => (0, WCODE_BACK),
        PAGE_UP => (0, WCODE_CH_UP_KEY),
        PAGE_DOWN => (0, WCODE_CH_DOWN_KEY),
        DELETE => (SDLK_BACKSPACE, 42),
        KEYPAD_CLEAR => (SDLK_CLEAR, 156),
        F1..=F4 => (0, WCODE_RED + scancode - F1),
        _ => (sym, scancode),
    }
}

/// One pointer event from the Magic Remote (or the simulator's mouse).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerEvent {
    pub kind: PointerKind,
    /// Where the pointer is on the 1920x1080 canvas; a wheel event says nowhere (0, 0).
    pub x: f32,
    pub y: f32,
    /// Milliseconds since boot when the app took the event off SDL's queue.
    pub at_ms: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerKind {
    Move,
    /// The remote's OK (or a mouse button) pressed.
    Down,
    Up,
    /// One step of the wheel, away from the viewer (`up`) or towards them.
    Wheel {
        up: bool,
    },
}

/// `SDL_MouseWheelEvent.direction` when the platform inverts the wheel (natural scrolling).
const SDL_MOUSEWHEEL_FLIPPED: u32 = 1;

/// A mouse event as a pointer event: window units times `scale` are drawable pixels
/// ([`crate::sdl::Video::pointer_scale`]), mapped onto the canvas. `None` for any other event,
/// and for a wheel event that turned less than a step. The simulator's SDL (sdl2-compat over
/// SDL3) may leave the wheel's integer y at 0 and carry the turn in `preciseY` (+32).
pub fn decode_pointer(ev: &Event, scale: f32, at_ms: u32) -> Option<PointerEvent> {
    let kind = match ev.kind() {
        sdl::SDL_MOUSEMOTION => PointerKind::Move,
        sdl::SDL_MOUSEBUTTONDOWN => PointerKind::Down,
        sdl::SDL_MOUSEBUTTONUP => PointerKind::Up,
        sdl::SDL_MOUSEWHEEL => {
            let mut dy = ev.i32_at(20);
            if dy == 0 && cfg!(feature = "hostsim") {
                dy = ev.f32_at(32).round() as i32;
            }
            if ev.u32_at(24) == SDL_MOUSEWHEEL_FLIPPED {
                dy = -dy;
            }
            return (dy != 0).then_some(PointerEvent {
                kind: PointerKind::Wheel { up: dy > 0 },
                x: 0.0,
                y: 0.0,
                at_ms,
            });
        }
        _ => return None,
    };
    let (px, py) = (ev.i32_at(20) as f32 * scale, ev.i32_at(24) as f32 * scale);
    let (x, y) = tv_base::surface::to_logical(px, py);
    Some(PointerEvent { kind, x, y, at_ms })
}

/// The event log's line for a pointer event: its kind and position, nothing else. A motion is
/// logged at most once a second (`last_move` keeps when), so a pointing hand does not flood the
/// log.
pub fn pointer_line(ev: &PointerEvent, last_move: &mut Option<u32>) -> Option<String> {
    let kind = match ev.kind {
        PointerKind::Wheel { up } => {
            return Some(format!("pointer: wheel {}", if up { "up" } else { "down" }))
        }
        PointerKind::Move => {
            if last_move.is_some_and(|t| ev.at_ms.wrapping_sub(t) < 1_000) {
                return None;
            }
            *last_move = Some(ev.at_ms);
            "move"
        }
        PointerKind::Down => "down",
        PointerKind::Up => "up",
    };
    Some(format!("pointer: {kind} {:.0},{:.0}", ev.x, ev.y))
}

/// Where `SDL_TEXTINPUT`'s `char text[32]` starts: LG's fork inserts a `u32 inputSource` at +12,
/// so +16 there; stock SDL2 has it at +12.
const TEXT_AT: usize = if cfg!(feature = "hostsim") { 12 } else { 16 };
const TEXT_LEN: usize = 32;

/// The text an `SDL_TEXTINPUT` event commits. The array is NUL-terminated and may cut a
/// character at either end: a leading continuation byte and a trailing incomplete sequence are
/// dropped, anything else that is not UTF-8 becomes U+FFFD.
pub fn decode_text(ev: &Event) -> String {
    let field = &ev.0[TEXT_AT..TEXT_AT + TEXT_LEN];
    let mut text = &field[..field.iter().position(|&b| b == 0).unwrap_or(TEXT_LEN)];
    while text.first().is_some_and(|b| b & 0xC0 == 0x80) {
        text = &text[1..];
    }
    if let Err(e) = std::str::from_utf8(text) {
        if e.error_len().is_none() {
            text = &text[..e.valid_up_to()];
        }
    }
    String::from_utf8_lossy(text).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(fields: &[(usize, u32)]) -> Event {
        let mut ev = Event::new();
        for &(off, v) in fields {
            ev.0[off..off + 4].copy_from_slice(&v.to_ne_bytes());
        }
        ev
    }

    #[test]
    fn every_remote_key_has_its_own_code_and_name() {
        for (i, a) in REMOTE.iter().enumerate() {
            assert!(
                REMOTE[i + 1..].iter().all(|b| b.0 != a.0 && b.2 != a.2),
                "{a:?}"
            );
            assert_eq!(name(a.2), Some(a.0));
        }
        assert_eq!(name(4), None);
        assert_eq!(name(WCODE_CH_UP_ALT), Some("CH+"));
        assert_eq!(name(WCODE_CH_DOWN_ALT), Some("CH-"));
    }

    #[test]
    fn only_control_keys_are_loggable_while_the_keyboard_types() {
        const KP: u32 = 1 << 30;
        for sym in [
            SDLK_RETURN,
            SDLK_BACKSPACE,
            0x7f,
            SDLK_CLEAR,
            SDLK_LEFT,
            KP | 88,
        ] {
            assert!(loggable_while_typing(sym), "{sym:#x}");
        }
        // Characters, as code points or as the keypad's digits and signs.
        for sym in [
            b'a' as u32,
            b'1' as u32,
            b' ' as u32,
            0x17c,
            KP | 84,
            KP | 89,
            KP | 98,
            KP | 99,
            KP | 103,
            KP | 176,
            KP | 221,
        ] {
            assert!(!loggable_while_typing(sym), "{sym:#x}");
        }
    }

    #[cfg(not(feature = "hostsim"))]
    #[test]
    fn the_forks_layout_decodes_with_its_repeat_bit() {
        let at = |state| decode(&event(&[(16, state), (20, WCODE_BACK), (24, 0)]), 7);
        assert_eq!(
            at(1),
            KeyEvent {
                sym: 0,
                wcode: WCODE_BACK,
                edge: Edge::Down,
                at_ms: 7
            }
        );
        assert_eq!(at(0x101).edge, Edge::Repeat);
        assert_eq!(at(0).edge, Edge::Up);
        let ok = decode(&event(&[(16, 1), (20, 40), (24, SDLK_RETURN)]), 0);
        assert_eq!(name(ok.wcode), Some("OK"));
        assert_eq!(ok.sym, SDLK_RETURN);
    }

    #[cfg(feature = "hostsim")]
    #[test]
    fn desktop_keys_arrive_as_the_remote_keys_they_stand_for() {
        let key = |scancode: u32, sym: u32, state: u8, repeat: u8| {
            let mut ev = event(&[(16, scancode), (20, sym)]);
            ev.0[12] = state;
            ev.0[13] = repeat;
            decode(&ev, 3)
        };
        let named = |scancode, sym| name(key(scancode, sym, 1, 0).wcode);
        assert_eq!(named(82, SDLK_UP), Some("Up"));
        assert_eq!(named(40, SDLK_RETURN), Some("OK"));
        assert_eq!(named(42, 8), Some("Back"));
        assert_eq!(named(75, 0x4000_004B), Some("CH+"));
        assert_eq!(named(78, 0x4000_004E), Some("CH-"));
        assert_eq!(named(58, 0x4000_003A), Some("Red"));
        assert_eq!(named(61, 0x4000_003D), Some("Blue"));
        assert_eq!(named(39, b'0' as u32), Some("0"));
        assert_eq!(
            key(42, 8, 1, 0),
            KeyEvent {
                sym: 0,
                wcode: WCODE_BACK,
                edge: Edge::Down,
                at_ms: 3
            }
        );
        assert_eq!(key(42, 8, 1, 1).edge, Edge::Repeat);
        assert_eq!(key(42, 8, 0, 0).edge, Edge::Up);
        assert_eq!(
            key(4, b'a' as u32, 1, 0).wcode,
            4,
            "an unmapped key passes through"
        );
        assert_eq!(named(76, 0x7f), Some("Del"));
        assert_eq!(named(83, 0x4000_0053), Some("Clear"));
    }

    #[test]
    fn mouse_events_decode_onto_the_canvas() {
        let mouse = |kind: u32, x: i32, y: i32| {
            event(&[(0, kind), (16, 1), (20, x as u32), (24, y as u32)])
        };
        let at = |ev: &Event, scale| decode_pointer(ev, scale, 9);
        let down = at(&mouse(sdl::SDL_MOUSEBUTTONDOWN, 812, 530), 1.0).unwrap();
        assert_eq!(
            down,
            PointerEvent {
                kind: PointerKind::Down,
                x: 812.0,
                y: 530.0,
                at_ms: 9
            }
        );
        let up = at(&mouse(sdl::SDL_MOUSEBUTTONUP, 812, 530), 1.0).unwrap();
        assert_eq!(up.kind, PointerKind::Up);
        // The simulator's window counts half-size points on a Retina display.
        let moved = at(&mouse(sdl::SDL_MOUSEMOTION, 480, 270), 2.0).unwrap();
        assert_eq!(
            (moved.kind, moved.x, moved.y),
            (PointerKind::Move, 960.0, 540.0)
        );
        assert_eq!(at(&mouse(sdl::SDL_KEYDOWN, 1, 1), 1.0), None);
        let wheel = |y: i32, flipped: u32| {
            let ev = event(&[(0, sdl::SDL_MOUSEWHEEL), (20, y as u32), (24, flipped)]);
            at(&ev, 1.0).map(|p| p.kind)
        };
        assert_eq!(wheel(1, 0), Some(PointerKind::Wheel { up: true }));
        assert_eq!(wheel(-2, 0), Some(PointerKind::Wheel { up: false }));
        assert_eq!(
            wheel(1, 1),
            Some(PointerKind::Wheel { up: false }),
            "flipped"
        );
        assert_eq!(wheel(0, 0), None, "less than a step");
    }

    #[test]
    fn the_log_says_the_pointer_kind_and_position_and_a_move_once_a_second() {
        let ev = |kind, at_ms| PointerEvent {
            kind,
            x: 812.4,
            y: 530.0,
            at_ms,
        };
        let mut last = None;
        let mut line = |kind, at| pointer_line(&ev(kind, at), &mut last);
        assert_eq!(
            line(PointerKind::Move, 100).as_deref(),
            Some("pointer: move 812,530")
        );
        assert_eq!(line(PointerKind::Move, 900), None);
        assert_eq!(
            line(PointerKind::Down, 950).as_deref(),
            Some("pointer: down 812,530")
        );
        assert_eq!(
            line(PointerKind::Up, 990).as_deref(),
            Some("pointer: up 812,530")
        );
        assert!(line(PointerKind::Move, 1_100).is_some());
        let wheel = line(PointerKind::Wheel { up: false }, 1_200).unwrap();
        assert_eq!(wheel, "pointer: wheel down");
        // The log's masking leaves the line as it is.
        let down = pointer_line(&ev(PointerKind::Down, 0), &mut None).unwrap();
        let masked = tv_base::eventlog::scrub::scrub_local(&iptv::redact::text(&down));
        assert_eq!(masked, down);
    }

    fn text_event(bytes: &[u8]) -> Event {
        let mut ev = event(&[(0, crate::sdl::SDL_TEXTINPUT)]);
        ev.0[TEXT_AT..TEXT_AT + bytes.len()].copy_from_slice(bytes);
        ev
    }

    #[test]
    fn text_events_decode_at_this_builds_offset_and_drop_cut_characters() {
        assert_eq!(decode_text(&text_event(b"news\0junk")), "news");
        assert_eq!(
            decode_text(&text_event("\u{17c}\u{f3}\u{142}w".as_bytes())),
            "\u{17c}\u{f3}\u{142}w"
        );
        // A full array cut inside a two-byte character at both ends: the halves go, the rest stays.
        let mut full = [b'a'; TEXT_LEN];
        full[0] = 0xBC;
        full[TEXT_LEN - 1] = 0xC5;
        assert_eq!(decode_text(&text_event(&full)), "a".repeat(TEXT_LEN - 2));
        assert_eq!(decode_text(&text_event(b"a\xffb")), "a\u{fffd}b");
        assert_eq!(decode_text(&text_event(b"")), "");
    }
}
