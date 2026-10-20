//! Channel number entry, shared by the player and the TV guide: digits typed on the remote name a
//! channel by its number (its declared number, else its position in the playlist), taken when
//! the typing stops ([`TIMEOUT_MS`]) or on OK. The player plays the channel, the guide moves its
//! focus there.

use std::rc::Rc;

use iptv::model::Channel;
use tv_gfx::gfx::tokens::with_a;
use tv_machine::machine::Measure;
use tv_ui::Rect;

use crate::data::{Group, TvData};
use crate::draw::{self, HAlign, Piece, VAlign};
use crate::input::Key;
use crate::theme::{self, radius, size, space};

/// How long a typed channel number waits for the next digit.
pub const TIMEOUT_MS: u64 = 2_000;
/// The most digits a channel number takes.
pub const DIGITS: usize = 4;

/// A channel number being typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumberEntry {
    pub digits: String,
    /// The channel the digits name so far, when there is one.
    pub preview: Option<String>,
    pub commit_at_ms: u64,
}

/// What a key other than a digit does while a number is being typed.
#[derive(Debug, PartialEq, Eq)]
pub enum Interrupt {
    /// OK: take the digits now.
    Commit(NumberEntry),
    /// Back: the digits are dropped and the key is spent.
    Dropped,
    /// Any other key, or no number being typed: the digits (if any) are dropped and the key acts
    /// as usual.
    Pass,
}

impl NumberEntry {
    /// Type `d` into `entry`, opening it with the first digit: the commit moves to [`TIMEOUT_MS`]
    /// from now and the preview names the channel the digits name so far.
    pub fn type_digit(entry: &mut Option<NumberEntry>, d: u8, data: &dyn TvData, now_ms: u64) {
        if d > 9 {
            return;
        }
        let e = entry.get_or_insert(NumberEntry {
            digits: String::new(),
            preview: None,
            commit_at_ms: 0,
        });
        if e.digits.len() < DIGITS {
            e.digits.push(char::from(b'0' + d));
        }
        e.commit_at_ms = now_ms + TIMEOUT_MS;
        e.preview = e.channel(data).map(|c| c.name.clone());
    }

    /// `entry` once the typing stopped long enough, taken out of it.
    pub fn due(entry: &mut Option<NumberEntry>, now_ms: u64) -> Option<NumberEntry> {
        entry.take_if(|e| now_ms >= e.commit_at_ms)
    }

    /// What `key` does to `entry` (see [`Interrupt`]); the entry is closed either way.
    pub fn interrupt(entry: &mut Option<NumberEntry>, key: Key) -> Interrupt {
        match (entry.take(), key) {
            (Some(e), Key::Ok) => Interrupt::Commit(e),
            (Some(_), Key::Back) => Interrupt::Dropped,
            _ => Interrupt::Pass,
        }
    }

    /// The channel the digits name.
    pub fn channel(&self, data: &dyn TvData) -> Option<Rc<Channel>> {
        self.digits.parse::<u32>().ok().and_then(|n| find(data, n))
    }
}

/// The channel numbered `n`: its declared number, else its position in the playlist.
pub fn find(data: &dyn TvData, n: u32) -> Option<Rc<Channel>> {
    data.channels(&Group::All)
        .iter()
        .enumerate()
        .find(|(i, c)| draw::channel_number(c, *i) == n)
        .map(|(_, c)| Rc::clone(c))
}

/// The digits typed so far and the channel they name, in a box whose top right corner is
/// `(right, top)`.
pub fn paint(out: &mut Vec<Piece>, entry: &NumberEntry, right: f32, top: f32, m: &dyn Measure) {
    let digits = entry.digits.as_str();
    let digits_w = m.width_str(digits, size::HEADER, true);
    let preview = entry
        .preview
        .as_deref()
        .map(|p| draw::fit(m, p, 600.0, size::BODY, false));
    let preview_w = preview.as_ref().map_or(0.0, |(_, w)| *w);
    let w = digits_w.max(preview_w) + 2.0 * space::L;
    let mut h = m.line_h(size::HEADER) + 2.0 * space::M;
    if preview.is_some() {
        h += m.line_h(size::BODY);
    }
    let panel = Rect::new(right - w, top, w, h);
    out.push(Piece::solid(
        panel,
        radius::PILL,
        with_a(theme::PANEL, 0.95),
    ));
    let line = |s, y, size, bold, color| Piece::Text {
        s,
        x: right - space::L,
        y,
        size,
        bold,
        color,
        h: HAlign::Right,
        v: VAlign::Top,
    };
    out.push(line(
        draw::run(digits),
        top + space::M,
        size::HEADER,
        true,
        theme::TEXT,
    ));
    if let Some((p, _)) = preview {
        out.push(line(
            p,
            top + space::M + m.line_h(size::HEADER),
            size::BODY,
            false,
            theme::TEXT_MUTED,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::{FakeData, Mono};

    #[test]
    fn typing_names_the_channel_and_ok_back_and_other_keys_close_the_entry() {
        let data = FakeData::new(0, 12);
        let mut entry = None;
        NumberEntry::type_digit(&mut entry, 1, &data, 100);
        NumberEntry::type_digit(&mut entry, 2, &data, 900);
        let e = entry.as_ref().unwrap();
        assert_eq!(e.digits, "12");
        assert_eq!(e.preview.as_deref(), Some("Channel 12"));
        assert_eq!(e.commit_at_ms, 900 + TIMEOUT_MS);
        assert_eq!(NumberEntry::due(&mut entry, 900 + TIMEOUT_MS - 1), None);
        assert!(entry.is_some());

        let taken = entry.clone();
        assert_eq!(
            NumberEntry::interrupt(&mut entry, Key::Ok),
            Interrupt::Commit(taken.unwrap())
        );
        NumberEntry::type_digit(&mut entry, 3, &data, 0);
        assert_eq!(
            NumberEntry::interrupt(&mut entry, Key::Back),
            Interrupt::Dropped
        );
        NumberEntry::type_digit(&mut entry, 3, &data, 0);
        assert_eq!(
            NumberEntry::interrupt(&mut entry, Key::Down),
            Interrupt::Pass
        );
        assert_eq!(entry, None, "every key but a digit closes the entry");
        assert_eq!(NumberEntry::interrupt(&mut entry, Key::Ok), Interrupt::Pass);
    }

    #[test]
    fn the_box_ends_at_its_corner_with_the_digits_over_the_channel() {
        let data = FakeData::new(0, 12);
        let mut entry = None;
        NumberEntry::type_digit(&mut entry, 7, &data, 0);
        let mut out = Vec::new();
        paint(&mut out, entry.as_ref().unwrap(), 1800.0, 40.0, &Mono);
        let [Piece::Fill { r, .. }, Piece::Text { s: digits, x, .. }, Piece::Text { s: name, .. }] =
            &out[..]
        else {
            panic!("a box, the digits and the channel: {out:?}");
        };
        assert_eq!((r.x + r.w, r.y), (1800.0, 40.0));
        assert_eq!(*x, 1800.0 - space::L);
        assert_eq!(digits.to_str(), Ok("7"));
        assert_eq!(name.to_str(), Ok("Channel 7"));
    }
}
