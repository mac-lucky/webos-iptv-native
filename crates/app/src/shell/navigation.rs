//! Left navigation shared by browsing pages. Playback and modal dialogs retain their controls.
use std::ffi::CString;

use iptv::vod::VodKind;
use tv_screens::data::{Navigate, Rect, VodSection};
use tv_screens::input::{Action, Hold, Key, Press};
use tv_screens::panels::keys;
use tv_ui::{Painter, Zoom};

use super::{msg, theme, Shell};
use crate::input::{PointerEvent, PointerKind};

const WIDTH: f32 = 320.0;
const SCALE: f32 = (1920.0 - WIDTH) / 1920.0;
const TOP: f32 = (1080.0 - 1080.0 * SCALE) * 0.5;
const ROW_Y: f32 = 144.0;
const ROW_H: f32 = 96.0;

pub(super) struct Navigation {
    pub focused: bool,
    index: usize,
}
impl Default for Navigation {
    fn default() -> Self {
        Self {
            focused: false,
            index: 1,
        }
    }
}

pub(super) fn hold(key: Key) -> Hold {
    match key {
        Key::Up | Key::Down => Hold::Repeat,
        _ => Hold::Once,
    }
}

pub(super) fn content_painter(p: Painter) -> Painter {
    p.translate(WIDTH, TOP).zoomed(Zoom {
        s: SCALE,
        ox: 0.0,
        oy: 0.0,
    })
}
pub(super) fn content_rect(r: Rect) -> Rect {
    Rect {
        x: (WIDTH + r.x as f32 * SCALE).round() as i32,
        y: (TOP + r.y as f32 * SCALE).round() as i32,
        w: (r.w as f32 * SCALE).round() as i32,
        h: (r.h as f32 * SCALE).round() as i32,
    }
}

impl Shell {
    fn navigation_target(&self) -> Option<Navigate> {
        self.top()
            .and_then(|i| self.stack[i].screen.navigation_target())
    }
    pub(super) fn navigation_available(&self) -> bool {
        self.settings.get(keys::LEFT_MENU) != "classic"
            && !self.typing
            && self.navigation_target().is_some()
    }
    pub(super) fn navigation_persistent(&self) -> bool {
        self.settings.get(keys::LEFT_MENU) == "persistent"
    }
    pub(super) fn navigation_visible(&self) -> bool {
        self.navigation_available() && (self.navigation_persistent() || self.navigation.focused)
    }
    fn navigation_items(&self) -> Vec<(&'static str, Navigate)> {
        let mut rows = vec![
            (msg::common_search(), Navigate::Search),
            (msg::nav_epg(), Navigate::Guide),
            (
                msg::common_channels(),
                Navigate::ChannelList { preview: false },
            ),
        ];
        for (kind, section, label) in [
            (VodKind::Movie, VodSection::Movies, msg::vod_movies()),
            (VodKind::Episode, VodSection::Shows, msg::vod_shows()),
        ] {
            if self
                .store
                .vod()
                .iter()
                .any(|v| v.kind == kind && !self.store.vod_locked(v))
            {
                rows.push((label, Navigate::Vod(section)));
            }
        }
        rows.push((msg::channel_history(), Navigate::History));
        rows.push((msg::common_settings(), Navigate::Settings));
        rows
    }
    pub(super) fn select_navigation(&mut self) {
        if let Some(target) = self.navigation_target() {
            if let Some(index) = self
                .navigation_items()
                .iter()
                .position(|(_, nav)| *nav == target)
            {
                self.navigation.index = index;
            }
        }
        self.navigation.focused = true;
        self.dirty = true;
    }
    fn activate_navigation(&mut self, now: u64) {
        let rows = self.navigation_items();
        let Some((_, nav)) = rows.get(self.navigation.index.min(rows.len() - 1)) else {
            return;
        };
        let nav = nav.clone();
        self.navigation.focused = false;
        if self.navigation_target().as_ref() != Some(&nav) {
            self.navigate(nav, None);
        }
        self.dirty = true;
        self.settle(now);
    }
    pub(super) fn navigation_action(&mut self, action: Action, now: u64) -> bool {
        if !self.navigation_available() {
            return false;
        }
        if !self.navigation.focused {
            if action.key == Key::Back {
                self.select_navigation();
                return true;
            }
            return false;
        }
        if action.press == Press::Long {
            return true;
        }
        let len = self.navigation_items().len();
        self.navigation.index = self.navigation.index.min(len - 1);
        match action.key {
            Key::Up => self.navigation.index = self.navigation.index.saturating_sub(1),
            Key::Down => self.navigation.index = (self.navigation.index + 1).min(len - 1),
            Key::Ok | Key::Right => self.activate_navigation(now),
            Key::Back => self.navigation.focused = false,
            _ => {}
        }
        self.dirty = true;
        true
    }
    pub(super) fn navigation_pointer(&mut self, ev: &mut PointerEvent) -> bool {
        if !self.navigation_visible() {
            return false;
        }
        if ev.x < WIDTH {
            if matches!(ev.kind, PointerKind::Move) && !self.pointer.moved(ev.x, ev.y) {
                return true;
            }
            let rows = self.navigation_items();
            let index = ((ev.y - ROW_Y) / ROW_H).floor() as isize;
            let hit = ev.y >= ROW_Y && index >= 0 && (index as usize) < rows.len();
            if hit {
                self.navigation.focused = true;
                self.navigation.index = index as usize;
                self.dirty = true;
                match ev.kind {
                    PointerKind::Up => {
                        if let Some(action) = self.remote.up(Key::Ok, ev.at_ms) {
                            self.navigation_action(action, u64::from(ev.at_ms));
                            self.settle(u64::from(ev.at_ms));
                        }
                    }
                    PointerKind::Down => {
                        let held = self.remote.holds(Key::Ok);
                        if self
                            .pointer
                            .ok_down(super::pointer::By::Button, ev.at_ms, held)
                        {
                            if let Some(action) = self.remote.down(Key::Ok, ev.at_ms, Hold::Once) {
                                self.navigation_action(action, u64::from(ev.at_ms));
                                self.settle(u64::from(ev.at_ms));
                            }
                        }
                    }
                    PointerKind::Wheel { up } => {
                        self.navigation_action(
                            Action {
                                key: if up { Key::Up } else { Key::Down },
                                press: Press::Short,
                            },
                            u64::from(ev.at_ms),
                        );
                    }
                    _ => {}
                }
            }
            return true;
        }
        if matches!(ev.kind, PointerKind::Move | PointerKind::Down)
            && self.pointer.moved(ev.x, ev.y)
        {
            self.navigation.focused = false;
            self.dirty = true;
        }
        if self.navigation_persistent() {
            ev.x = (ev.x - WIDTH) / SCALE;
            ev.y = (ev.y - TOP) / SCALE;
        }
        false
    }
    pub(super) fn paint_navigation(&self, p: Painter) {
        p.rect(
            tv_ui::Rect::new(0.0, 0.0, WIDTH, 1080.0),
            0.0,
            theme::PANEL_RAISED,
            theme::PANEL_RAISED,
            0.0,
        );
        let title = CString::new(msg::player_menu()).unwrap();
        p.text(title.as_ptr(), 28.0, 50.0, 38, theme::TEXT, 0, theme::BOLD);
        let target = self.navigation_target();
        let rows = self.navigation_items();
        let focus = self.navigation.index.min(rows.len() - 1);
        for (i, (label, nav)) in rows.iter().enumerate() {
            let y = ROW_Y + ROW_H * i as f32;
            let focused = self.navigation.focused && i == focus;
            let active = target.as_ref() == Some(nav);
            if focused {
                let fill = [0.88, 0.88, 0.91, 1.0];
                p.rect(
                    tv_ui::Rect::new(12.0, y, WIDTH - 24.0, ROW_H - 12.0),
                    10.0,
                    fill,
                    fill,
                    0.0,
                );
            } else if active {
                let fill = [0.27, 0.27, 0.32, 1.0];
                p.rect(
                    tv_ui::Rect::new(12.0, y, WIDTH - 24.0, ROW_H - 12.0),
                    10.0,
                    fill,
                    fill,
                    0.0,
                );
            }
            let label = CString::new(*label).unwrap();
            let color = if focused {
                [0.04, 0.04, 0.05, 1.0]
            } else {
                theme::TEXT
            };
            p.text(label.as_ptr(), 28.0, y + 22.0, 32, color, 0, theme::REGULAR);
        }
        for (y, hint) in [
            (940.0, msg::settings_left_menu_back()),
            (986.0, msg::settings_left_menu_open()),
        ] {
            let hint = CString::new(hint).unwrap();
            p.text(hint.as_ptr(), 24.0, y, 22, theme::TEXT, 0, theme::REGULAR);
        }
    }
}
