//! The parental PIN dialog: four digits from the remote, checked against the stored verifier.
//!
//! The number keys type a digit each. A remote without them uses Up and Down to pick the digit
//! in the next box and OK (or Right) to take it; Left or the keyboard's delete takes the last
//! one back, Back cancels. The fourth digit submits at once: the right PIN does the dialog's
//! [`PinTask`] and closes it, a wrong one clears the boxes and says so. Changing the PIN asks
//! for the current one, then the new one twice.
//!
//! The dialog holds the stored verifier, never the PIN, and the digits typed only until they
//! are checked. The shell opens it for [`Navigate::Pin`](crate::data::Navigate::Pin) and for a
//! play it held back because the channel is locked.

use std::ffi::CStr;

use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::consts::{SCR_H, SCR_W};
use tv_ui::{Painter, Rect};

use crate::data::{Command, Logos, Output, PinTask, Screen, TvData};
use crate::icons::{self, Icon};
use crate::input::{Hold, Key, Press};
use crate::panels::keys;
use crate::parental::{hash_pin, pin_matches, PIN_LEN};
use crate::theme::{self, size, space};

const CARD_W: f32 = 760.0;
const CARD_H: f32 = 420.0;
const BOX: f32 = 96.0;
const BOX_H: f32 = 112.0;
const BOX_GAP: f32 = space::L;
const RIM: f32 = 3.0;
const DOT: f32 = 22.0;
const LOCK: f32 = 40.0;
/// Over the screens under the dialog.
const DIM: [f32; 4] = [0.0, 0.0, 0.0, 0.6];
/// What a box shows of the digit Up and Down picked.
const DIGITS: [&CStr; 10] = [c"0", c"1", c"2", c"3", c"4", c"5", c"6", c"7", c"8", c"9"];

/// Which PIN the boxes take.
enum Stage {
    Current,
    New,
    /// The new PIN typed once, waiting for the same digits again.
    Confirm(String),
}

/// The line under the boxes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Note {
    Hint,
    Wrong,
    Mismatch,
}

pub struct PinDialog {
    task: PinTask,
    /// The stored verifier ([`keys::PARENTAL_PIN`]), checked against, never shown.
    stored: String,
    stage: Stage,
    digits: Vec<u8>,
    /// The digit Up and Down show in the next box, until OK takes it.
    wheel: Option<u8>,
    note: Note,
    closed: bool,
    /// The title and the note as drawn, read again when they change.
    title: &'static CStr,
    line: &'static CStr,
}

impl PinDialog {
    /// The dialog for `task`, checking against `stored` (the [`keys::PARENTAL_PIN`] value).
    pub fn new(task: PinTask, stored: &str) -> Self {
        let mut d = PinDialog {
            task,
            stored: stored.to_owned(),
            stage: Stage::Current,
            digits: Vec::with_capacity(PIN_LEN),
            wheel: None,
            note: Note::Hint,
            closed: false,
            title: c"",
            line: c"",
        };
        d.relabel();
        d
    }

    pub fn task(&self) -> &PinTask {
        &self.task
    }

    /// The title shown: which PIN is asked for.
    pub fn title(&self) -> &str {
        self.title.to_str().unwrap_or("")
    }

    /// The line under the boxes: the hint, or what went wrong.
    pub fn note(&self) -> &str {
        self.line.to_str().unwrap_or("")
    }

    /// Digits typed so far.
    pub fn typed(&self) -> usize {
        self.digits.len()
    }

    fn relabel(&mut self) {
        self.title = match self.stage {
            Stage::Current => msg::parental_enter_pin_c(),
            Stage::New => msg::parental_new_pin_c(),
            Stage::Confirm(_) => msg::parental_confirm_pin_c(),
        };
        self.line = match self.note {
            Note::Hint => msg::parental_hint_c(),
            Note::Wrong => msg::parental_wrong_pin_c(),
            Note::Mismatch => msg::parental_mismatch_c(),
        };
        tv_machine::idle::invalidate();
    }

    fn push(&mut self, digit: u8) -> Output {
        self.wheel = None;
        self.digits.push(digit % 10);
        if self.digits.len() < PIN_LEN {
            return Output::redraw();
        }
        let entered: String = self
            .digits
            .drain(..)
            .map(|d| char::from(b'0' + d))
            .collect();
        let out = self.submit(entered);
        self.relabel();
        Output {
            redraw: true,
            ..out
        }
    }

    fn submit(&mut self, entered: String) -> Output {
        match std::mem::replace(&mut self.stage, Stage::Current) {
            Stage::Current if !pin_matches(&self.stored, &entered) => {
                self.note = Note::Wrong;
                Output::default()
            }
            Stage::Current => {
                self.note = Note::Hint;
                match &self.task {
                    PinTask::Change => {
                        self.stage = Stage::New;
                        Output::default()
                    }
                    PinTask::Open(lock) => {
                        self.done(Output::command(Command::Unlock(lock.clone())))
                    }
                    PinTask::Set { key, value, toast } => {
                        let set = Command::SetSetting {
                            key: (*key).to_owned(),
                            value: value.clone(),
                        };
                        let toast = Some(toast.clone());
                        self.done(Output {
                            toast,
                            ..Output::command(set)
                        })
                    }
                }
            }
            Stage::New => {
                self.note = Note::Hint;
                self.stage = Stage::Confirm(entered);
                Output::default()
            }
            Stage::Confirm(new) if new != entered => {
                self.note = Note::Mismatch;
                self.stage = Stage::New;
                Output::default()
            }
            Stage::Confirm(new) => {
                let set = Command::SetSetting {
                    key: keys::PARENTAL_PIN.to_owned(),
                    value: hash_pin(&new),
                };
                self.done(Output {
                    toast: Some(msg::parental_pin_changed().to_owned()),
                    ..Output::command(set)
                })
            }
        }
    }

    /// The task's output, then the dialog closes.
    fn done(&mut self, out: Output) -> Output {
        self.closed = true;
        out.and(Output::close())
    }
}

impl Screen for PinDialog {
    fn hold(&self, key: Key) -> Hold {
        match key {
            Key::Up | Key::Down | Key::Backspace => Hold::Repeat,
            _ => Hold::Once,
        }
    }

    fn key(&mut self, key: Key, press: Press, _: &dyn TvData, _: u64) -> Output {
        if self.closed || press == Press::Long {
            return Output::default();
        }
        match key {
            Key::Digit(d) => self.push(d),
            Key::Up | Key::Down => {
                let step = if key == Key::Up { 1 } else { 9 };
                self.wheel = Some((self.wheel.unwrap_or(0) + step) % 10);
                tv_machine::idle::invalidate();
                Output::redraw()
            }
            Key::Ok | Key::Right => match self.wheel {
                Some(d) => self.push(d),
                None => Output::default(),
            },
            Key::Left | Key::Backspace => {
                self.wheel = None;
                self.digits.pop();
                tv_machine::idle::invalidate();
                Output::redraw()
            }
            Key::Clear => {
                self.wheel = None;
                self.digits.clear();
                tv_machine::idle::invalidate();
                Output::redraw()
            }
            Key::Back => {
                self.closed = true;
                Output::close()
            }
            _ => Output::default(),
        }
    }

    fn closing(&self) -> bool {
        self.closed
    }

    fn draw(&mut self, p: Painter, _: &dyn Measure, _: &dyn Logos) {
        if self.closed {
            return;
        }
        p.rrect(Rect::new(0.0, 0.0, SCR_W, SCR_H), 0.0, 0.0, DIM);
        let card = Rect::new(
            (SCR_W - CARD_W) * 0.5,
            (SCR_H - CARD_H) * 0.5,
            CARD_W,
            CARD_H,
        );
        p.shadow(card, theme::radius::PILL, 48.0, 0.0, [0.0, 0.0, 0.0, 0.5]);
        p.rect(card, theme::radius::PILL, theme::PANEL, theme::PANEL, 0.0);
        let cx = card.x + card.w * 0.5;
        let lock = Rect::new(cx - LOCK * 0.5, card.y + space::XL, LOCK, LOCK);
        icons::draw(p, Icon::Lock, lock, theme::TEXT_MUTED);
        let title_y = lock.y + LOCK + space::M;
        p.text(
            self.title.as_ptr(),
            cx,
            title_y,
            size::HEADING,
            theme::TEXT,
            1,
            theme::BOLD,
        );
        let row_w = PIN_LEN as f32 * BOX + (PIN_LEN - 1) as f32 * BOX_GAP;
        let top = title_y + size::HEADING as f32 * 1.2 + space::XL;
        let cursor = self.digits.len();
        for i in 0..PIN_LEN {
            let r = Rect::new(
                cx - row_w * 0.5 + i as f32 * (BOX + BOX_GAP),
                top,
                BOX,
                BOX_H,
            );
            let rim = if i == cursor {
                theme::ink(theme::ACCENT)
            } else {
                theme::CELL_ROW
            };
            p.rect(r, theme::radius::PILL, rim, rim, 0.0);
            let inner = Rect::new(r.x + RIM, r.y + RIM, r.w - 2.0 * RIM, r.h - 2.0 * RIM);
            p.rect(
                inner,
                theme::radius::PILL,
                theme::PANEL_RAISED,
                theme::PANEL_RAISED,
                0.0,
            );
            if i < cursor {
                let dot = Rect::new(r.x + (r.w - DOT) * 0.5, r.y + (r.h - DOT) * 0.5, DOT, DOT);
                p.rect(dot, DOT * 0.5, theme::TEXT, theme::TEXT, 0.0);
            } else if let Some(d) = self.wheel.filter(|_| i == cursor) {
                let y = r.y + (r.h - size::TITLE as f32 * 1.2) * 0.5;
                p.text(
                    DIGITS[usize::from(d)].as_ptr(),
                    r.x + r.w * 0.5,
                    y,
                    size::TITLE,
                    theme::TEXT,
                    1,
                    theme::BOLD,
                );
            }
        }
        let ink = if self.note == Note::Hint {
            theme::TEXT_MUTED
        } else {
            theme::ACCENT_TEXT
        };
        p.text(
            self.line.as_ptr(),
            cx,
            top + BOX_H + space::XL,
            size::LABEL,
            theme::ink(ink),
            1,
            theme::REGULAR,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::FakeData;
    use crate::data::Lock;
    use crate::panels::testing::{draw, press};
    use crate::parental::DEFAULT_PIN;
    use iptv::model::ChannelKey;

    fn digits(pin: &str) -> Vec<Key> {
        pin.bytes().map(|b| Key::Digit(b - b'0')).collect()
    }

    fn keys(d: &mut PinDialog, keys: &[Key]) -> Output {
        press(d, &FakeData::new(0, 0), keys)
    }

    fn open() -> PinTask {
        PinTask::Open(Lock::Channel(ChannelKey(5)))
    }

    #[test]
    fn the_default_pin_opens_and_closes_the_dialog() {
        let mut d = PinDialog::new(open(), "");
        assert_eq!(d.title(), "Enter PIN");
        assert_eq!(keys(&mut d, &digits("000")), Output::default());
        assert_eq!(d.typed(), 3);
        draw(&mut d);
        assert_eq!(
            keys(&mut d, &digits("0")),
            Output::command(Command::Unlock(Lock::Channel(ChannelKey(5)))).and(Output::close())
        );
        assert!(d.closing());
        assert_eq!(keys(&mut d, &[Key::Digit(1)]), Output::default(), "closed");
    }

    #[test]
    fn a_wrong_pin_clears_the_boxes_and_says_so() {
        let mut d = PinDialog::new(open(), &hash_pin("2468"));
        assert_eq!(keys(&mut d, &digits(DEFAULT_PIN)), Output::default());
        assert_eq!((d.typed(), d.note()), (0, "Wrong PIN, try again"));
        assert!(!d.closing());
        let out = keys(&mut d, &digits("2468"));
        assert_eq!(out.commands.len(), 1);
        assert!(d.closing());
    }

    #[test]
    fn up_down_and_ok_type_a_digit_and_left_takes_one_back() {
        let mut d = PinDialog::new(open(), &hash_pin("1909"));
        keys(&mut d, &[Key::Ok]);
        assert_eq!(d.typed(), 0, "OK without a picked digit does nothing");
        keys(&mut d, &[Key::Up, Key::Ok]);
        keys(&mut d, &[Key::Down, Key::Right]);
        keys(&mut d, &[Key::Digit(7), Key::Left]);
        assert_eq!(d.typed(), 2);
        draw(&mut d);
        keys(&mut d, &[Key::Up, Key::Up]);
        draw(&mut d);
        keys(&mut d, &[Key::Down, Key::Down, Key::Ok]);
        let out = keys(&mut d, &[Key::Digit(9)]);
        assert!(d.closing(), "1, 9, 0, 9: {out:?}");
        let mut d = PinDialog::new(open(), "");
        keys(&mut d, &[Key::Digit(1), Key::Digit(2), Key::Clear]);
        assert_eq!(d.typed(), 0);
    }

    #[test]
    fn back_cancels_without_doing_the_task() {
        let mut d = PinDialog::new(open(), "");
        assert_eq!(keys(&mut d, &[Key::Digit(0), Key::Back]), Output::close());
        assert!(d.closing());
        assert_eq!(
            d.key(Key::Ok, Press::Long, &FakeData::new(0, 0), 0),
            Output::default()
        );
    }

    #[test]
    fn a_set_task_stores_its_value_with_its_toast() {
        let task = PinTask::Set {
            key: keys::PARENTAL_GROUPS,
            value: "Kids".into(),
            toast: "Group locked".into(),
        };
        let mut d = PinDialog::new(task, "");
        let out = keys(&mut d, &digits(DEFAULT_PIN));
        assert_eq!(
            out,
            Output {
                toast: Some("Group locked".into()),
                ..Output::command(Command::SetSetting {
                    key: keys::PARENTAL_GROUPS.into(),
                    value: "Kids".into(),
                })
                .and(Output::close())
            }
        );
    }

    #[test]
    fn changing_the_pin_asks_the_current_one_then_the_new_one_twice() {
        let mut d = PinDialog::new(PinTask::Change, "");
        keys(&mut d, &digits("1111"));
        assert_eq!(d.note(), "Wrong PIN, try again");
        keys(&mut d, &digits(DEFAULT_PIN));
        assert_eq!(d.title(), "Enter a new PIN");
        keys(&mut d, &digits("4321"));
        assert_eq!(d.title(), "Enter the new PIN again");
        keys(&mut d, &digits("4322"));
        assert_eq!(
            (d.title(), d.note()),
            ("Enter a new PIN", "The PINs did not match")
        );
        keys(&mut d, &digits("4321"));
        let out = keys(&mut d, &digits("4321"));
        assert_eq!(out.toast.as_deref(), Some("PIN changed"));
        let [Command::SetSetting { key, value }] = &out.commands[..] else {
            panic!("{out:?}");
        };
        assert_eq!(key, keys::PARENTAL_PIN);
        assert!(!value.contains("4321") && pin_matches(value, "4321"));
        assert!(d.closing());
    }

    #[test]
    fn the_dialog_reads_in_polish() {
        let _pl =
            tv_platform::i18n::language_on_this_thread_for_test(tv_platform::i18n::Preference::Pl);
        let mut d = PinDialog::new(open(), "");
        assert_eq!(d.title(), "Wpisz PIN");
        keys(&mut d, &digits("1234"));
        assert_eq!(d.note(), "B\u{142}\u{119}dny PIN, spr\u{f3}buj ponownie");
    }
}
