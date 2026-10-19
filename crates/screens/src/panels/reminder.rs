//! The reminder prompt: when a reminded programme starts, a card at the bottom of the screen
//! names it and its channel. OK switches to the channel full screen (through the parental PIN
//! when the channel is locked); Back, or any other key, dismisses it, and it goes by itself
//! after [`PROMPT_MS`]. The shell opens it ([`crate::reminders`]).

use iptv::model::ChannelKey;
use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::consts::{SCR_H, SCR_W};
use tv_ui::{Painter, Rect};

use super::side::line;
use crate::data::{Command, Logos, Navigate, Output, Screen, Stream, TvData};
use crate::icons::{self, Icon};
use crate::input::{Hold, Key, Press};
use crate::reminders::Reminder;
use crate::theme::{self, size, space};

/// How long the prompt waits for OK.
pub const PROMPT_MS: u64 = 15_000;

const CARD_W: f32 = 960.0;
const CARD_H: f32 = 176.0;
const BOTTOM: f32 = 120.0;
const BELL: f32 = 56.0;

pub struct ReminderPrompt {
    channel: ChannelKey,
    title: String,
    line: String,
    hint: &'static str,
    /// When it goes by itself, on the shell's clock.
    until: u64,
    closed: bool,
}

impl ReminderPrompt {
    /// The prompt for `reminder` on the channel named `channel`, shown from `now_ms`.
    pub fn new(reminder: &Reminder, channel: &str, now_ms: u64) -> Self {
        ReminderPrompt {
            channel: reminder.channel,
            title: reminder.title.clone(),
            line: msg::reminder_starting(channel),
            hint: msg::reminder_watch_hint(),
            until: now_ms + PROMPT_MS,
            closed: false,
        }
    }

    pub fn channel(&self) -> ChannelKey {
        self.channel
    }

    /// The programme's title and the line under it.
    pub fn text(&self) -> (&str, &str) {
        (&self.title, &self.line)
    }
}

impl Screen for ReminderPrompt {
    fn hold(&self, _: Key) -> Hold {
        Hold::Once
    }

    fn key(&mut self, key: Key, press: Press, _: &dyn TvData, _: u64) -> Output {
        if self.closed || press == Press::Long {
            return Output::default();
        }
        self.closed = true;
        if key != Key::Ok {
            return Output {
                redraw: true,
                ..Output::close()
            };
        }
        let play = Command::Play {
            stream: Stream::Live(self.channel),
            group: None,
        };
        Output {
            redraw: true,
            ..Output::command(play).and(Output::navigate(Navigate::Player))
        }
    }

    fn tick(&mut self, _: &dyn TvData, now_ms: u64) -> Output {
        if self.closed || now_ms < self.until {
            return Output::default();
        }
        self.closed = true;
        Output {
            redraw: true,
            ..Output::close()
        }
    }

    fn next_wake(&self, _: u64) -> Option<u64> {
        (!self.closed).then_some(self.until)
    }

    fn closing(&self) -> bool {
        self.closed
    }

    fn draw(&mut self, p: Painter, _: &dyn Measure, _: &dyn Logos) {
        if self.closed {
            return;
        }
        let card = Rect::new(
            (SCR_W - CARD_W) * 0.5,
            SCR_H - BOTTOM - CARD_H,
            CARD_W,
            CARD_H,
        );
        p.shadow(card, theme::radius::PILL, 32.0, 0.0, [0.0, 0.0, 0.0, 0.5]);
        p.rect(
            card,
            theme::radius::PILL,
            theme::PANEL_RAISED,
            theme::PANEL_RAISED,
            0.0,
        );
        let bell = Rect::new(
            card.x + space::XL,
            card.y + (card.h - BELL) * 0.5,
            BELL,
            BELL,
        );
        icons::draw(p, Icon::Bell, bell, theme::ACCENT_TEXT);
        let x = bell.x + BELL + space::XL;
        let w = card.x + card.w - space::XL - x;
        let row = |i: f32| Rect::new(x, card.y + space::M + i * 48.0, w, 48.0);
        line(p, &self.title, size::BODY, theme::TEXT, true, row(0.0));
        line(
            p,
            &self.line,
            size::LABEL,
            theme::TEXT_MUTED,
            false,
            row(1.0),
        );
        line(
            p,
            self.hint,
            size::LABEL,
            theme::ink(theme::ACCENT_TEXT),
            false,
            row(2.0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::FakeData;
    use crate::panels::testing::draw;

    fn prompt() -> ReminderPrompt {
        let reminder = Reminder {
            channel: ChannelKey(9),
            start: 1_700_000_000,
            title: "Film night".into(),
        };
        ReminderPrompt::new(&reminder, "Channel 9", 1_000)
    }

    #[test]
    fn ok_switches_to_the_channel_full_screen() {
        let mut p = prompt();
        assert_eq!(p.text(), ("Film night", "Starting now on Channel 9"));
        draw(&mut p);
        let out = p.key(Key::Ok, Press::Short, &FakeData::new(0, 0), 2_000);
        let play = Command::Play {
            stream: Stream::Live(ChannelKey(9)),
            group: None,
        };
        assert_eq!(out.commands, [play]);
        assert_eq!(out.navigate, [Navigate::Player]);
        assert!(p.closing() && p.next_wake(2_000).is_none());
    }

    #[test]
    fn another_key_dismisses_it_and_it_goes_by_itself() {
        let data = FakeData::new(0, 0);
        let mut p = prompt();
        assert_eq!(p.key(Key::Ok, Press::Long, &data, 0), Output::default());
        assert_eq!(
            p.key(Key::Down, Press::Short, &data, 0).navigate,
            [Navigate::Close]
        );
        assert!(p.closing());
        let mut p = prompt();
        assert_eq!(p.next_wake(1_000), Some(1_000 + PROMPT_MS));
        assert_eq!(p.tick(&data, PROMPT_MS), Output::default(), "not yet");
        assert_eq!(p.tick(&data, 1_000 + PROMPT_MS).navigate, [Navigate::Close]);
        assert!(p.closing() && p.next_wake(0).is_none());
    }
}
