//! Search: a field typed with the TV keyboard and live results for channels and guide
//! programmes. OK on a channel plays it; on a programme it plays live when on air and from the
//! archive when past.
//!
//! Channels are matched here, by name. Programmes come from [`TvData::search`], which can take
//! longer than a frame: the panel asks again on every tick until the answer is in, and keeps the
//! results it has meanwhile. While the keyboard is down the pointer focuses the field or the
//! result it rests on.

use std::ffi::CString;
use std::rc::Rc;

use iptv::catchup::in_archive;
use iptv::model::{fold, Channel, ChannelKey, Timestamp};
use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::{Painter, Rect};

use super::side::line;
use super::{style, PanelKey, TextField};
use crate::data::{Command, GridProgramme, Group, Logos, Navigate, Output, Screen, Stream, TvData};
use crate::draw::cstr;
use crate::icons::{self, Icon};
use crate::input::{Action, Hold, Key, Press};
use crate::lists::{Cursor, Rows};
use crate::point::{Spot, Spots};
use crate::time::{self, hhmm, weekday, When};

const MAX_CHANNEL_HITS: usize = 50;
/// Programme titles are searched from this many folded characters on; one letter matches most
/// of a guide.
const MIN_PROGRAMME_QUERY: usize = 2;
/// How soon a programme search that is still running is asked again: the next frame.
const POLL_MS: u64 = 16;

/// One result row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    Channel {
        key: ChannelKey,
        title: String,
        detail: String,
    },
    Programme {
        channel: ChannelKey,
        programme: GridProgramme,
        detail: String,
    },
}

impl Hit {
    pub fn title(&self) -> &str {
        match self {
            Hit::Channel { title, .. } => title,
            Hit::Programme { programme, .. } => &programme.title,
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Hit::Channel { detail, .. } | Hit::Programme { detail, .. } => detail,
        }
    }
}

pub struct Search {
    field: TextField,
    /// What the field draws: the draft, or the hint while it is empty.
    shown: CString,
    /// Every channel with its folded name.
    channels: Vec<(String, Rc<Channel>)>,
    channel_hits: Vec<Hit>,
    programme_hits: Vec<Hit>,
    /// The folded query the channel hits answer.
    query: String,
    /// The draft whose programme search is still running, asked again on every tick.
    pending: Option<String>,
    /// The focus is on the field, not on a result.
    on_field: bool,
    /// The focused result and the first one drawn.
    cursor: Cursor,
    /// When the programme hits were found.
    now: Timestamp,
    /// The field and the results in the frame drawn last, for the pointer.
    spots: Spots,
}

impl Search {
    /// Open with the keyboard up.
    pub fn new(data: &dyn TvData) -> Self {
        let mut field = TextField::new("");
        field.set_editing(true);
        let mut search = Self {
            field,
            shown: CString::default(),
            channels: Vec::new(),
            channel_hits: Vec::new(),
            programme_hits: Vec::new(),
            query: String::new(),
            pending: None,
            on_field: true,
            cursor: Cursor::default(),
            now: data.now(),
            spots: Spots::default(),
        };
        search.index(data);
        search.show_field();
        search
    }

    pub fn draft(&self) -> &str {
        self.field.text()
    }

    /// The focused result, `None` on the field.
    pub fn focus(&self) -> Option<usize> {
        (!self.on_field).then_some(self.cursor.index)
    }

    pub fn len(&self) -> usize {
        self.channel_hits.len() + self.programme_hits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn hit(&self, i: usize) -> Option<&Hit> {
        match i.checked_sub(self.channel_hits.len()) {
            None => self.channel_hits.get(i),
            Some(j) => self.programme_hits.get(j),
        }
    }

    pub fn hits(&self) -> impl Iterator<Item = &Hit> {
        self.channel_hits.iter().chain(&self.programme_hits)
    }

    fn handle(&mut self, key: PanelKey, data: &dyn TvData) -> Output {
        tv_machine::idle::invalidate();
        if matches!(key, PanelKey::Backspace | PanelKey::Clear) {
            if self.field.key(key) {
                self.edited(data);
            }
            return Output::redraw();
        }
        if self.field.editing() {
            match key {
                PanelKey::Ok | PanelKey::Down => {
                    self.field.set_editing(false);
                    if !self.is_empty() {
                        self.move_to(Some(0));
                    }
                }
                PanelKey::Back => self.field.set_editing(false),
                _ => {}
            }
            return Output::redraw();
        }
        match (self.focus(), key) {
            (None, PanelKey::Ok) => self.field.set_editing(true),
            (None, PanelKey::Down) if !self.is_empty() => self.move_to(Some(0)),
            (None, PanelKey::Back) => return Output::close(),
            (Some(i), PanelKey::Up) => self.move_to(i.checked_sub(1)),
            (Some(i), PanelKey::Down) if i + 1 < self.len() => self.move_to(Some(i + 1)),
            (Some(_), PanelKey::Back) => self.move_to(None),
            (Some(i), PanelKey::Ok) => return self.activate(i, data),
            _ => return Output::default(),
        }
        Output::redraw()
    }

    fn move_to(&mut self, focus: Option<usize>) {
        self.on_field = focus.is_none();
        self.cursor.set(focus.unwrap_or(0), self.len(), ROWS);
    }

    fn activate(&self, i: usize, data: &dyn TvData) -> Output {
        let now = data.now();
        let toast = |text: &str| Output {
            toast: Some(text.to_owned()),
            ..Output::redraw()
        };
        match self.hit(i) {
            Some(Hit::Channel { key, .. }) => play(Stream::Live(*key)),
            Some(Hit::Programme {
                channel, programme, ..
            }) => match time::when(programme.start, programme.stop, now) {
                When::OnAir => play(Stream::Live(*channel)),
                When::Past => {
                    let programme = data
                        .programme_at(*channel, programme.start)
                        .unwrap_or_else(|| programme.clone().into());
                    let archived = data
                        .channel(*channel)
                        .is_some_and(|c| in_archive(&c, &programme, now));
                    if archived {
                        play(Stream::Catchup {
                            channel: *channel,
                            programme,
                            offset_secs: 0,
                        })
                    } else {
                        toast(msg::epg_catchup_unavailable())
                    }
                }
                When::Future => toast(msg::epg_not_aired()),
            },
            None => Output::default(),
        }
    }

    /// Every channel, folded for matching.
    fn index(&mut self, data: &dyn TvData) {
        self.channels = data
            .channels(&Group::All)
            .iter()
            .map(|c| (fold(&c.name), Rc::clone(c)))
            .collect();
    }

    /// The draft changed: typing puts the focus back on the field, and the results follow.
    fn edited(&mut self, data: &dyn TvData) {
        self.move_to(None);
        self.show_field();
        let query = fold(self.field.text().trim());
        if query != self.query {
            self.query = query;
            self.find(data);
        }
    }

    /// Look the query up: the channels here, the programmes through the store.
    fn find(&mut self, data: &dyn TvData) {
        self.channel_hits = self.channel_hits_for(&self.query);
        if self.query.chars().count() >= MIN_PROGRAMME_QUERY {
            self.pending = Some(self.field.text().to_owned());
            self.poll(data);
        } else {
            self.pending = None;
            self.programme_hits.clear();
        }
    }

    /// Take the running programme search's answer if it is in; returns whether it was.
    fn poll(&mut self, data: &dyn TvData) -> bool {
        let Some(found) = self.pending.as_deref().and_then(|draft| data.search(draft)) else {
            return false;
        };
        self.pending = None;
        self.now = data.now();
        let offset = data.utc_offset_secs();
        self.programme_hits = found
            .hits
            .into_iter()
            .filter_map(|(key, p)| {
                let channel = data.channel(key)?;
                let detail = msg::search_programme_detail(
                    &channel.name,
                    &hhmm(p.start, offset),
                    &hhmm(p.stop, offset),
                    weekday(p.start, offset),
                );
                Some(Hit::Programme {
                    channel: key,
                    programme: p,
                    detail,
                })
            })
            .collect();
        self.move_to(self.focus().filter(|_| !self.is_empty()));
        tv_machine::idle::invalidate();
        true
    }

    fn channel_hits_for(&self, query: &str) -> Vec<Hit> {
        if query.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<(bool, &Channel)> = self
            .channels
            .iter()
            .filter_map(|(folded, c)| folded.find(query).map(|at| (at != 0, &**c)))
            .collect();
        // prefix matches first, then playlist order
        found.sort_by_key(|(inner, _)| *inner);
        found
            .into_iter()
            .take(MAX_CHANNEL_HITS)
            .map(|(_, c)| Hit::Channel {
                key: c.key,
                title: match c.number {
                    Some(n) => format!("{n}  {}", c.name),
                    None => c.name.clone(),
                },
                detail: c
                    .groups
                    .first()
                    .cloned()
                    .unwrap_or_else(|| Group::All.name().to_owned()),
            })
            .collect()
    }

    fn show_field(&mut self) {
        let text = if self.field.text().is_empty() {
            msg::search_hint()
        } else {
            self.field.text()
        };
        self.shown = cstr(text);
    }

    fn paint(&mut self, p: Painter) {
        let full = Rect::new(0.0, 0.0, style::SCREEN_W, style::SCREEN_H);
        p.rect(full, 0.0, style::SCREEN_BG, style::SCREEN_BG, 0.0);
        let x = MARGIN;
        let w = style::SCREEN_W - 2.0 * MARGIN;
        let field = Rect::new(x, FIELD_Y, w, FIELD_H);
        self.draw_field(p, field);
        self.spots.clear();
        self.spots.add(field, Spot::row(FIELD, 0));
        let mut y = FIELD_Y + FIELD_H + style::GAP;
        if self.is_empty() && !self.query.is_empty() && self.pending.is_none() {
            line(
                p,
                msg::search_empty(),
                style::ROW_SZ,
                style::TEXT_SECONDARY,
                false,
                Rect::new(x, y, w, ROW_H),
            );
            return;
        }
        for i in self.cursor.visible(self.len(), ROWS) {
            let Some(hit) = self.hit(i) else { break };
            let r = Rect::new(x, y, w, ROW_H);
            self.draw_hit(p, r, hit, self.focus() == Some(i));
            self.spots.add(r, Spot::row(RESULT, i));
            y += ROW_H;
        }
    }

    fn draw_field(&self, p: Painter, r: Rect) {
        let hot = self.field.editing() || self.on_field;
        p.rect(
            r,
            style::PLATE_RADIUS,
            style::FIELD_FILL,
            style::FIELD_FILL,
            0.0,
        );
        p.rring(
            r,
            style::PLATE_RADIUS,
            2.0,
            if hot {
                crate::theme::ink(style::FIELD_RIM_HOT)
            } else {
                style::FIELD_RIM
            },
        );
        let icon = Rect::new(
            r.x + style::FIELD_PAD,
            r.y + (r.h - style::ICON) * 0.5,
            style::ICON,
            style::ICON,
        );
        icons::draw(p, Icon::Search, icon, style::TEXT_SECONDARY);
        let tx = icon.x + style::ICON + style::GAP;
        let ink = if self.field.text().is_empty() {
            style::TEXT_SECONDARY
        } else {
            style::TEXT_PRIMARY
        };
        let tw = tv_ui::label::Label::new(self.shown.as_ptr(), style::ROW_SZ, ink).draw(
            p,
            Rect::new(tx, r.y, r.x + r.w - tx - style::FIELD_PAD, r.h),
        );
        if self.field.editing() {
            let cx = if self.field.text().is_empty() {
                tx
            } else {
                tx + tw + 2.0
            };
            let caret = Rect::new(
                cx.min(r.x + r.w - style::FIELD_PAD),
                r.y + 18.0,
                3.0,
                r.h - 36.0,
            );
            p.rect(caret, 0.0, style::TEXT_PRIMARY, style::TEXT_PRIMARY, 0.0);
        }
    }

    fn draw_hit(&self, p: Painter, r: Rect, hit: &Hit, focused: bool) {
        let (ink, sub) = if focused {
            p.rect(
                r,
                style::PLATE_RADIUS,
                style::FOCUS_FILL,
                style::FOCUS_FILL,
                0.0,
            );
            (style::FOCUS_INK, style::FOCUS_INK_SECONDARY)
        } else {
            (style::TEXT_PRIMARY, style::TEXT_SECONDARY)
        };
        let x = r.x + style::FIELD_PAD;
        let mut w = r.w - 2.0 * style::FIELD_PAD;
        // past programmes are listed only when the archive has them
        if let Hit::Programme { programme, .. } = hit {
            if time::when(programme.start, programme.stop, self.now) == When::Past {
                let icon = Rect::new(
                    r.x + r.w - style::FIELD_PAD - style::ICON,
                    r.y + (r.h - style::ICON) * 0.5,
                    style::ICON,
                    style::ICON,
                );
                icons::draw(p, Icon::CatchUp, icon, sub);
                w -= style::ICON + style::GAP;
            }
        }
        line(
            p,
            hit.title(),
            style::ROW_SZ,
            ink,
            false,
            Rect::new(x, r.y + 6.0, w, r.h * 0.5),
        );
        line(
            p,
            hit.detail(),
            style::VALUE_SZ,
            sub,
            false,
            Rect::new(x, r.y + r.h * 0.5 - 2.0, w, r.h * 0.5),
        );
    }
}

impl Screen for Search {
    fn hold(&self, key: Key) -> Hold {
        PanelKey::hold(key)
    }

    fn key(&mut self, key: Key, press: Press, data: &dyn TvData, _: u64) -> Output {
        PanelKey::from_action(Action { key, press })
            .map_or_else(Output::default, |k| self.handle(k, data))
    }

    fn text(&mut self, text: &str, data: &dyn TvData) -> Output {
        if self.field.on_text(text) {
            self.edited(data);
        }
        Output::redraw()
    }

    fn editing(&self) -> bool {
        self.field.editing()
    }

    /// The field or a result, while the keyboard is down.
    fn point(&mut self, x: f32, y: f32, _: &dyn TvData, _: u64) -> Output {
        if self.field.editing() {
            return Output::default();
        }
        let before = self.focus();
        match self.spots.at(x, y) {
            Some(s) if s.area == RESULT => {
                self.cursor.point(s.index, self.len(), ROWS);
                self.on_field = false;
            }
            Some(_) => self.on_field = true,
            None => {}
        }
        if self.focus() == before {
            return Output::default();
        }
        tv_machine::idle::invalidate();
        Output::redraw()
    }

    fn tick(&mut self, data: &dyn TvData, _: u64) -> Output {
        if self.poll(data) {
            Output::redraw()
        } else {
            Output::default()
        }
    }

    fn next_wake(&self, now_ms: u64) -> Option<u64> {
        self.pending.as_ref().map(|_| now_ms + POLL_MS)
    }

    fn refresh(&mut self, data: &dyn TvData) {
        let focus = self.focus();
        self.index(data);
        self.find(data);
        self.move_to(focus.filter(|_| !self.is_empty()));
    }

    fn draw(&mut self, p: Painter, _: &dyn Measure, _: &dyn Logos) {
        self.paint(p);
    }
}

/// Play `stream` with every screen closed: the full-screen player in front.
fn play(stream: Stream) -> Output {
    Output::command(Command::Play {
        stream,
        group: None,
    })
    .and(Output::navigate(Navigate::Player))
}

const MARGIN: f32 = 120.0;
const FIELD_Y: f32 = 60.0;
const FIELD_H: f32 = 88.0;
const ROW_H: f32 = 92.0;
/// Result rows below the field; the focus may sit on the first and the last.
const ROWS: Rows = Rows::new(9, 0);
/// [`Spot`] areas: the field and the results.
const FIELD: u8 = 0;
const RESULT: u8 = 1;

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;
    use crate::data::fake::FakeData;
    use crate::data::{HistoryEntry, Playback, SearchResult};
    use crate::panels::testing::{draw, press};
    use crate::time::DAY;
    use iptv::model::{Catchup, CatchupMode, Programme};

    const NOW: Timestamp = 1_700_000_000;
    const WIADOMOSCI: &str = "Wiadomo\u{15b}ci";
    const WIECZORNE: &str = "Wiadomo\u{15b}ci wieczorne";

    fn channel(i: usize, name: &str, days: u16) -> Channel {
        let url = format!("http://host.test/ch{i}.m3u8");
        let source = (days > 0).then(|| format!("{url}?utc={{utc}}"));
        Channel {
            key: ChannelKey::of_url(&url),
            name: name.into(),
            number: Some(i as u32 + 1),
            logo: None,
            groups: vec!["News".into()],
            url,
            tvg_id: None,
            tvg_name: None,
            tvg_shift_minutes: 0,
            catchup: Catchup {
                mode: (days > 0).then_some(CatchupMode::Shift),
                days,
                source,
                ..Default::default()
            },
            radio: false,
            http_headers: vec![],
            extra: vec![],
        }
    }

    fn at(start: Timestamp, title: &str) -> Programme {
        Programme {
            start,
            stop: start + 1800,
            title: title.into(),
            description: None,
            category: None,
            icon: None,
        }
    }

    /// Four channels (the third without catch-up, the second with three days of it) and their
    /// programmes around [`NOW`].
    fn guide() -> FakeData {
        let programmes = [
            (0, at(NOW - 2 * DAY, WIADOMOSCI)),
            (0, at(NOW - 600, "Fakty")),
            (0, at(NOW + 3 * 3600, WIECZORNE)),
            (1, at(NOW - 5 * DAY, WIADOMOSCI)),
            (1, at(NOW - DAY, "Sport")),
            (2, at(NOW - 3600, WIADOMOSCI)),
            (2, at(NOW - 600, "Muzyka")),
            (3, at(NOW - DAY + 60, WIADOMOSCI)),
        ];
        let mut data = FakeData::new(NOW, 0).with_schedule(move |i, from, to| {
            programmes
                .iter()
                .filter(|(c, p)| *c == i && p.stop > from && p.start < to)
                .map(|(_, p)| p.clone())
                .collect()
        });
        data.channels = vec![
            channel(0, "Polsat News", 7),
            channel(1, "TVN", 3),
            channel(2, "Radio Zet", 0),
            channel(3, "News 24", 7),
        ];
        data
    }

    fn open(data: &dyn TvData) -> Search {
        let search = Search::new(data);
        assert!(search.editing(), "opens with the keyboard up");
        search
    }

    fn titles(search: &Search) -> Vec<&str> {
        search.hits().map(Hit::title).collect()
    }

    fn type_text(search: &mut Search, data: &dyn TvData, text: &str) {
        for c in text.chars() {
            assert_eq!(search.text(&c.to_string(), data), Output::redraw());
        }
    }

    fn keys(search: &mut Search, data: &dyn TvData, keys: &[Key]) -> Output {
        press(search, data, keys)
    }

    #[test]
    fn programmes_come_from_the_archive_window_and_ahead() {
        let data = guide();
        let mut search = open(&data);
        type_text(&mut search, &data, "wiadomosci");
        // past newest first, then upcoming; outside TVN's 3 days and the channel without
        // catch-up are left out
        assert_eq!(titles(&search), [WIADOMOSCI, WIADOMOSCI, WIECZORNE]);
        let details: Vec<_> = search.hits().map(Hit::detail).collect();
        assert_eq!(details[0], "News 24, Mon 22:14 - 22:44");
        assert!(details[1].starts_with("Polsat News, Sun "));
    }

    #[test]
    fn channels_match_by_name_prefix_first() {
        let data = guide();
        let mut search = open(&data);
        type_text(&mut search, &data, "news");
        assert_eq!(titles(&search), ["4  News 24", "1  Polsat News"]);
        assert_eq!(search.hit(0).unwrap().detail(), "News");
    }

    #[test]
    fn one_letter_searches_channels_only() {
        let data = guide();
        let mut search = open(&data);
        type_text(&mut search, &data, "f");
        assert!(search.is_empty());
        type_text(&mut search, &data, "a");
        assert_eq!(titles(&search), ["Fakty"]);
    }

    #[test]
    fn backspace_widens_and_clear_empties() {
        let data = guide();
        let mut search = open(&data);
        type_text(&mut search, &data, "wiadomosci w");
        assert_eq!(titles(&search), [WIECZORNE]);
        keys(&mut search, &data, &[Key::Backspace, Key::Backspace]);
        assert_eq!(search.draft(), "wiadomosci");
        assert_eq!(search.len(), 3);
        keys(&mut search, &data, &[Key::Clear]);
        assert!(search.is_empty());
        assert_eq!(search.draft(), "");
    }

    #[test]
    fn ok_plays_live_or_from_the_archive() {
        let data = guide();
        let play = |stream| {
            Output::command(Command::Play {
                stream,
                group: None,
            })
            .and(Output::navigate(Navigate::Player))
        };
        let mut search = open(&data);
        type_text(&mut search, &data, "fakty");
        assert_eq!(keys(&mut search, &data, &[Key::Ok]), Output::default());
        assert!(!search.editing(), "OK puts the keyboard away");
        assert_eq!(search.focus(), Some(0));
        let key = data.channels[0].key;
        assert_eq!(
            keys(&mut search, &data, &[Key::Ok]),
            play(Stream::Live(key))
        );

        let mut search = open(&data);
        type_text(&mut search, &data, "sport");
        assert_eq!(
            keys(&mut search, &data, &[Key::Down, Key::Ok]),
            play(Stream::Catchup {
                channel: data.channels[1].key,
                programme: at(NOW - DAY, "Sport"),
                offset_secs: 0
            })
        );

        let mut search = open(&data);
        type_text(&mut search, &data, "wieczorne");
        assert_eq!(
            keys(&mut search, &data, &[Key::Down, Key::Ok])
                .toast
                .as_deref(),
            Some(msg::epg_not_aired())
        );

        let mut search = open(&data);
        type_text(&mut search, &data, "tvn");
        assert_eq!(
            keys(&mut search, &data, &[Key::Down, Key::Ok]),
            play(Stream::Live(data.channels[1].key))
        );
    }

    #[test]
    fn a_past_programme_out_of_the_archive_by_now_says_so() {
        let mut data = guide();
        let mut search = open(&data);
        type_text(&mut search, &data, "sport");
        keys(&mut search, &data, &[Key::Down]);
        data.now = NOW + 3 * DAY;
        assert_eq!(
            keys(&mut search, &data, &[Key::Ok]).toast.as_deref(),
            Some(msg::epg_catchup_unavailable())
        );
    }

    #[test]
    fn focus_moves_between_the_field_and_the_results() {
        let data = guide();
        let mut search = open(&data);
        type_text(&mut search, &data, "wiadomosci");
        keys(&mut search, &data, &[Key::Back]);
        assert!(!search.editing());
        assert_eq!(search.focus(), None);
        keys(&mut search, &data, &[Key::Down, Key::Down, Key::Down]);
        assert_eq!(keys(&mut search, &data, &[Key::Down]), Output::default());
        assert_eq!(search.focus(), Some(2));
        keys(&mut search, &data, &[Key::Back]);
        assert_eq!(search.focus(), None);
        keys(&mut search, &data, &[Key::Down, Key::Up]);
        assert_eq!(search.focus(), None);
        keys(&mut search, &data, &[Key::Ok]);
        assert!(search.editing(), "OK on the field raises the keyboard");
        keys(&mut search, &data, &[Key::Back]);
        assert!(!search.editing());
        assert_eq!(keys(&mut search, &data, &[Key::Back]), Output::close());
    }

    #[test]
    fn the_pointer_focuses_a_result_or_the_field_while_the_keyboard_is_down() {
        let data = guide();
        let mut search = open(&data);
        type_text(&mut search, &data, "wiadomosci");
        let row = |i: usize| FIELD_Y + FIELD_H + style::GAP + (i as f32 + 0.5) * ROW_H;
        let x = MARGIN + 40.0;
        assert_eq!(
            search.point(x, row(1), &data, 0),
            Output::default(),
            "typing"
        );
        keys(&mut search, &data, &[Key::Back]);
        draw(&mut search);
        assert_eq!(search.point(x, row(1), &data, 0), Output::redraw());
        assert_eq!(search.focus(), Some(1));
        assert_eq!(search.point(x, row(1) + 5.0, &data, 0), Output::default());
        // Up carries on from the pointed result; the field takes the focus back.
        keys(&mut search, &data, &[Key::Up]);
        draw(&mut search);
        assert_eq!(search.focus(), Some(0));
        assert_eq!(search.point(x, FIELD_Y + 10.0, &data, 0), Output::redraw());
        assert_eq!(search.focus(), None);
        assert_eq!(
            search.point(x, row(8), &data, 0),
            Output::default(),
            "no row"
        );
    }

    #[test]
    fn results_scroll_with_the_focus() {
        let mut data = guide();
        data.channels = (0..20).map(|i| channel(i, "News", 0)).collect();
        let mut search = open(&data);
        type_text(&mut search, &data, "news");
        assert_eq!(search.len(), 20);
        keys(&mut search, &data, &[Key::Down; 12]);
        assert_eq!(search.focus(), Some(11));
        assert_eq!(search.cursor.top, 11 + 1 - ROWS.shown);
        keys(&mut search, &data, &[Key::Back]);
        assert_eq!((search.focus(), search.cursor.top), (None, 0));
    }

    #[test]
    fn typing_with_the_keyboard_believed_down_takes_it_as_up() {
        let data = guide();
        let mut search = open(&data);
        type_text(&mut search, &data, "tv");
        keys(&mut search, &data, &[Key::Down]);
        assert!(!search.editing());
        type_text(&mut search, &data, "n");
        assert!(search.editing());
        assert_eq!(search.focus(), None);
        assert_eq!(titles(&search), ["2  TVN"]);
    }

    /// [`FakeData`] whose programme search is still running for its first `slow` asks.
    struct Slow {
        data: FakeData,
        slow: Cell<usize>,
        asked: RefCell<Vec<String>>,
    }

    impl TvData for Slow {
        fn now(&self) -> Timestamp {
            self.data.now()
        }
        fn groups(&self) -> Vec<Group> {
            self.data.groups()
        }
        fn channels(&self, group: &Group) -> Rc<[Rc<Channel>]> {
            self.data.channels(group)
        }
        fn programmes(
            &self,
            channel: ChannelKey,
            from: Timestamp,
            to: Timestamp,
        ) -> Vec<Programme> {
            self.data.programmes(channel, from, to)
        }
        fn is_favorite(&self, channel: ChannelKey) -> bool {
            self.data.is_favorite(channel)
        }
        fn recent_channels(&self) -> Vec<ChannelKey> {
            self.data.recent_channels()
        }
        fn history(&self) -> Vec<HistoryEntry> {
            self.data.history()
        }
        fn playback(&self) -> Playback {
            self.data.playback()
        }
        fn search(&self, query: &str) -> Option<SearchResult> {
            self.asked.borrow_mut().push(query.to_owned());
            if self.slow.get() > 0 {
                self.slow.set(self.slow.get() - 1);
                return None;
            }
            self.data.search(query)
        }
    }

    #[test]
    fn a_running_search_is_asked_again_on_each_tick() {
        let data = Slow {
            data: guide(),
            slow: Cell::new(2),
            asked: RefCell::default(),
        };
        let mut search = open(&data);
        assert_eq!(search.next_wake(1_000), None);
        search.text("Fakty", &data);
        assert!(search.is_empty());
        assert_eq!(search.next_wake(1_000), Some(1_016));
        draw(&mut search);
        assert_eq!(search.tick(&data, 1_016), Output::default());
        assert_eq!(search.next_wake(1_016), Some(1_032));
        assert_eq!(search.tick(&data, 1_032), Output::redraw());
        assert_eq!(titles(&search), ["Fakty"]);
        assert_eq!(search.next_wake(1_032), None);
        assert_eq!(search.tick(&data, 1_048), Output::default());
        // the draft as typed, each time; one letter asks nothing
        assert_eq!(*data.asked.borrow(), ["Fakty"; 3]);
        keys(&mut search, &data, &[Key::Clear]);
        search.text("F", &data);
        assert_eq!(data.asked.borrow().len(), 3);
        assert_eq!(search.next_wake(2_000), None);
    }

    #[test]
    fn results_that_arrive_keep_the_focus_on_the_list() {
        let data = Slow {
            data: guide(),
            slow: Cell::new(0),
            asked: RefCell::default(),
        };
        let mut search = open(&data);
        type_text(&mut search, &data, "wiadomosci");
        keys(&mut search, &data, &[Key::Down, Key::Down, Key::Down]);
        assert_eq!(search.focus(), Some(2));
        // the same draft again after the guide changed: one result fewer
        data.slow.set(1);
        let mut fewer = guide();
        fewer.channels.truncate(3);
        let data = Slow {
            data: fewer,
            ..data
        };
        search.refresh(&data);
        assert_eq!(search.len(), 3, "the old hits stay while it runs");
        assert_eq!(search.tick(&data, 0), Output::redraw());
        assert_eq!(titles(&search), [WIADOMOSCI, WIECZORNE]);
        assert_eq!(search.focus(), Some(1));
    }

    #[test]
    fn draws_results_the_field_and_the_empty_state() {
        let data = guide();
        let mut search = open(&data);
        draw(&mut search);
        type_text(&mut search, &data, "wiadomosci");
        keys(&mut search, &data, &[Key::Down]);
        draw(&mut search);
        search.text("zzz", &data);
        assert!(search.is_empty());
        draw(&mut search);
    }
}
