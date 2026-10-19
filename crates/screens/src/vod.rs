//! TiviMate's Movies and Shows sections, for playlists that carry VOD
//! ([`iptv::vod`]).
//!
//! - **Movies**: categories on the left (Continue watching, All, the playlist's groups), the
//!   movies of the focused one on the right. OK on a movie plays it full screen; one stopped part
//!   way asks first whether to resume or start over ([`Prompt`]).
//! - **Shows**: the same with series; OK on a series opens it, its seasons on the left and the
//!   season's episodes on the right, played as movies are. Back returns to the series.
//! - Rows show the poster, the title, and how far the viewer got: a bar with the time left, or
//!   Watched once an item was played to its last minutes.
//!
//! Playing goes through [`Command::PlayVod`] and puts the section's [`player::VodPlayer`] in
//! front, over the full-screen video (the section itself is not drawn then); stopping there
//! ([`Command::StopVod`]) brings the section back with the new position. Keys: Up/Down move
//! (held: repeat), Right/OK go in, Left/Back come out, Back on the categories leaves for the
//! guide. Like every screen this is a pure state machine; the view is laid out once per change
//! ([`view`]).

pub mod catalog;
pub mod fake;
pub mod player;
mod view;

#[cfg(test)]
mod tests;

use std::rc::Rc;

use iptv::vod::VodItem;
use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::Painter;

use crate::data::{Command, Logos, Navigate, Output, Screen, TvData, VodSection};
use crate::draw::Cached;
use crate::input::{Action, Hold, Key, Press};
use crate::lists::Cursor;
use crate::panels::{Nav, Page, PanelKey, Row, SidePanel};
use crate::player::PlayerSettings;
pub use catalog::{Catalog, Category, Entry, Mark, Season, Series};
pub use player::VodPlayer;

/// Category rows on screen.
pub const CATEGORY_ROWS: usize = 11;
/// Item rows on screen.
pub const ITEM_ROWS: usize = 7;

/// Which column has the focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Categories,
    Items,
    /// A series is open: its seasons, then the season's episodes.
    Seasons,
    Episodes,
}

/// The resume prompt's two answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Resume,
    FromStart,
}

/// The resume prompt over the section: the item it asks about and where it stopped.
pub struct Prompt {
    pub panel: SidePanel<Choice>,
    pub item: Rc<VodItem>,
    pub position_secs: u32,
}

pub struct VodScreen {
    section: VodSection,
    settings: PlayerSettings,
    catalog: Catalog,
    focus: Focus,
    categories: Cursor,
    items: Cursor,
    /// The open series (Shows), with its season and episode cursors.
    series: Option<Rc<Series>>,
    seasons: Cursor,
    episodes: Cursor,
    prompt: Option<Prompt>,
    /// In front while a movie or an episode plays.
    player: Option<VodPlayer>,
    generation: u64,
    view: Cached,
    player_view: Cached,
}

impl VodScreen {
    /// `section` of what `data` lists; its player seeks and times out as `settings` say.
    pub fn new(section: VodSection, data: &dyn TvData, settings: PlayerSettings) -> Self {
        let mut s = VodScreen {
            section,
            settings,
            catalog: Catalog::build(data, section),
            focus: Focus::Categories,
            categories: Cursor::default(),
            items: Cursor::default(),
            series: None,
            seasons: Cursor::default(),
            episodes: Cursor::default(),
            prompt: None,
            player: None,
            generation: 0,
            view: Cached::default(),
            player_view: Cached::default(),
        };
        // Open on All rather than on what is being watched, as TiviMate does.
        let all = all_index(&s.catalog).unwrap_or(0);
        s.categories
            .set(all, s.catalog.categories.len(), CATEGORY_ROWS);
        s
    }

    pub fn section(&self) -> VodSection {
        self.section
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn category(&self) -> usize {
        self.categories.index
    }

    pub fn item(&self) -> usize {
        self.items.index
    }

    pub fn series(&self) -> Option<&Series> {
        self.series.as_deref()
    }

    pub fn season(&self) -> usize {
        self.seasons.index
    }

    pub fn episode(&self) -> usize {
        self.episodes.index
    }

    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref().filter(|p| !p.panel.closing())
    }

    pub fn player(&self) -> Option<&VodPlayer> {
        self.player.as_ref()
    }

    /// The section's title: Movies, Shows, or the open series' name.
    pub fn title(&self) -> &str {
        match (&self.series, self.section) {
            (Some(s), _) => &s.name,
            (None, VodSection::Movies) => msg::vod_movies(),
            (None, VodSection::Shows) => msg::vod_shows(),
        }
    }

    fn items_len(&self) -> usize {
        self.catalog.list_len(self.categories.index)
    }

    fn season_episodes(&self) -> &[Rc<VodItem>] {
        self.series
            .as_ref()
            .and_then(|s| s.seasons.get(self.seasons.index))
            .map_or(&[], |s| &s.episodes)
    }

    fn changed(&mut self) -> Output {
        self.generation += 1;
        Output::redraw()
    }

    /// Play `item` from `offset_secs` in, its player in front.
    fn play(&mut self, item: &VodItem, offset_secs: i64, now_ms: u64) -> Output {
        let title = item.full_title();
        self.player = Some(VodPlayer::new(
            item.key,
            title,
            self.settings.clone(),
            now_ms,
        ));
        self.player_view.invalidate();
        self.generation += 1;
        Output {
            commands: vec![Command::PlayVod {
                item: item.key,
                offset_secs,
            }],
            ..Output::redraw()
        }
    }

    /// Play `item`, asking first when it stopped part way.
    fn start(&mut self, item: Rc<VodItem>, now_ms: u64) -> Output {
        let at = self.catalog.progress(item.key).and_then(|p| p.resume_at());
        match at {
            Some(position_secs) => {
                let resume = msg::vod_resume_from(&crate::time::clock_secs(position_secs.into()));
                let rows = vec![
                    Row::action(Choice::Resume, &resume),
                    Row::action(Choice::FromStart, msg::vod_from_start()),
                ];
                self.prompt = Some(Prompt {
                    panel: SidePanel::new(Page::list(&item.full_title(), rows)),
                    item,
                    position_secs,
                });
                self.changed()
            }
            None => self.play(&item, 0, now_ms),
        }
    }

    fn key_prompt(&mut self, key: Key, press: Press, now_ms: u64) -> Output {
        let Some(prompt) = self.prompt.as_mut() else {
            return Output::default();
        };
        let Some(key) = PanelKey::from_action(Action { key, press }) else {
            return Output::default();
        };
        let offset = match prompt.panel.key(key) {
            Nav::Activate(Choice::Resume) => Some(i64::from(prompt.position_secs)),
            Nav::Activate(Choice::FromStart) => Some(0),
            Nav::Close => None,
            _ => return Output::redraw(),
        };
        prompt.panel.hide();
        let item = Rc::clone(&prompt.item);
        self.generation += 1;
        match offset {
            Some(offset) => self.play(&item, offset, now_ms),
            None => Output::redraw(),
        }
    }

    fn key_categories(&mut self, key: Key) -> Output {
        let len = self.catalog.categories.len();
        match key {
            Key::Up | Key::Down => {
                if !step_column(&mut self.categories, key, len) {
                    return Output::default();
                }
                self.items = Cursor::default();
                self.changed()
            }
            Key::Right | Key::Ok if self.items_len() > 0 => {
                self.focus = Focus::Items;
                self.changed()
            }
            Key::Back => Output::navigate(Navigate::Guide),
            _ => Output::default(),
        }
    }

    fn key_items(&mut self, key: Key, now_ms: u64) -> Output {
        let len = self.items_len();
        match key {
            Key::Up | Key::Down | Key::ChannelUp | Key::ChannelDown => {
                self.items.step(item_step(key), len, ITEM_ROWS, false);
                self.changed()
            }
            Key::Left | Key::Back => {
                self.focus = Focus::Categories;
                self.changed()
            }
            Key::Ok => match self.catalog.entry(self.categories.index, self.items.index) {
                Some(Entry::Movie(m)) => {
                    let m = Rc::clone(m);
                    self.start(m, now_ms)
                }
                Some(Entry::Series(s)) => {
                    let s = Rc::clone(s);
                    self.open_series(s);
                    self.changed()
                }
                None => Output::default(),
            },
            _ => Output::default(),
        }
    }

    /// Open `series` on the episode to watch next: the first not watched to its end.
    fn open_series(&mut self, series: Rc<Series>) {
        let mut at = (0, 0);
        'seasons: for (si, season) in series.seasons.iter().enumerate() {
            for (ei, e) in season.episodes.iter().enumerate() {
                if !self.catalog.progress(e.key).is_some_and(|p| p.watched) {
                    at = (si, ei);
                    break 'seasons;
                }
            }
        }
        self.seasons.set(at.0, series.seasons.len(), CATEGORY_ROWS);
        let episodes = series.seasons.get(at.0).map_or(0, |s| s.episodes.len());
        self.episodes.set(at.1, episodes, ITEM_ROWS);
        self.series = Some(series);
        self.focus = Focus::Episodes;
    }

    fn close_series(&mut self) -> Output {
        self.series = None;
        self.focus = Focus::Items;
        self.changed()
    }

    fn key_seasons(&mut self, key: Key) -> Output {
        let len = self.series.as_ref().map_or(0, |s| s.seasons.len());
        match key {
            Key::Up | Key::Down => {
                if !step_column(&mut self.seasons, key, len) {
                    return Output::default();
                }
                self.episodes = Cursor::default();
                self.changed()
            }
            Key::Right | Key::Ok if !self.season_episodes().is_empty() => {
                self.focus = Focus::Episodes;
                self.changed()
            }
            Key::Back => self.close_series(),
            _ => Output::default(),
        }
    }

    fn key_episodes(&mut self, key: Key, now_ms: u64) -> Output {
        let len = self.season_episodes().len();
        match key {
            Key::Up | Key::Down | Key::ChannelUp | Key::ChannelDown => {
                self.episodes.step(item_step(key), len, ITEM_ROWS, false);
                self.changed()
            }
            Key::Left => {
                self.focus = Focus::Seasons;
                self.changed()
            }
            Key::Back => self.close_series(),
            Key::Ok => match self.season_episodes().get(self.episodes.index) {
                Some(e) => {
                    let e = Rc::clone(e);
                    self.start(e, now_ms)
                }
                None => Output::default(),
            },
            _ => Output::default(),
        }
    }

    /// What the player's key or tick did: when the item stopped, the section is in front again.
    fn after_player(&mut self, step: player::Step) -> Output {
        if step.done {
            self.player = None;
            self.generation += 1;
        }
        step.out
    }
}

/// Where All is among `catalog`'s categories.
fn all_index(catalog: &Catalog) -> Option<usize> {
    catalog.categories.iter().position(|c| *c == Category::All)
}

/// Up or Down on a left column (categories, seasons) of `len` rows; whether the focus moved.
fn step_column(cursor: &mut Cursor, key: Key, len: usize) -> bool {
    let before = cursor.index;
    let by = if key == Key::Up { -1 } else { 1 };
    cursor.step(by, len, CATEGORY_ROWS, false);
    cursor.index != before
}

/// How far a key moves an item or episode list: Up/Down a row, CH+/CH- a screen of rows.
fn item_step(key: Key) -> isize {
    match key {
        Key::Up => -1,
        Key::Down => 1,
        Key::ChannelUp => -(ITEM_ROWS as isize),
        _ => ITEM_ROWS as isize,
    }
}

impl Screen for VodScreen {
    fn hold(&self, key: Key) -> Hold {
        if let Some(player) = &self.player {
            return player.hold(key);
        }
        if self.prompt().is_some() {
            return PanelKey::hold(key);
        }
        match key {
            Key::Up | Key::Down | Key::ChannelUp | Key::ChannelDown => Hold::Repeat,
            _ => Hold::Once,
        }
    }

    fn key(&mut self, key: Key, press: Press, _: &dyn TvData, now_ms: u64) -> Output {
        if let Some(player) = &mut self.player {
            let step = player.key(key, press, now_ms);
            return self.after_player(step);
        }
        if self.prompt().is_some() {
            return self.key_prompt(key, press, now_ms);
        }
        if press == Press::Long {
            return Output::default();
        }
        match self.focus {
            Focus::Categories => self.key_categories(key),
            Focus::Items => self.key_items(key, now_ms),
            Focus::Seasons => self.key_seasons(key),
            Focus::Episodes => self.key_episodes(key, now_ms),
        }
    }

    fn tick(&mut self, data: &dyn TvData, now_ms: u64) -> Output {
        match &mut self.player {
            Some(player) => {
                let step = player.tick(data, now_ms);
                self.after_player(step)
            }
            None => Output::default(),
        }
    }

    fn next_wake(&self, _now_ms: u64) -> Option<u64> {
        self.player.as_ref().and_then(VodPlayer::next_wake)
    }

    /// The positions moved (a movie was stopped) or the playlist changed: read the section
    /// again, keeping the focus on the same category, movie or series and episode.
    fn refresh(&mut self, data: &dyn TvData) {
        let category = self.catalog.categories.get(self.categories.index).cloned();
        let entry = self
            .catalog
            .entry(self.categories.index, self.items.index)
            .cloned();
        let open = self.series.as_ref().map(|s| s.name.clone());
        let (season, episode) = (self.seasons.index, self.episodes.index);
        self.catalog = Catalog::build(data, self.section);
        let len = self.catalog.categories.len();
        let (c, e) = match &category {
            Some(category) => self.catalog.find(category, entry.as_ref()),
            None => (None, None),
        };
        // A category that went away (Continue watching, once nothing is left to continue): All.
        let c = c.or_else(|| all_index(&self.catalog));
        self.categories.set(c.unwrap_or(0), len, CATEGORY_ROWS);
        let items = self.items_len();
        self.items
            .set(e.unwrap_or(self.items.index), items, ITEM_ROWS);
        self.series = open.and_then(|name| {
            self.catalog.entries.iter().find_map(|x| match x {
                Entry::Series(s) if s.name == name => Some(Rc::clone(s)),
                _ => None,
            })
        });
        match &self.series {
            Some(s) => {
                self.seasons.set(season, s.seasons.len(), CATEGORY_ROWS);
                let n = self.season_episodes().len();
                self.episodes.set(episode, n, ITEM_ROWS);
            }
            None if matches!(self.focus, Focus::Seasons | Focus::Episodes) => {
                self.focus = Focus::Items;
            }
            None => {}
        }
        if self.focus == Focus::Items && items == 0 {
            self.focus = Focus::Categories;
        }
        self.generation += 1;
    }

    fn update(&mut self, dt: f32) {
        if let Some(p) = &mut self.prompt {
            p.panel.update(dt);
            if p.panel.gone() {
                self.prompt = None;
            }
        }
    }

    /// The section, or while an item plays its player alone, over the video.
    fn draw(&mut self, p: Painter, m: &dyn Measure, logos: &dyn Logos) {
        if let Some(player) = &self.player {
            self.player_view
                .prepare(player.generation(), |out| player.build(m, out));
            self.player_view.paint(p, logos);
            return;
        }
        let mut cached = std::mem::take(&mut self.view);
        cached.prepare(self.generation, |out| view::build(self, m, out));
        self.view = cached;
        self.view.paint(p, logos);
        if let Some(prompt) = &self.prompt {
            prompt.panel.draw(p);
        }
    }
}
