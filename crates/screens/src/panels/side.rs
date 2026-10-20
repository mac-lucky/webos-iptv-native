//! The side panel: a stack of pages sliding in from the right edge. Context menus and Settings are
//! both built from it; their owners decide what a row does, this module only moves the focus,
//! drills in and out, and draws. The pointer focuses the row it rests on ([`SidePanel::point`]).

use std::ffi::CString;

use tv_ui::text_view::TextView;
use tv_ui::{Painter, Rect, Spring};

use super::{style, PanelKey};
use crate::data::{Navigate, Output};
use crate::draw::cstr;
use crate::icons::{self, Icon};
use crate::lists::{Cursor, Rows};
use crate::point::{Spot, Spots};

/// A page's rows on screen: the focus may sit on the first and the last.
const ROWS: Rows = Rows::new(style::VISIBLE_ROWS, 0);

/// One line of a page.
#[derive(Clone, Debug)]
pub struct Row<Id> {
    pub id: Id,
    label: String,
    value: Option<CString>,
    chevron: bool,
    /// `Some` on an option row of a choice page: whether it is the current one.
    check: Option<bool>,
}

impl<Id> Row<Id> {
    /// A row that does something when OK is pressed.
    pub fn action(id: Id, label: &str) -> Self {
        Self {
            id,
            label: label.to_owned(),
            value: None,
            chevron: false,
            check: None,
        }
    }

    /// A row that opens another page.
    pub fn submenu(id: Id, label: &str) -> Self {
        Self::action(id, label).chevron()
    }

    /// An option of a choice page; `checked` marks the current one.
    pub fn check(id: Id, label: &str, checked: bool) -> Self {
        Self {
            check: Some(checked),
            ..Self::action(id, label)
        }
    }

    /// A value drawn at the row's right edge (a setting's current value).
    pub fn value(mut self, value: &str) -> Self {
        self.set_value(value);
        self
    }

    pub fn chevron(mut self) -> Self {
        self.chevron = true;
        self
    }

    pub fn set_value(&mut self, value: &str) {
        self.value = (!value.is_empty()).then(|| cstr(value));
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn value_text(&self) -> Option<&str> {
        self.value.as_ref().and_then(|v| v.to_str().ok())
    }

    pub fn checked(&self) -> Option<bool> {
        self.check
    }
}

/// One page: a title and either rows or paragraphs of text (a description, About), optionally
/// headed by a text field.
#[derive(Clone, Debug)]
pub struct Page<Id> {
    title: String,
    rows: Vec<Row<Id>>,
    body: Vec<String>,
    /// What the text field shows, on an entry page.
    field: Option<CString>,
    /// The focused row and the first one drawn; on a text page, the first paragraph drawn.
    cursor: Cursor,
}

impl<Id> Page<Id> {
    pub fn list(title: &str, rows: Vec<Row<Id>>) -> Self {
        Self {
            title: title.to_owned(),
            rows,
            body: Vec::new(),
            field: None,
            cursor: Cursor::default(),
        }
    }

    pub fn text(title: &str, body: Vec<String>) -> Self {
        Self {
            body,
            ..Self::list(title, Vec::new())
        }
    }

    /// A text entry page; [`SidePanel::set_field`] updates what the field shows.
    pub fn entry(title: &str) -> Self {
        Self {
            field: Some(CString::default()),
            ..Self::list(title, Vec::new())
        }
    }

    /// Start with row `i` focused.
    pub fn focused(mut self, i: usize) -> Self {
        self.cursor.set(i, self.rows.len(), ROWS);
        self
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn rows(&self) -> &[Row<Id>] {
        &self.rows
    }

    pub fn body(&self) -> &[String] {
        &self.body
    }

    pub fn field(&self) -> Option<&str> {
        self.field.as_ref().and_then(|f| f.to_str().ok())
    }

    pub fn focus(&self) -> usize {
        self.cursor.index
    }

    pub fn top(&self) -> usize {
        self.cursor.top
    }
}

/// What a key did to the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav<Id> {
    /// Nothing changed.
    None,
    /// The focus or the scroll moved.
    Moved,
    /// OK (or Right on a submenu row) on this row: the owner decides.
    Activate(Id),
    /// Back left a sub-page.
    Popped,
    /// Back on the first page: the owner closes the panel.
    Close,
}

/// The screen edge a panel slides in from. Menus on content open on the right; the main menu
/// opens on the left, beside the groups panel it is reached from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
}

/// The panel: its page stack and the slide spring.
pub struct SidePanel<Id> {
    pages: Vec<Page<Id>>,
    edge: Edge,
    /// 0 hidden, 1 fully in.
    slide: Spring,
    shown: bool,
    /// Whether the text field is being typed into (draws the caret and the focus rim).
    editing: bool,
    /// The rows in the frame drawn last, for the pointer.
    spots: Spots,
}

impl<Id: Copy> SidePanel<Id> {
    /// A panel on the right edge.
    pub fn new(root: Page<Id>) -> Self {
        Self::at(Edge::Right, root)
    }

    pub fn at(edge: Edge, root: Page<Id>) -> Self {
        Self {
            pages: vec![root],
            edge,
            slide: Spring::at(0.0),
            shown: true,
            editing: false,
            spots: Spots::default(),
        }
    }

    pub fn page(&self) -> &Page<Id> {
        self.pages.last().expect("a panel always has a page")
    }

    fn page_mut(&mut self) -> &mut Page<Id> {
        self.pages.last_mut().expect("a panel always has a page")
    }

    pub fn depth(&self) -> usize {
        self.pages.len()
    }

    /// The focused row's id, if the page has rows.
    pub fn focused(&self) -> Option<Id> {
        let page = self.page();
        page.rows.get(page.focus()).map(|r| r.id)
    }

    pub fn push(&mut self, page: Page<Id>) {
        self.pages.push(page);
        tv_machine::idle::invalidate();
    }

    /// Put `page` in place of the current page (its rows changed), the pages under it kept.
    pub fn replace(&mut self, page: Page<Id>) {
        *self.page_mut() = page;
        tv_machine::idle::invalidate();
    }

    /// Leave the current sub-page; the first page stays.
    pub fn pop(&mut self) -> bool {
        if self.pages.len() < 2 {
            return false;
        }
        self.pages.pop();
        self.editing = false;
        tv_machine::idle::invalidate();
        true
    }

    /// What the current page's text field shows, and whether it is being typed into.
    pub fn set_field(&mut self, shown: &str, editing: bool) {
        self.page_mut().field = Some(cstr(shown));
        self.editing = editing;
        tv_machine::idle::invalidate();
    }

    /// Visit every row on every page (to refresh values after a change).
    pub fn for_each_row(&mut self, mut f: impl FnMut(&mut Row<Id>)) {
        for page in &mut self.pages {
            page.rows.iter_mut().for_each(&mut f);
        }
        tv_machine::idle::invalidate();
    }

    /// Start sliding out ([`handle`](Self::handle) does once its output closes the panel); the
    /// shell keeps calling `update` and `draw` until [`gone`](Self::gone).
    pub fn hide(&mut self) {
        self.shown = false;
    }

    /// [`hide`](Self::hide) was called: the panel takes no more keys.
    pub fn closing(&self) -> bool {
        !self.shown
    }

    /// Slid fully out after [`hide`](Self::hide).
    pub fn gone(&self) -> bool {
        !self.shown && self.slide.pos <= style::HIDDEN
    }

    /// Step the slide. Idle once settled: the spring reports its own motion.
    pub fn update(&mut self, dt: f32) {
        let target = if self.shown { 1.0 } else { 0.0 };
        self.slide.step(target, style::SLIDE_K, dt);
    }

    /// A key for the panel's owner. Nothing once it is closing; what a row does is `activate`'s
    /// to say (it may push a page); Back on the first page closes. An output that closes the
    /// panel (`Close`, or `Player`, which closes every screen) starts it sliding out.
    pub fn handle(
        &mut self,
        key: PanelKey,
        activate: impl FnOnce(&mut Self, Id) -> Output,
    ) -> Output {
        if self.closing() {
            return Output::default();
        }
        let out = match self.key(key) {
            Nav::None => return Output::default(),
            Nav::Moved | Nav::Popped => Output::default(),
            Nav::Activate(id) => activate(self, id),
            Nav::Close => Output::close(),
        };
        if out
            .navigate
            .iter()
            .any(|n| matches!(n, Navigate::Close | Navigate::Player))
        {
            self.hide();
        }
        Output {
            redraw: true,
            ..out
        }
    }

    pub fn key(&mut self, key: PanelKey) -> Nav<Id> {
        let nav = self.navigate(key);
        if !matches!(nav, Nav::None) {
            tv_machine::idle::invalidate();
        }
        nav
    }

    fn navigate(&mut self, key: PanelKey) -> Nav<Id> {
        match key {
            PanelKey::Up => self.step(-1),
            PanelKey::Down => self.step(1),
            PanelKey::Ok => self.focused().map_or(Nav::None, Nav::Activate),
            // Right panel: Right drills into a submenu, Left steps back. Left panel: the side
            // facing the content (Right) steps back and closes, as Back does.
            PanelKey::Right if self.edge == Edge::Right => {
                let page = self.page();
                match page.rows.get(page.focus()) {
                    Some(row) if row.chevron => Nav::Activate(row.id),
                    _ => Nav::None,
                }
            }
            PanelKey::Back | PanelKey::Left | PanelKey::Right => {
                let closes =
                    key == PanelKey::Back || (key == PanelKey::Right && self.edge == Edge::Left);
                if key == PanelKey::Left && self.edge == Edge::Left {
                    Nav::None
                } else if self.pop() {
                    Nav::Popped
                } else if closes {
                    Nav::Close
                } else {
                    Nav::None
                }
            }
            _ => Nav::None,
        }
    }

    /// The pointer at `(x, y)`: focus the row drawn there, as Up and Down do. A redraw when the
    /// focus moved.
    pub fn point(&mut self, x: f32, y: f32) -> Output {
        let Some(spot) = self.spots.at(x, y).filter(|_| !self.closing()) else {
            return Output::default();
        };
        let page = self.page_mut();
        if !page.cursor.point(spot.index, page.rows.len(), ROWS) {
            return Output::default();
        }
        tv_machine::idle::invalidate();
        Output::redraw()
    }

    /// Move the focus (rows) or scroll (paragraphs, one at a time) by one, without wrapping.
    fn step(&mut self, by: isize) -> Nav<Id> {
        let page = self.page_mut();
        let before = page.cursor;
        if page.rows.is_empty() {
            page.cursor
                .step(by, page.body.len(), Rows::new(1, 0), false);
        } else {
            page.cursor.step(by, page.rows.len(), ROWS, false);
        }
        if page.cursor == before {
            Nav::None
        } else {
            Nav::Moved
        }
    }

    /// Where the panel sits this frame on the 1920x1080 plane.
    pub fn frame(&self) -> Rect {
        let w = style::PANEL_W;
        let shown = w * self.slide.pos.clamp(0.0, 1.0);
        let x = match self.edge {
            Edge::Right => style::SCREEN_W - shown,
            Edge::Left => shown - w,
        };
        Rect::new(x, 0.0, w, style::SCREEN_H)
    }

    /// Draw the current page.
    pub fn draw(&mut self, p: Painter) {
        self.spots.clear();
        if self.slide.pos <= style::HIDDEN {
            return;
        }
        let f = self.frame();
        p.shadow(f, 0.0, style::SHADOW_BLUR, 0.0, style::SHADOW);
        p.rect(f, 0.0, style::PANEL_BG, style::PANEL_BG, 0.0);
        let page = self.pages.last().expect("a panel always has a page");
        let x = f.x + style::PAD_X;
        let w = f.w - 2.0 * style::PAD_X;
        line(
            p,
            &page.title,
            style::TITLE_SZ,
            style::TEXT_SECONDARY,
            true,
            Rect::new(x, style::TITLE_Y, w, style::TITLE_H),
        );
        let mut y = style::ROWS_TOP;
        if let Some(text) = &page.field {
            draw_field(p, Rect::new(x, y, w, style::ROW_H), text, self.editing);
            y += style::ROW_H + style::GAP;
        }
        for i in page.cursor.visible(page.rows.len(), ROWS) {
            let r = Rect::new(f.x, y, f.w, style::ROW_H);
            draw_row(p, r, &page.rows[i], i == page.focus());
            self.spots.add(r, Spot::row(0, i));
            y += style::ROW_H;
        }
        for para in page.body.iter().skip(page.top()) {
            if y >= style::SCREEN_H - style::PAD_X {
                break;
            }
            y += TextView::new(para, style::BODY_SZ, style::TEXT_PRIMARY)
                .draw(p, Rect::new(x, y, w, 0.0))
                + style::GAP;
        }
    }
}

/// One line of `text` in `r`, vertically centred on its cap band and ellipsised to `r.w`.
pub(crate) fn line(p: Painter, text: &str, sz: i32, col: [f32; 4], bold: bool, r: Rect) {
    let (cap_top, baseline) = tv_gfx::text::text_cap_band(sz, bold as i32);
    let y = r.y + (r.h - (baseline - cap_top)) * 0.5;
    let tv = TextView::new(text, sz, col).max_lines(1);
    let tv = if bold { tv.bold() } else { tv };
    tv.draw(p, Rect::new(r.x, y, r.w.max(0.0), 0.0));
}

fn draw_row<Id>(p: Painter, r: Rect, row: &Row<Id>, focused: bool) {
    let (ink, sub) = if focused {
        let plate = Rect::new(
            r.x + style::PLATE_INSET,
            r.y + 4.0,
            r.w - 2.0 * style::PLATE_INSET,
            r.h - 8.0,
        );
        p.rect(
            plate,
            style::PLATE_RADIUS,
            style::FOCUS_FILL,
            style::FOCUS_FILL,
            0.0,
        );
        (style::FOCUS_INK, style::FOCUS_INK_SECONDARY)
    } else {
        (style::TEXT_PRIMARY, style::TEXT_SECONDARY)
    };
    let icon = |x: f32| Rect::new(x, r.y + (r.h - style::ICON) * 0.5, style::ICON, style::ICON);
    let mut x = r.x + style::PAD_X;
    let mut right = r.x + r.w - style::PAD_X;
    if let Some(checked) = row.check {
        if checked {
            tv_ui::icons::draw(p, tv_ui::icons::Icon::Check, icon(x), ink);
        }
        x += style::ICON + style::GAP;
    }
    if row.chevron {
        icons::draw(p, Icon::ChevronRight, icon(right - style::ICON), sub);
        right -= style::ICON + style::GAP;
    }
    if let Some(value) = &row.value {
        // A value takes at most this share of the row; a long one (a masked URL) is ellipsised.
        let w = tv_gfx::text::text_width(value.as_ptr(), style::VALUE_SZ, 0).min((right - x) * 0.5);
        let text = value.to_str().unwrap_or("");
        line(
            p,
            text,
            style::VALUE_SZ,
            sub,
            false,
            Rect::new(right - w, r.y, w, r.h),
        );
        right -= w + style::GAP;
    }
    line(
        p,
        &row.label,
        style::ROW_SZ,
        ink,
        false,
        Rect::new(x, r.y, right - x, r.h),
    );
}

fn draw_field(p: Painter, r: Rect, text: &CString, editing: bool) {
    let rim = if editing {
        crate::theme::ink(style::FIELD_RIM_HOT)
    } else {
        style::FIELD_RIM
    };
    p.rect(
        r,
        style::PLATE_RADIUS,
        style::FIELD_FILL,
        style::FIELD_FILL,
        0.0,
    );
    p.rring(r, style::PLATE_RADIUS, 2.0, rim);
    let x = r.x + style::FIELD_PAD;
    let room = r.w - 2.0 * style::FIELD_PAD;
    let s = text.to_str().unwrap_or("");
    line(
        p,
        s,
        style::ROW_SZ,
        style::TEXT_PRIMARY,
        false,
        Rect::new(x, r.y, room, r.h),
    );
    if editing {
        let w = tv_gfx::text::text_width(text.as_ptr(), style::ROW_SZ, 0).min(room);
        let cx = x + w + 2.0;
        let caret = Rect::new(cx, r.y + 16.0, 3.0, r.h - 32.0);
        p.rect(caret, 0.0, style::TEXT_PRIMARY, style::TEXT_PRIMARY, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize) -> Vec<Row<usize>> {
        (0..n)
            .map(|i| Row::action(i, &format!("Row {i}")))
            .collect()
    }

    #[test]
    fn focus_moves_without_wrapping_and_scrolls_into_view() {
        let mut panel = SidePanel::new(Page::list("Menu", rows(15)));
        assert_eq!(panel.key(PanelKey::Up), Nav::None);
        for _ in 0..12 {
            assert_eq!(panel.key(PanelKey::Down), Nav::Moved);
        }
        assert_eq!(panel.page().focus(), 12);
        assert_eq!(panel.page().top(), 12 + 1 - style::VISIBLE_ROWS);
        for _ in 0..5 {
            panel.key(PanelKey::Down);
        }
        assert_eq!(panel.focused(), Some(14));
        assert_eq!(panel.key(PanelKey::Down), Nav::None);
        for _ in 0..14 {
            panel.key(PanelKey::Up);
        }
        assert_eq!((panel.page().focus(), panel.page().top()), (0, 0));
    }

    #[test]
    fn ok_activates_and_right_only_opens_submenus() {
        let mut panel = SidePanel::new(Page::list(
            "Menu",
            vec![Row::action(1, "Do"), Row::submenu(2, "More")],
        ));
        assert_eq!(panel.key(PanelKey::Right), Nav::None);
        assert_eq!(panel.key(PanelKey::Ok), Nav::Activate(1));
        panel.key(PanelKey::Down);
        assert_eq!(panel.key(PanelKey::Right), Nav::Activate(2));
    }

    #[test]
    fn back_pops_sub_pages_then_closes() {
        let mut panel = SidePanel::new(Page::list("Menu", rows(2)));
        assert_eq!(panel.key(PanelKey::Left), Nav::None);
        panel.push(Page::list("Sub", rows(1)));
        panel.push(Page::text("Text", vec!["a".into()]));
        assert_eq!(panel.depth(), 3);
        assert_eq!(panel.key(PanelKey::Left), Nav::Popped);
        assert_eq!(panel.key(PanelKey::Back), Nav::Popped);
        assert_eq!(panel.page().title(), "Menu");
        assert_eq!(panel.key(PanelKey::Back), Nav::Close);
    }

    #[test]
    fn a_left_panel_mirrors_the_keys_and_the_slide() {
        let mut panel = SidePanel::at(
            Edge::Left,
            Page::list("Menu", vec![Row::submenu(0, "More")]),
        );
        assert_eq!(panel.frame().x, -style::PANEL_W);
        for _ in 0..120 {
            panel.update(1.0 / 60.0);
        }
        assert!(panel.frame().x.abs() < 1.0);
        assert_eq!(panel.key(PanelKey::Left), Nav::None);
        assert_eq!(panel.key(PanelKey::Ok), Nav::Activate(0));
        panel.push(Page::list("Sub", rows(1)));
        assert_eq!(panel.key(PanelKey::Right), Nav::Popped);
        assert_eq!(panel.key(PanelKey::Right), Nav::Close);
    }

    #[test]
    fn text_pages_scroll_by_paragraph() {
        let mut panel = SidePanel::<usize>::new(Page::text(
            "About",
            vec!["a".into(), "b".into(), "c".into()],
        ));
        assert_eq!(panel.key(PanelKey::Ok), Nav::None);
        assert_eq!(panel.key(PanelKey::Down), Nav::Moved);
        assert_eq!(panel.key(PanelKey::Down), Nav::Moved);
        assert_eq!(panel.key(PanelKey::Down), Nav::None);
        assert_eq!(panel.page().top(), 2);
        assert_eq!(panel.key(PanelKey::Up), Nav::Moved);
    }

    #[test]
    fn slides_in_from_the_right_and_out_again() {
        let mut panel = SidePanel::new(Page::list("Menu", rows(3)));
        assert_eq!(panel.frame().x, style::SCREEN_W);
        for _ in 0..120 {
            panel.update(1.0 / 60.0);
        }
        assert!((panel.frame().x - (style::SCREEN_W - style::PANEL_W)).abs() < 1.0);
        assert!(!panel.gone());
        panel.hide();
        for _ in 0..120 {
            panel.update(1.0 / 60.0);
        }
        assert!(panel.gone());
    }

    #[test]
    fn values_and_checks_render_from_rows() {
        let mut row = Row::check(0, "Name", true).value("7 days");
        assert_eq!(
            (row.label(), row.value_text(), row.checked()),
            ("Name", Some("7 days"), Some(true))
        );
        row.set_value("");
        assert_eq!(row.value_text(), None);
    }

    #[test]
    fn draws_with_a_recording_painter() {
        let mut panel = SidePanel::new(Page::list(
            "Menu",
            vec![
                Row::submenu(0, "More").value("x"),
                Row::check(1, "Pick", true),
            ],
        ));
        panel.update(0.5);
        panel.draw(Painter::recording());
        panel.push(Page::entry("URL"));
        panel.set_field("http://h.test/***", true);
        assert_eq!(panel.page().field(), Some("http://h.test/***"));
        panel.draw(Painter::recording());
        panel.push(Page::text(
            "About",
            vec!["One paragraph.".into(), "Two.".into()],
        ));
        panel.draw(Painter::recording());
    }

    #[test]
    fn the_pointer_focuses_the_row_drawn_under_it() {
        let mut panel = SidePanel::new(Page::list("Menu", rows(20)));
        panel.update(10.0);
        panel.draw(Painter::recording());
        let f = panel.frame();
        let row_y = |row: usize| style::ROWS_TOP + (row as f32 + 0.5) * style::ROW_H;
        assert_eq!(panel.point(f.x + 40.0, row_y(4)), Output::redraw());
        assert_eq!(panel.focused(), Some(4));
        assert_eq!(panel.point(f.x + 60.0, row_y(4)), Output::default());
        assert_eq!(
            panel.point(f.x - 1.0, row_y(5)),
            Output::default(),
            "left of it"
        );
        assert_eq!(
            panel.point(f.x + 40.0, 40.0),
            Output::default(),
            "the title"
        );
        // Below the last row on screen: nothing to scroll to.
        let below = row_y(style::VISIBLE_ROWS);
        assert_eq!(panel.point(f.x + 40.0, below), Output::default());
        assert_eq!(panel.page().top(), 0);
        // Down carries on from the pointed row; OK activates it.
        assert_eq!(panel.key(PanelKey::Down), Nav::Moved);
        assert_eq!(panel.key(PanelKey::Ok), Nav::Activate(5));
        // A closing panel takes no pointer.
        panel.hide();
        assert_eq!(panel.point(f.x + 40.0, row_y(2)), Output::default());
        assert_eq!(panel.focused(), Some(5));
    }

    #[test]
    fn handle_asks_the_owner_and_slides_out_on_close() {
        let mut panel = SidePanel::new(Page::list(
            "Menu",
            vec![Row::submenu(0, "More"), Row::action(1, "Go")],
        ));
        let out = panel.handle(PanelKey::Ok, |panel, id| {
            assert_eq!(id, 0);
            panel.push(Page::list("Sub", rows(1)));
            Output::default()
        });
        assert!(out.redraw && out.navigate.is_empty());
        assert_eq!(panel.depth(), 2);
        assert_eq!(
            panel.handle(PanelKey::Up, |_, _| unreachable!()),
            Output::default()
        );
        assert!(panel.handle(PanelKey::Back, |_, _| unreachable!()).redraw);
        panel.handle(PanelKey::Down, |_, _| unreachable!());
        let out = panel.handle(PanelKey::Ok, |_, id| {
            assert_eq!(id, 1);
            Output::close()
        });
        assert_eq!(out.navigate, [Navigate::Close]);
        assert!(panel.closing());
        assert_eq!(
            panel.handle(PanelKey::Ok, |_, _| unreachable!()),
            Output::default()
        );

        let mut panel = SidePanel::new(Page::list("Menu", rows(1)));
        let out = panel.handle(PanelKey::Back, |_, _| unreachable!());
        assert_eq!(out.navigate, [Navigate::Close]);
        assert!(panel.closing());

        // Going to the player closes it too.
        let mut panel = SidePanel::new(Page::list("Menu", rows(1)));
        panel.handle(PanelKey::Ok, |_, _| Output::navigate(Navigate::Player));
        assert!(panel.closing());
    }
}
