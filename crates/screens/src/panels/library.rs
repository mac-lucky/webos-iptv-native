//! The library panel: the playlists, from Settings, and the viewer's
//! groups, from a group's long-OK menu. A side panel of its own that slides in over what opened
//! it.
//!
//! | Page | Rows |
//! |---|---|
//! | Playlists | one per playlist (its channel count, or Off), Add playlist |
//! | a playlist | Enabled, Playlist URL, Move up, Move down, Remove playlist |
//! | Manage groups | New group, Reorder groups, then every group: OK shows or hides it, a custom one opens its page |
//! | a custom group | Manage channels, Rename group, Delete group |
//! | Manage channels | Copy to group, Move to group, Remove from group, Reorder channels, then every channel: OK selects it |
//! | Reorder | OK picks the focused row up and drops it, Up and Down carry it, Back keeps the order |
//!
//! Every change is a [`Command::Library`]; playlist 1's URL is the Settings URL
//! ([`keys::PLAYLIST_URL`]), so changing or removing it is a [`Command::SetSetting`] of that. The
//! panel shows a change at once and builds the page in front again from the data whenever the
//! shell refreshes it; a page under it is built again when Back shows it. Playlist URLs carry
//! tokens: they are shown masked ([`redact::url`]) and registered for masking as they are typed.

use std::collections::HashSet;

use iptv::model::ChannelKey;
use iptv::redact;
use iptv::storage::{MAIN_PLAYLIST, MAX_GROUP_NAME, MAX_PLAYLISTS};
use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::Painter;

use super::menu::group_hidden;
use super::settings::{is_http_url, keys, Settings};
use super::side::{Page, Row, SidePanel};
use super::{PanelCtx, PanelKey, TextField};
use crate::data::{Command, Group, Logos, Output, Screen, TvData};
use crate::input::{Action, Hold, Key, Press};
use crate::library::{LibraryEdit, LibraryPage, PlaylistEntry, SecretUrl};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// A playlist's page, by its storage row.
    Playlist(i64),
    AddPlaylist,
    Enabled(i64),
    Url(i64),
    MoveUp(i64),
    MoveDown(i64),
    Remove(i64),
    ConfirmRemove(i64),
    /// Leave a confirmation page.
    Cancel,
    NewGroup,
    ReorderGroups,
    /// The `usize`th group of Manage groups.
    Group(usize),
    /// A custom group page's rows.
    Channels,
    Rename,
    Delete,
    ConfirmDelete,
    CopyTo,
    MoveTo,
    RemoveSelected,
    ReorderChannels,
    /// The `usize`th channel of Manage channels: OK selects it.
    Channel(usize),
    /// The `usize`th custom group a copy or a move can go to.
    Target(usize),
    NewTarget,
    /// The `usize`th row of a reorder page.
    Order(usize),
}

/// What a page of the panel shows, so it can be built again from fresh data.
#[derive(Clone, Debug, PartialEq, Eq)]
enum View {
    Playlists,
    Playlist(i64),
    ConfirmRemove(i64),
    Groups,
    CustomGroup(String),
    ConfirmDelete(String),
    Channels(Group),
    Targets { moving: bool },
    Reorder(Reorder),
    Entry(EntryFor),
}

/// What a text entry page is typed for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EntryFor {
    AddPlaylist,
    PlaylistUrl(i64),
    NewGroup,
    /// A new group the selected channels are copied or moved into.
    NewTarget {
        moving: bool,
    },
    Rename(String),
}

impl EntryFor {
    fn is_url(&self) -> bool {
        matches!(self, EntryFor::AddPlaylist | EntryFor::PlaylistUrl(_))
    }
}

/// A reorder page: its rows in the order being made.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Reorder {
    /// The group whose channels these are; `None` when they are groups.
    group: Option<Group>,
    items: Vec<Placed>,
    focus: usize,
    /// The row being carried.
    picked: Option<usize>,
    changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Placed {
    Group(Group),
    Channel(ChannelKey, String),
}

impl Placed {
    fn label(&self) -> &str {
        match self {
            Placed::Group(g) => g.name(),
            Placed::Channel(_, name) => name,
        }
    }
}

pub struct LibraryPanel {
    panel: SidePanel<Item>,
    state: State,
}

/// What the rows act on; `views` runs parallel to the panel's pages.
struct State {
    settings: Settings,
    playlists: Vec<PlaylistEntry>,
    /// Every group, as Manage groups lists them.
    groups: Vec<Group>,
    /// The channels of the group Manage channels shows, in its order.
    channels: Vec<(ChannelKey, String)>,
    selected: HashSet<ChannelKey>,
    views: Vec<View>,
    /// The draft, while an entry page is open.
    entry: Option<TextField>,
}

/// Groups the viewer can put in order: the built-in ones always lead.
fn reorderable(group: &Group) -> bool {
    matches!(group, Group::Playlist(_) | Group::Custom(_))
}

/// `group` is the custom group `name`.
fn is_custom(group: &Group, name: &str) -> bool {
    matches!(group, Group::Custom(n) if n == name)
}

fn on_off(on: bool) -> &'static str {
    if on {
        msg::widgets_toggle_on()
    } else {
        msg::widgets_toggle_off()
    }
}

fn with_toast(text: impl Into<String>, out: Output) -> Output {
    Output {
        toast: Some(text.into()),
        ..out
    }
}

fn library(edit: LibraryEdit) -> Output {
    Output::command(Command::Library(edit))
}

/// The setting that shows or hides `group`, and its new value; none for a custom group, which
/// is always shown.
fn visibility(group: &Group, settings: &Settings) -> Option<(&'static str, String)> {
    let hidden = group_hidden(group, settings);
    Some(match group {
        Group::Favorites => (keys::GROUPS_SHOW_FAVORITES, hidden.to_string()),
        Group::All => (keys::GROUPS_SHOW_ALL, hidden.to_string()),
        Group::History => (keys::GROUPS_SHOW_HISTORY, hidden.to_string()),
        Group::Playlist(name) => (keys::GROUPS_HIDDEN, settings.hidden_groups_toggled(name)),
        Group::Custom(_) => return None,
    })
}

impl LibraryPanel {
    /// The panel on `page`; `None` for a custom group that is not there.
    pub fn open(page: &LibraryPage, cx: &PanelCtx) -> Option<Self> {
        let mut state = State {
            settings: cx.settings.clone(),
            playlists: cx.data.playlists(),
            groups: cx.data.all_groups(),
            channels: Vec::new(),
            selected: HashSet::new(),
            views: Vec::new(),
            entry: None,
        };
        let view = match page {
            LibraryPage::Playlists => View::Playlists,
            LibraryPage::Groups => View::Groups,
            LibraryPage::Channels(group) => {
                state.load_channels(group, cx.data);
                View::Channels(group.clone())
            }
            LibraryPage::CustomGroup(name) => View::CustomGroup(name.clone()),
        };
        let root = state.page(&view)?;
        state.views.push(view);
        Some(LibraryPanel {
            panel: SidePanel::new(root),
            state,
        })
    }

    pub fn panel(&self) -> &SidePanel<Item> {
        &self.panel
    }

    pub fn settings(&self) -> &Settings {
        &self.state.settings
    }
}

impl Screen for LibraryPanel {
    fn hold(&self, key: Key) -> Hold {
        PanelKey::hold(key)
    }

    fn key(&mut self, key: Key, press: Press, data: &dyn TvData, _: u64) -> Output {
        let Some(key) = PanelKey::from_action(Action { key, press }) else {
            return Output::default();
        };
        if self.panel.closing() {
            return Output::default();
        }
        let state = &mut self.state;
        let out = match state.views.last() {
            Some(View::Entry(_)) => Output {
                redraw: true,
                ..state.entry_key(&mut self.panel, key)
            },
            Some(View::Reorder(_)) => Output {
                redraw: true,
                ..state.reorder_key(&mut self.panel, key)
            },
            _ => self
                .panel
                .handle(key, |panel, item| state.activate(panel, item, data)),
        };
        // Back left a page: the one under it shows again, as the data is now.
        if self.panel.depth() < self.state.views.len() {
            self.state.views.truncate(self.panel.depth());
            if !self.state.refresh_top(&mut self.panel) {
                self.panel.hide();
                return Output::close();
            }
        }
        out
    }

    fn text(&mut self, text: &str, _: &dyn TvData) -> Output {
        let Some(field) = self.state.entry.as_mut() else {
            return Output::default();
        };
        field.on_text(text);
        self.state.show_entry(&mut self.panel);
        Output::redraw()
    }

    /// An entry page is open and typing.
    fn editing(&self) -> bool {
        self.state.entry.as_ref().is_some_and(TextField::editing)
    }

    fn refresh(&mut self, data: &dyn TvData) {
        let s = &mut self.state;
        s.playlists = data.playlists();
        s.groups = data.all_groups();
        if let Some(group) = s.channels_group().cloned() {
            s.load_channels(&group, data);
        }
        if !s.refresh_top(&mut self.panel) {
            self.panel.hide();
        }
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

impl State {
    /// Read `group`'s channels; the selection keeps those still there.
    fn load_channels(&mut self, group: &Group, data: &dyn TvData) {
        self.channels = data
            .channels(group)
            .iter()
            .map(|c| (c.key, c.name.clone()))
            .collect();
        let listed: HashSet<ChannelKey> = self.channels.iter().map(|(k, _)| *k).collect();
        self.selected.retain(|k| listed.contains(k));
    }

    /// The custom groups a copy or a move from the group Manage channels shows can go to.
    fn targets(&self) -> Vec<&str> {
        let from = match self.channels_group() {
            Some(Group::Custom(name)) => Some(name.as_str()),
            _ => None,
        };
        self.groups
            .iter()
            .filter_map(|g| match g {
                Group::Custom(name) if Some(name.as_str()) != from => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Custom group `name` is there.
    fn has_custom(&self, name: &str) -> bool {
        self.groups.iter().any(|g| is_custom(g, name))
    }

    /// The group Manage channels shows.
    fn channels_group(&self) -> Option<&Group> {
        self.views.iter().find_map(|v| match v {
            View::Channels(group) => Some(group),
            _ => None,
        })
    }

    /// The custom group whose page is open.
    fn custom_group(&self) -> Option<&str> {
        self.views.iter().rev().find_map(|v| match v {
            View::CustomGroup(name) => Some(name.as_str()),
            _ => None,
        })
    }

    /// The selected channels, in list order.
    fn selection(&self) -> Vec<ChannelKey> {
        self.channels
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| self.selected.contains(k))
            .collect()
    }

    /// `view` as a page, from the data as it is; `None` when what it shows is gone (and for an
    /// entry page, which is never built again).
    fn page(&self, view: &View) -> Option<Page<Item>> {
        Some(match view {
            View::Playlists => {
                let mut rows: Vec<Row<Item>> = self
                    .playlists
                    .iter()
                    .map(|p| {
                        let value = if p.enabled {
                            msg::library_channel_count(p.channels as i64)
                        } else {
                            on_off(false).to_owned()
                        };
                        Row::submenu(Item::Playlist(p.id), &p.name).value(&value)
                    })
                    .collect();
                if self.playlists.len() < MAX_PLAYLISTS {
                    rows.push(Row::action(Item::AddPlaylist, msg::library_add_playlist()));
                }
                Page::list(msg::settings_playlists(), rows)
            }
            View::Playlist(id) => {
                let at = self.playlists.iter().position(|p| p.id == *id)?;
                let p = &self.playlists[at];
                let mut rows = vec![
                    Row::action(Item::Enabled(p.id), msg::library_enabled())
                        .value(on_off(p.enabled)),
                    Row::action(Item::Url(p.id), msg::settings_playlist_url())
                        .value(&redact::url(&p.url.0))
                        .chevron(),
                ];
                if at > 0 {
                    rows.push(Row::action(Item::MoveUp(p.id), msg::library_move_up()));
                }
                if at + 1 < self.playlists.len() {
                    rows.push(Row::action(Item::MoveDown(p.id), msg::library_move_down()));
                }
                rows.push(Row::submenu(
                    Item::Remove(p.id),
                    msg::library_remove_playlist(),
                ));
                Page::list(&p.name, rows)
            }
            View::ConfirmRemove(id) => {
                self.playlists.iter().find(|p| p.id == *id)?;
                Page::list(
                    msg::library_remove_question(),
                    vec![
                        Row::action(Item::ConfirmRemove(*id), msg::library_remove_playlist()),
                        Row::action(Item::Cancel, msg::library_cancel()),
                    ],
                )
            }
            View::Groups => {
                let mut rows = vec![Row::action(Item::NewGroup, msg::library_new_group())];
                if self.groups.iter().filter(|g| reorderable(g)).count() > 1 {
                    rows.push(Row::submenu(
                        Item::ReorderGroups,
                        msg::library_reorder_groups(),
                    ));
                }
                rows.extend(self.groups.iter().enumerate().map(|(i, g)| match g {
                    Group::Custom(_) => Row::submenu(Item::Group(i), g.name()),
                    _ => Row::check(Item::Group(i), g.name(), !group_hidden(g, &self.settings)),
                }));
                Page::list(msg::library_groups(), rows)
            }
            View::CustomGroup(name) => {
                self.has_custom(name).then_some(())?;
                Page::list(
                    name,
                    vec![
                        Row::submenu(Item::Channels, msg::library_channels()),
                        Row::action(Item::Rename, msg::library_rename_group()),
                        Row::submenu(Item::Delete, msg::library_delete_group()),
                    ],
                )
            }
            View::ConfirmDelete(name) => {
                self.has_custom(name).then_some(())?;
                Page::list(
                    msg::library_delete_question(),
                    vec![
                        Row::action(Item::ConfirmDelete, msg::library_delete_group()),
                        Row::action(Item::Cancel, msg::library_cancel()),
                    ],
                )
            }
            View::Channels(group) => {
                let picked = self.selected.len();
                let count = if picked > 0 {
                    msg::library_selected(picked as i64)
                } else {
                    String::new()
                };
                let mut rows =
                    vec![Row::submenu(Item::CopyTo, msg::library_copy_to()).value(&count)];
                let own = reorderable(group);
                if own {
                    rows.push(Row::submenu(Item::MoveTo, msg::library_move_to()));
                }
                if matches!(group, Group::Custom(_)) {
                    rows.push(Row::action(
                        Item::RemoveSelected,
                        msg::library_remove_from_group(),
                    ));
                }
                if own && self.channels.len() > 1 {
                    rows.push(Row::submenu(
                        Item::ReorderChannels,
                        msg::library_reorder_channels(),
                    ));
                }
                rows.extend(self.channels.iter().enumerate().map(|(i, (key, name))| {
                    Row::check(Item::Channel(i), name, self.selected.contains(key))
                }));
                Page::list(group.name(), rows)
            }
            View::Targets { moving } => {
                let mut rows: Vec<Row<Item>> = self
                    .targets()
                    .into_iter()
                    .enumerate()
                    .map(|(i, name)| Row::action(Item::Target(i), name))
                    .collect();
                rows.push(Row::action(Item::NewTarget, msg::library_new_group()));
                let title = if *moving {
                    msg::library_move_to()
                } else {
                    msg::library_copy_to()
                };
                Page::list(title, rows)
            }
            View::Reorder(order) => {
                let rows = order
                    .items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let row = Row::action(Item::Order(i), item.label());
                        match order.picked {
                            Some(at) if at == i => row.value(msg::library_move_here()),
                            None if i == order.focus => row.value(msg::library_pick_up()),
                            _ => row,
                        }
                    })
                    .collect();
                let title = if order.group.is_some() {
                    msg::library_reorder_channels()
                } else {
                    msg::library_reorder_groups()
                };
                Page::list(title, rows).focused(order.focus)
            }
            View::Entry(_) => return None,
        })
    }

    /// Build the page in front again from the data, keeping its focus; pages whose subject is
    /// gone are left. False when the first page's subject is gone: the panel closes.
    fn refresh_top(&mut self, panel: &mut SidePanel<Item>) -> bool {
        loop {
            let Some(view) = self.views.last() else {
                return false;
            };
            if matches!(view, View::Entry(_)) {
                return true;
            }
            let focus = panel.page().focus();
            match self.page(view) {
                Some(page) => {
                    let last = page.rows().len().saturating_sub(1);
                    let focus = match view {
                        View::Reorder(order) => order.focus,
                        _ => focus.min(last),
                    };
                    panel.replace(page.focused(focus));
                    return true;
                }
                None => {
                    if !panel.pop() {
                        return false;
                    }
                    self.views.pop();
                }
            }
        }
    }

    /// Open `view` as a new page.
    fn push(&mut self, panel: &mut SidePanel<Item>, view: View) {
        if let Some(page) = self.page(&view) {
            panel.push(page);
            self.views.push(view);
        }
    }

    /// Leave `pages` pages, then show the one in front as the data is now.
    fn leave(&mut self, panel: &mut SidePanel<Item>, pages: usize) {
        for _ in 0..pages {
            if panel.pop() {
                self.views.pop();
            }
        }
        self.refresh_top(panel);
    }

    fn activate(&mut self, panel: &mut SidePanel<Item>, item: Item, data: &dyn TvData) -> Output {
        match item {
            Item::Playlist(id) => {
                self.push(panel, View::Playlist(id));
                Output::default()
            }
            Item::AddPlaylist => {
                self.open_entry(panel, EntryFor::AddPlaylist, "");
                Output::default()
            }
            Item::Enabled(id) => {
                let Some(p) = self.playlists.iter_mut().find(|p| p.id == id) else {
                    return Output::default();
                };
                p.enabled = !p.enabled;
                let enabled = p.enabled;
                self.refresh_top(panel);
                library(LibraryEdit::SetPlaylistEnabled {
                    playlist: id,
                    enabled,
                })
            }
            Item::Url(id) => {
                let url = self
                    .playlists
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.url.0.clone())
                    .unwrap_or_default();
                self.open_entry(panel, EntryFor::PlaylistUrl(id), &url);
                Output::default()
            }
            Item::MoveUp(id) | Item::MoveDown(id) => {
                let up = matches!(item, Item::MoveUp(_));
                let Some(at) = self.playlists.iter().position(|p| p.id == id) else {
                    return Output::default();
                };
                let to = if up { at.checked_sub(1) } else { Some(at + 1) };
                let Some(to) = to.filter(|&to| to < self.playlists.len()) else {
                    return Output::default();
                };
                self.playlists.swap(at, to);
                self.refresh_top(panel);
                library(LibraryEdit::MovePlaylist { playlist: id, up })
            }
            Item::Remove(id) => {
                self.push(panel, View::ConfirmRemove(id));
                Output::default()
            }
            Item::ConfirmRemove(id) => {
                self.playlists.retain(|p| p.id != id);
                // Playlist 1 is the Settings URL's: clearing it keeps the two in step.
                let cleared = (id == MAIN_PLAYLIST)
                    .then(|| self.settings.change(keys::PLAYLIST_URL, String::new()))
                    .flatten();
                self.leave(panel, 2);
                let out = cleared
                    .map_or_else(Output::default, Output::command)
                    .and(library(LibraryEdit::RemovePlaylist(id)));
                with_toast(msg::library_playlist_removed(), out)
            }
            Item::Cancel => {
                self.leave(panel, 1);
                Output::default()
            }
            Item::NewGroup => {
                self.open_entry(panel, EntryFor::NewGroup, "");
                Output::default()
            }
            Item::ReorderGroups => {
                let items = self
                    .groups
                    .iter()
                    .filter(|g| reorderable(g))
                    .map(|g| Placed::Group(g.clone()))
                    .collect();
                self.open_reorder(panel, None, items);
                Output::default()
            }
            Item::Group(i) => {
                let Some(group) = self.groups.get(i).cloned() else {
                    return Output::default();
                };
                if let Group::Custom(name) = group {
                    self.push(panel, View::CustomGroup(name));
                    return Output::default();
                }
                let saved = visibility(&group, &self.settings)
                    .and_then(|(key, value)| self.settings.change(key, value));
                self.refresh_top(panel);
                saved.map_or_else(Output::default, Output::command)
            }
            Item::Channels => {
                let Some(name) = self.custom_group().map(str::to_owned) else {
                    return Output::default();
                };
                let group = Group::Custom(name);
                self.selected.clear();
                self.load_channels(&group, data);
                self.push(panel, View::Channels(group));
                Output::default()
            }
            Item::Rename => {
                let Some(name) = self.custom_group().map(str::to_owned) else {
                    return Output::default();
                };
                self.open_entry(panel, EntryFor::Rename(name.clone()), &name);
                Output::default()
            }
            Item::Delete => {
                if let Some(name) = self.custom_group().map(str::to_owned) {
                    self.push(panel, View::ConfirmDelete(name));
                }
                Output::default()
            }
            Item::ConfirmDelete => {
                let Some(name) = self.custom_group().map(str::to_owned) else {
                    return Output::default();
                };
                self.groups.retain(|g| !is_custom(g, &name));
                let out = with_toast(
                    msg::library_group_deleted(),
                    library(LibraryEdit::DeleteGroup(name)),
                );
                // The confirmation and the group's page go; opened on the group, the panel too.
                self.leave(panel, 2);
                if self.views.len() == 1 && matches!(self.views[0], View::CustomGroup(_)) {
                    return out.and(Output::close());
                }
                out
            }
            Item::CopyTo | Item::MoveTo => {
                if self.selected.is_empty() {
                    return with_toast(msg::library_select_first(), Output::default());
                }
                let moving = item == Item::MoveTo;
                self.push(panel, View::Targets { moving });
                Output::default()
            }
            Item::RemoveSelected => {
                let Some(Group::Custom(name)) = self.channels_group().cloned() else {
                    return Output::default();
                };
                let channels = self.selection();
                if channels.is_empty() {
                    return with_toast(msg::library_select_first(), Output::default());
                }
                self.channels.retain(|(k, _)| !self.selected.contains(k));
                self.selected.clear();
                self.refresh_top(panel);
                with_toast(
                    msg::library_removed(channels.len() as i64),
                    library(LibraryEdit::RemoveFromGroup {
                        group: name,
                        channels,
                    }),
                )
            }
            Item::ReorderChannels => {
                let group = self.channels_group().cloned();
                let items = self
                    .channels
                    .iter()
                    .map(|(k, name)| Placed::Channel(*k, name.clone()))
                    .collect();
                self.open_reorder(panel, group, items);
                Output::default()
            }
            Item::Channel(i) => {
                if let Some((key, _)) = self.channels.get(i) {
                    if !self.selected.remove(key) {
                        self.selected.insert(*key);
                    }
                    self.refresh_top(panel);
                }
                Output::default()
            }
            Item::Target(i) => {
                let Some(to) = self.targets().get(i).map(|s| (*s).to_owned()) else {
                    return Output::default();
                };
                let moving = matches!(self.views.last(), Some(View::Targets { moving: true }));
                let out = self.put_selection(&to, moving);
                self.leave(panel, 1);
                out
            }
            Item::NewTarget => {
                let moving = matches!(self.views.last(), Some(View::Targets { moving: true }));
                self.open_entry(panel, EntryFor::NewTarget { moving }, "");
                Output::default()
            }
            Item::Order(_) => Output::default(),
        }
    }

    /// Copy or move the selection into custom group `to`, which shows at once: moved channels
    /// leave the list.
    fn put_selection(&mut self, to: &str, moving: bool) -> Output {
        let channels = self.selection();
        let from = self.channels_group().filter(|_| moving).cloned();
        if moving {
            self.channels.retain(|(k, _)| !self.selected.contains(k));
        }
        self.selected.clear();
        let n = channels.len() as i64;
        let toast = if moving {
            msg::library_moved(n, to)
        } else {
            msg::library_copied(n, to)
        };
        with_toast(
            toast,
            library(LibraryEdit::AddToGroup {
                to: to.to_owned(),
                channels,
                from,
            }),
        )
    }

    fn open_reorder(
        &mut self,
        panel: &mut SidePanel<Item>,
        group: Option<Group>,
        items: Vec<Placed>,
    ) {
        let order = Reorder {
            group,
            items,
            focus: 0,
            picked: None,
            changed: false,
        };
        self.push(panel, View::Reorder(order));
    }

    /// Keys on a reorder page.
    fn reorder_key(&mut self, panel: &mut SidePanel<Item>, key: PanelKey) -> Output {
        let Some(View::Reorder(order)) = self.views.last_mut() else {
            return Output::default();
        };
        let last = order.items.len().saturating_sub(1);
        match key {
            PanelKey::Up | PanelKey::Down => {
                let to = if key == PanelKey::Up {
                    order.focus.checked_sub(1)
                } else {
                    Some(order.focus + 1).filter(|&to| to <= last)
                };
                let Some(to) = to else {
                    return Output::default();
                };
                if order.picked.is_some() {
                    order.items.swap(order.focus, to);
                    order.picked = Some(to);
                    order.changed = true;
                }
                order.focus = to;
            }
            PanelKey::Ok => {
                order.picked = match order.picked {
                    Some(_) => None,
                    None => Some(order.focus),
                };
            }
            PanelKey::Back | PanelKey::Left => {
                // The page goes: its order moves out rather than being copied.
                let order = std::mem::take(order);
                let out = self.keep_order(order);
                self.leave(panel, 1);
                return out;
            }
            _ => return Output::default(),
        }
        self.refresh_top(panel);
        Output::default()
    }

    /// The order a reorder page made, saved and shown; nothing when it did not change.
    fn keep_order(&mut self, order: Reorder) -> Output {
        if !order.changed {
            return Output::default();
        }
        match order.group {
            None => {
                let placed: Vec<Group> = order
                    .items
                    .into_iter()
                    .filter_map(|item| match item {
                        Placed::Group(g) => Some(g),
                        Placed::Channel(..) => None,
                    })
                    .collect();
                self.groups.retain(|g| !reorderable(g));
                self.groups.extend(placed.iter().cloned());
                library(LibraryEdit::SetGroupOrder(placed))
            }
            Some(group) => {
                self.channels = order
                    .items
                    .into_iter()
                    .filter_map(|item| match item {
                        Placed::Channel(k, name) => Some((k, name)),
                        Placed::Group(_) => None,
                    })
                    .collect();
                let channels = self.channels.iter().map(|(k, _)| *k).collect();
                // A manual order shows under the playlist sorting.
                let sorting = self
                    .settings
                    .change(keys::GROUPS_CHANNEL_SORT, "playlist".to_owned());
                library(LibraryEdit::SetChannelOrder { group, channels })
                    .and(sorting.map_or_else(Output::default, Output::command))
            }
        }
    }

    fn open_entry(&mut self, panel: &mut SidePanel<Item>, purpose: EntryFor, text: &str) {
        let title = match &purpose {
            EntryFor::AddPlaylist => msg::library_add_playlist(),
            EntryFor::PlaylistUrl(_) => msg::settings_playlist_url(),
            EntryFor::NewGroup | EntryFor::NewTarget { .. } | EntryFor::Rename(_) => {
                msg::library_group_name()
            }
        };
        let mut field = TextField::new(text);
        field.set_editing(true);
        panel.push(Page::entry(title));
        self.views.push(View::Entry(purpose));
        self.entry = Some(field);
        self.show_entry(panel);
    }

    /// The field shows a URL masked, like every other place one appears.
    fn show_entry(&self, panel: &mut SidePanel<Item>) {
        let (Some(field), Some(View::Entry(purpose))) = (&self.entry, self.views.last()) else {
            return;
        };
        let text = field.text();
        let shown = if purpose.is_url() && !text.is_empty() {
            redact::url(text)
        } else {
            text.to_owned()
        };
        panel.set_field(&shown, field.editing());
    }

    fn close_entry(&mut self, panel: &mut SidePanel<Item>) {
        self.entry = None;
        self.leave(panel, 1);
    }

    /// Keys on an entry page: OK raises the keyboard, or saves while it is up; Back leaves
    /// without saving.
    fn entry_key(&mut self, panel: &mut SidePanel<Item>, key: PanelKey) -> Output {
        let Some(field) = self.entry.as_mut() else {
            return Output::default();
        };
        match key {
            PanelKey::Ok if field.editing() => self.save_entry(panel),
            PanelKey::Ok => {
                field.set_editing(true);
                self.show_entry(panel);
                Output::default()
            }
            // Left is the keyboard's own caret key while it is up, so only Back leaves.
            PanelKey::Back => {
                self.close_entry(panel);
                Output::default()
            }
            PanelKey::Backspace | PanelKey::Clear => {
                field.key(key);
                self.show_entry(panel);
                Output::default()
            }
            _ => Output::default(),
        }
    }

    fn save_entry(&mut self, panel: &mut SidePanel<Item>) -> Output {
        let (Some(field), Some(View::Entry(purpose))) = (&self.entry, self.views.last()) else {
            return Output::default();
        };
        let purpose = purpose.clone();
        if purpose.is_url() {
            let url = field.text().trim().to_owned();
            if !is_http_url(&url) {
                return with_toast(msg::settings_bad_url(), Output::default());
            }
            redact::register_secrets(&url);
            let out = self.save_url(purpose, url);
            self.close_entry(panel);
            return out;
        }
        let name: String = field.text().trim().chars().take(MAX_GROUP_NAME).collect();
        let name = name.trim_end().to_owned();
        if name.is_empty() {
            return Output::default();
        }
        let renaming = match &purpose {
            EntryFor::Rename(from) => Some(from.as_str()),
            _ => None,
        };
        let taken = self
            .groups
            .iter()
            .any(|g| reorderable(g) && g.name() == name && renaming != Some(name.as_str()));
        if taken {
            return with_toast(msg::library_name_taken(), Output::default());
        }
        self.close_entry(panel);
        match purpose {
            EntryFor::Rename(from) if from == name => Output::default(),
            EntryFor::Rename(from) => {
                for g in &mut self.groups {
                    if is_custom(g, &from) {
                        *g = Group::Custom(name.clone());
                    }
                }
                for view in &mut self.views {
                    if matches!(view, View::CustomGroup(n) if *n == from) {
                        *view = View::CustomGroup(name.clone());
                    }
                }
                self.refresh_top(panel);
                with_toast(
                    msg::library_group_renamed(),
                    library(LibraryEdit::RenameGroup { from, to: name }),
                )
            }
            EntryFor::NewTarget { moving } => {
                let created = library(LibraryEdit::CreateGroup(name.clone()));
                self.groups.push(Group::Custom(name.clone()));
                let put = self.put_selection(&name, moving);
                // Back past the targets, to the channels.
                self.leave(panel, 1);
                created.and(put)
            }
            _ => {
                self.groups.push(Group::Custom(name.clone()));
                self.refresh_top(panel);
                with_toast(
                    msg::library_group_created(),
                    library(LibraryEdit::CreateGroup(name)),
                )
            }
        }
    }

    /// A typed playlist URL: a new playlist, or playlist `id`'s new URL. Playlist 1 is the
    /// Settings URL's, so its URL, and a playlist added while there is none, go there.
    fn save_url(&mut self, purpose: EntryFor, url: String) -> Output {
        let id = match purpose {
            EntryFor::PlaylistUrl(id) => id,
            _ if self.playlists.iter().any(|p| p.id == MAIN_PLAYLIST) => {
                return with_toast(
                    msg::library_playlist_added(),
                    library(LibraryEdit::AddPlaylist(SecretUrl(url))),
                );
            }
            _ => MAIN_PLAYLIST,
        };
        if let Some(p) = self.playlists.iter_mut().find(|p| p.id == id) {
            p.url = SecretUrl(url.clone());
        }
        let out = if id == MAIN_PLAYLIST {
            self.settings
                .change(keys::PLAYLIST_URL, url)
                .map_or_else(Output::default, Output::command)
        } else {
            library(LibraryEdit::SetPlaylistUrl {
                playlist: id,
                url: SecretUrl(url),
            })
        };
        if matches!(purpose, EntryFor::AddPlaylist) {
            with_toast(msg::library_playlist_added(), out)
        } else {
            out
        }
    }
}

#[cfg(test)]
mod tests;
