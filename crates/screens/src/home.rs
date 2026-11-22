//! **Home**: the first page of the home navigation (Settings > Appearance > Navigation), a grid of
//! large tiles, one per section: the channels, the guide, the movies and series when the playlists
//! carry any the PIN does not hide, the history, Search and Settings. OK (or a click) opens the
//! section over this page, and the shell brings this page back when the section is left.
//!
//! Nothing plays here: the shell stops the live channel when it comes back to this page, so the
//! page is opaque and claims no video rectangle.

use std::os::raw::c_int;

use iptv::vod::VodKind;
use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::{Painter, Rect};

use crate::data::{Logos, Navigate, Output, Screen, TvData, VodSection};
use crate::draw::{self, HAlign, Piece};
use crate::icons::Icon;
use crate::input::{Hold, Key, Press};
use crate::theme;

/// Tiles per row.
pub const COLUMNS: usize = 4;
const TILE_W: f32 = 380.0;
const TILE_H: f32 = 280.0;
const GAP: f32 = 40.0;
/// The top of the first row, under the title.
const TOP: f32 = 300.0;
const ICON: f32 = 112.0;
const LABEL_SIZE: c_int = 36;
const TITLE_SIZE: c_int = theme::size::HEADER;
const RADIUS: f32 = 14.0;

/// One section of the home page.
#[derive(Clone, Debug, PartialEq)]
pub struct Tile {
    pub icon: Icon,
    pub label: &'static str,
    pub open: Navigate,
}

/// The sections, in their order on the page: the VOD ones only when `data` has an entry of
/// that kind the PIN does not lock.
pub fn tiles(data: &dyn TvData) -> Vec<Tile> {
    let vod = data.vod();
    let has = |kind| vod.iter().any(|v| v.kind == kind && !data.vod_locked(v));
    let mut tiles = vec![
        Tile {
            icon: Icon::Live,
            label: msg::common_channels(),
            open: Navigate::ChannelList { preview: true },
        },
        Tile {
            icon: Icon::Guide,
            label: msg::nav_epg(),
            open: Navigate::Guide,
        },
    ];
    if has(VodKind::Movie) {
        tiles.push(Tile {
            icon: Icon::Film,
            label: msg::vod_movies(),
            open: Navigate::Vod(VodSection::Movies),
        });
    }
    if has(VodKind::Episode) {
        tiles.push(Tile {
            icon: Icon::Show,
            label: msg::vod_shows(),
            open: Navigate::Vod(VodSection::Shows),
        });
    }
    tiles.extend([
        Tile {
            icon: Icon::CatchUp,
            label: msg::channel_history(),
            open: Navigate::History,
        },
        Tile {
            icon: Icon::Search,
            label: msg::common_search(),
            open: Navigate::Search,
        },
        Tile {
            icon: Icon::Settings,
            label: msg::common_settings(),
            open: Navigate::Settings,
        },
    ]);
    tiles
}

/// Where tile `i` of `count` is drawn: rows of [`COLUMNS`], each centred on the screen.
pub fn tile_rect(i: usize, count: usize) -> Rect {
    let row = i / COLUMNS;
    let in_row = (count - row * COLUMNS).min(COLUMNS);
    let width = in_row as f32 * TILE_W + (in_row.saturating_sub(1)) as f32 * GAP;
    let x0 = (tv_ui::consts::SCR_W - width) * 0.5;
    let col = i % COLUMNS;
    Rect::new(
        x0 + col as f32 * (TILE_W + GAP),
        TOP + row as f32 * (TILE_H + GAP),
        TILE_W,
        TILE_H,
    )
}

pub struct HomeScreen {
    tiles: Vec<Tile>,
    focus: usize,
}

impl HomeScreen {
    pub fn new(data: &dyn TvData) -> Self {
        Self {
            tiles: tiles(data),
            focus: 0,
        }
    }

    /// The tiles shown, for the app's navigation tests.
    pub fn tiles(&self) -> &[Tile] {
        &self.tiles
    }

    /// Focus the tile that opens `section`, when there is one.
    pub fn focus_on(&mut self, section: &Navigate) {
        if let Some(i) = self.tiles.iter().position(|t| t.open == *section) {
            self.focus = i;
        }
    }

    /// The focused tile's index.
    pub fn focus(&self) -> usize {
        self.focus
    }

    /// Move the focus to `to` when it is a tile and differs.
    fn move_to(&mut self, to: Option<usize>) -> Output {
        match to.filter(|&t| t < self.tiles.len() && t != self.focus) {
            Some(t) => {
                self.focus = t;
                Output::redraw()
            }
            None => Output::default(),
        }
    }

    /// The tile the arrow `key` leads to from the focus: along the row, or to the same column
    /// of the next row, the last tile when that row is shorter.
    fn step(&self, key: Key) -> Option<usize> {
        let (row, col) = (self.focus / COLUMNS, self.focus % COLUMNS);
        let last = self.tiles.len().checked_sub(1)?;
        match key {
            Key::Left if col > 0 => Some(self.focus - 1),
            Key::Right if col + 1 < COLUMNS && self.focus < last => Some(self.focus + 1),
            Key::Up if row > 0 => Some(self.focus - COLUMNS),
            Key::Down if (row + 1) * COLUMNS <= last => Some((self.focus + COLUMNS).min(last)),
            _ => None,
        }
    }
}

impl Screen for HomeScreen {
    fn hold(&self, key: Key) -> Hold {
        match key {
            Key::Up | Key::Down | Key::Left | Key::Right => Hold::Repeat,
            _ => Hold::Once,
        }
    }

    fn key(&mut self, key: Key, press: Press, _data: &dyn TvData, _now_ms: u64) -> Output {
        match key {
            Key::Ok if press == Press::Short => self
                .tiles
                .get(self.focus)
                .map_or_else(Output::default, |t| Output::navigate(t.open.clone())),
            Key::Up | Key::Down | Key::Left | Key::Right => self.move_to(self.step(key)),
            // The first page: Back has nowhere to go.
            _ => Output::default(),
        }
    }

    fn point(&mut self, x: f32, y: f32, _data: &dyn TvData, _now_ms: u64) -> Output {
        let count = self.tiles.len();
        let hit = (0..count).find(|&i| {
            let r = tile_rect(i, count);
            x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
        });
        self.move_to(hit)
    }

    fn refresh(&mut self, data: &dyn TvData) {
        let focused = self.tiles.get(self.focus).map(|t| t.open.clone());
        self.tiles = tiles(data);
        self.focus = focused
            .and_then(|f| self.tiles.iter().position(|t| t.open == f))
            .unwrap_or(0);
    }

    fn hidden_when_covered(&self) -> bool {
        true
    }

    fn draw(&mut self, p: Painter, _m: &dyn Measure, logos: &dyn Logos) {
        let screen = Rect::new(0.0, 0.0, tv_ui::consts::SCR_W, tv_ui::consts::SCR_H);
        let mut pieces = vec![
            Piece::solid(screen, 0.0, theme::BG),
            Piece::text_in(
                draw::run(msg::nav_home()),
                Rect::new(0.0, 120.0, tv_ui::consts::SCR_W, 100.0),
                HAlign::Center,
                TITLE_SIZE,
                true,
                theme::TEXT,
            ),
        ];
        let count = self.tiles.len();
        for (i, tile) in self.tiles.iter().enumerate() {
            let r = tile_rect(i, count);
            let focused = i == self.focus;
            let (fill, ink) = if focused {
                (theme::FOCUS, theme::TEXT_ON_FOCUS)
            } else {
                (theme::PANEL, theme::TEXT)
            };
            pieces.push(Piece::solid(r, RADIUS, fill));
            pieces.push(Piece::Icon {
                icon: tile.icon,
                r: Rect::new(r.x + (r.w - ICON) * 0.5, r.y + 52.0, ICON, ICON),
                tint: if focused { theme::ACCENT } else { ink },
            });
            pieces.push(Piece::text_in(
                draw::run(tile.label),
                Rect::new(r.x, r.y + r.h - 96.0, r.w, 60.0),
                HAlign::Center,
                LABEL_SIZE,
                true,
                ink,
            ));
        }
        draw::paint(&pieces, p, logos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::FakeData;

    fn home(vod: bool) -> (HomeScreen, FakeData) {
        tv_platform::i18n::initialize(tv_platform::i18n::Preference::En, true);
        let mut data = FakeData::new(1_700_000_000, 6);
        if vod {
            data.vod = crate::vod::fake::items("");
        }
        (HomeScreen::new(&data), data)
    }

    fn opens(h: &HomeScreen) -> Vec<Navigate> {
        h.tiles().iter().map(|t| t.open.clone()).collect()
    }

    #[test]
    fn the_vod_tiles_show_only_with_vod() {
        let (h, _) = home(false);
        assert_eq!(
            opens(&h),
            [
                Navigate::ChannelList { preview: true },
                Navigate::Guide,
                Navigate::History,
                Navigate::Search,
                Navigate::Settings,
            ]
        );
        let (h, _) = home(true);
        assert_eq!(h.tiles().len(), 7);
        assert_eq!(h.tiles()[2].open, Navigate::Vod(VodSection::Movies));
        assert_eq!(h.tiles()[3].open, Navigate::Vod(VodSection::Shows));
    }

    #[test]
    fn the_arrows_walk_the_grid_and_ok_opens_the_tile() {
        let (mut h, d) = home(true);
        let key = |h: &mut HomeScreen, k| h.key(k, Press::Short, &d, 0);
        assert_eq!(
            key(&mut h, Key::Left),
            Output::default(),
            "nothing left of the first"
        );
        key(&mut h, Key::Right);
        key(&mut h, Key::Right);
        key(&mut h, Key::Right);
        assert_eq!(h.focus(), 3);
        assert_eq!(key(&mut h, Key::Right), Output::default(), "the row ends");
        key(&mut h, Key::Down);
        assert_eq!(h.focus(), 6, "the shorter row's last tile");
        key(&mut h, Key::Up);
        assert_eq!(h.focus(), 2);
        assert_eq!(
            key(&mut h, Key::Ok),
            Output::navigate(Navigate::Vod(VodSection::Movies))
        );
        assert_eq!(key(&mut h, Key::Back), Output::default());
    }

    #[test]
    fn the_pointer_focuses_the_tile_under_it() {
        let (mut h, d) = home(false);
        let r = tile_rect(4, h.tiles().len());
        assert_eq!(h.point(r.x + 10.0, r.y + 10.0, &d, 0), Output::redraw());
        assert_eq!(h.focus(), 4);
        assert_eq!(h.point(5.0, 5.0, &d, 0), Output::default(), "no tile");
    }
}
