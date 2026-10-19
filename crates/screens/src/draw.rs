//! Display lists: what a screen draws, laid out once per change and painted every frame.
//!
//! A screen measures and fits its text when its state changes ([`Cached::prepare`]), into a list
//! of [`Piece`]s in screen coordinates; [`paint`] then walks the list every frame without
//! measuring or allocating. The helpers here are the ones every screen builds such a list with:
//! text runs ([`cstr`], [`fit`], [`wrap`]), progress bars ([`bar`]) and channel logos
//! ([`contain`], [`logo_fit`], [`initials`], [`channel_number`]).

use std::ffi::{CStr, CString};
use std::os::raw::c_int;
use std::rc::Rc;

use iptv::model::Channel;
use tv_machine::machine::Measure;
use tv_ui::{Painter, Rect};

use crate::data::Logos;
use crate::icons::{self, Icon};
use crate::theme::{self, radius};

/// A text run, already fitted.
pub type Run = Rc<CStr>;

/// The largest box a channel logo is drawn in (the player's logo card). The app scales logo
/// pictures down to fit it once, when they load.
pub const LOGO_BOX: (u32, u32) = (240, 165);

/// Which end (or the middle) of a text run its `x` names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// What a text run's `y` names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VAlign {
    /// The top of the line: the text renderer's own origin.
    #[default]
    Top,
    /// The middle of the cap band, so a run sits centred on a row whatever its descenders (how
    /// `tv_ui::label::Label` centres text in a box).
    Middle,
}

/// One display-list entry, in screen coordinates.
#[derive(Clone, Debug, PartialEq)]
pub enum Piece {
    /// `r` filled top to bottom from `top` to `bot`; the same colour twice is a solid fill.
    Fill {
        r: Rect,
        rad: f32,
        top: [f32; 4],
        bot: [f32; 4],
    },
    Text {
        s: Run,
        x: f32,
        y: f32,
        size: c_int,
        bold: bool,
        color: [f32; 4],
        h: HAlign,
        v: VAlign,
    },
    Icon {
        icon: Icon,
        r: Rect,
        tint: [f32; 4],
    },
    /// `channel`'s logo fitted into `r`; while it has no texture, a `tile` with the `fallback`
    /// letters at `size`.
    Logo {
        channel: Option<Rc<Channel>>,
        r: Rect,
        fallback: Run,
        size: c_int,
        tile: [f32; 4],
    },
}

impl Piece {
    pub fn solid(r: Rect, rad: f32, color: [f32; 4]) -> Piece {
        Piece::Fill {
            r,
            rad,
            top: color,
            bot: color,
        }
    }

    /// `s` from `(x, y)`, the top left of its line.
    pub fn text(s: Run, x: f32, y: f32, size: c_int, bold: bool, color: [f32; 4]) -> Piece {
        Piece::Text {
            s,
            x,
            y,
            size,
            bold,
            color,
            h: HAlign::Left,
            v: VAlign::Top,
        }
    }

    /// `s` on the middle of `r`, against its left or right edge or centred on it.
    pub fn text_in(s: Run, r: Rect, h: HAlign, size: c_int, bold: bool, color: [f32; 4]) -> Piece {
        let x = match h {
            HAlign::Left => r.x,
            HAlign::Center => r.x + r.w * 0.5,
            HAlign::Right => r.x + r.w,
        };
        Piece::Text {
            s,
            x,
            y: r.y + r.h * 0.5,
            size,
            bold,
            color,
            h,
            v: VAlign::Middle,
        }
    }

    /// `channel`'s logo in `r`, with its initials on `tile` until the texture is there.
    pub fn logo(channel: Option<Rc<Channel>>, r: Rect, size: c_int, tile: [f32; 4]) -> Piece {
        let fallback = run(&initials(channel.as_ref().map_or("", |c| &c.name)));
        Piece::Logo {
            channel,
            r,
            fallback,
            size,
            tile,
        }
    }
}

/// Paint a display list, with the channel logos `logos` has loaded.
pub fn paint(pieces: &[Piece], p: Painter, logos: &dyn Logos) {
    for piece in pieces {
        match piece {
            Piece::Fill { r, rad, top, bot } => p.rect(*r, *rad, *top, *bot, 0.0),
            Piece::Text {
                s,
                x,
                y,
                size,
                bold,
                color,
                h,
                v,
            } => draw_text(p, s, *x, *y, *size, *bold, *color, *h, *v),
            Piece::Icon { icon, r, tint } => icons::draw(p, *icon, *r, *tint),
            Piece::Logo {
                channel,
                r,
                fallback,
                size,
                tile,
            } => match channel.as_deref().and_then(|c| logo_fit(logos, c, *r)) {
                Some((tex, fitted)) => p.tex(tex, fitted, radius::CELL, theme::TEXT),
                None => {
                    p.rrect(*r, radius::CELL, radius::CELL, *tile);
                    let (x, y) = (r.x + r.w * 0.5, r.y + r.h * 0.5);
                    let (h, v) = (HAlign::Center, VAlign::Middle);
                    draw_text(p, fallback, x, y, *size, true, theme::TEXT_MUTED, h, v);
                }
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    p: Painter,
    s: &CStr,
    x: f32,
    y: f32,
    size: c_int,
    bold: bool,
    color: [f32; 4],
    h: HAlign,
    v: VAlign,
) {
    if s.is_empty() {
        return;
    }
    let bold = c_int::from(bold);
    let y = match v {
        VAlign::Top => y,
        VAlign::Middle => {
            let (cap_top, baseline) = tv_gfx::text::text_cap_band(size, bold);
            y - (cap_top + baseline) * 0.5
        }
    };
    let align = match h {
        HAlign::Left => 0,
        HAlign::Center => 1,
        HAlign::Right => 2,
    };
    p.text(s.as_ptr(), x, y, size, color, align, bold);
}

/// A display list and the generation of the screen state it was built from.
#[derive(Debug, Default)]
pub struct Cached {
    built: Option<u64>,
    pieces: Vec<Piece>,
}

impl Cached {
    /// Build the list again with `build` unless it already shows `generation`.
    pub fn prepare(&mut self, generation: u64, build: impl FnOnce(&mut Vec<Piece>)) {
        if self.built != Some(generation) {
            self.pieces.clear();
            build(&mut self.pieces);
            self.built = Some(generation);
        }
    }

    /// The next [`prepare`](Self::prepare) builds whatever the generation.
    pub fn invalidate(&mut self) {
        self.built = None;
    }

    pub fn pieces(&self) -> &[Piece] {
        &self.pieces
    }

    pub fn paint(&self, p: Painter, logos: &dyn Logos) {
        paint(&self.pieces, p, logos);
    }
}

/// `s` as a C string for the text renderer. A stray NUL (guide and playlist text can carry one)
/// is dropped; the rest of the text stays.
pub fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|e| {
        let mut bytes = e.into_vec();
        bytes.retain(|&b| b != 0);
        CString::new(bytes).unwrap_or_default()
    })
}

/// [`cstr`] as a [`Run`].
pub fn run(s: &str) -> Run {
    cstr(s).into()
}

/// `s` ellipsised to `width` when it is wider, and the width it takes.
pub fn fit(m: &dyn Measure, s: &str, width: f32, size: c_int, bold: bool) -> (Run, f32) {
    let w = m.width_str(s, size, bold);
    if w <= width {
        return (run(s), w);
    }
    let fitted = m.fit_line(s, width, size, bold);
    let w = m.width(&fitted, size, bold);
    (fitted, w)
}

/// `text` as at most `max` lines of `width`, broken at spaces: greedy, the last line ellipsised
/// when text remains, a word wider than a line ellipsised on a line of its own. A line's width is
/// the sum of its words' widths and the spaces between them, so every word is measured once.
pub fn wrap(
    m: &dyn Measure,
    text: &str,
    width: f32,
    size: c_int,
    bold: bool,
    max: usize,
) -> Vec<Run> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let space = m.width_str(" ", size, bold);
    let mut lines = Vec::new();
    let mut i = 0;
    // The width of `words[i]`, when the line before measured it and found no room for it.
    let mut carried = None;
    while i < words.len() && lines.len() < max {
        if lines.len() + 1 == max {
            lines.push(m.fit_line(&words[i..].join(" "), width, size, bold));
            break;
        }
        let first = i;
        let mut w = carried
            .take()
            .unwrap_or_else(|| m.width_str(words[i], size, bold));
        i += 1;
        while let Some(word) = words.get(i) {
            let ww = m.width_str(word, size, bold);
            if w + space + ww > width {
                carried = Some(ww);
                break;
            }
            w += space + ww;
            i += 1;
        }
        let line = words[first..i].join(" ");
        lines.push(if w <= width {
            run(&line)
        } else {
            m.fit_line(&line, width, size, bold)
        });
    }
    lines
}

/// `w` x `h` scaled to fit inside `r` with its aspect kept, centred.
pub fn contain(r: Rect, w: f32, h: f32) -> Rect {
    if w <= 0.0 || h <= 0.0 {
        return r;
    }
    let s = (r.w / w).min(r.h / h);
    let (w, h) = (w * s, h * s);
    Rect::new(r.x + (r.w - w) * 0.5, r.y + (r.h - h) * 0.5, w, h)
}

/// `channel`'s logo texture and where it goes inside `r`, once it has loaded.
pub fn logo_fit(logos: &dyn Logos, channel: &Channel, r: Rect) -> Option<(u32, Rect)> {
    logos
        .logo(channel)
        .map(|(tex, w, h)| (tex, contain(r, w, h)))
}

/// A rounded bar of `frac` filled over its track (a fill never shorter than it is high).
pub fn bar(r: Rect, frac: f32, fill: [f32; 4], track: [f32; 4]) -> [Piece; 2] {
    let rad = r.h * 0.5;
    let w = (r.w * frac.clamp(0.0, 1.0)).max(r.h);
    [
        Piece::solid(r, rad, track),
        Piece::solid(Rect::new(r.x, r.y, w, r.h), rad, fill),
    ]
}

/// Up to two letters standing in for a missing logo: the first letters of the name's first two
/// words that start with one.
pub fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}

/// The number a channel shows: its declared one, else its position in the list it is in.
pub fn channel_number(c: &Channel, index: usize) -> u32 {
    c.number.unwrap_or(index as u32 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::{FakeData, Mono};

    fn lines(runs: &[Run]) -> Vec<String> {
        runs.iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect()
    }

    /// Counts every width it is asked for.
    struct Counting(std::cell::Cell<usize>);

    impl Measure for Counting {
        fn width(&self, s: &CStr, sz: i32, bold: bool) -> f32 {
            self.0.set(self.0.get() + 1);
            Mono.width(s, sz, bold)
        }
        fn cap_h(&self, sz: i32) -> f32 {
            Mono.cap_h(sz)
        }
        fn line_h(&self, sz: i32) -> f32 {
            Mono.line_h(sz)
        }
    }

    #[test]
    fn wrap_fills_lines_greedily_and_ellipsises_the_last() {
        // Ten characters a line at size 20 (10 px each).
        let w = 100.0;
        let text = "one two three four five six seven";
        assert_eq!(
            lines(&wrap(&Mono, text, w, 20, false, 5)),
            ["one two", "three four", "five six", "seven"]
        );
        assert_eq!(
            lines(&wrap(&Mono, text, w, 20, false, 2)),
            ["one two", "three fou\u{2026}"]
        );
        assert_eq!(lines(&wrap(&Mono, "  a   b  ", w, 20, false, 3)), ["a b"]);
        assert!(wrap(&Mono, "", w, 20, false, 3).is_empty());
        assert!(wrap(&Mono, text, w, 20, false, 0).is_empty());
    }

    #[test]
    fn wrap_cuts_a_word_wider_than_the_line_and_goes_on() {
        let out = wrap(&Mono, "a supercalifragilistic word", 100.0, 20, false, 3);
        assert_eq!(lines(&out), ["a", "supercali\u{2026}", "word"]);
    }

    #[test]
    fn wrap_measures_each_word_once() {
        let m = Counting(std::cell::Cell::new(0));
        let text = "aa bb cc dd ee ff gg hh ii jj";
        let out = wrap(&m, text, 80.0, 20, false, 10);
        assert_eq!(out.len(), 4);
        // The space, then every word once.
        assert_eq!(m.0.get(), 1 + 10);
    }

    #[test]
    fn fit_keeps_what_fits_and_reports_the_width() {
        let (s, w) = fit(&Mono, "News", 100.0, 20, false);
        assert_eq!((s.to_str().unwrap(), w), ("News", 40.0));
        let (s, w) = fit(&Mono, "Breaking news", 60.0, 20, false);
        assert_eq!(s.to_str().unwrap(), "Break\u{2026}");
        assert!(w <= 60.0);
    }

    #[test]
    fn contain_keeps_the_aspect_and_centres() {
        let r = Rect::new(0.0, 0.0, 240.0, 165.0);
        assert_eq!(contain(r, 100.0, 100.0), Rect::new(37.5, 0.0, 165.0, 165.0));
        assert_eq!(contain(r, 480.0, 165.0), Rect::new(0.0, 41.25, 240.0, 82.5));
        assert_eq!(contain(r, 0.0, 10.0), r);
    }

    #[test]
    fn a_stray_nul_is_dropped() {
        assert_eq!(cstr("ab\0cd").to_bytes(), b"abcd");
        assert_eq!(cstr("plain").to_bytes(), b"plain");
        assert_eq!(run("\0").to_bytes(), b"");
    }

    #[test]
    fn bars_never_shrink_below_their_height() {
        let r = Rect::new(10.0, 20.0, 100.0, 4.0);
        let [track, fill] = bar(r, 0.5, [1.0; 4], [0.0; 4]);
        assert_eq!(track, Piece::solid(r, 2.0, [0.0; 4]));
        assert_eq!(
            fill,
            Piece::solid(Rect::new(10.0, 20.0, 50.0, 4.0), 2.0, [1.0; 4])
        );
        let [_, empty] = bar(r, -1.0, [1.0; 4], [0.0; 4]);
        assert_eq!(
            empty,
            Piece::solid(Rect::new(10.0, 20.0, 4.0, 4.0), 2.0, [1.0; 4])
        );
    }

    #[test]
    fn initials_and_numbers() {
        assert_eq!(initials("|NL| TV Noord"), "NT");
        assert_eq!(initials("bbc one"), "BO");
        assert_eq!(initials(""), "");
        let d = FakeData::new(0, 3);
        assert_eq!(channel_number(&d.channels[2], 7), 3);
        let mut c = d.channels[0].clone();
        c.number = None;
        assert_eq!(channel_number(&c, 7), 8);
        let Piece::Logo { fallback, .. } =
            Piece::logo(Some(Rc::new(c)), Rect::default(), 27, [0.0; 4])
        else {
            panic!("a logo piece");
        };
        assert_eq!(fallback.to_bytes(), b"C1");
    }

    #[test]
    fn text_in_places_on_the_middle_of_the_box() {
        let r = Rect::new(10.0, 20.0, 100.0, 40.0);
        let Piece::Text { x, y, h, v, .. } =
            Piece::text_in(run("x"), r, HAlign::Right, 20, false, [1.0; 4])
        else {
            panic!("a text piece");
        };
        assert_eq!((x, y, h, v), (110.0, 40.0, HAlign::Right, VAlign::Middle));
    }

    #[test]
    fn cached_builds_once_per_generation() {
        let mut c = Cached::default();
        let mut builds = 0;
        for generation in [1, 1, 2, 2] {
            c.prepare(generation, |out| {
                builds += 1;
                out.push(Piece::solid(Rect::default(), 0.0, [0.0; 4]));
            });
        }
        assert_eq!((builds, c.pieces().len()), (2, 1));
        c.invalidate();
        c.prepare(2, |_| builds += 1);
        assert_eq!((builds, c.pieces().len()), (3, 0));
    }
}
