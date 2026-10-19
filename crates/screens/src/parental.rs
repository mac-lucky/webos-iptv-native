//! Parental controls: playlist groups and channels blocked behind a four-digit PIN.
//!
//! What is blocked lives in the settings ([`keys::PARENTAL_CHANNELS`], one channel key in hex
//! per line, and [`keys::PARENTAL_GROUPS`], one playlist group name per line); the PIN is stored
//! as a salted PBKDF2 verifier ([`keys::PARENTAL_PIN`]), never as digits, and reads as
//! [`DEFAULT_PIN`] while none is set. [`Parental`] is the running state the data layer answers
//! from: the blocked sets plus what the PIN opened since the app came to the front, until a new
//! lock ([`Parental::follow`]) or the background ([`Parental::relock`]). A blocked channel stays
//! listed everywhere, but playing it takes the PIN; a blocked group's list reads empty until the
//! PIN opens it, and its channels need the PIN wherever else they are listed.
//!
//! Every change to what is blocked, and the PIN itself, goes through the PIN dialog
//! ([`crate::panels::pin`]), which carries a [`PinTask`]: the shell opens it for a request
//! ([`Navigate::Pin`](crate::data::Navigate::Pin)) or for a play it held back.

use std::collections::HashSet;

use iptv::model::{Channel, ChannelKey};

use crate::data::{Group, Navigate, Output};
use crate::panels::keys;
use crate::panels::settings::{accepts, spec, Settings};

/// The PIN until one is set.
pub const DEFAULT_PIN: &str = "0000";
/// Digits in a PIN.
pub const PIN_LEN: usize = 4;
/// PBKDF2 rounds: a few tens of milliseconds on the TV. Four digits are ten thousand guesses
/// for anyone who can read the settings; the salt and the rounds only keep a PIN reused
/// elsewhere (a phone, a card) from being read straight off the stored value.
const ROUNDS: u32 = 4096;
const SALT_LEN: usize = 16;
/// The stored verifier's format tag.
const VERSION: &str = "v1";

/// Something the PIN opens until the app exits or goes to the background.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Lock {
    Channel(ChannelKey),
    /// A playlist group, by name.
    Group(String),
}

/// What the PIN dialog does once the right PIN is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinTask {
    /// Open `Lock` for the session ([`Command::Unlock`](crate::data::Command::Unlock)).
    Open(Lock),
    /// Store `value` under the parental setting `key`, then show `toast`.
    Set {
        key: &'static str,
        value: String,
        toast: String,
    },
    /// Ask for the current PIN, then a new one twice, and store it.
    Change,
}

/// The blocked channels and groups, and what the PIN opened since the app started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parental {
    channels: HashSet<ChannelKey>,
    groups: HashSet<String>,
    open: HashSet<Lock>,
}

impl Parental {
    pub fn from_settings(settings: &Settings) -> Self {
        let mut p = Parental::default();
        p.follow(settings);
        p
    }

    /// Read what is blocked again. What the PIN opened stays open, unless something new was
    /// locked: a lock takes effect at once, even on a channel the PIN opened by itself before
    /// its group was locked, or on one opened, unlocked and locked again.
    pub fn follow(&mut self, settings: &Settings) {
        let channels: HashSet<ChannelKey> = blocked_channels(settings).collect();
        let groups: HashSet<String> = blocked_groups(settings).map(str::to_owned).collect();
        if !channels.is_subset(&self.channels) || !groups.is_subset(&self.groups) {
            self.open.clear();
        }
        self.channels = channels;
        self.groups = groups;
    }

    /// The right PIN was entered for `lock`: it stays open until the app exits or goes to the
    /// background ([`relock`](Self::relock)), or something new is locked.
    pub fn open(&mut self, lock: Lock) {
        self.open.insert(lock);
    }

    /// Everything the PIN opened locks again (the app went to the background: whoever brings it
    /// back, after the TV was turned off and on, asks for the PIN anew).
    pub fn relock(&mut self) {
        self.open.clear();
    }

    /// Nothing is blocked.
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty() && self.groups.is_empty()
    }

    /// Playing `channel` takes the PIN: it, or a group it is in, is blocked, and the PIN has not
    /// opened it (or every blocked group of it) this session.
    pub fn locked(&self, channel: &Channel) -> bool {
        if self.is_empty() || self.open.contains(&Lock::Channel(channel.key)) {
            return false;
        }
        self.channels.contains(&channel.key) || channel.groups.iter().any(|g| self.name_locked(g))
    }

    /// `group` is a blocked playlist group the PIN has not opened this session.
    pub fn group_locked(&self, group: &Group) -> bool {
        match group {
            Group::Playlist(name) => self.name_locked(name),
            _ => false,
        }
    }

    fn name_locked(&self, name: &str) -> bool {
        self.groups.contains(name) && !self.open.contains(&Lock::Group(name.to_owned()))
    }
}

/// What OK on a locked group asks for: the PIN dialog that opens it.
pub fn ask_to_open(group: &Group) -> Output {
    match group {
        Group::Playlist(name) => {
            Output::navigate(Navigate::Pin(PinTask::Open(Lock::Group(name.clone()))))
        }
        _ => Output::default(),
    }
}

/// The blocked channels' keys; lines that are not a key are skipped.
pub fn blocked_channels(settings: &Settings) -> impl Iterator<Item = ChannelKey> + '_ {
    lines(settings.get(keys::PARENTAL_CHANNELS)).filter_map(ChannelKey::from_hex)
}

/// The blocked playlist group names.
pub fn blocked_groups(settings: &Settings) -> impl Iterator<Item = &str> {
    lines(settings.get(keys::PARENTAL_GROUPS))
}

pub fn channel_blocked(settings: &Settings, key: ChannelKey) -> bool {
    blocked_channels(settings).any(|k| k == key)
}

pub fn group_blocked(settings: &Settings, name: &str) -> bool {
    blocked_groups(settings).any(|g| g == name)
}

/// The value of [`keys::PARENTAL_CHANNELS`] with `key` added or removed; `None` when adding it
/// would make the list longer than a setting may be.
pub fn channels_toggled(settings: &Settings, key: ChannelKey) -> Option<String> {
    let mut list: Vec<ChannelKey> = blocked_channels(settings).collect();
    match list.iter().position(|&k| k == key) {
        Some(i) => {
            list.remove(i);
        }
        None => list.push(key),
    }
    let value = list.iter().map(|k| k.hex()).collect::<Vec<_>>().join("\n");
    fits(keys::PARENTAL_CHANNELS, value)
}

/// The value of [`keys::PARENTAL_GROUPS`] with `name` added or removed; `None` when adding it
/// would make the list longer than a setting may be.
pub fn groups_toggled(settings: &Settings, name: &str) -> Option<String> {
    let mut list: Vec<&str> = blocked_groups(settings).collect();
    match list.iter().position(|g| *g == name) {
        Some(i) => {
            list.remove(i);
        }
        None => list.push(name),
    }
    fits(keys::PARENTAL_GROUPS, list.join("\n"))
}

fn fits(key: &str, value: String) -> Option<String> {
    spec(key).filter(|s| accepts(s, &value)).map(|_| value)
}

fn lines(value: &str) -> impl Iterator<Item = &str> {
    value.lines().map(str::trim).filter(|l| !l.is_empty())
}

/// The stored verifier of `pin`: `v1:<salt>:<PBKDF2-HMAC-SHA-256>`, both in hex.
pub fn hash_pin(pin: &str) -> String {
    let salt = salt();
    format!("{VERSION}:{}:{}", hex(&salt), hex(&derive(pin, &salt)))
}

/// Whether `pin` is the PIN `stored` verifies. An empty or unreadable value verifies
/// [`DEFAULT_PIN`]: a damaged setting falls back to the documented default rather than
/// locking the household out of its own TV.
pub fn pin_matches(stored: &str, pin: &str) -> bool {
    match parse_verifier(stored) {
        Some((salt, hash)) => tv_base::sha256::ct_eq(&derive(pin, &salt), &hash),
        None => tv_base::sha256::ct_eq(pin.as_bytes(), DEFAULT_PIN.as_bytes()),
    }
}

/// Whether a PIN has been set (the stored value is a verifier).
pub fn pin_set(stored: &str) -> bool {
    parse_verifier(stored).is_some()
}

fn derive(pin: &str, salt: &[u8]) -> [u8; 32] {
    tv_base::sha256::pbkdf2_hmac_sha256(pin.as_bytes(), salt, ROUNDS)
}

fn parse_verifier(stored: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut parts = stored.trim().split(':');
    let (Some(VERSION), Some(salt), Some(hash), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let (salt, hash) = (unhex(salt)?, unhex(hash)?);
    (salt.len() == SALT_LEN && hash.len() == 32).then_some((salt, hash))
}

/// Sixteen bytes no two PINs share: `RandomState` keys come from the OS's random source.
fn salt() -> [u8; SALT_LEN] {
    use std::hash::{BuildHasher, RandomState};
    let now = std::time::SystemTime::now();
    let mut out = [0u8; SALT_LEN];
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let n = RandomState::new().hash_one((i, now));
        chunk.copy_from_slice(&n.to_le_bytes());
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fake::FakeData;

    fn settings(pairs: &[(&str, &str)]) -> Settings {
        Settings::from_stored(pairs.iter().copied())
    }

    #[test]
    fn the_pin_is_stored_as_a_salted_verifier() {
        assert!(pin_matches("", DEFAULT_PIN), "no PIN set: 0000");
        assert!(!pin_matches("", "1234"));
        assert!(!pin_set(""));
        let stored = hash_pin("4821");
        assert!(pin_set(&stored));
        assert!(!stored.contains("4821"), "{stored}");
        assert!(stored.starts_with("v1:") && stored.len() == 3 + 32 + 1 + 64);
        assert!(pin_matches(&stored, "4821"));
        assert!(
            !pin_matches(&stored, DEFAULT_PIN),
            "a set PIN replaces 0000"
        );
        assert!(!pin_matches(&stored, "4822"));
        assert_ne!(
            hash_pin("4821"),
            stored,
            "salted: the same PIN stores differently"
        );
    }

    #[test]
    fn a_damaged_verifier_falls_back_to_the_default_pin() {
        let good = hash_pin("1111");
        for bad in [
            "News\nSport".to_owned(),
            good.replacen("v1", "v9", 1),
            good[..good.len() - 2].to_owned(),
            format!("{good}:00"),
            good.replace(':', ";"),
        ] {
            assert!(!pin_set(&bad), "{bad}");
            assert!(pin_matches(&bad, DEFAULT_PIN), "{bad}");
            assert!(!pin_matches(&bad, "1111"), "{bad}");
        }
    }

    #[test]
    fn channels_and_their_groups_lock_until_the_pin_opens_them() {
        let data = FakeData::new(0, 6);
        let [news, sport] = [&data.channels[0], &data.channels[1]];
        let s = Settings::new();
        let value = channels_toggled(&s, news.key).unwrap();
        assert_eq!(value, news.key.hex());
        let s = settings(&[
            (keys::PARENTAL_CHANNELS, value.as_str()),
            (keys::PARENTAL_GROUPS, "Sport"),
        ]);
        assert!(channel_blocked(&s, news.key) && group_blocked(&s, "Sport"));
        let mut p = Parental::from_settings(&s);
        assert!(p.locked(news) && p.locked(sport), "by key, by group");
        assert!(!p.locked(&data.channels[2]));
        assert!(p.group_locked(&Group::Playlist("Sport".into())));
        assert!(!p.group_locked(&Group::Playlist("News".into())));
        assert!(!p.group_locked(&Group::All) && !p.group_locked(&Group::Favorites));
        p.open(Lock::Channel(news.key));
        assert!(!p.locked(news), "opened for the session");
        p.open(Lock::Group("Sport".into()));
        assert!(!p.locked(sport) && !p.group_locked(&Group::Playlist("Sport".into())));
        // A change that locks nothing new keeps what the PIN opened.
        p.follow(&settings(&[(keys::PARENTAL_GROUPS, "Sport")]));
        assert!(!p.locked(sport) && !p.locked(news));
        // A new lock locks again everything the PIN opened.
        p.follow(&settings(&[(keys::PARENTAL_GROUPS, "Sport\nMovies")]));
        assert!(p.locked(sport) && p.locked(&data.channels[2]));
        p.open(Lock::Group("Sport".into()));
        p.relock();
        assert!(p.locked(sport), "relocked");
        assert_eq!(channels_toggled(&s, news.key).as_deref(), Some(""));
        assert_eq!(groups_toggled(&s, "Sport").as_deref(), Some(""));
        assert_eq!(groups_toggled(&s, "Kids").as_deref(), Some("Sport\nKids"));
    }

    #[test]
    fn locking_a_channels_group_holds_a_channel_the_pin_opened() {
        let data = FakeData::new(0, 6);
        let sport = &data.channels[1];
        let hex = sport.key.hex();
        let mut p = Parental::from_settings(&settings(&[(keys::PARENTAL_CHANNELS, hex.as_str())]));
        p.open(Lock::Channel(sport.key));
        assert!(!p.locked(sport));
        p.follow(&settings(&[
            (keys::PARENTAL_CHANNELS, hex.as_str()),
            (keys::PARENTAL_GROUPS, "Sport"),
        ]));
        assert!(p.locked(sport), "its group was locked since");
        // Opened, unlocked and locked again: locked.
        p.open(Lock::Group("Sport".into()));
        p.follow(&Settings::new());
        p.follow(&settings(&[(keys::PARENTAL_GROUPS, "Sport")]));
        assert!(p.locked(sport));
    }

    #[test]
    fn a_list_that_would_not_fit_a_setting_is_refused() {
        let long = "g".repeat(3000);
        let s = settings(&[(keys::PARENTAL_GROUPS, long.as_str())]);
        assert!(groups_toggled(&s, &"h".repeat(2000)).is_none());
        assert_eq!(groups_toggled(&s, &long).as_deref(), Some(""));
    }

    /// The labels of the page `screen`'s panel shows.
    fn rows<Id: Copy>(panel: &crate::panels::SidePanel<Id>) -> Vec<String> {
        panel
            .page()
            .rows()
            .iter()
            .map(|r| r.label().to_owned())
            .collect()
    }

    /// Focus the row labelled `label` on `menu` and press OK.
    fn choose(menu: &mut crate::panels::ContextMenu, label: &str) -> Output {
        let data = FakeData::new(0, 0);
        let i = rows(menu.panel()).iter().position(|l| l == label).unwrap();
        let mut keys = vec![crate::input::Key::Down; i];
        keys.push(crate::input::Key::Ok);
        crate::panels::testing::press(menu, &data, &keys)
    }

    fn asks_pin(key: &'static str, value: &str, toast: &str) -> Output {
        Output::close().and(Output::navigate(Navigate::Pin(PinTask::Set {
            key,
            value: value.to_owned(),
            toast: toast.to_owned(),
        })))
    }

    #[test]
    fn the_long_ok_menus_lock_and_unlock_through_the_pin() {
        use crate::panels::{ContextMenu, PanelCtx};
        let data = FakeData::new(0, 6);
        let ch = &data.channels[1];
        let open = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &open,
        };
        let mut menu = ContextMenu::channel(ch, None, None, &cx);
        assert_eq!(
            choose(&mut menu, "Lock channel"),
            asks_pin(keys::PARENTAL_CHANNELS, &ch.key.hex(), "Channel locked")
        );
        let sport = Group::Playlist("Sport".into());
        let mut menu = ContextMenu::group(&sport, &cx);
        assert_eq!(
            choose(&mut menu, "Lock group"),
            asks_pin(keys::PARENTAL_GROUPS, "Sport", "Group locked")
        );
        assert!(!rows(ContextMenu::group(&Group::All, &cx).panel()).contains(&"Lock group".into()));

        let hex = ch.key.hex();
        let locked = settings(&[
            (keys::PARENTAL_CHANNELS, hex.as_str()),
            (keys::PARENTAL_GROUPS, "Sport"),
        ]);
        let cx = PanelCtx {
            data: &data,
            settings: &locked,
        };
        let mut menu = ContextMenu::channel(ch, None, Some(&sport), &cx);
        assert_eq!(
            choose(&mut menu, "Unlock channel"),
            asks_pin(keys::PARENTAL_CHANNELS, "", "Channel unlocked")
        );
        let mut menu = ContextMenu::channel(ch, None, Some(&sport), &cx);
        choose(&mut menu, "Group options");
        assert_eq!(
            choose(&mut menu, "Unlock group"),
            asks_pin(keys::PARENTAL_GROUPS, "", "Group unlocked")
        );
    }

    #[test]
    fn settings_has_change_pin_and_channel_names() {
        use crate::data::Screen;
        use crate::input::{Key, Press};
        use crate::panels::SettingsPanel;
        fn press(panel: &mut SettingsPanel, key: Key) -> Output {
            panel.key(key, Press::Short, &FakeData::new(0, 0), 0)
        }
        fn open(panel: &mut SettingsPanel, label: &str) -> Output {
            let i = rows(panel.panel()).iter().position(|l| l == label).unwrap();
            while panel.panel().page().focus() < i {
                press(panel, Key::Down);
            }
            press(panel, Key::Ok)
        }
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        open(&mut panel, "Parental control");
        assert_eq!(rows(panel.panel()), ["Change PIN"]);
        let out = open(&mut panel, "Change PIN");
        assert_eq!(out.navigate, [Navigate::Pin(PinTask::Change)]);
        assert!(!panel.closing(), "the dialog opens over Settings");
        press(&mut panel, Key::Back);
        while panel.panel().page().focus() > 0 {
            press(&mut panel, Key::Up);
        }
        open(&mut panel, "Playlists");
        let out = open(&mut panel, "Channel names");
        assert_eq!(out.navigate, [Navigate::ChannelNames]);
    }

    #[test]
    fn a_locked_group_reads_empty_and_ok_asks_for_the_pin() {
        use crate::data::{Screen, TvData};
        use crate::guide::GuideScreen;
        use crate::input::{Key, Press};
        use crate::lists::{ChannelList, Mode};
        let mut data = FakeData::new(0, 6);
        data.parental = Parental::from_settings(&settings(&[(keys::PARENTAL_GROUPS, "Sport")]));
        let sport = Group::Playlist("Sport".into());
        assert!(data.channels(&sport).is_empty());
        assert_eq!(data.channels(&Group::All).len(), 6, "listed elsewhere");
        let ask = ask_to_open(&sport);
        assert_eq!(
            ask.navigate,
            [Navigate::Pin(PinTask::Open(Lock::Group("Sport".into())))]
        );
        let mut guide = GuideScreen::new(&data, sport.clone());
        guide.key(Key::Back, Press::Short, &data, 0);
        let out = guide.key(Key::Ok, Press::Short, &data, 0);
        assert_eq!(out.navigate, ask.navigate, "guide");
        let mut list = ChannelList::new(Mode::Overlay, sport.clone(), &data);
        list.key(Key::Left, Press::Short, &data, 0);
        let out = list.key(Key::Ok, Press::Short, &data, 0);
        assert_eq!(out.navigate, ask.navigate, "channel list");
        data.parental.open(Lock::Group("Sport".into()));
        assert_eq!(data.channels(&sport).len(), 2, "open for the session");
        assert!(ask_to_open(&Group::All).navigate.is_empty());
    }

    #[test]
    fn zapping_steps_over_a_locked_channel() {
        use crate::data::Screen;
        use crate::input::{Key, Press};
        use crate::player::{PlayerScreen, PlayerSettings};
        let mut data = FakeData::new(1_700_000_000, 4);
        let keys_of: Vec<ChannelKey> = data.channels.iter().map(|c| c.key).collect();
        let blocked = keys_of[1].hex();
        data.parental =
            Parental::from_settings(&settings(&[(keys::PARENTAL_CHANNELS, blocked.as_str())]));
        data.playback.channel = Some(keys_of[0]);
        let mut player = PlayerScreen::new(PlayerSettings::default());
        player.zap_through(Some(Group::All), Some(keys_of[0]), &data);
        let out = player.key(Key::Up, Press::Short, &data, 0);
        let played: Vec<ChannelKey> = out
            .commands
            .iter()
            .filter_map(|c| match c {
                crate::data::Command::Play { stream, .. } => Some(stream.channel()),
                _ => None,
            })
            .collect();
        assert_eq!(played, [keys_of[2]]);
    }

    #[test]
    fn lines_that_are_not_a_key_are_skipped() {
        let s = settings(&[(keys::PARENTAL_CHANNELS, "News\n000000000000002a\n")]);
        assert_eq!(blocked_channels(&s).collect::<Vec<_>>(), [ChannelKey(42)]);
    }
}
