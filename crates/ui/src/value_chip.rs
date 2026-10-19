//! A menu-opening capsule with a regular label, bold value and optional owner annotation.
//! Factored from Library's existing toolbar; callers own the strings, state and menu action.

use std::ffi::CStr;
use super::{Env, Painter, Rect, View, icons::{self, Icon}, theme};
use tv_machine::{machine::Measure};

const PAD: f32 = 24.0;
const ICON_SIZE: f32 = 22.0;
const ICON_GAP: f32 = 10.0;
const NOTE_ALPHA: f32 = 0.62;

pub struct ValueChip<'a> {
    name: &'a CStr,
    value: &'a CStr,
    note: Option<&'a CStr>,
    rect: Rect,
    focused: bool,
    icon: Icon,
}

impl<'a> ValueChip<'a> {
    /// `value` and `note` include their own separators (for example `" · Title"` and `"  owner"`).
    pub fn new(name: &'a CStr, value: &'a CStr, note: Option<&'a CStr>, rect: Rect) -> Self {
        Self { name, value, note, rect, focused: false, icon: Icon::ChevronDown }
    }

    pub fn focused(mut self, focused: bool) -> Self { self.focused = focused; self }
    pub fn icon(mut self, icon: Icon) -> Self { self.icon = icon; self }

    /// The same font roles used by draw. Layout must use the host's measurement capability,
    /// including the recording/replay table, rather than consulting a live font directly.
    pub fn width(measure: &dyn Measure, name: &CStr, value: &CStr, note: Option<&CStr>) -> f32 {
        PAD * 2.0 + ICON_GAP + ICON_SIZE
            + measure.width(name, theme::size::LABEL, false)
            + measure.width(value, theme::size::LABEL, true)
            + note.map_or(0.0, |note| measure.width(note, theme::size::MICRO, false))
    }
}

impl View for ValueChip<'_> {
    fn draw(&self, _: &Env, p: Painter) {
        let (fill, value_ink, name_ink) = if self.focused {
            (super::ACCENT, super::ACCENT_INK, theme::with_a(super::ACCENT_INK, NOTE_ALPHA))
        } else {
            (theme::CONTROL_IDLE_FILL, theme::CONTROL_IDLE_INK, theme::TEXT_SECONDARY)
        };
        let r = self.rect;
        p.rrect(r, r.h * 0.5, r.h * 0.5, fill);
        let y = tv_gfx::text::text_vcenter_y(theme::size::LABEL, 1, r.y + r.h * 0.5);
        let mut x = r.x + PAD;
        x += p.text(self.name.as_ptr(), x, y, theme::size::LABEL, name_ink, 0, 0);
        x += p.text(self.value.as_ptr(), x, y, theme::size::LABEL, value_ink, 0, 1);
        if let Some(note) = self.note {
            let y = tv_gfx::text::baseline_y(theme::size::MICRO, 0, theme::size::LABEL, 1, y);
            x += p.text(note.as_ptr(), x, y, theme::size::MICRO,
                theme::with_a(value_ink, NOTE_ALPHA), 0, 0);
        }
        icons::draw(p, self.icon, Rect::new(x + ICON_GAP, r.y + (r.h - ICON_SIZE) * 0.5, ICON_SIZE, ICON_SIZE),
            if self.focused { super::ACCENT_INK } else { theme::TEXT_SECONDARY });
    }
}

/// A row of chips' left edges from their widths (Search's addon filter, Home's and the Library's
/// type filter): laid out from the margin `gap` apart, and shifted left just enough that the
/// focused chip ends inside the right margin (a row that fits never moves).
pub fn row_xs(widths: &[f32], focused: Option<usize>, gap: f32) -> Vec<f32> {
    use crate::consts::{MARGIN_X, SCR_W};
    let mut xs = Vec::with_capacity(widths.len());
    let mut x = MARGIN_X;
    for w in widths {
        xs.push(x);
        x += w + gap;
    }
    let shift = focused
        .and_then(|i| Some(xs.get(i)? + widths.get(i)?))
        .map_or(0.0, |right| (right - (SCR_W - MARGIN_X)).max(0.0));
    xs.into_iter().map(|x| x - shift).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Metrics(RefCell<Vec<(String, i32, bool)>>);
    impl Measure for Metrics {
        fn width(&self, text: &CStr, size: i32, bold: bool) -> f32 {
            self.0.borrow_mut().push((text.to_string_lossy().into_owned(), size, bold));
            text.to_bytes().len() as f32 * if bold { 9.0 } else { 5.0 }
        }
        fn cap_h(&self, _: i32) -> f32 { panic!("width needs no vertical metric") }
        fn line_h(&self, _: i32) -> f32 { panic!("width needs no vertical metric") }
    }

    #[test]
    fn width_preserves_the_legacy_padding_icon_and_font_roles() {
        let metrics = Metrics::default();
        let width = ValueChip::width(&metrics, c"Sort", c" - Title", Some(c"  owner"));
        assert_eq!(width, 24.0 + 4.0 * 5.0 + 8.0 * 9.0 + 7.0 * 5.0 + 10.0 + 22.0 + 24.0);
        assert_eq!(*metrics.0.borrow(), vec![
            ("Sort".into(), theme::size::LABEL, false),
            (" - Title".into(), theme::size::LABEL, true),
            ("  owner".into(), theme::size::MICRO, false),
        ]);
    }

    /// Fourteen chips run past the screen's right edge; whichever is focused must be laid out on
    /// screen, and with focus elsewhere the row starts at the margin.
    #[test]
    fn the_focused_chip_of_a_row_is_always_on_screen() {
        use crate::consts::{MARGIN_X, SCR_W};
        let widths = [260.0_f32; 14];
        let right = SCR_W - MARGIN_X;
        for focused in 0..widths.len() {
            let x = row_xs(&widths, Some(focused), 12.0)[focused];
            assert!(x >= MARGIN_X - 0.5 && x + widths[focused] <= right + 0.5, "chip {focused} at {x}");
        }
        assert_eq!(row_xs(&widths, None, 12.0)[0], MARGIN_X);
        assert_eq!(row_xs(&widths, Some(0), 12.0)[0], MARGIN_X);
        assert_eq!(row_xs(&[200.0, 200.0], Some(1), 12.0)[0], MARGIN_X, "a row that fits never scrolls");
    }

    #[test]
    fn an_absent_note_occupies_no_width_and_focus_does_not_change_layout() {
        let metrics = Metrics::default();
        let width = ValueChip::width(&metrics, c"Filter", c" - All", None);
        assert_eq!(metrics.0.borrow().len(), 2);
        let rect = Rect::new(96.0, 200.0, width, 52.0);
        let chip = ValueChip::new(c"Filter", c" - All", None, rect).focused(true).icon(Icon::ChevronUp);
        assert_eq!(chip.rect.w, width);
        assert!(chip.note.is_none());
        assert!(chip.focused);
    }
}
