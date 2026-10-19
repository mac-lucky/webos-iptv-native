//! Drawing a VOD section and its player: [`build`] and [`player`] lay them out into a display
//! list when the screen's generation moves, measuring and fitting text then, never while
//! painting. Colours, type and spacing come from [`crate::theme`]; the seek bar is the catch-up
//! playback panel's.

use std::rc::Rc;

use iptv::model::Channel;
use tv_gfx::gfx::tokens::with_a;
use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::consts::{SCR_H, SCR_W};
use tv_ui::Rect;

use super::catalog::{Entry, Mark};
use super::player::VodPlayer;
use super::{Focus, VodScreen, CATEGORY_ROWS, ITEM_ROWS};
use crate::data::VodSection;
use crate::draw::{self, HAlign, Piece};
use crate::icons::Icon;
use crate::lists::Cursor;
use crate::theme::{self, radius, size, space};

/// Between the parts of a row's second line.
const DOT: &str = " \u{b7} ";

/// Geometry at 1080p.
mod geom {
    /// Left edge of the title and the left column.
    pub const X: f32 = 64.0;
    pub const TITLE_Y: f32 = 44.0;
    /// Top of the first row of both columns.
    pub const TOP: f32 = 168.0;
    pub const COL_W: f32 = 440.0;
    pub const COL_ROW_H: f32 = 72.0;
    /// Left edge of the item list.
    pub const LIST_X: f32 = 552.0;
    pub const ROW_H: f32 = 124.0;
    pub const POSTER_W: f32 = 72.0;
    pub const POSTER_H: f32 = 108.0;
    /// Left edge of a row's text, from the row's.
    pub const TEXT_DX: f32 = 112.0;
    pub const BAR_W: f32 = 200.0;
    pub const BAR_H: f32 = 4.0;
    // The player, laid out as the catch-up playback panel is.
    pub const PX: f32 = 81.0;
    pub const SCRIM_TOP: f32 = 620.0;
    pub const PLAYER_TITLE_Y: f32 = 744.0;
    pub const SEEK_Y: f32 = 870.0;
    pub const SEEK_H: f32 = 6.0;
    pub const KNOB_R: f32 = 14.0;
    pub const READOUT_Y: f32 = 924.0;
    pub const ICON: f32 = 56.0;
}

/// Lay the section out into `out`.
pub fn build(s: &VodScreen, m: &dyn Measure, out: &mut Vec<Piece>) {
    out.push(Piece::solid(Rect::FULL, 0.0, theme::BG));
    let width = SCR_W - 2.0 * geom::X;
    let (title, _) = draw::fit(m, s.title(), width, size::HEADER, true);
    out.push(Piece::text(
        title,
        geom::X,
        geom::TITLE_Y,
        size::HEADER,
        true,
        theme::TEXT,
    ));
    if s.catalog.entries.is_empty() {
        let text = match s.section {
            VodSection::Movies => msg::vod_no_movies(),
            VodSection::Shows => msg::vod_no_shows(),
        };
        out.push(Piece::text_in(
            draw::run(text),
            Rect::FULL,
            HAlign::Center,
            size::BODY,
            false,
            theme::TEXT_MUTED,
        ));
        return;
    }
    match &s.series {
        Some(series) => {
            let seasons: Vec<(String, String)> = series
                .seasons
                .iter()
                .map(|x| (x.name(), x.episodes.len().to_string()))
                .collect();
            column(out, m, &seasons, s.seasons, s.focus == Focus::Seasons);
            let Some(season) = series.seasons.get(s.seasons.index) else {
                return;
            };
            let focused = s.focus == Focus::Episodes;
            for (row, i) in s
                .episodes
                .visible(season.episodes.len(), ITEM_ROWS)
                .enumerate()
            {
                let e = &season.episodes[i];
                let poster = e.poster.as_deref().or(series.poster.as_deref());
                let line = RowText {
                    title: &e.label(),
                    poster_name: &series.name,
                    sub: "",
                    mark: Mark::of(s.catalog.progress(e.key)),
                };
                item_row(out, m, row, poster, &line, focused && i == s.episodes.index);
            }
        }
        None => {
            let categories: Vec<(String, String)> = s
                .catalog
                .categories
                .iter()
                .enumerate()
                .map(|(i, c)| (c.name().to_owned(), s.catalog.list_len(i).to_string()))
                .collect();
            column(
                out,
                m,
                &categories,
                s.categories,
                s.focus == Focus::Categories,
            );
            let c = s.categories.index;
            let focused = s.focus == Focus::Items;
            for (row, i) in s
                .items
                .visible(s.catalog.list_len(c), ITEM_ROWS)
                .enumerate()
            {
                let Some(entry) = s.catalog.entry(c, i) else {
                    continue;
                };
                let (sub, mark) = match entry {
                    Entry::Movie(v) => {
                        let parts: Vec<String> = v
                            .year
                            .map(|y| y.to_string())
                            .into_iter()
                            .chain(v.groups.first().cloned())
                            .collect();
                        (parts.join(DOT), Mark::of(s.catalog.progress(v.key)))
                    }
                    Entry::Series(x) => {
                        let seasons = msg::vod_season_count(x.seasons.len() as i64);
                        let episodes = msg::vod_episode_count(x.len() as i64);
                        (format!("{seasons}{DOT}{episodes}"), Mark::None)
                    }
                };
                let line = RowText {
                    title: entry.title(),
                    poster_name: entry.title(),
                    sub: &sub,
                    mark,
                };
                let at_focus = focused && i == s.items.index;
                item_row(out, m, row, entry.poster(), &line, at_focus);
            }
        }
    }
}

/// The left column: `rows` of (name, count), the cursor's row marked, a pill on it while the
/// column has the focus.
fn column(
    out: &mut Vec<Piece>,
    m: &dyn Measure,
    rows: &[(String, String)],
    cursor: Cursor,
    focused: bool,
) {
    for (row, i) in cursor.visible(rows.len(), CATEGORY_ROWS).enumerate() {
        let r = Rect::new(
            geom::X,
            geom::TOP + row as f32 * geom::COL_ROW_H,
            geom::COL_W,
            geom::COL_ROW_H - space::XS,
        );
        let selected = i == cursor.index;
        let (text, muted) = match (selected, focused) {
            (true, true) => {
                out.push(Piece::solid(r, radius::PILL, theme::FOCUS));
                (theme::TEXT_ON_FOCUS, theme::TEXT_ON_FOCUS_MUTED)
            }
            (true, false) => {
                out.push(Piece::solid(r, radius::PILL, theme::PANEL_RAISED));
                (theme::TEXT, theme::TEXT_MUTED)
            }
            _ => (theme::TEXT_MUTED, theme::TEXT_MUTED),
        };
        let (name, count) = &rows[i];
        let inner = Rect::new(r.x + space::M, r.y, r.w - 2.0 * space::M, r.h);
        let count_w = m.width_str(count, size::LABEL, false) + space::M;
        let (name, _) = draw::fit(m, name, inner.w - count_w, size::BODY, selected);
        out.push(Piece::text_in(
            name,
            inner,
            HAlign::Left,
            size::BODY,
            selected,
            text,
        ));
        out.push(Piece::text_in(
            draw::run(count),
            inner,
            HAlign::Right,
            size::LABEL,
            false,
            muted,
        ));
    }
}

/// What one row of the item list says.
struct RowText<'a> {
    title: &'a str,
    /// Whose initials stand in for a poster not (yet) there: the item's, an episode's series'.
    poster_name: &'a str,
    /// The second line: year and group, or a series' seasons and episodes.
    sub: &'a str,
    mark: Mark,
}

/// Row `row` of the item list: the poster, the title and the second line with the mark.
fn item_row(
    out: &mut Vec<Piece>,
    m: &dyn Measure,
    row: usize,
    poster: Option<&str>,
    line: &RowText<'_>,
    focused: bool,
) {
    let r = Rect::new(
        geom::LIST_X,
        geom::TOP + row as f32 * geom::ROW_H,
        SCR_W - geom::LIST_X - geom::X,
        geom::ROW_H - space::XS,
    );
    let (text, muted, accent) = if focused {
        out.push(Piece::solid(r, radius::PILL, theme::FOCUS));
        (
            theme::TEXT_ON_FOCUS,
            theme::TEXT_ON_FOCUS_MUTED,
            theme::TEXT_ON_FOCUS,
        )
    } else {
        (theme::TEXT, theme::TEXT_MUTED, theme::ACCENT_TEXT)
    };
    // The logo cache loads by URL: a stand-in channel carries the poster's.
    let stand_in = Rc::new(Channel {
        name: line.poster_name.to_owned(),
        logo: poster.map(str::to_owned),
        ..Channel::default()
    });
    let pr = Rect::new(
        r.x + space::M,
        r.y + (r.h - geom::POSTER_H) * 0.5,
        geom::POSTER_W,
        geom::POSTER_H,
    );
    out.push(Piece::logo(
        Some(stand_in),
        pr,
        size::LABEL,
        theme::PANEL_RAISED,
    ));
    let x = r.x + geom::TEXT_DX;
    let right = r.x + r.w - space::XL;
    let (title, _) = draw::fit(m, line.title, right - x, size::BODY, true);
    out.push(Piece::text(title, x, r.y + 22.0, size::BODY, true, text));
    let y = r.y + 70.0;
    let line_h = m.line_h(size::LABEL);
    let mut at = x;
    if !line.sub.is_empty() {
        let (sub, w) = draw::fit(m, line.sub, right - at, size::LABEL, false);
        out.push(Piece::text(sub, at, y, size::LABEL, false, muted));
        at += w + space::XL;
    }
    match &line.mark {
        Mark::None => {}
        Mark::Watched => {
            if at < right {
                let (s, _) = draw::fit(m, msg::vod_watched(), right - at, size::LABEL, true);
                out.push(Piece::text(s, at, y, size::LABEL, true, accent));
            }
        }
        Mark::Started(frac, left) => {
            if at + geom::BAR_W < right {
                let bar = Rect::new(
                    at,
                    y + (line_h - geom::BAR_H) * 0.5,
                    geom::BAR_W,
                    geom::BAR_H,
                );
                let (fill, track) = if focused {
                    (theme::TEXT_ON_FOCUS, theme::TEXT_ON_FOCUS_MUTED)
                } else {
                    (theme::ACCENT, theme::PROGRESS_TRACK)
                };
                out.extend(draw::bar(bar, *frac, fill, track));
                at += geom::BAR_W + space::M;
            }
            if !left.is_empty() && at < right {
                let (s, _) = draw::fit(m, left, right - at, size::LABEL, false);
                out.push(Piece::text(s, at, y, size::LABEL, false, muted));
            }
        }
    }
}

/// Lay the player's panel out into `out`: the title, the seek bar, the read-out and the
/// play/pause state, over a scrim that darkens the bottom of the video.
pub fn player(p: &VodPlayer, m: &dyn Measure, out: &mut Vec<Piece>) {
    out.push(Piece::Fill {
        r: Rect::new(0.0, geom::SCRIM_TOP, SCR_W, SCR_H - geom::SCRIM_TOP),
        rad: 0.0,
        top: with_a(theme::BG, 0.0),
        bot: theme::SCRIM,
    });
    let width = SCR_W - 2.0 * geom::PX;
    let (title, _) = draw::fit(m, p.title(), width, size::TITLE, true);
    out.push(Piece::text(
        title,
        geom::PX,
        geom::PLAYER_TITLE_Y,
        size::TITLE,
        true,
        theme::TEXT,
    ));
    let r = p.readout();
    let track = Rect::new(
        geom::PX,
        geom::SEEK_Y - geom::SEEK_H * 0.5,
        width,
        geom::SEEK_H,
    );
    let rad = geom::SEEK_H * 0.5;
    out.push(Piece::solid(track, rad, theme::SEEK_TRACK));
    let played = Rect::new(track.x, track.y, (track.w * r.knob).max(track.h), track.h);
    out.push(Piece::solid(played, rad, theme::ACCENT));
    let k = geom::KNOB_R;
    let kx = track.x + track.w * r.knob;
    out.push(Piece::solid(
        Rect::new(kx - k, geom::SEEK_Y - k, 2.0 * k, 2.0 * k),
        k,
        theme::TEXT,
    ));
    let mut x = geom::PX;
    for (s, color) in [
        (r.elapsed.as_str(), theme::TEXT),
        (
            if r.total.is_empty() { "" } else { " / " },
            theme::TEXT_MUTED,
        ),
        (r.total.as_str(), theme::TEXT_MUTED),
    ] {
        if s.is_empty() {
            continue;
        }
        out.push(Piece::text(
            draw::run(s),
            x,
            geom::READOUT_Y,
            size::BODY,
            false,
            color,
        ));
        x += m.width_str(s, size::BODY, false);
    }
    if !r.offset.is_empty() {
        out.push(Piece::text(
            draw::run(&r.offset),
            x + space::M,
            geom::READOUT_Y,
            size::BODY,
            false,
            theme::TEXT,
        ));
    }
    let i = geom::ICON;
    let cy = geom::READOUT_Y + m.line_h(size::BODY) * 0.5;
    out.push(Piece::Icon {
        icon: if r.paused { Icon::Play } else { Icon::Pause },
        r: Rect::new(SCR_W * 0.5 - i * 0.5, cy - i * 0.5, i, i),
        tint: theme::TEXT,
    });
}
