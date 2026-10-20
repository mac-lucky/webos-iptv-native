//! The Magic Remote's pointer: hover, click and wheel, and the hand-over back to the keys.
//!
//! While its cursor shows, webOS sends the remote as pointer events: motion, its OK (the wheel
//! pressed) as a button, the wheel turned. Motion focuses what the screen in front draws under
//! the pointer ([`Screen::point`](tv_screens::data::Screen::point)). The button is OK, pressed
//! and released through the same [`Remote`](tv_screens::input::Remote) as the key, so a held
//! wheel is a long OK; a click on nothing is OK on the focus, and a click on a row the screen
//! draws but lets nobody point at does nothing
//! ([`Screen::dead`](tv_screens::data::Screen::dead)). A wheel step is Up or Down where the
//! screen in front scrolls ([`Screen::wheel`](tv_screens::data::Screen::wheel)).
//!
//! Keys and pointer share one focus. A key (or a wheel step) moves it away from the pointer, and
//! the pointer takes it back only once it has travelled [`TRAVEL`] since: the hand holding the
//! remote drifts the cursor, and pressing OK jiggles it. Until then a click is OK on what the
//! keys focused. One press the remote reports both as a key and as a button counts once, as a
//! click whichever came first, and a press may end on the other kind of edge (a button pressed,
//! a key released): either release lets go of the OK.

use tv_screens::data::Output;
use tv_screens::input::{Action, Key, Press};

use super::Shell;
use crate::input::{PointerEvent, PointerKind};

/// Pointer travel (canvas px, summed) after a key before motion focuses again.
const TRAVEL: f32 = 120.0;
/// A key and a button for the same OK arrive this close together.
const TWIN_MS: u32 = 150;
/// The wheel steps at most this often: one notch can arrive as a burst of events.
const WHEEL_MS: u32 = 80;

/// Which kind of edge pressed the OK the remote holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum By {
    Key,
    Button,
}

#[derive(Debug, Default)]
pub struct Pointer {
    /// Where the pointer was last seen.
    at: Option<(f32, f32)>,
    /// Travel since the last key while the keys have the focus; `None` once the pointer has it.
    gated: Option<f32>,
    /// `gated` before the last key: the button report of an OK whose key report came first puts
    /// it back, as the press was a click.
    ungated: Option<f32>,
    /// The last OK press: by which edge, when, and whether it was dropped (a click on a dead
    /// spot, whose other report goes too).
    ok: Option<(By, u32, bool)>,
    /// The last wheel step.
    wheel_at: Option<u32>,
    /// When the event log last noted a motion.
    logged_move: Option<u32>,
}

impl Pointer {
    /// A key moved the focus: the pointer takes it back once it travels.
    fn gate(&mut self) {
        self.ungated = self.gated.replace(0.0);
    }

    /// The pointer is at `(x, y)`: whether it has the focus.
    fn moved(&mut self, x: f32, y: f32) -> bool {
        if let (Some((px, py)), Some(travel)) = (self.at, self.gated.as_mut()) {
            *travel += (x - px).abs() + (y - py).abs();
        }
        self.at = Some((x, y));
        if self.gated.is_some_and(|t| t >= TRAVEL) {
            self.gated = None;
        }
        self.gated.is_none()
    }

    /// An OK down by `by` at `at`: `false` when it is the other report of the press the remote
    /// already holds, or of one that was dropped. A button report after the key report takes
    /// back the gate the key put on the pointer.
    fn ok_down(&mut self, by: By, at: u32, held: bool) -> bool {
        let twin = self.ok.is_some_and(|(b, t, dropped)| {
            (held || dropped) && b != by && at.wrapping_sub(t) < TWIN_MS
        });
        if !twin {
            self.ok = Some((by, at, false));
        } else if by == By::Button {
            self.gated = self.ungated;
        }
        !twin
    }

    /// The OK press just taken does nothing.
    fn drop_ok(&mut self) {
        if let Some(ok) = self.ok.as_mut() {
            ok.2 = true;
        }
    }
}

impl Shell {
    /// The event log's line for pointer event `ev`, asked before [`pointer`](Self::pointer)
    /// takes it ([`crate::input::pointer_line`]). Nothing while the keyboard types (where a click
    /// lands on it is what it typed) or a secret is entered (the wheel and a click pick its
    /// digits).
    pub fn pointer_line(&mut self, ev: &PointerEvent) -> Option<String> {
        if self.typing || self.secret() {
            return None;
        }
        crate::input::pointer_line(ev, &mut self.pointer.logged_move)
    }

    /// One pointer event. While the TV keyboard is up it has the remote, and the pointer is
    /// left to it.
    pub fn pointer(&mut self, ev: PointerEvent) {
        if self.typing {
            return;
        }
        let now = u64::from(ev.at_ms);
        match ev.kind {
            PointerKind::Move => {
                if self.pointer.moved(ev.x, ev.y) {
                    self.hover(ev.x, ev.y, now);
                }
            }
            PointerKind::Down => {
                let held = self.remote.holds(Key::Ok);
                let first = self.pointer.ok_down(By::Button, ev.at_ms, held);
                let free = self.pointer.moved(ev.x, ev.y);
                if free && self.screen(self.top()).0.dead(ev.x, ev.y) {
                    if !first && held {
                        // The key report came first: the OK it holds goes too.
                        self.remote.forget();
                    }
                    self.pointer.drop_ok();
                    return;
                }
                if !first {
                    return;
                }
                if free {
                    self.hover(ev.x, ev.y, now);
                }
                let hold = self.screen(self.top()).0.hold(Key::Ok);
                if let Some(action) = self.remote.down(Key::Ok, ev.at_ms, hold) {
                    self.act(action, now);
                }
            }
            PointerKind::Up => {
                if let Some(action) = self.remote.up(Key::Ok, ev.at_ms) {
                    self.act(action, now);
                }
            }
            PointerKind::Wheel { up } => {
                let last = self.pointer.wheel_at;
                let early = last.is_some_and(|t| ev.at_ms.wrapping_sub(t) < WHEEL_MS);
                if early || !self.screen(self.top()).0.wheel() {
                    return;
                }
                self.pointer.wheel_at = Some(ev.at_ms);
                self.pointer.gate();
                let key = if up { Key::Up } else { Key::Down };
                self.act(
                    Action {
                        key,
                        press: Press::Short,
                    },
                    now,
                );
            }
        }
    }

    /// A key-down from the remote: `false` when the pointer's button already pressed it (one OK
    /// reported twice), else the keys take the focus from the pointer.
    pub(super) fn key_down(&mut self, key: Key, at: u32) -> bool {
        if key == Key::Ok {
            let held = self.remote.holds(Key::Ok);
            if !self.pointer.ok_down(By::Key, at, held) {
                return false;
            }
        }
        self.pointer.gate();
        true
    }

    /// Focus what the screen in front draws at `(x, y)`; nothing is redrawn when that is what
    /// has the focus already.
    fn hover(&mut self, x: f32, y: f32, now: u64) {
        let from = self.top();
        let (screen, data) = self.screen(from);
        let out = screen.point(x, y, data, now);
        if out != Output::default() {
            self.apply(out, from, now);
            self.settle(now);
        }
    }
}
