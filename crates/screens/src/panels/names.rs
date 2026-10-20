//! The channel name editor: Settings > Playlists > Channel names, a side panel of its own that
//! slides in over Settings.
//!
//! Its first page lists the two rule lists with how many rules each holds; a list page starts
//! with an Add row, which opens a text entry typed with the TV keyboard (as the URL rows are),
//! and lists the rules, where OK removes one. Every change is a [`Command::SetSetting`] of the
//! whole list ([`crate::names`] has the rules and how they apply).

use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::Painter;

use super::side::{Page, Row, SidePanel};
use super::{PanelKey, TextField};
use crate::data::{Logos, Output, Screen, TvData};
use crate::input::{Action, Hold, Key, Press};
use crate::names::{rules, with_rule, without_rule, End, Refused};
use crate::panels::settings::Settings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// The rules of one end.
    List(End),
    Add(End),
    /// The `usize`th rule of an end: OK removes it.
    Rule(End, usize),
}

pub struct NamesPanel {
    panel: SidePanel<Item>,
    settings: Settings,
    /// The end being added to and the draft, while the entry page is open.
    entry: Option<(End, TextField)>,
}

fn title(end: End) -> &'static str {
    match end {
        End::Prefix => msg::names_prefixes(),
        End::Suffix => msg::names_suffixes(),
    }
}

fn add_label(end: End) -> &'static str {
    match end {
        End::Prefix => msg::names_add_prefix(),
        End::Suffix => msg::names_add_suffix(),
    }
}

impl NamesPanel {
    pub fn new(settings: Settings) -> Self {
        let rows = [End::Prefix, End::Suffix]
            .into_iter()
            .map(|end| Row::submenu(Item::List(end), title(end)).value(&count(&settings, end)))
            .collect();
        NamesPanel {
            panel: SidePanel::new(Page::list(msg::names_title(), rows)),
            settings,
            entry: None,
        }
    }

    pub fn panel(&self) -> &SidePanel<Item> {
        &self.panel
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// `end`'s page, focused on row `focus`.
    fn list_page(&self, end: End, focus: usize) -> Page<Item> {
        let mut rows = vec![Row::action(Item::Add(end), add_label(end))];
        rows.extend(
            rules(&self.settings, end)
                .enumerate()
                .map(|(i, rule)| Row::action(Item::Rule(end, i), rule).value(msg::names_remove())),
        );
        let focus = focus.min(rows.len() - 1);
        Page::list(title(end), rows).focused(focus)
    }

    /// Show `end`'s page again after a change, focused on row `focus`; the first page's counts
    /// follow.
    fn relist(&mut self, end: End, focus: usize) {
        let page = self.list_page(end, focus);
        self.panel.replace(page);
        let settings = &self.settings;
        self.panel.for_each_row(|row| {
            if let Item::List(end) = row.id {
                row.set_value(&count(settings, end));
            }
        });
    }

    fn activate(&mut self, item: Item) -> Output {
        match item {
            Item::List(end) => {
                let page = self.list_page(end, 0);
                self.panel.push(page);
                Output::default()
            }
            Item::Add(end) => {
                let mut field = TextField::new("");
                field.set_editing(true);
                self.panel.push(Page::entry(add_label(end)));
                self.panel.set_field("", true);
                self.entry = Some((end, field));
                Output::default()
            }
            Item::Rule(end, i) => {
                let value = without_rule(&self.settings, end, i);
                let saved = self.settings.change(end.key(), value);
                self.relist(end, i + 1);
                toast(saved, msg::names_removed())
            }
        }
    }

    /// Keys on the entry page: OK raises the keyboard, or adds the rule while it is up; Back
    /// leaves without adding.
    fn entry_key(&mut self, key: PanelKey) -> Output {
        let Some((end, field)) = self.entry.as_mut() else {
            return Output::default();
        };
        let end = *end;
        match key {
            PanelKey::Ok if field.editing() => {
                let refused = match with_rule(&self.settings, end, field.text()) {
                    Ok(value) => {
                        let saved = self.settings.change(end.key(), value);
                        self.close_entry();
                        let last = rules(&self.settings, end).count();
                        self.relist(end, last);
                        return toast(saved, msg::names_added());
                    }
                    Err(Refused::Invalid) => msg::names_invalid(),
                    Err(Refused::Exists) => msg::names_exists(),
                    Err(Refused::Full) => msg::names_full(),
                };
                Output {
                    toast: Some(refused.to_owned()),
                    ..Output::default()
                }
            }
            PanelKey::Ok => {
                field.set_editing(true);
                self.show_entry();
                Output::default()
            }
            // Left is the keyboard's own caret key while it is up, so only Back leaves.
            PanelKey::Back => {
                self.close_entry();
                Output::default()
            }
            PanelKey::Backspace | PanelKey::Clear => {
                field.key(key);
                self.show_entry();
                Output::default()
            }
            _ => Output::default(),
        }
    }

    fn close_entry(&mut self) {
        self.entry = None;
        self.panel.pop();
    }

    fn show_entry(&mut self) {
        if let Some((_, field)) = &self.entry {
            self.panel.set_field(field.text(), field.editing());
        }
    }
}

/// How many rules `end` holds, as its row's value.
fn count(settings: &Settings, end: End) -> String {
    msg::names_count(rules(settings, end).count() as i64)
}

/// The command a change made, with `text` as its toast; nothing when nothing changed.
fn toast(saved: Option<crate::data::Command>, text: &str) -> Output {
    match saved {
        Some(command) => Output {
            toast: Some(text.to_owned()),
            ..Output::command(command)
        },
        None => Output::default(),
    }
}

impl Screen for NamesPanel {
    fn hold(&self, key: Key) -> Hold {
        PanelKey::hold(key)
    }

    fn key(&mut self, key: Key, press: Press, _: &dyn TvData, _: u64) -> Output {
        let Some(key) = PanelKey::from_action(Action { key, press }) else {
            return Output::default();
        };
        if self.entry.is_some() {
            return Output {
                redraw: true,
                ..self.entry_key(key)
            };
        }
        let mut picked = None;
        let out = self.panel.handle(key, |_, item| {
            picked = Some(item);
            Output::default()
        });
        match picked {
            Some(item) => out.and(self.activate(item)),
            None => out,
        }
    }

    fn text(&mut self, text: &str, _: &dyn TvData) -> Output {
        let Some((_, field)) = self.entry.as_mut() else {
            return Output::default();
        };
        field.on_text(text);
        self.show_entry();
        Output::redraw()
    }

    /// The entry is open and typing.
    fn editing(&self) -> bool {
        self.entry.as_ref().is_some_and(|(_, f)| f.editing())
    }

    /// The entry takes the remote's keys alone.
    fn point(&mut self, x: f32, y: f32, _: &dyn TvData, _: u64) -> Output {
        if self.entry.is_some() {
            return Output::default();
        }
        self.panel.point(x, y)
    }

    fn closing(&self) -> bool {
        self.panel.closing()
    }

    fn gone(&self) -> bool {
        self.panel.gone()
    }

    fn update(&mut self, dt: f32) {
        self.panel.update(dt);
    }

    fn draw(&mut self, p: Painter, _: &dyn Measure, _: &dyn Logos) {
        self.panel.draw(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::FakeData;
    use crate::data::Command;
    use crate::panels::keys;
    use crate::panels::testing::{draw, press};

    fn keys(panel: &mut NamesPanel, keys: &[Key]) -> Output {
        press(panel, &FakeData::new(0, 0), keys)
    }

    fn type_text(panel: &mut NamesPanel, text: &str) -> Output {
        panel.text(text, &FakeData::new(0, 0))
    }

    fn labels(panel: &NamesPanel) -> Vec<(String, Option<String>)> {
        let rows = panel.panel().page().rows();
        rows.iter()
            .map(|r| (r.label().to_owned(), r.value_text().map(str::to_owned)))
            .collect()
    }

    fn set(key: &str, value: &str, toast: &str) -> Output {
        Output {
            toast: Some(toast.to_owned()),
            ..Output::command(Command::SetSetting {
                key: key.to_owned(),
                value: value.to_owned(),
            })
        }
    }

    #[test]
    fn a_prefix_is_typed_added_and_removed() {
        let mut panel = NamesPanel::new(Settings::new());
        assert_eq!(panel.panel().page().title(), "Channel names");
        assert_eq!(
            labels(&panel),
            [
                ("Prefixes to remove".to_owned(), Some("0 rules".to_owned())),
                ("Suffixes to remove".to_owned(), Some("0 rules".to_owned())),
            ]
        );
        keys(&mut panel, &[Key::Ok]);
        assert_eq!(labels(&panel), [("Add prefix".to_owned(), None)]);
        keys(&mut panel, &[Key::Ok]);
        assert!(panel.editing(), "the entry raises the keyboard");
        assert_eq!(type_text(&mut panel, " PL: "), Output::redraw());
        assert_eq!(panel.panel().page().field(), Some(" PL: "));
        assert_eq!(
            keys(&mut panel, &[Key::Ok]),
            set(keys::NAMES_PREFIXES, "PL:", "Added")
        );
        assert!(!panel.editing());
        assert_eq!(
            labels(&panel),
            [
                ("Add prefix".to_owned(), None),
                ("PL:".to_owned(), Some("Remove".to_owned()))
            ]
        );
        assert_eq!(panel.panel().page().focus(), 1, "on the new rule");
        draw(&mut panel);
        // Another, then remove the first.
        keys(&mut panel, &[Key::Up, Key::Ok]);
        type_text(&mut panel, "UK |");
        assert_eq!(
            keys(&mut panel, &[Key::Ok]),
            set(keys::NAMES_PREFIXES, "PL:\nUK |", "Added")
        );
        assert_eq!(
            keys(&mut panel, &[Key::Up, Key::Ok]),
            set(keys::NAMES_PREFIXES, "UK |", "Removed")
        );
        assert_eq!(panel.settings().get(keys::NAMES_PREFIXES), "UK |");
        keys(&mut panel, &[Key::Back]);
        assert_eq!(
            labels(&panel)[0].1.as_deref(),
            Some("1 rule"),
            "the count follows"
        );
        assert_eq!(keys(&mut panel, &[Key::Back]), Output::close());
        assert!(panel.closing());
    }

    #[test]
    fn a_bad_or_repeated_rule_is_refused_and_back_drops_the_draft() {
        let stored = Settings::from_stored([(keys::NAMES_SUFFIXES, "HD")]);
        let mut panel = NamesPanel::new(stored);
        keys(&mut panel, &[Key::Down, Key::Ok, Key::Ok]);
        type_text(&mut panel, "hd");
        let out = keys(&mut panel, &[Key::Ok]);
        assert_eq!(
            (out.toast.as_deref(), out.commands.len()),
            (Some("Already on the list"), 0)
        );
        assert!(panel.editing(), "the entry stays for another try");
        keys(&mut panel, &[Key::Clear]);
        let out = keys(&mut panel, &[Key::Ok]);
        assert_eq!(out.toast.as_deref(), Some("Type 1 to 32 characters"));
        type_text(&mut panel, "FHD");
        assert_eq!(keys(&mut panel, &[Key::Back]), Output::default());
        assert!(!panel.editing());
        assert_eq!(panel.settings().get(keys::NAMES_SUFFIXES), "HD");
        assert_eq!(panel.panel().page().title(), "Suffixes to remove");
        assert_eq!(
            type_text(&mut panel, "x"),
            Output::default(),
            "no entry open"
        );
    }

    #[test]
    fn the_editor_reads_in_polish() {
        let _pl =
            tv_platform::i18n::language_on_this_thread_for_test(tv_platform::i18n::Preference::Pl);
        let stored = Settings::from_stored([
            (keys::NAMES_PREFIXES, "PL:"),
            (keys::NAMES_SUFFIXES, "HD\nFHD\n4K"),
        ]);
        let panel = NamesPanel::new(stored);
        assert_eq!(panel.panel().page().title(), "Nazwy kana\u{142}\u{f3}w");
        let values: Vec<_> = labels(&panel).into_iter().filter_map(|(_, v)| v).collect();
        assert_eq!(values, ["1 regu\u{142}a", "3 regu\u{142}y"]);
        assert_eq!(count(&Settings::new(), End::Prefix), "0 regu\u{142}");
    }
}
