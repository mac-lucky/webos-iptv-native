//! Settings: the one list of keys and defaults, the stored values, and the Settings panel tree.
//!
//! Every value is a string, because that is what [`Command::SetSetting`] carries and what the
//! app stores. [`SPECS`] declares each key's default and what it accepts; [`Settings`] holds the
//! current values and refuses anything a spec does not accept, so a stale or hand-edited store can
//! never put an impossible value in front of a screen. Other screens read typed values through
//! [`Settings::flag`] and [`Settings::number`] with the constants in [`keys`].

use std::collections::BTreeMap;

use iptv::redact;
use tv_machine::machine::Measure;
use tv_platform::i18n::msg;
use tv_ui::Painter;
use Label::{Days, Hours, Millis, Minutes, Seconds};

use super::side::{Page, Row, SidePanel};
use super::{PanelKey, TextField};
use crate::data::{Command, Logos, Navigate, Output, Screen, TvData};
use crate::input::{Action, Hold, Key, Press};
use crate::theme::Accent;

mod backup;
pub use backup::BackupRow;

/// Setting keys. Stored as given; renaming one orphans what users saved under the old name.
pub mod keys {
    pub const LAST_CHANNEL_ON_START: &str = "general.last_channel_on_start";
    /// The History rules are storage's: it reads these two itself, and re-dates the kept
    /// entries when the second changes.
    pub const HISTORY_MIN_SECS: &str = iptv::storage::HISTORY_MIN_SECS;
    pub const HISTORY_KEEP_DAYS: &str = iptv::storage::HISTORY_DAYS;
    pub const PLAYLIST_URL: &str = "playlist.url";
    pub const PLAYLIST_UPDATE_ON_START: &str = "playlist.update_on_start";
    /// A guide URL that replaces the playlist's own; empty means the playlist's.
    pub const EPG_URL: &str = "epg.url";
    pub const EPG_UPDATE_HOURS: &str = "epg.update_hours";
    pub const EPG_PAST_DAYS: &str = "epg.past_days";
    pub const PLAYER_INFO_ON_ZAP: &str = "player.info_on_zap";
    pub const PLAYER_INFO_SECS: &str = "player.info_secs";
    /// A channel starts with subtitles on when it has any.
    pub const SUBTITLES_SHOW: &str = "subtitles.show";
    /// `small`, `medium` or `large`.
    pub const SUBTITLES_SIZE: &str = "subtitles.size";
    /// How the picture fills the screen ([`crate::playback::DisplayMode::value`]).
    pub const PLAYER_DISPLAY_MODE: &str = "player.display_mode";
    pub const GROUPS_SHOW_FAVORITES: &str = "groups.show_favorites";
    pub const GROUPS_SHOW_ALL: &str = "groups.show_all";
    pub const GROUPS_SHOW_HISTORY: &str = "groups.show_history";
    /// Playlist group names hidden from the groups panel, one per line.
    pub const GROUPS_HIDDEN: &str = "groups.hidden";
    /// `playlist`, `number` or `name`.
    pub const GROUPS_CHANNEL_SORT: &str = "groups.channel_sort";
    pub const SKIP_BACK_SECS: &str = "playback.skip_back_secs";
    pub const SKIP_FORWARD_SECS: &str = "playback.skip_forward_secs";
    /// The audio offset of every channel without one of its own, ms ([`crate::playback`]).
    pub const AUDIO_OFFSET_MS: &str = "playback.audio_offset_ms";
    /// The channels' own audio offsets, `<key hex> <ms>` per line, most recently adjusted first;
    /// set from the player menu ([`crate::playback::with_channel_offset`]).
    pub const CHANNEL_AUDIO_OFFSETS: &str = "playback.channel_audio_offsets";
    pub const LONG_PRESS_MS: &str = "remote.long_press_ms";
    /// A [`crate::theme::Accent`] key; the shell paints the accent roles in it.
    pub const ACCENT_COLOR: &str = "appearance.accent_color";
    /// The parental PIN's salted verifier ([`crate::parental::hash_pin`]); empty is 0000.
    pub const PARENTAL_PIN: &str = "parental.pin";
    /// Blocked channels, one [`iptv::model::ChannelKey::hex`] per line.
    pub const PARENTAL_CHANNELS: &str = "parental.channels";
    /// Blocked playlist group names, one per line.
    pub const PARENTAL_GROUPS: &str = "parental.groups";
    /// Prefixes and suffixes cut from channel names, one per line ([`crate::names`]).
    pub const NAMES_PREFIXES: &str = "names.prefixes";
    pub const NAMES_SUFFIXES: &str = "names.suffixes";
    /// Programme reminders, one per line ([`crate::reminders`]).
    pub const REMINDERS: &str = "reminders.list";
}

/// What a key accepts.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    /// `true` or `false`.
    Toggle,
    /// One of the listed stored values, each with its label.
    Choice(&'static [(&'static str, Label)]),
    /// An http(s) URL or empty; drawn masked, entered with the TV keyboard. `empty` labels "".
    Url { empty: fn() -> &'static str },
    /// Newline-separated names, edited from menus rather than the Settings tree.
    List,
}

/// How a choice reads, resolved in the UI language when drawn.
#[derive(Clone, Copy, Debug)]
pub enum Label {
    Text(fn() -> &'static str),
    Hours(i64),
    Days(i64),
    Minutes(i64),
    Seconds(i64),
    Millis(i64),
    /// A signed audio offset, ms ([`crate::playback::offset_label`]).
    Offset(i64),
}

impl Label {
    /// The label in the UI language.
    pub fn text(self) -> String {
        let cx = tv_platform::i18n::current();
        match self {
            Label::Text(text) => text().to_owned(),
            Label::Hours(n) => msg::unit_hours(n),
            Label::Days(n) => msg::unit_days(n),
            Label::Minutes(n) => msg::core_duration_long_minutes(n),
            Label::Seconds(n) => msg::core_seconds(&cx.number(n)),
            Label::Millis(n) => msg::unit_milliseconds(n),
            Label::Offset(n) => crate::playback::offset_label(n),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub key: &'static str,
    pub label: fn() -> &'static str,
    pub default: &'static str,
    pub kind: Kind,
}

const HOURS: &[(&str, Label)] = &[
    ("6", Hours(6)),
    ("12", Hours(12)),
    ("24", Hours(24)),
    ("48", Hours(48)),
];
/// No more than the week of past programmes a guide import keeps ([`iptv::xmltv::WINDOW_SECS`]).
const PAST_DAYS: &[(&str, Label)] = &[
    ("1", Days(1)),
    ("2", Days(2)),
    ("3", Days(3)),
    ("5", Days(5)),
    ("7", Days(7)),
];
const HISTORY_MIN: &[(&str, Label)] = &[
    ("5", Seconds(5)),
    ("10", Seconds(10)),
    ("30", Seconds(30)),
    ("60", Minutes(1)),
];
const HISTORY_KEEP: &[(&str, Label)] = &[
    ("1", Days(1)),
    ("3", Days(3)),
    ("7", Days(7)),
    ("14", Days(14)),
    ("30", Days(30)),
];
const INFO_SECS: &[(&str, Label)] = &[
    ("3", Seconds(3)),
    ("5", Seconds(5)),
    ("8", Seconds(8)),
    ("10", Seconds(10)),
];
const SORT: &[(&str, Label)] = &[
    ("playlist", Label::Text(msg::settings_sort_playlist)),
    ("number", Label::Text(msg::settings_sort_number)),
    ("name", Label::Text(msg::settings_name)),
];
const SKIP_BACK: &[(&str, Label)] = &[
    ("5", Seconds(5)),
    ("10", Seconds(10)),
    ("15", Seconds(15)),
    ("30", Seconds(30)),
    ("60", Minutes(1)),
];
const SKIP_FORWARD: &[(&str, Label)] = &[
    ("10", Seconds(10)),
    ("15", Seconds(15)),
    ("30", Seconds(30)),
    ("60", Minutes(1)),
    ("120", Minutes(2)),
];
const SUBTITLE_SIZES: &[(&str, Label)] = &[
    ("small", Label::Text(msg::subtitles_size_small)),
    ("medium", Label::Text(msg::subtitles_size_medium)),
    ("large", Label::Text(msg::subtitles_size_large)),
];
const LONG_PRESS: &[(&str, Label)] = &[
    ("300", Millis(300)),
    ("400", Millis(400)),
    ("500", Millis(500)),
    ("600", Millis(600)),
    ("800", Millis(800)),
    ("1000", Seconds(1)),
];
/// In [`crate::playback::DisplayMode::ALL`]'s order.
const DISPLAY_MODES: &[(&str, Label)] = &[
    ("normal", Label::Text(msg::playback_display_normal)),
    ("fill", Label::Text(msg::playback_display_fill)),
    ("4:3", Label::Text(msg::playback_display_4_3)),
    ("16:9", Label::Text(msg::playback_display_16_9)),
];
/// Every step of [`crate::playback::AUDIO_OFFSET_STEP_MS`] across the range; the player menu's
/// adjuster offers the same.
#[rustfmt::skip]
const AUDIO_OFFSETS: &[(&str, Label)] = {
    use Label::Offset as O;
    &[
        ("-1000", O(-1000)), ("-950", O(-950)), ("-900", O(-900)), ("-850", O(-850)),
        ("-800", O(-800)), ("-750", O(-750)), ("-700", O(-700)), ("-650", O(-650)),
        ("-600", O(-600)), ("-550", O(-550)), ("-500", O(-500)), ("-450", O(-450)),
        ("-400", O(-400)), ("-350", O(-350)), ("-300", O(-300)), ("-250", O(-250)),
        ("-200", O(-200)), ("-150", O(-150)), ("-100", O(-100)), ("-50", O(-50)),
        ("0", O(0)),
        ("50", O(50)), ("100", O(100)), ("150", O(150)), ("200", O(200)),
        ("250", O(250)), ("300", O(300)), ("350", O(350)), ("400", O(400)),
        ("450", O(450)), ("500", O(500)), ("550", O(550)), ("600", O(600)),
        ("650", O(650)), ("700", O(700)), ("750", O(750)), ("800", O(800)),
        ("850", O(850)), ("900", O(900)), ("950", O(950)), ("1000", O(1000)),
    ]
};
/// In [`Accent::ALL`]'s order.
const ACCENTS: &[(&str, Label)] = &[
    (Accent::Blue.key(), Label::Text(msg::accent_blue)),
    (Accent::Green.key(), Label::Text(msg::accent_green)),
    (Accent::Red.key(), Label::Text(msg::accent_red)),
    (Accent::Orange.key(), Label::Text(msg::accent_orange)),
    (Accent::Purple.key(), Label::Text(msg::accent_purple)),
    (Accent::Teal.key(), Label::Text(msg::accent_teal)),
];

/// Every setting, with its default. The single source for keys, defaults and accepted values.
pub const SPECS: &[Spec] = &[
    Spec {
        key: keys::LAST_CHANNEL_ON_START,
        label: msg::settings_auto_play,
        default: "true",
        kind: Kind::Toggle,
    },
    Spec {
        key: keys::HISTORY_MIN_SECS,
        label: msg::settings_history_min,
        default: "10",
        kind: Kind::Choice(HISTORY_MIN),
    },
    Spec {
        key: keys::HISTORY_KEEP_DAYS,
        label: msg::settings_history_keep,
        default: "7",
        kind: Kind::Choice(HISTORY_KEEP),
    },
    Spec {
        key: keys::PLAYLIST_URL,
        label: msg::settings_playlist_url,
        default: "",
        kind: Kind::Url {
            empty: msg::settings_not_set,
        },
    },
    Spec {
        key: keys::PLAYLIST_UPDATE_ON_START,
        label: msg::settings_update_on_start,
        default: "true",
        kind: Kind::Toggle,
    },
    Spec {
        key: keys::EPG_URL,
        label: msg::settings_epg_url,
        default: "",
        kind: Kind::Url {
            empty: msg::settings_from_playlist,
        },
    },
    Spec {
        key: keys::EPG_UPDATE_HOURS,
        label: msg::settings_epg_refresh_interval,
        default: "24",
        kind: Kind::Choice(HOURS),
    },
    Spec {
        key: keys::EPG_PAST_DAYS,
        label: msg::settings_past_days,
        default: "7",
        kind: Kind::Choice(PAST_DAYS),
    },
    Spec {
        key: keys::PLAYER_INFO_ON_ZAP,
        label: msg::settings_info_on_zap,
        default: "true",
        kind: Kind::Toggle,
    },
    Spec {
        key: keys::PLAYER_INFO_SECS,
        label: msg::settings_player_osd_timeout,
        default: "5",
        kind: Kind::Choice(INFO_SECS),
    },
    Spec {
        key: keys::PLAYER_DISPLAY_MODE,
        label: msg::playback_display_mode,
        default: "normal",
        kind: Kind::Choice(DISPLAY_MODES),
    },
    Spec {
        key: keys::GROUPS_SHOW_FAVORITES,
        label: msg::settings_show_favorites,
        default: "true",
        kind: Kind::Toggle,
    },
    Spec {
        key: keys::GROUPS_SHOW_ALL,
        label: msg::settings_show_all,
        default: "true",
        kind: Kind::Toggle,
    },
    Spec {
        key: keys::GROUPS_SHOW_HISTORY,
        label: msg::settings_show_history,
        default: "true",
        kind: Kind::Toggle,
    },
    Spec {
        key: keys::GROUPS_HIDDEN,
        label: msg::settings_hidden_groups,
        default: "",
        kind: Kind::List,
    },
    Spec {
        key: keys::GROUPS_CHANNEL_SORT,
        label: msg::group_channel_sorting,
        default: "playlist",
        kind: Kind::Choice(SORT),
    },
    Spec {
        key: keys::SKIP_BACK_SECS,
        label: msg::settings_skip_back,
        default: "10",
        kind: Kind::Choice(SKIP_BACK),
    },
    Spec {
        key: keys::SKIP_FORWARD_SECS,
        label: msg::settings_skip_forward,
        default: "30",
        kind: Kind::Choice(SKIP_FORWARD),
    },
    Spec {
        key: keys::SUBTITLES_SHOW,
        label: msg::subtitles_show,
        default: "false",
        kind: Kind::Toggle,
    },
    Spec {
        key: keys::SUBTITLES_SIZE,
        label: msg::subtitles_size,
        default: "medium",
        kind: Kind::Choice(SUBTITLE_SIZES),
    },
    Spec {
        key: keys::AUDIO_OFFSET_MS,
        label: msg::playback_default_audio_offset,
        default: "0",
        kind: Kind::Choice(AUDIO_OFFSETS),
    },
    // Set from the player menu, a line per channel.
    Spec {
        key: keys::CHANNEL_AUDIO_OFFSETS,
        label: msg::playback_audio_offset,
        default: "",
        kind: Kind::List,
    },
    Spec {
        key: keys::LONG_PRESS_MS,
        label: msg::settings_long_press,
        default: "500",
        kind: Kind::Choice(LONG_PRESS),
    },
    Spec {
        key: keys::ACCENT_COLOR,
        label: msg::accent_color,
        default: Accent::Blue.key(),
        kind: Kind::Choice(ACCENTS),
    },
    // Edited through the PIN dialog, the long-OK menus, the name editor and the guide, never
    // as plain rows.
    Spec {
        key: keys::PARENTAL_PIN,
        label: msg::parental_change_pin,
        default: "",
        kind: Kind::List,
    },
    Spec {
        key: keys::PARENTAL_CHANNELS,
        label: msg::parental_locked_channels,
        default: "",
        kind: Kind::List,
    },
    Spec {
        key: keys::PARENTAL_GROUPS,
        label: msg::parental_locked_groups,
        default: "",
        kind: Kind::List,
    },
    Spec {
        key: keys::NAMES_PREFIXES,
        label: msg::names_prefixes,
        default: "",
        kind: Kind::List,
    },
    Spec {
        key: keys::NAMES_SUFFIXES,
        label: msg::names_suffixes,
        default: "",
        kind: Kind::List,
    },
    Spec {
        key: keys::REMINDERS,
        label: msg::reminder_list,
        default: "",
        kind: Kind::List,
    },
];

/// The longest URL or list value accepted; anything longer is not a value a person typed.
const MAX_TEXT: usize = 4096;

pub fn spec(key: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.key == key)
}

/// Whether `value` is acceptable for `spec`.
pub fn accepts(spec: &Spec, value: &str) -> bool {
    match spec.kind {
        Kind::Toggle => matches!(value, "true" | "false"),
        Kind::Choice(options) => options.iter().any(|(v, _)| *v == value),
        Kind::Url { .. } => value.is_empty() || is_http_url(value),
        Kind::List => {
            value.len() <= MAX_TEXT && !value.chars().any(|c| c.is_control() && c != '\n')
        }
    }
}

/// An absolute http or https URL with a host and no whitespace or control characters.
pub fn is_http_url(value: &str) -> bool {
    if value.len() > MAX_TEXT || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let lower = value.get(..8).unwrap_or(value).to_ascii_lowercase();
    let rest = if lower.starts_with("https://") {
        &value[8..]
    } else if lower.starts_with("http://") {
        &value[7..]
    } else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or("");
    !host.is_empty() && !host.starts_with(':')
}

/// Current values of every [`SPECS`] key. Missing or refused values read as the default.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Settings {
    values: BTreeMap<&'static str, String>,
}

/// URL values masked ([`redact::url`]): the playlist and guide URLs carry account tokens.
impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = f.debug_map();
        for (key, value) in &self.values {
            match spec(key).map(|s| s.kind) {
                Some(Kind::Url { .. }) => map.entry(key, &redact::url(value)),
                // Four digits are quick to find from their verifier: it is never printed.
                _ if *key == keys::PARENTAL_PIN => map.entry(key, &"<set>"),
                _ => map.entry(key, value),
            };
        }
        map.finish()
    }
}

impl Settings {
    /// Everything at its default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Values from storage. Unknown keys and values a spec refuses are dropped.
    pub fn from_stored<K: AsRef<str>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        let mut out = Self::default();
        for (k, v) in pairs {
            out.set(k.as_ref(), v.into());
        }
        out
    }

    /// Set a value if its spec accepts it. Returns whether anything changed. A URL's secrets are
    /// registered for masking ([`redact::register_secrets`]) first, so no log line shows them
    /// from before the value is stored anywhere.
    pub fn set(&mut self, key: &str, value: String) -> bool {
        let Some(spec) = spec(key) else {
            return false;
        };
        if !accepts(spec, &value) {
            return false;
        }
        if matches!(spec.kind, Kind::Url { .. }) && !value.is_empty() {
            redact::register_secrets(&value);
        }
        if self.get(key) == value {
            return false;
        }
        if value == spec.default {
            self.values.remove(spec.key);
        } else {
            self.values.insert(spec.key, value);
        }
        true
    }

    /// The value of `key`, or its default. Unknown keys read as "".
    pub fn get(&self, key: &str) -> &str {
        match self.values.get(key) {
            Some(v) => v,
            None => spec(key).map_or("", |s| s.default),
        }
    }

    /// A toggle's value; `false` for keys that are not toggles.
    pub fn flag(&self, key: &str) -> bool {
        self.get(key) == "true"
    }

    /// A numeric choice's value; 0 for keys that are not numbers.
    pub fn number(&self, key: &str) -> u32 {
        self.get(key).parse().unwrap_or(0)
    }

    /// The hidden playlist group names.
    pub fn hidden_groups(&self) -> impl Iterator<Item = &str> {
        self.get(keys::GROUPS_HIDDEN)
            .lines()
            .filter(|l| !l.is_empty())
    }

    /// The stored value of [`keys::GROUPS_HIDDEN`] with `group` added or removed.
    pub fn hidden_groups_toggled(&self, group: &str) -> String {
        let mut names: Vec<&str> = self.hidden_groups().collect();
        if let Some(i) = names.iter().position(|n| *n == group) {
            names.remove(i);
        } else {
            names.push(group);
        }
        names.join("\n")
    }

    /// The label a Settings row shows for `key`'s current value.
    pub fn display(&self, key: &str) -> String {
        let value = self.get(key);
        match spec(key).map(|s| s.kind) {
            Some(Kind::Toggle) => if value == "true" {
                msg::widgets_toggle_on()
            } else {
                msg::widgets_toggle_off()
            }
            .to_owned(),
            Some(Kind::Choice(options)) => options
                .iter()
                .find(|(v, _)| *v == value)
                .map_or_else(|| value.to_owned(), |(_, label)| label.text()),
            Some(Kind::Url { empty }) if value.is_empty() => empty().to_owned(),
            Some(Kind::Url { .. }) => redact::url(value),
            Some(Kind::List) | None => String::new(),
        }
    }

    /// Set `key` and return the command that persists it, or nothing when it is refused or the
    /// same.
    pub(crate) fn change(&mut self, key: &'static str, value: String) -> Option<Command> {
        self.set(key, value.clone()).then(|| Command::SetSetting {
            key: key.to_owned(),
            value,
        })
    }
}

/// Which "Update now" row; both send [`Command::RefreshNow`], which reloads the playlist and
/// its guides together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    Playlist,
    Epg,
}

/// A row of the Settings tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// A category or sub-category page.
    Open(Section),
    /// A setting row: toggles flip, choices open their options, URLs open the keyboard.
    Setting(&'static str),
    /// One option on a choice page.
    Pick(&'static str, &'static str),
    Refresh(Refresh),
    /// A row of the Backup pages.
    Backup(BackupRow),
    /// A row that opens a screen of its own over Settings.
    Opens(Opens),
}

/// The Settings rows that open a screen of their own: the PIN dialog, the name editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opens {
    ChangePin,
    ChannelNames,
    /// The library panel's playlists and groups.
    Playlists,
    Groups,
}

impl Opens {
    fn label(self) -> &'static str {
        match self {
            Opens::ChangePin => msg::parental_change_pin(),
            Opens::ChannelNames => msg::names_title(),
            Opens::Playlists => msg::library_playlists(),
            Opens::Groups => msg::library_groups(),
        }
    }

    fn navigate(self) -> Navigate {
        match self {
            Opens::ChangePin => Navigate::Pin(crate::parental::PinTask::Change),
            Opens::ChannelNames => Navigate::ChannelNames,
            Opens::Playlists => Navigate::Library(crate::library::LibraryPage::Playlists),
            Opens::Groups => Navigate::Library(crate::library::LibraryPage::Groups),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    General,
    Playlists,
    Epg,
    Appearance,
    Player,
    Groups,
    Playback,
    Remote,
    About,
    Backup,
    Parental,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::General => msg::settings_general(),
            Section::Playlists => msg::settings_playlists(),
            Section::Epg => msg::settings_epg(),
            Section::Appearance => msg::settings_appearance(),
            Section::Player => msg::settings_player(),
            Section::Groups => msg::common_groups(),
            Section::Playback => msg::settings_playback(),
            Section::Remote => msg::settings_remote(),
            Section::About => msg::settings_about(),
            Section::Backup => msg::backup_title(),
            Section::Parental => msg::parental_title(),
        }
    }

    fn items(self) -> &'static [Item] {
        use Item::{Open, Refresh as R, Setting};
        match self {
            Section::General => &[
                Setting(keys::LAST_CHANNEL_ON_START),
                Setting(keys::HISTORY_MIN_SECS),
                Setting(keys::HISTORY_KEEP_DAYS),
                Open(Section::Backup),
            ],
            Section::Playlists => &[
                Setting(keys::PLAYLIST_URL),
                Setting(keys::PLAYLIST_UPDATE_ON_START),
                R(Refresh::Playlist),
                Item::Opens(Opens::ChannelNames),
                Item::Opens(Opens::Playlists),
            ],
            Section::Epg => &[
                Setting(keys::EPG_URL),
                Setting(keys::EPG_UPDATE_HOURS),
                Setting(keys::EPG_PAST_DAYS),
                R(Refresh::Epg),
            ],
            Section::Appearance => &[
                Setting(keys::ACCENT_COLOR),
                Open(Section::Player),
                Open(Section::Groups),
            ],
            Section::Player => &[
                Setting(keys::PLAYER_INFO_ON_ZAP),
                Setting(keys::PLAYER_INFO_SECS),
                Setting(keys::PLAYER_DISPLAY_MODE),
            ],
            Section::Groups => &[
                Setting(keys::GROUPS_SHOW_FAVORITES),
                Setting(keys::GROUPS_SHOW_ALL),
                Setting(keys::GROUPS_SHOW_HISTORY),
                Setting(keys::GROUPS_CHANNEL_SORT),
                Item::Opens(Opens::Groups),
            ],
            Section::Playback => &[
                Setting(keys::SKIP_BACK_SECS),
                Setting(keys::SKIP_FORWARD_SECS),
                Setting(keys::AUDIO_OFFSET_MS),
                Setting(keys::SUBTITLES_SHOW),
                Setting(keys::SUBTITLES_SIZE),
            ],
            Section::Remote => &[Setting(keys::LONG_PRESS_MS)],
            Section::About => &[],
            Section::Backup => &[
                Item::Backup(BackupRow::Export),
                Item::Backup(BackupRow::Import),
            ],
            Section::Parental => &[Item::Opens(Opens::ChangePin)],
        }
    }
}

const ROOT: &[Section] = &[
    Section::General,
    Section::Playlists,
    Section::Epg,
    Section::Appearance,
    Section::Playback,
    Section::Remote,
    Section::Parental,
    Section::About,
];

/// Where the source of this build can be had: the GPL offer and the FFmpeg recipe.
pub const SOURCE_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// The About page: version, licences, the FFmpeg notice and the source offer.
pub fn about_text(version: &str) -> Vec<String> {
    vec![
        format!("webos-iptv-rs {version}"),
        msg::about_copyright().to_owned(),
        msg::about_licence().to_owned(),
        msg::about_derived().to_owned(),
        msg::about_ffmpeg(SOURCE_URL),
        msg::about_source(SOURCE_URL),
        msg::about_fonts().to_owned(),
    ]
}

/// The Settings side panel: the category tree, choice pages and the URL entry.
pub struct SettingsPanel {
    panel: SidePanel<Item>,
    editor: Editor,
}

/// What the panel's rows edit.
struct Editor {
    settings: Settings,
    version: String,
    /// The setting and its draft, while the URL entry page is open.
    entry: Option<(&'static str, TextField)>,
}

impl SettingsPanel {
    /// Open on the category list. `version` is the app version shown in About.
    pub fn new(settings: Settings, version: &str) -> Self {
        let rows = ROOT
            .iter()
            .map(|&sec| Row::submenu(Item::Open(sec), sec.title()))
            .collect();
        Self {
            panel: SidePanel::new(Page::list(msg::common_settings(), rows)),
            editor: Editor {
                settings,
                version: version.to_owned(),
                entry: None,
            },
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.editor.settings
    }

    pub fn panel(&self) -> &SidePanel<Item> {
        &self.panel
    }
}

impl Screen for SettingsPanel {
    fn hold(&self, key: Key) -> Hold {
        PanelKey::hold(key)
    }

    fn key(&mut self, key: Key, press: Press, _: &dyn TvData, _: u64) -> Output {
        let Some(key) = PanelKey::from_action(Action { key, press }) else {
            return Output::default();
        };
        if self.editor.entry.is_some() {
            return Output {
                redraw: true,
                ..self.editor.entry_key(&mut self.panel, key)
            };
        }
        let editor = &mut self.editor;
        self.panel
            .handle(key, |panel, item| editor.activate(panel, item))
    }

    fn text(&mut self, text: &str, _: &dyn TvData) -> Output {
        let Some((_, field)) = self.editor.entry.as_mut() else {
            return Output::default();
        };
        field.on_text(text);
        self.editor.show_entry(&mut self.panel);
        Output::redraw()
    }

    /// The URL entry is open and typing.
    fn editing(&self) -> bool {
        self.editor.entry.as_ref().is_some_and(|(_, f)| f.editing())
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

impl Editor {
    /// Keys on the URL entry page. OK raises the keyboard, or saves while it is up; Back leaves
    /// without saving.
    fn entry_key(&mut self, panel: &mut SidePanel<Item>, key: PanelKey) -> Output {
        let Some((spec_key, field)) = self.entry.as_mut() else {
            return Output::default();
        };
        let spec_key = *spec_key;
        match key {
            PanelKey::Ok if field.editing() => {
                let value = field.text().trim().to_owned();
                if !value.is_empty() && !is_http_url(&value) {
                    return Output {
                        toast: Some(msg::settings_bad_url().to_owned()),
                        ..Output::default()
                    };
                }
                let saved = self.settings.change(spec_key, value);
                self.close_entry(panel);
                saved.map_or_else(Output::default, Output::command)
            }
            PanelKey::Ok => {
                field.set_editing(true);
                self.show_entry(panel);
                Output::default()
            }
            // Left is the keyboard's own caret key while it is up, so only Back leaves
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

    fn close_entry(&mut self, panel: &mut SidePanel<Item>) {
        self.entry = None;
        panel.pop();
        self.refresh_values(panel);
    }

    /// The field shows the draft masked, like every other place a URL appears.
    fn show_entry(&self, panel: &mut SidePanel<Item>) {
        if let Some((_, field)) = &self.entry {
            let shown = if field.text().is_empty() {
                String::new()
            } else {
                redact::url(field.text())
            };
            panel.set_field(&shown, field.editing());
        }
    }

    fn activate(&mut self, panel: &mut SidePanel<Item>, item: Item) -> Output {
        match item {
            Item::Open(Section::About) => {
                panel.push(Page::text(msg::settings_about(), about_text(&self.version)));
                Output::default()
            }
            Item::Open(sec) => {
                let rows = sec.items().iter().map(|&i| self.row(i)).collect();
                panel.push(Page::list(sec.title(), rows));
                Output::default()
            }
            Item::Setting(key) => self.setting(panel, key),
            Item::Backup(row) => backup::activate(panel, row),
            Item::Opens(opens) => Output::navigate(opens.navigate()),
            Item::Pick(key, value) => {
                let saved = self.settings.change(key, value.to_owned());
                panel.pop();
                self.refresh_values(panel);
                saved.map_or_else(Output::default, Output::command)
            }
            Item::Refresh(what) => {
                let toast = match what {
                    Refresh::Playlist => msg::settings_updating_playlist(),
                    Refresh::Epg => msg::settings_updating_epg(),
                };
                Output {
                    toast: Some(toast.to_owned()),
                    ..Output::command(Command::RefreshNow)
                }
            }
        }
    }

    fn setting(&mut self, panel: &mut SidePanel<Item>, key: &'static str) -> Output {
        let Some(spec) = spec(key) else {
            return Output::default();
        };
        match spec.kind {
            Kind::Toggle => {
                let next = if self.settings.flag(key) {
                    "false"
                } else {
                    "true"
                };
                let saved = self.settings.change(key, next.to_owned());
                self.refresh_values(panel);
                saved.map_or_else(Output::default, Output::command)
            }
            Kind::Choice(options) => {
                let current = self.settings.get(key);
                let rows = options
                    .iter()
                    .map(|&(value, label)| {
                        Row::check(Item::Pick(key, value), &label.text(), value == current)
                    })
                    .collect();
                let focus = options.iter().position(|(v, _)| *v == current).unwrap_or(0);
                panel.push(Page::list((spec.label)(), rows).focused(focus));
                Output::default()
            }
            Kind::Url { .. } => {
                let mut field = TextField::new(self.settings.get(key));
                field.set_editing(true);
                panel.push(Page::entry((spec.label)()));
                self.entry = Some((key, field));
                self.show_entry(panel);
                Output::default()
            }
            Kind::List => Output::default(),
        }
    }

    fn row(&self, item: Item) -> Row<Item> {
        match item {
            Item::Open(sec) => Row::submenu(item, sec.title()),
            Item::Setting(key) => {
                let spec = spec(key);
                let row = Row::action(item, spec.map_or(key, |s| (s.label)()))
                    .value(&self.settings.display(key));
                match spec.map(|s| s.kind) {
                    Some(Kind::Choice(_) | Kind::Url { .. }) => row.chevron(),
                    _ => row,
                }
            }
            Item::Refresh(Refresh::Playlist) => Row::action(item, msg::settings_update_playlist()),
            Item::Refresh(Refresh::Epg) => Row::action(item, msg::settings_update_epg()),
            Item::Pick(_, label) => Row::action(item, label),
            Item::Backup(row) => backup::row(row),
            Item::Opens(opens) => Row::submenu(item, opens.label()),
        }
    }

    /// Re-read every setting row's value after a change.
    fn refresh_values(&self, panel: &mut SidePanel<Item>) {
        let settings = &self.settings;
        panel.for_each_row(|row| {
            if let Item::Setting(key) = row.id {
                row.set_value(&settings.display(key));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::FakeData;
    use crate::panels::testing::{draw, press};

    fn set(key: &str, value: &str) -> Output {
        Output::command(Command::SetSetting {
            key: key.to_owned(),
            value: value.to_owned(),
        })
    }

    /// Short presses of `keys`, chained.
    fn keys(panel: &mut SettingsPanel, keys: &[Key]) -> Output {
        press(panel, &FakeData::new(0, 0), keys)
    }

    fn type_text(panel: &mut SettingsPanel, text: &str) -> Output {
        panel.text(text, &FakeData::new(0, 0))
    }

    /// Focus the root row titled `title` and open it.
    fn open(panel: &mut SettingsPanel, title: &str) {
        let i = panel
            .panel()
            .page()
            .rows()
            .iter()
            .position(|r| r.label() == title)
            .unwrap();
        keys(panel, &vec![Key::Down; i]);
        assert_eq!(keys(panel, &[Key::Ok]), Output::default());
        assert_eq!(panel.panel().page().title(), title);
    }

    fn labels(panel: &SettingsPanel) -> Vec<(String, Option<String>)> {
        let rows = panel.panel().page().rows();
        rows.iter()
            .map(|r| (r.label().to_owned(), r.value_text().map(str::to_owned)))
            .collect()
    }

    #[test]
    fn values_read_in_the_ui_language() {
        let s = Settings::from_stored([(keys::EPG_PAST_DAYS, "1"), (keys::LONG_PRESS_MS, "1000")]);
        assert_eq!(s.display(keys::EPG_PAST_DAYS), "1 day");
        assert_eq!(s.display(keys::LONG_PRESS_MS), "1 s");
        let _pl =
            tv_platform::i18n::language_on_this_thread_for_test(tv_platform::i18n::Preference::Pl);
        let s =
            Settings::from_stored([(keys::EPG_PAST_DAYS, "2"), (keys::SKIP_FORWARD_SECS, "120")]);
        assert_eq!(s.display(keys::EPG_PAST_DAYS), "2 dni");
        assert_eq!(s.display(keys::EPG_UPDATE_HOURS), "24 godz.");
        assert_eq!(s.display(keys::SKIP_FORWARD_SECS), "2 min");
        assert_eq!(s.display(keys::LAST_CHANNEL_ON_START), "W\u{142}.");
        assert_eq!(s.display(keys::PLAYLIST_URL), "Nie ustawiono");
        let panel = SettingsPanel::new(Settings::new(), "1.0");
        assert_eq!(panel.panel().page().title(), "Ustawienia");
    }

    #[test]
    fn specs_are_unique_and_defaults_valid() {
        let s = Settings::new();
        for (i, spec) in SPECS.iter().enumerate() {
            assert!(
                SPECS[..i].iter().all(|o| o.key != spec.key),
                "{} twice",
                spec.key
            );
            assert!(accepts(spec, spec.default), "{} default", spec.key);
            assert_eq!(s.get(spec.key), spec.default);
        }
        assert_eq!(s.number(keys::LONG_PRESS_MS), 500);
        assert!(s.flag(keys::LAST_CHANNEL_ON_START));
        assert_eq!(s.get("unknown.key"), "");
    }

    #[test]
    fn the_history_rows_default_to_what_storage_applies_without_them() {
        let storage = iptv::storage::Storage::open_in_memory().unwrap();
        let s = Settings::new();
        assert_eq!(
            s.number(keys::HISTORY_MIN_SECS),
            storage.history_min_secs().unwrap()
        );
        assert_eq!(
            s.number(keys::HISTORY_KEEP_DAYS),
            storage.history_days().unwrap()
        );
        // Every value the rows offer is one storage takes as it is.
        for key in [keys::HISTORY_MIN_SECS, keys::HISTORY_KEEP_DAYS] {
            let Some(Kind::Choice(options)) = spec(key).map(|s| s.kind) else {
                panic!("{key} is a choice");
            };
            for (value, _) in options {
                storage.set_setting(key, value).unwrap();
                let read = match key {
                    keys::HISTORY_MIN_SECS => storage.history_min_secs(),
                    _ => storage.history_days(),
                };
                assert_eq!(read.unwrap().to_string(), *value, "{key}");
            }
        }
    }

    #[test]
    fn general_holds_the_start_and_history_rows() {
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        open(&mut panel, "General");
        assert_eq!(
            labels(&panel),
            [
                (
                    "Start with the last channel".to_owned(),
                    Some("On".to_owned())
                ),
                ("Add to History after".to_owned(), Some("10 s".to_owned())),
                ("Keep History for".to_owned(), Some("7 days".to_owned())),
                ("Backup".to_owned(), None),
            ]
        );
        assert_eq!(
            keys(&mut panel, &[Key::Down, Key::Ok, Key::Down, Key::Ok]),
            set(keys::HISTORY_MIN_SECS, "30")
        );
        assert_eq!(
            keys(&mut panel, &[Key::Down, Key::Ok, Key::Up, Key::Ok]),
            set(keys::HISTORY_KEEP_DAYS, "3")
        );
        assert_eq!(labels(&panel)[2].1.as_deref(), Some("3 days"));
    }

    #[test]
    fn stored_values_are_validated() {
        let s = Settings::from_stored([
            ("remote.long_press_ms", "250"),
            ("no.such.key", "x"),
            ("guide.rows", "8"),
            ("player.info_secs", "8"),
            ("playlist.url", "ftp://host.test/list"),
            ("general.last_channel_on_start", "yes"),
            ("groups.hidden", "News\nSport"),
            ("epg.past_days", "14"),
        ]);
        assert_eq!(s.number(keys::EPG_PAST_DAYS), 7, "past the kept week");
        assert_eq!(s.number(keys::LONG_PRESS_MS), 500);
        assert_eq!(s.get("guide.rows"), "", "a key no setting has any more");
        assert_eq!(s.number(keys::PLAYER_INFO_SECS), 8);
        assert_eq!(s.get(keys::PLAYLIST_URL), "");
        assert!(s.flag(keys::LAST_CHANNEL_ON_START));
        assert_eq!(s.hidden_groups().collect::<Vec<_>>(), ["News", "Sport"]);
        assert_eq!(s.hidden_groups_toggled("News"), "Sport");
        assert_eq!(s.hidden_groups_toggled("Kids"), "News\nSport\nKids");
    }

    #[test]
    fn defaults_are_not_stored_and_unchanged_values_report_no_change() {
        let mut s = Settings::new();
        assert!(!s.set(keys::PLAYER_INFO_SECS, "5".into()));
        assert!(s.set(keys::PLAYER_INFO_SECS, "10".into()));
        assert!(!s.set(keys::PLAYER_INFO_SECS, "10".into()));
        assert!(s.set(keys::PLAYER_INFO_SECS, "5".into()));
        assert_eq!(s, Settings::new());
    }

    #[test]
    fn only_http_urls_with_a_host_are_accepted() {
        for ok in [
            "http://h.test",
            "HTTPS://h.test:8080/a?b=c",
            "http://u@h.test/x",
        ] {
            assert!(is_http_url(ok), "{ok}");
        }
        for bad in [
            "",
            "ftp://h.test",
            "http://",
            "http:///path",
            "http://h.test/a b",
            "file:///etc",
            "h.test",
            "http://:80/",
        ] {
            assert!(!is_http_url(bad), "{bad}");
        }
        assert!(!is_http_url(&format!(
            "http://h.test/{}",
            "a".repeat(MAX_TEXT)
        )));
    }

    #[test]
    fn setting_a_url_registers_its_secrets() {
        // Plain words: in free text only a registered secret is masked.
        let (account, key) = ("quinceaccount", "marmaladekey");
        let line = format!("refused for {account} with {key}");
        assert_eq!(redact::text(&line), line);
        let url = format!("http://pl.example.test/{account}/list.m3u?k={key}");
        let mut s = Settings::new();
        assert!(s.set(keys::GROUPS_HIDDEN, url.clone()));
        assert_eq!(redact::text(&line), line, "only URL settings register");
        assert!(!s.set(keys::PLAYLIST_URL, format!("ftp{}", &url[4..])));
        assert_eq!(redact::text(&line), line, "nor a refused value");
        assert!(s.set(keys::PLAYLIST_URL, url));
        let masked = redact::text(&line);
        assert!(
            !masked.contains(account) && !masked.contains(key),
            "{masked}"
        );
    }

    #[test]
    fn the_tree_has_the_tivimate_sections() {
        let panel = SettingsPanel::new(Settings::new(), "1.2.3");
        let titles: Vec<_> = labels(&panel).into_iter().map(|(l, _)| l).collect();
        assert_eq!(
            titles,
            [
                "General",
                "Playlists",
                "EPG",
                "Appearance",
                "Playback",
                "Remote control",
                "Parental control",
                "About"
            ]
        );
        let mut panel = panel;
        open(&mut panel, "Appearance");
        let subs: Vec<_> = labels(&panel).into_iter().map(|(l, _)| l).collect();
        assert_eq!(subs, ["Accent color", "Player", "Groups"]);
        open(&mut panel, "Groups");
        assert_eq!(
            labels(&panel)[0],
            ("Favorites group".to_owned(), Some("On".to_owned()))
        );
    }

    #[test]
    fn toggles_flip_and_persist() {
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        open(&mut panel, "General");
        assert_eq!(
            keys(&mut panel, &[Key::Ok]),
            set(keys::LAST_CHANNEL_ON_START, "false")
        );
        assert_eq!(labels(&panel)[0].1.as_deref(), Some("Off"));
        assert_eq!(
            keys(&mut panel, &[Key::Ok]),
            set(keys::LAST_CHANNEL_ON_START, "true")
        );
        assert!(panel.settings().flag(keys::LAST_CHANNEL_ON_START));
        assert_eq!(keys(&mut panel, &[Key::Back, Key::Back]), Output::close());
        assert!(panel.closing());
    }

    #[test]
    fn choices_open_on_the_current_value_and_save_the_pick() {
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        open(&mut panel, "Remote control");
        assert_eq!(
            labels(&panel)[0],
            ("Long press".to_owned(), Some("500 ms".to_owned()))
        );
        keys(&mut panel, &[Key::Ok]);
        let page = panel.panel().page();
        assert_eq!(page.title(), "Long press");
        assert_eq!(page.focus(), 2);
        assert_eq!(page.rows()[2].checked(), Some(true));
        assert_eq!(
            keys(&mut panel, &[Key::Down, Key::Ok]),
            set(keys::LONG_PRESS_MS, "600")
        );
        assert_eq!(panel.panel().page().title(), "Remote control");
        assert_eq!(labels(&panel)[0].1.as_deref(), Some("600 ms"));
        // the same pick again changes nothing
        assert_eq!(keys(&mut panel, &[Key::Ok, Key::Ok]), Output::default());
    }

    #[test]
    fn the_subtitle_rows_sit_in_playback() {
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        open(&mut panel, "Playback");
        let rows = labels(&panel);
        let at = rows
            .iter()
            .position(|(label, _)| label == "Show subtitles")
            .unwrap();
        assert_eq!(rows[at].1.as_deref(), Some("Off"));
        assert_eq!(
            rows[at + 1],
            ("Subtitle size".to_owned(), Some("Medium".to_owned()))
        );
        keys(&mut panel, &vec![Key::Down; at]);
        assert_eq!(
            keys(&mut panel, &[Key::Ok]),
            set(keys::SUBTITLES_SHOW, "true")
        );
        keys(&mut panel, &[Key::Down, Key::Ok]);
        assert_eq!(panel.panel().page().title(), "Subtitle size");
        assert_eq!(
            keys(&mut panel, &[Key::Down, Key::Ok]),
            set(keys::SUBTITLES_SIZE, "large")
        );
        let s = Settings::from_stored([(keys::SUBTITLES_SIZE, "huge")]);
        assert_eq!(
            s.get(keys::SUBTITLES_SIZE),
            "medium",
            "an unknown size is refused"
        );
    }

    #[test]
    fn the_accent_color_is_picked_in_appearance() {
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        open(&mut panel, "Appearance");
        assert_eq!(
            labels(&panel)[0],
            ("Accent color".to_owned(), Some("Blue".to_owned()))
        );
        keys(&mut panel, &[Key::Ok]);
        let names: Vec<_> = labels(&panel).into_iter().map(|(l, _)| l).collect();
        assert_eq!(names, ["Blue", "Green", "Red", "Orange", "Purple", "Teal"]);
        assert_eq!(
            keys(&mut panel, &[Key::Down, Key::Ok]),
            set(keys::ACCENT_COLOR, "green")
        );
        assert_eq!(labels(&panel)[0].1.as_deref(), Some("Green"));
        assert_eq!(
            Accent::from_key(panel.settings().get(keys::ACCENT_COLOR)),
            Some(Accent::Green)
        );
        let stored = Settings::from_stored([(keys::ACCENT_COLOR, "pink")]);
        assert_eq!(stored.get(keys::ACCENT_COLOR), "blue", "refused");
        let _pl =
            tv_platform::i18n::language_on_this_thread_for_test(tv_platform::i18n::Preference::Pl);
        assert_eq!(panel.settings().display(keys::ACCENT_COLOR), "Zielony");
    }

    #[test]
    fn debug_output_masks_urls() {
        let token: String = "q7Rk".repeat(4).chars().take(13).collect();
        let mut settings = Settings::new();
        let url = format!("http://iptv.example/{token}/list.m3u?pass={token}");
        assert!(settings.set(keys::PLAYLIST_URL, url));
        assert!(settings.set(keys::GROUPS_CHANNEL_SORT, "name".into()));
        let shown = format!("{settings:?}");
        assert!(!shown.contains(&token), "{shown}");
        assert!(
            shown.contains("iptv.example") && shown.contains("\"name\""),
            "{shown}"
        );
    }

    #[test]
    fn playlist_url_is_typed_with_the_keyboard_and_never_shown_whole() {
        let secret = "Ab3".repeat(8);
        let url = &format!("http://pl.example.test/{secret}/get.php?token=SECRET9");
        let masked = redact::url(url);
        assert!(!masked.contains(&secret) && !masked.contains("SECRET9"));
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        open(&mut panel, "Playlists");
        assert_eq!(
            labels(&panel)[0],
            ("Playlist URL".to_owned(), Some("Not set".to_owned()))
        );
        assert!(!panel.editing());
        assert_eq!(keys(&mut panel, &[Key::Ok]), Output::default());
        assert!(panel.editing(), "the entry raises the keyboard");
        assert_eq!(type_text(&mut panel, url), Output::redraw());
        assert_eq!(panel.panel().page().field(), Some(masked.as_str()));
        assert_eq!(keys(&mut panel, &[Key::Ok]), set(keys::PLAYLIST_URL, url));
        assert!(!panel.editing(), "saving dismisses it");
        assert_eq!(panel.settings().get(keys::PLAYLIST_URL), url);
        assert_eq!(panel.panel().page().title(), "Playlists");
        assert_eq!(labels(&panel)[0].1.as_deref(), Some(masked.as_str()));
        // reopening masks the stored value too
        keys(&mut panel, &[Key::Ok]);
        assert_eq!(panel.panel().page().field(), Some(masked.as_str()));
    }

    #[test]
    fn a_bad_url_is_refused_and_back_discards_the_draft() {
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        open(&mut panel, "EPG");
        keys(&mut panel, &[Key::Ok]);
        type_text(&mut panel, "ftp://epg.test/guide.xml");
        assert_eq!(
            keys(&mut panel, &[Key::Ok]).toast.as_deref(),
            Some(msg::settings_bad_url())
        );
        assert!(panel.editing());
        keys(&mut panel, &[Key::Clear]);
        assert_eq!(panel.panel().page().field(), Some(""));
        assert_eq!(keys(&mut panel, &[Key::Back]), Output::default());
        assert!(!panel.editing());
        assert_eq!(panel.settings().get(keys::EPG_URL), "");
        assert_eq!(labels(&panel)[0].1.as_deref(), Some("From playlist"));
    }

    #[test]
    fn clearing_a_url_saves_empty() {
        let url = "http://epg.test/guide.xml";
        let mut panel = SettingsPanel::new(Settings::from_stored([(keys::EPG_URL, url)]), "1");
        open(&mut panel, "EPG");
        assert_eq!(
            keys(&mut panel, &[Key::Ok, Key::Backspace, Key::Clear, Key::Ok]),
            set(keys::EPG_URL, "")
        );
    }

    #[test]
    fn text_with_the_keyboard_believed_down_takes_it_as_up() {
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        open(&mut panel, "Playlists");
        keys(&mut panel, &[Key::Ok]);
        type_text(&mut panel, "http://a.test");
        let field = &mut panel.editor.entry.as_mut().unwrap().1;
        field.set_editing(false);
        assert!(!panel.editing());
        assert_eq!(type_text(&mut panel, "/"), Output::redraw());
        assert!(panel.editing());
        assert_eq!(panel.panel().page().field(), Some("http://a.test/"));
    }

    #[test]
    fn text_after_the_entry_closed_goes_nowhere() {
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        open(&mut panel, "Playlists");
        keys(&mut panel, &[Key::Ok]);
        type_text(&mut panel, "http://a.test");
        assert_eq!(keys(&mut panel, &[Key::Back]), Output::default());
        assert!(!panel.editing());
        assert_eq!(type_text(&mut panel, "x"), Output::default());
        assert!(!panel.editing());
        assert_eq!(panel.settings().get(keys::PLAYLIST_URL), "");
    }

    #[test]
    fn update_rows_ask_for_a_refresh() {
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        open(&mut panel, "EPG");
        assert_eq!(
            keys(&mut panel, &[Key::Down, Key::Down, Key::Down, Key::Ok]),
            Output {
                toast: Some(msg::settings_updating_epg().to_owned()),
                ..Output::command(Command::RefreshNow)
            }
        );
    }

    #[test]
    fn about_carries_the_licence_notices_and_the_source_offer() {
        let mut panel = SettingsPanel::new(Settings::new(), "0.4.1");
        open(&mut panel, "About");
        let body = panel.panel().page().body().join("\n");
        for needle in [
            "0.4.1",
            "GNU General Public License",
            "version 3",
            "FFmpeg",
            "LGPL-2.1-or-later",
            "replaceable",
            "https://github.com/",
        ] {
            assert!(body.contains(needle), "{needle}");
        }
        assert!(body.is_ascii());
        draw(&mut panel);
    }

    #[test]
    fn every_section_draws() {
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        panel.update(0.5);
        for title in ["General", "Playlists", "EPG", "Playback", "Remote control"] {
            open(&mut panel, title);
            draw(&mut panel);
            keys(&mut panel, &[Key::Back]);
            while panel.panel().page().focus() > 0 {
                keys(&mut panel, &[Key::Up]);
            }
        }
    }
}
