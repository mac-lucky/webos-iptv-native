//! The menus on the side panel: the long-OK menu for a channel (with the focused programme, if
//! any) or a group, the player menu, and the main menu on the left edge.

use iptv::catchup::in_archive;
use iptv::model::{Channel, ChannelKey, Programme};
use iptv::vod::VodKind;
use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::Painter;

use super::settings::{keys, Kind, Label, Settings};
use super::side::{Edge, Page, Row, SidePanel};
use super::{PanelCtx, PanelKey};
use crate::data::VodSection;
use crate::data::{Command, Group, Logos, Navigate, Output, Screen, ScreenMessage, Stream, TvData};
use crate::input::{Action, Hold, Key, Press};
use crate::library::LibraryPage;
use crate::parental::{self, PinTask};
use crate::playback::{
    audio_offset, offset_label, sleep_label, sleep_left_label, with_channel_offset, SLEEP_MINUTES,
};
use crate::reminders::{self, Toggled};
use crate::time::{hhmm, weekday};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuItem {
    /// Add to or remove from favourites.
    Favorite,
    Description,
    WatchFromStart,
    GroupOptions,
    HideGroup,
    ChannelSorting,
    /// One option of the channel sorting page.
    Sort(&'static str),
    Search,
    Settings,
    // main menu
    Guide,
    Channels,
    History,
    /// The Movies and Shows sections, listed while the playlists carry VOD.
    Movies,
    Shows,
    // player menu
    AudioTrack,
    /// One track of the audio track page.
    Track(usize),
    /// The subtitles page, and one row of it: a track, or `None` for off.
    Subtitles,
    Subtitle(Option<usize>),
    ReturnToLive,
    ManageFavorites,
    /// Picture in picture: the guide full screen, the video playing on in a corner window.
    Pip,
    /// The playing channel's audio offset page, and one step of it (ms).
    AudioOffset,
    Offset(i32),
    /// The display mode page, and one mode of it (its stored value).
    DisplayMode,
    Display(&'static str),
    /// The sleep timer page, and one choice of it (minutes; 0 is off).
    SleepTimer,
    Sleep(u32),
    /// Set or cancel the reminder for the focused future programme.
    Remind,
    /// Lock or unlock the channel, or the group, with the parental PIN.
    LockChannel,
    LockGroup,
    /// The viewer's groups: Manage groups, the group's channels, a custom
    /// group's own page.
    ManageGroups,
    ManageChannels,
    EditGroup,
}

struct ChannelTarget {
    key: ChannelKey,
    favorite: bool,
    programme: Option<Programme>,
    /// The programme can be played from the archive.
    from_start: bool,
    /// The parental settings block the channel itself.
    blocked: bool,
    /// For a programme still to start: whether a reminder is set for it.
    reminder: Option<bool>,
}

impl ChannelTarget {
    fn new(channel: &Channel, programme: Option<&Programme>, cx: &PanelCtx) -> Self {
        let now = cx.data.now();
        Self {
            key: channel.key,
            favorite: cx.data.is_favorite(channel.key),
            programme: programme.cloned(),
            from_start: programme.is_some_and(|p| in_archive(channel, p, now)),
            blocked: parental::channel_blocked(cx.settings, channel.key),
            reminder: programme
                .filter(|p| p.start > now)
                .map(|p| reminders::has(cx.settings, channel.key, p.start)),
        }
    }

    /// Remind me (on a programme still to start) and the channel's lock.
    fn user_rows(&self) -> Vec<Row<MenuItem>> {
        let mut rows = Vec::new();
        if let Some(set) = self.reminder {
            let label = if set {
                msg::reminder_cancel()
            } else {
                msg::reminder_set()
            };
            rows.push(Row::action(MenuItem::Remind, label));
        }
        let lock = if self.blocked {
            msg::parental_unlock_channel()
        } else {
            msg::parental_lock_channel()
        };
        rows.push(Row::action(MenuItem::LockChannel, lock));
        rows
    }

    /// Favourite, description and watch-from-start rows.
    fn rows(&self) -> Vec<Row<MenuItem>> {
        let fav = if self.favorite {
            msg::channel_favorite_remove()
        } else {
            msg::channel_favorite_add()
        };
        let mut rows = vec![Row::action(MenuItem::Favorite, fav)];
        if self.programme.is_some() {
            rows.push(Row::submenu(MenuItem::Description, msg::epg_description()));
        }
        if self.from_start {
            rows.push(Row::action(
                MenuItem::WatchFromStart,
                msg::epg_watch_from_start(),
            ));
        }
        rows
    }
}

/// Close the menu, then open `nav`.
fn close_then(nav: Navigate) -> Output {
    Output::close().and(Output::navigate(nav))
}

/// Run `command` and close the menu.
fn done(command: Command) -> Output {
    Output::command(command).and(Output::close())
}

/// Close the menu and ask for the PIN, which stores `value` under the parental `key`; a list
/// with no room left says so instead.
fn with_pin(key: &'static str, value: Option<String>, toast: &str) -> Output {
    match value {
        Some(value) => close_then(Navigate::Pin(PinTask::Set {
            key,
            value,
            toast: toast.to_owned(),
        })),
        None => Output {
            toast: Some(msg::parental_full().to_owned()),
            ..Output::close()
        },
    }
}

/// Play `programme` from the archive, from its start, in full screen: every screen closes, as
/// a Search or History pick does, rather than leaving it in the guide's preview.
fn watch_from_start(channel: ChannelKey, programme: Programme) -> Output {
    let stream = Stream::Catchup {
        channel,
        programme,
        offset_secs: 0,
    };
    Output::command(Command::Play {
        stream,
        group: None,
    })
    .and(Output::navigate(Navigate::Player))
}

/// A menu on the side panel.
pub struct ContextMenu {
    panel: SidePanel<MenuItem>,
    menu: Menu,
}

/// What the menu's rows act on.
struct Menu {
    channel: Option<ChannelTarget>,
    group: Option<Group>,
    settings: Settings,
    utc_offset_secs: i32,
    /// The player's audio tracks and the current one, for the player menu.
    tracks: Vec<String>,
    track: usize,
    /// The player's subtitle tracks and the one shown (`None`: off), for the player menu.
    subtitles: Vec<String>,
    subtitle: Option<usize>,
    /// The sleep timer's time left as the player menu opened.
    sleep_left_secs: Option<u32>,
}

impl Menu {
    fn new(channel: Option<ChannelTarget>, group: Option<Group>, cx: &PanelCtx) -> Self {
        Self {
            channel,
            group,
            settings: cx.settings.clone(),
            utc_offset_secs: cx.data.utc_offset_secs(),
            tracks: Vec::new(),
            track: 0,
            subtitles: Vec::new(),
            subtitle: None,
            sleep_left_secs: None,
        }
    }
}

impl ContextMenu {
    /// The menu a [`Navigate::ContextMenu`], [`Navigate::GroupMenu`], [`Navigate::PlayerMenu`] or
    /// [`Navigate::MainMenu`] asks for; `None` for any other request or a channel that is no
    /// longer in the playlist. Group options act on the channel's first playlist group.
    pub fn open(nav: &Navigate, cx: &PanelCtx) -> Option<Self> {
        match nav {
            Navigate::ContextMenu { channel, programme } => {
                let ch = cx.data.channel(*channel)?;
                let group = ch.groups.first().map(|g| Group::Playlist(g.clone()));
                Some(Self::channel(&ch, programme.as_ref(), group.as_ref(), cx))
            }
            Navigate::GroupMenu(group) => Some(Self::group(group, cx)),
            Navigate::PlayerMenu => Some(Self::player(cx)),
            Navigate::MainMenu => Some(Self::main(cx)),
            _ => None,
        }
    }

    /// The menu for `channel`, as focused in the guide or a channel list. `programme` is the
    /// focused programme, `group` the group being browsed (for Group options).
    pub fn channel(
        channel: &Channel,
        programme: Option<&Programme>,
        group: Option<&Group>,
        cx: &PanelCtx,
    ) -> Self {
        let target = ChannelTarget::new(channel, programme, cx);
        let mut rows = target.rows();
        rows.push(Row::action(
            MenuItem::ManageFavorites,
            msg::channel_favorite_manage(),
        ));
        if group.is_some() {
            rows.push(Row::submenu(MenuItem::GroupOptions, msg::group_options()));
        }
        rows.extend(target.user_rows());
        rows.push(Row::action(MenuItem::Search, msg::common_search()));
        rows.push(Row::action(MenuItem::Settings, msg::common_settings()));
        Self {
            panel: SidePanel::new(Page::list(&channel.name, rows)),
            menu: Menu::new(Some(target), group.cloned(), cx),
        }
    }

    /// The player menu: audio track, return to live while an archive plays, the playing
    /// channel's audio offset, the display mode and the sleep timer, then its favourite,
    /// description and watch-from-start rows, picture in picture, and Settings.
    pub fn player(cx: &PanelCtx) -> Self {
        let playback = cx.data.playback();
        let now = cx.data.now();
        let channel = playback.channel.and_then(|key| cx.data.channel(key));
        let mut rows = Vec::new();
        if playback.audio_tracks.len() > 1 {
            let current = playback.audio_tracks.get(playback.audio_track);
            rows.push(
                Row::submenu(MenuItem::AudioTrack, msg::player_audio_track())
                    .value(current.map_or("", String::as_str)),
            );
        }
        if !playback.subtitle_tracks.is_empty() {
            let current = playback
                .subtitle_track
                .and_then(|i| playback.subtitle_tracks.get(i));
            rows.push(
                Row::submenu(MenuItem::Subtitles, msg::subtitles_title())
                    .value(current.map_or(msg::subtitles_off(), String::as_str)),
            );
        }
        if playback.catchup.is_some() {
            rows.push(Row::action(MenuItem::ReturnToLive, msg::player_go_live()));
        }
        if let Some(ch) = &channel {
            rows.extend(playback_rows(cx.settings, ch.key, playback.sleep_left_secs));
        }
        let target = channel.as_ref().map(|ch| {
            // the archived programme playing, else the one on air
            let programme = playback
                .catchup
                .clone()
                .or_else(|| cx.data.now_next(ch.key, now).0);
            ChannelTarget::new(ch, programme.as_ref(), cx)
        });
        if let Some(t) = &target {
            rows.extend(t.rows());
        }
        if channel.is_some() {
            rows.push(Row::action(MenuItem::Pip, msg::pip_start()));
        }
        rows.push(Row::action(MenuItem::Settings, msg::common_settings()));
        let title = channel
            .as_ref()
            .map_or(msg::settings_player(), |c| c.name.as_str());
        let mut menu = Menu::new(target, None, cx);
        menu.tracks = playback.audio_tracks;
        menu.track = playback.audio_track;
        menu.subtitles = playback.subtitle_tracks;
        menu.subtitle = playback.subtitle_track;
        menu.sleep_left_secs = playback.sleep_left_secs;
        Self {
            panel: SidePanel::new(Page::list(title, rows)),
            menu,
        }
    }

    /// The main menu, on the left edge.
    pub fn main(cx: &PanelCtx) -> Self {
        let mut rows = vec![
            Row::action(MenuItem::Search, msg::common_search()),
            Row::action(MenuItem::Guide, msg::nav_epg()),
            Row::action(MenuItem::Channels, msg::common_channels()),
        ];
        // A section only while it lists something the parental lock leaves.
        let vod = cx.data.vod();
        let any = |kind| vod.iter().any(|v| v.kind == kind && !cx.data.vod_locked(v));
        if any(VodKind::Movie) {
            rows.push(Row::action(MenuItem::Movies, msg::vod_movies()));
        }
        if any(VodKind::Episode) {
            rows.push(Row::action(MenuItem::Shows, msg::vod_shows()));
        }
        rows.push(Row::action(MenuItem::History, msg::channel_history()));
        rows.push(Row::action(MenuItem::Settings, msg::common_settings()));
        Self {
            panel: SidePanel::at(Edge::Left, Page::list(msg::player_menu(), rows)),
            menu: Menu::new(None, None, cx),
        }
    }

    /// The menu for a group in the groups panel: its options first.
    pub fn group(group: &Group, cx: &PanelCtx) -> Self {
        let mut rows = group_rows(group, cx.settings);
        rows.extend(library_rows(group));
        rows.push(Row::action(
            MenuItem::ManageFavorites,
            msg::channel_favorite_manage(),
        ));
        rows.push(Row::action(MenuItem::Search, msg::common_search()));
        rows.push(Row::action(MenuItem::Settings, msg::common_settings()));
        Self {
            panel: SidePanel::new(Page::list(group.name(), rows)),
            menu: Menu::new(None, Some(group.clone()), cx),
        }
    }

    pub fn panel(&self) -> &SidePanel<MenuItem> {
        &self.panel
    }
}

impl Screen for ContextMenu {
    fn hold(&self, key: Key) -> Hold {
        PanelKey::hold(key)
    }

    fn key(&mut self, key: Key, press: Press, _: &dyn TvData, _: u64) -> Output {
        let Some(key) = PanelKey::from_action(Action { key, press }) else {
            return Output::default();
        };
        let menu = &mut self.menu;
        self.panel
            .handle(key, |panel, item| menu.activate(panel, item))
    }

    fn point(&mut self, x: f32, y: f32, _: &dyn TvData, _: u64) -> Output {
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

impl Menu {
    fn activate(&mut self, panel: &mut SidePanel<MenuItem>, item: MenuItem) -> Output {
        match item {
            MenuItem::Favorite => {
                let Some(ch) = &self.channel else {
                    return Output::default();
                };
                let toast = if ch.favorite {
                    msg::channel_favorite_removed()
                } else {
                    msg::channel_favorite_added()
                };
                Output {
                    toast: Some(toast.to_owned()),
                    ..done(Command::ToggleFavorite(ch.key))
                }
            }
            MenuItem::Description => {
                if let Some(p) = self.channel.as_ref().and_then(|c| c.programme.as_ref()) {
                    panel.push(Page::text(&p.title, describe(p, self.utc_offset_secs)));
                }
                Output::default()
            }
            MenuItem::WatchFromStart => match &self.channel {
                Some(ChannelTarget {
                    key,
                    programme: Some(programme),
                    from_start: true,
                    ..
                }) => watch_from_start(*key, programme.clone()),
                _ => Output::default(),
            },
            MenuItem::GroupOptions => {
                if let Some(group) = &self.group {
                    panel.push(Page::list(group.name(), group_rows(group, &self.settings)));
                }
                Output::default()
            }
            MenuItem::HideGroup => self.toggle_group(),
            MenuItem::ChannelSorting => {
                let current = self.settings.get(keys::GROUPS_CHANNEL_SORT);
                let rows = sort_options()
                    .iter()
                    .map(|&(value, label)| {
                        Row::check(MenuItem::Sort(value), &label.text(), value == current)
                    })
                    .collect();
                let focus = sort_options()
                    .iter()
                    .position(|(v, _)| *v == current)
                    .unwrap_or(0);
                panel.push(Page::list(msg::group_channel_sorting(), rows).focused(focus));
                Output::default()
            }
            MenuItem::Sort(value) => {
                let out = self
                    .settings
                    .change(keys::GROUPS_CHANNEL_SORT, value.to_owned());
                panel.pop();
                let shown = self.settings.display(keys::GROUPS_CHANNEL_SORT);
                panel.for_each_row(|row| {
                    if row.id == MenuItem::ChannelSorting {
                        row.set_value(&shown);
                    }
                });
                out.map_or_else(Output::default, Output::command)
            }
            MenuItem::Search => close_then(Navigate::Search),
            MenuItem::Settings => close_then(Navigate::Settings),
            MenuItem::Guide => close_then(Navigate::Guide),
            MenuItem::Channels => close_then(Navigate::ChannelList { preview: false }),
            MenuItem::History => close_then(Navigate::History),
            MenuItem::Movies => close_then(Navigate::Vod(VodSection::Movies)),
            MenuItem::Shows => close_then(Navigate::Vod(VodSection::Shows)),
            MenuItem::AudioTrack => {
                let rows = self
                    .tracks
                    .iter()
                    .enumerate()
                    .map(|(i, name)| Row::check(MenuItem::Track(i), name, i == self.track))
                    .collect();
                panel.push(Page::list(msg::player_audio_track(), rows).focused(self.track));
                Output::default()
            }
            MenuItem::Track(i) => done(Command::SelectAudioTrack(i)),
            MenuItem::Subtitles => {
                let off = Row::check(
                    MenuItem::Subtitle(None),
                    msg::subtitles_off(),
                    self.subtitle.is_none(),
                );
                let tracks = self.subtitles.iter().enumerate().map(|(i, name)| {
                    Row::check(MenuItem::Subtitle(Some(i)), name, self.subtitle == Some(i))
                });
                let rows = std::iter::once(off).chain(tracks).collect();
                let focus = self.subtitle.map_or(0, |i| i + 1);
                panel.push(Page::list(msg::subtitles_title(), rows).focused(focus));
                Output::default()
            }
            MenuItem::Subtitle(pick) => done(Command::SelectSubtitleTrack(pick)),
            MenuItem::ReturnToLive => done(Command::ReturnToLive),
            MenuItem::Pip => close_then(Navigate::Pip),
            MenuItem::ManageFavorites => Output {
                message: Some(ScreenMessage::ManageFavorites),
                ..Output::close()
            },
            MenuItem::ManageGroups => close_then(Navigate::Library(LibraryPage::Groups)),
            MenuItem::ManageChannels => match &self.group {
                Some(group) => close_then(Navigate::Library(LibraryPage::Channels(group.clone()))),
                None => Output::default(),
            },
            MenuItem::EditGroup => match &self.group {
                Some(Group::Custom(name)) => {
                    close_then(Navigate::Library(LibraryPage::CustomGroup(name.clone())))
                }
                _ => Output::default(),
            },
            MenuItem::AudioOffset => {
                if let Some(key) = self.channel.as_ref().map(|c| c.key) {
                    panel.push(offset_page(&self.settings, key));
                }
                Output::default()
            }
            MenuItem::Offset(ms) => self.set_offset(panel, ms),
            MenuItem::DisplayMode => {
                let current = self.settings.get(keys::PLAYER_DISPLAY_MODE);
                let options = choices(keys::PLAYER_DISPLAY_MODE);
                let rows = options
                    .iter()
                    .map(|&(value, label)| {
                        Row::check(MenuItem::Display(value), &label.text(), value == current)
                    })
                    .collect();
                let focus = options.iter().position(|(v, _)| *v == current);
                let page = Page::list(msg::playback_display_mode(), rows);
                panel.push(page.focused(focus.unwrap_or(0)));
                Output::default()
            }
            MenuItem::Display(value) => {
                let out = self
                    .settings
                    .change(keys::PLAYER_DISPLAY_MODE, value.to_owned());
                panel.pop();
                let shown = self.settings.display(keys::PLAYER_DISPLAY_MODE);
                panel.for_each_row(|row| {
                    if row.id == MenuItem::DisplayMode {
                        row.set_value(&shown);
                    }
                });
                out.map_or_else(Output::default, Output::command)
            }
            MenuItem::SleepTimer => {
                // No timer: Off is the current choice; a running one shows its time left instead.
                let off = self.sleep_left_secs.is_none();
                let rows = SLEEP_MINUTES
                    .iter()
                    .map(|&m| Row::check(MenuItem::Sleep(m), &sleep_label(m), m == 0 && off))
                    .collect();
                panel.push(Page::list(msg::playback_sleep_timer(), rows));
                Output::default()
            }
            MenuItem::Sleep(minutes) => done(Command::SleepTimer { minutes }),
            MenuItem::Remind => self.toggle_reminder(),
            MenuItem::LockChannel => self.toggle_channel_lock(),
            MenuItem::LockGroup => self.toggle_group_lock(),
        }
    }

    /// Set the reminder for the focused programme, or cancel it.
    fn toggle_reminder(&mut self) -> Output {
        let Some((key, Some(p))) = self.channel.as_ref().map(|c| (c.key, c.programme.as_ref()))
        else {
            return Output::default();
        };
        let (value, toast) = match reminders::toggled(&self.settings, key, p.start, &p.title) {
            Toggled::Added(v) => (v, msg::reminder_added()),
            Toggled::Removed(v) => (v, msg::reminder_removed()),
            Toggled::Full => {
                return Output {
                    toast: Some(msg::reminder_full().to_owned()),
                    ..Output::close()
                }
            }
        };
        let Some(command) = self.settings.change(keys::REMINDERS, value) else {
            return Output::close();
        };
        Output {
            toast: Some(toast.to_owned()),
            ..done(command)
        }
    }

    /// Lock the channel, or unlock it: either takes the PIN.
    fn toggle_channel_lock(&self) -> Output {
        let Some(ch) = &self.channel else {
            return Output::default();
        };
        let toast = if ch.blocked {
            msg::parental_channel_unlocked()
        } else {
            msg::parental_channel_locked()
        };
        let value = parental::channels_toggled(&self.settings, ch.key);
        with_pin(keys::PARENTAL_CHANNELS, value, toast)
    }

    /// Lock the playlist group, or unlock it: either takes the PIN.
    fn toggle_group_lock(&self) -> Output {
        let Some(Group::Playlist(name)) = &self.group else {
            return Output::default();
        };
        let toast = if parental::group_blocked(&self.settings, name) {
            msg::parental_group_unlocked()
        } else {
            msg::parental_group_locked()
        };
        let value = parental::groups_toggled(&self.settings, name);
        with_pin(keys::PARENTAL_GROUPS, value, toast)
    }

    /// The playing channel's audio offset becomes `ms`: stored as its own, heard at once. The
    /// page stays, the pick checked, so another step can be tried by ear.
    fn set_offset(&mut self, panel: &mut SidePanel<MenuItem>, ms: i32) -> Output {
        let Some(key) = self.channel.as_ref().map(|c| c.key) else {
            return Output::default();
        };
        let list = with_channel_offset(&self.settings, key, ms);
        let out = self.settings.change(keys::CHANNEL_AUDIO_OFFSETS, list);
        panel.replace(offset_page(&self.settings, key));
        let shown = offset_label(i64::from(audio_offset(&self.settings, Some(key))));
        panel.for_each_row(|row| {
            if row.id == MenuItem::AudioOffset {
                row.set_value(&shown);
            }
        });
        out.map_or_else(Output::default, Output::command)
    }

    /// Hide the group, or show it again if it is hidden.
    fn toggle_group(&mut self) -> Output {
        let Some(group) = &self.group else {
            return Output::default();
        };
        let hidden = group_hidden(group, &self.settings);
        let (key, value) = match group {
            Group::Favorites => (keys::GROUPS_SHOW_FAVORITES, hidden.to_string()),
            Group::All => (keys::GROUPS_SHOW_ALL, hidden.to_string()),
            Group::History => (keys::GROUPS_SHOW_HISTORY, hidden.to_string()),
            Group::Playlist(name) => (
                keys::GROUPS_HIDDEN,
                self.settings.hidden_groups_toggled(name),
            ),
            Group::Custom(_) => return Output::default(),
        };
        let Some(command) = self.settings.change(key, value) else {
            return Output::close();
        };
        let toast = if hidden {
            msg::group_shown()
        } else {
            msg::group_hidden()
        };
        Output {
            toast: Some(toast.to_owned()),
            ..done(command)
        }
    }
}

fn group_rows(group: &Group, settings: &Settings) -> Vec<Row<MenuItem>> {
    let hide = if group_hidden(group, settings) {
        msg::group_show()
    } else {
        msg::group_hide()
    };
    let mut rows = Vec::new();
    // A custom group is not hidden but deleted.
    if !matches!(group, Group::Custom(_)) {
        rows.push(Row::action(MenuItem::HideGroup, hide));
    }
    rows.push(
        Row::submenu(MenuItem::ChannelSorting, msg::group_channel_sorting())
            .value(&settings.display(keys::GROUPS_CHANNEL_SORT)),
    );
    if let Group::Playlist(name) = group {
        let lock = if parental::group_blocked(settings, name) {
            msg::parental_unlock_group()
        } else {
            msg::parental_lock_group()
        };
        rows.push(Row::action(MenuItem::LockGroup, lock));
    }
    rows
}

/// Whether the settings hide `group` from the groups panel.
pub fn group_hidden(group: &Group, settings: &Settings) -> bool {
    match group {
        Group::Favorites => !settings.flag(keys::GROUPS_SHOW_FAVORITES),
        Group::All => !settings.flag(keys::GROUPS_SHOW_ALL),
        Group::History => !settings.flag(keys::GROUPS_SHOW_HISTORY),
        Group::Playlist(name) => settings.hidden_groups().any(|n| n == name),
        Group::Custom(_) => false,
    }
}

/// The rows of the viewer's groups: the group's channels (to copy, move or reorder them; not
/// History's), a custom group's own page, Manage groups.
fn library_rows(group: &Group) -> Vec<Row<MenuItem>> {
    let mut rows = Vec::new();
    if *group != Group::History {
        rows.push(Row::action(
            MenuItem::ManageChannels,
            msg::library_channels(),
        ));
    }
    if matches!(group, Group::Custom(_)) {
        rows.push(Row::action(MenuItem::EditGroup, msg::library_edit_group()));
    }
    rows.push(Row::action(MenuItem::ManageGroups, msg::library_groups()));
    rows
}

fn sort_options() -> &'static [(&'static str, Label)] {
    choices(keys::GROUPS_CHANNEL_SORT)
}

/// The options of the choice setting `key`.
fn choices(key: &str) -> &'static [(&'static str, Label)] {
    match super::settings::spec(key).map(|s| s.kind) {
        Some(Kind::Choice(options)) => options,
        _ => &[],
    }
}

/// The player menu's rows for the sound and picture of `channel`, and the sleep timer, each
/// with its current value.
fn playback_rows(
    settings: &Settings,
    channel: ChannelKey,
    sleep_left_secs: Option<u32>,
) -> Vec<Row<MenuItem>> {
    let offset = offset_label(i64::from(audio_offset(settings, Some(channel))));
    let sleep = sleep_left_secs.map_or_else(|| sleep_label(0), sleep_left_label);
    vec![
        Row::submenu(MenuItem::AudioOffset, msg::playback_audio_offset()).value(&offset),
        Row::submenu(MenuItem::DisplayMode, msg::playback_display_mode())
            .value(&settings.display(keys::PLAYER_DISPLAY_MODE)),
        Row::submenu(MenuItem::SleepTimer, msg::playback_sleep_timer()).value(&sleep),
    ]
}

/// The audio offset page for `channel`: every step of the range, the one it plays with checked
/// and focused, so Up and Down move it a step at a time.
fn offset_page(settings: &Settings, channel: ChannelKey) -> Page<MenuItem> {
    let current = audio_offset(settings, Some(channel));
    let rows: Vec<_> = choices(keys::AUDIO_OFFSET_MS)
        .iter()
        .filter_map(|&(value, label)| {
            let ms = value.parse().ok()?;
            Some(Row::check(
                MenuItem::Offset(ms),
                &label.text(),
                ms == current,
            ))
        })
        .collect();
    let focus = rows.iter().position(|r| r.checked() == Some(true));
    Page::list(msg::playback_audio_offset(), rows).focused(focus.unwrap_or(0))
}

/// The description page: when it airs, then its text.
fn describe(p: &Programme, utc_offset_secs: i32) -> Vec<String> {
    let when = msg::epg_when(
        p.duration_secs().max(0) / 60,
        &hhmm(p.start, utc_offset_secs),
        &hhmm(p.stop, utc_offset_secs),
        weekday(p.start, utc_offset_secs),
    );
    let mut out = vec![when];
    if let Some(category) = p.category.as_deref().filter(|c| !c.is_empty()) {
        out.push(category.to_owned());
    }
    out.push(
        p.description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or(msg::epg_no_description())
            .to_owned(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::FakeData;
    use crate::panels::testing::{draw, press};
    use crate::time::DAY;
    use iptv::model::Timestamp;

    const NOW: Timestamp = 1_700_000_000;

    fn programme(start: Timestamp, mins: i64) -> Programme {
        Programme {
            start,
            stop: start + mins * 60,
            title: "Evening news".into(),
            description: Some("Headlines.".into()),
            category: Some("News".into()),
            icon: None,
        }
    }

    fn labels(menu: &ContextMenu) -> Vec<&str> {
        menu.panel()
            .page()
            .rows()
            .iter()
            .map(|r| r.label())
            .collect()
    }

    /// Focus the row labelled `label` and press OK.
    fn choose(menu: &mut ContextMenu, label: &str) -> Output {
        let i = labels(menu).iter().position(|l| *l == label).unwrap();
        let data = FakeData::new(NOW, 0);
        let focus = menu.panel().page().focus();
        let moves = if focus > i {
            vec![Key::Up; focus - i]
        } else {
            vec![Key::Down; i - focus]
        };
        press(menu, &data, &moves);
        press(menu, &data, &[Key::Ok])
    }

    /// Short presses of `keys` on `menu`.
    fn keys(menu: &mut ContextMenu, keys: &[Key]) -> Output {
        press(menu, &FakeData::new(NOW, 0), keys)
    }

    fn set(key: &str, value: &str) -> Command {
        Command::SetSetting {
            key: key.to_owned(),
            value: value.to_owned(),
        }
    }

    fn toast(text: &str, out: Output) -> Output {
        Output {
            toast: Some(text.to_owned()),
            ..out
        }
    }

    #[test]
    fn channel_menu_offers_the_tivimate_actions() {
        let data = FakeData::new(NOW, 6);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let past = programme(NOW - 3 * 3600, 30);
        let group = Group::Playlist("News".into());
        let menu = ContextMenu::channel(&data.channels[0], Some(&past), Some(&group), &cx);
        assert_eq!(menu.panel().page().title(), "Channel 1");
        assert_eq!(
            labels(&menu),
            [
                "Add to favorites",
                "Programme description",
                "Watch from start",
                "Manage favorites",
                "Group options",
                "Lock channel",
                "Search",
                "Settings"
            ]
        );
        let bare = ContextMenu::channel(&data.channels[0], None, None, &cx);
        assert_eq!(
            labels(&bare),
            [
                "Add to favorites",
                "Manage favorites",
                "Lock channel",
                "Search",
                "Settings"
            ]
        );
    }

    #[test]
    fn manage_favorites_hands_the_guide_its_mode() {
        let data = FakeData::new(NOW, 3);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let expected = Output {
            message: Some(ScreenMessage::ManageFavorites),
            ..Output::close()
        };
        let mut menu = ContextMenu::channel(&data.channels[0], None, None, &cx);
        assert_eq!(choose(&mut menu, "Manage favorites"), expected);
        assert!(menu.closing());
        assert_eq!(keys(&mut menu, &[Key::Ok]), Output::default());
        let mut menu = ContextMenu::group(&Group::Favorites, &cx);
        assert_eq!(choose(&mut menu, "Manage favorites"), expected);
        assert!(!labels(&ContextMenu::player(&cx)).contains(&"Manage favorites"));
    }

    #[test]
    fn favourite_toggles_with_a_toast() {
        let mut data = FakeData::new(NOW, 6);
        let settings = Settings::new();
        let key = data.channels[1].key;
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::channel(&data.channels[1], None, None, &cx);
        assert_eq!(
            keys(&mut menu, &[Key::Ok]),
            toast("Added to favorites", done(Command::ToggleFavorite(key)))
        );
        data.favorites.push(key);
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::channel(&data.channels[1], None, None, &cx);
        assert_eq!(labels(&menu)[0], "Remove from favorites");
        assert_eq!(
            keys(&mut menu, &[Key::Ok]).toast.as_deref(),
            Some("Removed from favorites")
        );
    }

    #[test]
    fn watch_from_start_needs_a_started_programme_inside_the_archive() {
        let data = FakeData::new(NOW, 6);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let on_air = programme(NOW - 600, 30);
        let mut menu = ContextMenu::channel(&data.channels[0], Some(&on_air), None, &cx);
        let play = Command::Play {
            stream: Stream::Catchup {
                channel: data.channels[0].key,
                programme: on_air.clone(),
                offset_secs: 0,
            },
            group: None,
        };
        // Full screen: every screen closes, the guide under the menu too.
        assert_eq!(
            choose(&mut menu, "Watch from start"),
            Output::command(play).and(Output::navigate(Navigate::Player))
        );
        assert!(menu.closing(), "the menu slides out");
        let has_start = |ch: usize, p: &Programme| {
            labels(&ContextMenu::channel(
                &data.channels[ch],
                Some(p),
                None,
                &cx,
            ))
            .contains(&"Watch from start")
        };
        // channel 3 has no catch-up
        assert!(!has_start(2, &on_air));
        assert!(!has_start(0, &programme(NOW + 600, 30)));
        assert!(!has_start(0, &programme(NOW, 30)));
        assert!(has_start(0, &programme(NOW - 7 * DAY + 60, 30)));
        assert!(!has_start(0, &programme(NOW - 7 * DAY - 60, 30)));
    }

    #[test]
    fn description_opens_a_page_and_back_returns() {
        let data = FakeData::new(NOW, 3);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        // 22:00 UTC, a Tuesday
        let p = programme(1_699_999_200, 45);
        let mut menu = ContextMenu::channel(&data.channels[0], Some(&p), None, &cx);
        assert_eq!(
            choose(&mut menu, "Programme description"),
            Output::default()
        );
        let page = menu.panel().page();
        assert_eq!(page.title(), "Evening news");
        assert_eq!(
            page.body(),
            ["Tue 22:00 - 22:45 (45 min)", "News", "Headlines."]
        );
        draw(&mut menu);
        assert_eq!(keys(&mut menu, &[Key::Back]), Output::default());
        assert_eq!(menu.panel().depth(), 1);
        assert_eq!(keys(&mut menu, &[Key::Back]), Output::close());
    }

    #[test]
    fn group_options_hide_and_show_a_playlist_group() {
        let data = FakeData::new(NOW, 3);
        let settings = Settings::from_stored([(keys::GROUPS_HIDDEN, "Kids")]);
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let news = Group::Playlist("News".into());
        let mut menu = ContextMenu::channel(&data.channels[0], None, Some(&news), &cx);
        assert_eq!(choose(&mut menu, "Group options"), Output::default());
        assert_eq!(menu.panel().page().title(), "News");
        assert_eq!(
            choose(&mut menu, "Hide group"),
            toast("Group hidden", done(set(keys::GROUPS_HIDDEN, "Kids\nNews")))
        );
        let kids = Group::Playlist("Kids".into());
        let mut menu = ContextMenu::group(&kids, &cx);
        assert_eq!(
            labels(&menu),
            [
                "Show group",
                "Channel sorting",
                "Lock group",
                "Manage channels",
                "Manage groups",
                "Manage favorites",
                "Search",
                "Settings"
            ]
        );
        assert_eq!(
            keys(&mut menu, &[Key::Ok]).commands,
            [set(keys::GROUPS_HIDDEN, "")]
        );
    }

    #[test]
    fn a_groups_menu_opens_the_library_pages() {
        let data = FakeData::new(NOW, 3);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let kids = Group::Custom("Kids".into());
        let mut menu = ContextMenu::group(&kids, &cx);
        assert_eq!(
            labels(&menu)[..4],
            [
                "Channel sorting",
                "Manage channels",
                "Edit group",
                "Manage groups"
            ],
            "a custom group is deleted, not hidden"
        );
        let open = |page| Output::close().and(Output::navigate(Navigate::Library(page)));
        assert_eq!(
            choose(&mut menu, "Edit group"),
            open(LibraryPage::CustomGroup("Kids".into()))
        );
        let mut menu = ContextMenu::group(&kids, &cx);
        assert_eq!(
            choose(&mut menu, "Manage channels"),
            open(LibraryPage::Channels(kids.clone()))
        );
        let mut menu = ContextMenu::group(&Group::History, &cx);
        assert!(!labels(&menu).contains(&"Manage channels"));
        assert_eq!(
            choose(&mut menu, "Manage groups"),
            open(LibraryPage::Groups)
        );
        assert!(!group_hidden(&kids, &settings));
    }

    #[test]
    fn built_in_groups_hide_through_their_toggles() {
        let data = FakeData::new(NOW, 3);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        for (group, key) in [
            (Group::Favorites, keys::GROUPS_SHOW_FAVORITES),
            (Group::All, keys::GROUPS_SHOW_ALL),
            (Group::History, keys::GROUPS_SHOW_HISTORY),
        ] {
            let mut menu = ContextMenu::group(&group, &cx);
            assert_eq!(menu.panel().page().title(), group.name());
            assert_eq!(keys(&mut menu, &[Key::Ok]).commands, [set(key, "false")]);
        }
    }

    #[test]
    fn channel_sorting_picks_and_shows_the_value() {
        let data = FakeData::new(NOW, 3);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::group(&Group::All, &cx);
        assert_eq!(
            menu.panel().page().rows()[1].value_text(),
            Some("Playlist order")
        );
        assert_eq!(choose(&mut menu, "Channel sorting"), Output::default());
        assert_eq!(menu.panel().page().focus(), 0);
        assert_eq!(
            choose(&mut menu, "Name"),
            Output::command(set(keys::GROUPS_CHANNEL_SORT, "name"))
        );
        assert_eq!(menu.panel().page().rows()[1].value_text(), Some("Name"));
    }

    #[test]
    fn search_and_settings_hand_over_to_the_shell() {
        let data = FakeData::new(NOW, 3);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        for (label, nav) in [
            ("Search", Navigate::Search),
            ("Settings", Navigate::Settings),
        ] {
            let mut menu = ContextMenu::channel(&data.channels[0], None, None, &cx);
            assert_eq!(choose(&mut menu, label), close_then(nav));
            assert!(menu.closing());
        }
    }

    #[test]
    fn a_closed_menu_takes_no_keys_and_slides_out() {
        let data = FakeData::new(NOW, 3);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::channel(&data.channels[0], None, None, &cx);
        for _ in 0..30 {
            menu.update(1.0 / 60.0);
        }
        assert!(!menu.closing());
        assert_eq!(keys(&mut menu, &[Key::Back]), Output::close());
        assert_eq!(keys(&mut menu, &[Key::Ok]), Output::default());
        assert!(menu.closing() && !menu.gone());
        for _ in 0..120 {
            menu.update(1.0 / 60.0);
        }
        assert!(menu.gone());
    }

    #[test]
    fn long_presses_and_other_keys_do_nothing() {
        let data = FakeData::new(NOW, 3);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::channel(&data.channels[0], None, None, &cx);
        assert_eq!(menu.hold(Key::Down), Hold::Repeat);
        let long = menu.key(Key::Ok, Press::Long, &data, 0);
        assert_eq!(long, Output::default());
        assert_eq!(keys(&mut menu, &[Key::Red]), Output::default());
        assert!(!menu.editing());
    }

    #[test]
    fn opens_from_a_navigate_request() {
        let data = FakeData::new(NOW, 3);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let nav = Navigate::ContextMenu {
            channel: data.channels[1].key,
            programme: Some(programme(NOW - 600, 30)),
        };
        let mut menu = ContextMenu::open(&nav, &cx).unwrap();
        assert_eq!(menu.panel().page().title(), "Channel 2");
        assert_eq!(choose(&mut menu, "Group options"), Output::default());
        assert_eq!(menu.panel().page().title(), "Sport");
        let group = ContextMenu::open(&Navigate::GroupMenu(Group::All), &cx).unwrap();
        assert_eq!(group.panel().page().title(), "All channels");
        let gone = Navigate::ContextMenu {
            channel: ChannelKey(0),
            programme: None,
        };
        assert!(ContextMenu::open(&gone, &cx).is_none());
        assert!(ContextMenu::open(&Navigate::Search, &cx).is_none());
    }

    #[test]
    fn player_menu_follows_what_plays() {
        let mut data = FakeData::new(NOW, 3);
        let key = data.channels[0].key;
        data.playback.channel = Some(key);
        data.playback.audio_tracks = vec!["Polish".into(), "English".into()];
        data.playback.audio_track = 1;
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::open(&Navigate::PlayerMenu, &cx).unwrap();
        assert_eq!(menu.panel().page().title(), "Channel 1");
        assert_eq!(
            labels(&menu),
            [
                "Audio track",
                "Audio offset",
                "Display mode",
                "Sleep timer",
                "Add to favorites",
                "Programme description",
                "Watch from start",
                "Picture in picture",
                "Settings"
            ]
        );
        assert_eq!(menu.panel().page().rows()[0].value_text(), Some("English"));
        assert_eq!(keys(&mut menu, &[Key::Ok]), Output::default());
        assert_eq!(menu.panel().page().focus(), 1);
        assert_eq!(menu.panel().page().rows()[1].checked(), Some(true));
        assert_eq!(
            choose(&mut menu, "Polish"),
            done(Command::SelectAudioTrack(0))
        );

        let archived = programme(NOW - 3600, 30);
        data.playback.catchup = Some(archived.clone());
        data.playback.audio_tracks.clear();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::player(&cx);
        assert_eq!(labels(&menu)[0], "Return to live");
        assert_eq!(keys(&mut menu, &[Key::Ok]), done(Command::ReturnToLive));
        let mut menu = ContextMenu::player(&cx);
        assert_eq!(
            choose(&mut menu, "Programme description"),
            Output::default()
        );
        assert_eq!(menu.panel().page().title(), archived.title);
    }

    #[test]
    fn picture_in_picture_opens_the_guide_with_the_video_in_a_corner() {
        let mut data = FakeData::new(NOW, 3);
        data.playback.channel = Some(data.channels[1].key);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::player(&cx);
        assert_eq!(
            choose(&mut menu, "Picture in picture"),
            close_then(Navigate::Pip)
        );
    }

    #[test]
    fn the_subtitles_page_picks_a_track_or_off() {
        let mut data = FakeData::new(NOW, 3);
        data.playback.channel = Some(data.channels[0].key);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let menu = ContextMenu::player(&cx);
        assert!(
            !labels(&menu).contains(&"Subtitles"),
            "no row without a track"
        );

        data.playback.subtitle_tracks = vec!["Polski".into(), "English".into()];
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::player(&cx);
        assert_eq!(labels(&menu)[0], "Subtitles");
        assert_eq!(menu.panel().page().rows()[0].value_text(), Some("Off"));
        assert_eq!(keys(&mut menu, &[Key::Ok]), Output::default());
        assert_eq!(labels(&menu), ["Off", "Polski", "English"]);
        assert_eq!(menu.panel().page().focus(), 0);
        assert_eq!(menu.panel().page().rows()[0].checked(), Some(true));
        assert_eq!(
            choose(&mut menu, "English"),
            done(Command::SelectSubtitleTrack(Some(1)))
        );

        data.playback.subtitle_track = Some(1);
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::player(&cx);
        assert_eq!(menu.panel().page().rows()[0].value_text(), Some("English"));
        keys(&mut menu, &[Key::Ok]);
        assert_eq!(menu.panel().page().focus(), 2, "on the track shown");
        assert_eq!(
            choose(&mut menu, "Off"),
            done(Command::SelectSubtitleTrack(None))
        );
    }

    #[test]
    fn player_menu_without_a_channel_offers_settings() {
        let data = FakeData::new(NOW, 3);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let menu = ContextMenu::player(&cx);
        assert_eq!(menu.panel().page().title(), "Player");
        assert_eq!(labels(&menu), ["Settings"]);
    }

    #[test]
    fn main_menu_opens_on_the_left_and_hands_over() {
        let data = FakeData::new(NOW, 3);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::open(&Navigate::MainMenu, &cx).unwrap();
        assert_eq!(
            labels(&menu),
            ["Search", "TV guide", "Channels", "History", "Settings"]
        );
        for _ in 0..120 {
            menu.update(1.0 / 60.0);
        }
        assert!(menu.panel().frame().x.abs() < 1.0);
        for (label, nav) in [
            ("TV guide", Navigate::Guide),
            ("Channels", Navigate::ChannelList { preview: false }),
            ("History", Navigate::History),
        ] {
            let mut menu = ContextMenu::main(&cx);
            assert_eq!(choose(&mut menu, label), close_then(nav));
        }
        // Left goes nowhere on the left edge; Right, toward the content, closes like Back
        assert_eq!(keys(&mut menu, &[Key::Left]), Output::default());
        assert_eq!(keys(&mut menu, &[Key::Right]), Output::close());
    }

    #[test]
    fn the_main_menu_lists_movies_and_shows_while_the_playlist_has_them() {
        let mut data = FakeData::new(NOW, 3);
        data.vod = crate::vod::fake::items("");
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let menu = ContextMenu::main(&cx);
        assert_eq!(
            labels(&menu),
            ["Search", "TV guide", "Channels", "Movies", "Shows", "History", "Settings"]
        );
        for (label, section) in [
            ("Movies", crate::data::VodSection::Movies),
            ("Shows", crate::data::VodSection::Shows),
        ] {
            let mut menu = ContextMenu::main(&cx);
            assert_eq!(choose(&mut menu, label), close_then(Navigate::Vod(section)));
        }
        data.vod.retain(|v| v.kind == VodKind::Movie);
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        assert!(!labels(&ContextMenu::main(&cx)).contains(&"Shows"));
    }

    /// The value shown on the root row `label` of `menu`.
    fn value_of(menu: &ContextMenu, label: &str) -> Option<String> {
        let rows = menu.panel().page().rows();
        let row = rows.iter().find(|r| r.label() == label)?;
        row.value_text().map(str::to_owned)
    }

    #[test]
    fn the_audio_offset_steps_by_fifty_ms_and_is_kept_for_the_channel() {
        let mut data = FakeData::new(NOW, 3);
        let key = data.channels[0].key;
        data.playback.channel = Some(key);
        let settings = Settings::from_stored([(keys::AUDIO_OFFSET_MS, "-100")]);
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::player(&cx);
        assert_eq!(value_of(&menu, "Audio offset").as_deref(), Some("-100 ms"));
        assert_eq!(choose(&mut menu, "Audio offset"), Output::default());
        let page = menu.panel().page();
        assert_eq!((page.title(), page.rows().len()), ("Audio offset", 41));
        assert_eq!(
            page.rows()[page.focus()].label(),
            "-100 ms",
            "on the default"
        );
        assert_eq!(page.rows()[page.focus()].checked(), Some(true));
        // one step later, then another: each is stored at once and the page stays
        let stored = |ms: i32| {
            let s = Settings::from_stored([(keys::AUDIO_OFFSET_MS, "-100")]);
            Output::command(Command::SetSetting {
                key: keys::CHANNEL_AUDIO_OFFSETS.to_owned(),
                value: with_channel_offset(&s, key, ms),
            })
        };
        assert_eq!(keys(&mut menu, &[Key::Down, Key::Ok]), stored(-50));
        assert_eq!(keys(&mut menu, &[Key::Down, Key::Ok]), stored(0));
        let page = menu.panel().page();
        assert_eq!(page.title(), "Audio offset");
        assert_eq!(page.rows()[page.focus()].label(), "0 ms");
        assert_eq!(page.rows()[page.focus()].checked(), Some(true));
        assert_eq!(keys(&mut menu, &[Key::Back]), Output::default());
        assert_eq!(value_of(&menu, "Audio offset").as_deref(), Some("0 ms"));
        assert!(!menu.closing());
    }

    #[test]
    fn the_display_mode_is_picked_from_its_page() {
        let mut data = FakeData::new(NOW, 3);
        data.playback.channel = Some(data.channels[0].key);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::player(&cx);
        assert_eq!(value_of(&menu, "Display mode").as_deref(), Some("Normal"));
        choose(&mut menu, "Display mode");
        let page = menu.panel().page();
        assert_eq!(labels(&menu), ["Normal", "Fill", "4:3", "16:9"]);
        assert_eq!(page.rows()[0].checked(), Some(true));
        assert_eq!(
            choose(&mut menu, "4:3"),
            Output::command(set(keys::PLAYER_DISPLAY_MODE, "4:3"))
        );
        assert_eq!(menu.panel().depth(), 1, "back on the menu");
        assert_eq!(value_of(&menu, "Display mode").as_deref(), Some("4:3"));
    }

    #[test]
    fn the_sleep_timer_offers_its_choices_and_shows_the_time_left() {
        let mut data = FakeData::new(NOW, 3);
        data.playback.channel = Some(data.channels[0].key);
        let settings = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::player(&cx);
        assert_eq!(value_of(&menu, "Sleep timer").as_deref(), Some("Off"));
        choose(&mut menu, "Sleep timer");
        assert_eq!(
            labels(&menu),
            ["Off", "15 min", "30 min", "60 min", "90 min", "120 min"]
        );
        assert_eq!(menu.panel().page().rows()[0].checked(), Some(true));
        assert_eq!(
            choose(&mut menu, "30 min"),
            done(Command::SleepTimer { minutes: 30 })
        );
        data.playback.sleep_left_secs = Some(25 * 60 - 10);
        let cx = PanelCtx {
            data: &data,
            settings: &settings,
        };
        let mut menu = ContextMenu::player(&cx);
        assert_eq!(
            value_of(&menu, "Sleep timer").as_deref(),
            Some("25 min left")
        );
        choose(&mut menu, "Sleep timer");
        let page = menu.panel().page();
        assert!(page.rows().iter().all(|r| r.checked() == Some(false)));
        assert_eq!(
            choose(&mut menu, "Off"),
            done(Command::SleepTimer { minutes: 0 })
        );
    }
}
