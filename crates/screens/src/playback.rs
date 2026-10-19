//! The viewer's playback options, shared by the player menu, Settings and the app: how the
//! picture fills its rectangle, the audio sync offset (a default, and one per channel the viewer
//! adjusted) and the sleep timer's choices.
//!
//! What persists is settings ([`crate::panels::settings`]): this module reads those values and
//! builds the ones the player menu stores. The sleep timer is the running app's alone.

use iptv::model::ChannelKey;
use tv_platform::i18n::msg;

use crate::panels::settings::{keys, Settings};

/// How the picture fills the video rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayMode {
    /// The picture's own aspect, fitted inside (letterboxed or pillarboxed).
    #[default]
    Normal,
    /// The rectangle covered, what overhangs cropped.
    Fill,
    /// The whole picture shown at 4:3, pillarboxed.
    Ratio4x3,
    /// The whole picture shown at 16:9.
    Ratio16x9,
}

impl DisplayMode {
    /// Every mode, in the menu's order.
    pub const ALL: [DisplayMode; 4] = [
        DisplayMode::Normal,
        DisplayMode::Fill,
        DisplayMode::Ratio4x3,
        DisplayMode::Ratio16x9,
    ];

    /// The stored value ([`keys::PLAYER_DISPLAY_MODE`]).
    pub fn value(self) -> &'static str {
        match self {
            DisplayMode::Normal => "normal",
            DisplayMode::Fill => "fill",
            DisplayMode::Ratio4x3 => "4:3",
            DisplayMode::Ratio16x9 => "16:9",
        }
    }

    /// The mode stored as `value`; Normal for anything else.
    pub fn from_value(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|m| m.value() == value)
            .unwrap_or_default()
    }

    /// The mode the settings hold.
    pub fn of(settings: &Settings) -> Self {
        Self::from_value(settings.get(keys::PLAYER_DISPLAY_MODE))
    }
}

/// The audio offset goes this far either way, ms.
pub const AUDIO_OFFSET_MAX_MS: i32 = 1_000;
/// One step of the offset, ms.
pub const AUDIO_OFFSET_STEP_MS: i32 = 50;
/// At most this many channels keep an offset of their own; one more forgets the one adjusted
/// longest ago. Their lines stay well under a setting's size limit.
pub const CHANNEL_OFFSETS_MAX: usize = 64;

/// How an offset reads: `+150 ms`, `0 ms`, `-50 ms`.
pub fn offset_label(ms: i64) -> String {
    let n = msg::unit_milliseconds(i64::try_from(ms.unsigned_abs()).unwrap_or(i64::MAX));
    match ms.signum() {
        1 => format!("+{n}"),
        -1 => format!("-{n}"),
        _ => n,
    }
}

/// The audio offset `channel` plays with, ms: its own when the viewer adjusted it, else the
/// default ([`keys::AUDIO_OFFSET_MS`]).
pub fn audio_offset(settings: &Settings, channel: Option<ChannelKey>) -> i32 {
    channel
        .and_then(|c| channel_offsets(settings).find(|(k, _)| *k == c))
        .map_or_else(|| default_offset(settings), |(_, ms)| ms)
}

/// The stored value of [`keys::CHANNEL_AUDIO_OFFSETS`] with `channel` at `ms`, first (the most
/// recently adjusted). An offset equal to the default drops the channel's own: it follows the
/// default again.
pub fn with_channel_offset(settings: &Settings, channel: ChannelKey, ms: i32) -> String {
    let own = (ms != default_offset(settings)).then_some((channel, ms));
    let others = channel_offsets(settings).filter(|(k, _)| *k != channel);
    own.into_iter()
        .chain(others)
        .take(CHANNEL_OFFSETS_MAX)
        .map(|(k, ms)| format!("{} {ms}", k.hex()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn default_offset(settings: &Settings) -> i32 {
    settings
        .get(keys::AUDIO_OFFSET_MS)
        .parse()
        .ok()
        .filter(|ms| in_range(*ms))
        .unwrap_or(0)
}

/// The channels with an offset of their own, most recently adjusted first. A line that does not
/// read `<16 hex digits> <ms>` with `ms` a step inside the range is skipped.
fn channel_offsets(settings: &Settings) -> impl Iterator<Item = (ChannelKey, i32)> + '_ {
    settings
        .get(keys::CHANNEL_AUDIO_OFFSETS)
        .lines()
        .filter_map(|line| {
            let (key, ms) = line.split_once(' ')?;
            let key = ChannelKey::from_hex(key)?;
            let ms = ms.parse().ok().filter(|ms| in_range(*ms))?;
            Some((key, ms))
        })
}

fn in_range(ms: i32) -> bool {
    (-AUDIO_OFFSET_MAX_MS..=AUDIO_OFFSET_MAX_MS).contains(&ms) && ms % AUDIO_OFFSET_STEP_MS == 0
}

/// The sleep timer's choices, minutes; 0 turns it off.
pub const SLEEP_MINUTES: [u32; 6] = [0, 15, 30, 60, 90, 120];

/// How a sleep timer choice reads.
pub fn sleep_label(minutes: u32) -> String {
    match minutes {
        0 => msg::playback_sleep_off().to_owned(),
        m => msg::core_duration_long_minutes(i64::from(m)),
    }
}

/// How the time a sleep timer has left reads, rounded up to whole minutes.
pub fn sleep_left_label(left_secs: u32) -> String {
    msg::playback_sleep_left(i64::from(left_secs.div_ceil(60)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locale() {
        tv_platform::i18n::initialize(tv_platform::i18n::Preference::En, true);
    }

    fn with(pairs: &[(&str, &str)]) -> Settings {
        Settings::from_stored(pairs.iter().copied())
    }

    #[test]
    fn display_modes_round_trip_and_read_normal_otherwise() {
        for m in DisplayMode::ALL {
            assert_eq!(DisplayMode::from_value(m.value()), m);
        }
        assert_eq!(DisplayMode::from_value("zoom"), DisplayMode::Normal);
        assert_eq!(DisplayMode::of(&Settings::new()), DisplayMode::Normal);
        let s = with(&[(keys::PLAYER_DISPLAY_MODE, "4:3")]);
        assert_eq!(DisplayMode::of(&s), DisplayMode::Ratio4x3);
    }

    #[test]
    fn offsets_read_signed_in_milliseconds() {
        locale();
        assert_eq!(offset_label(150), "+150 ms");
        assert_eq!(offset_label(0), "0 ms");
        assert_eq!(offset_label(-50), "-50 ms");
    }

    #[test]
    fn a_channel_plays_with_its_own_offset_else_the_default() {
        let (a, b) = (ChannelKey(1), ChannelKey(-2));
        let s = Settings::new();
        assert_eq!(audio_offset(&s, Some(a)), 0);
        let s = with(&[(keys::AUDIO_OFFSET_MS, "-100")]);
        assert_eq!(audio_offset(&s, Some(a)), -100, "the default");
        assert_eq!(audio_offset(&s, None), -100, "nothing plays: the default");
        let list = with_channel_offset(&s, a, 250);
        assert_eq!(list, "0000000000000001 250");
        let s = with(&[
            (keys::AUDIO_OFFSET_MS, "-100"),
            (keys::CHANNEL_AUDIO_OFFSETS, &list),
        ]);
        assert_eq!(audio_offset(&s, Some(a)), 250);
        assert_eq!(audio_offset(&s, Some(b)), -100);
        // the latest adjusted first; back at the default, the channel's own goes
        let list = with_channel_offset(&s, b, 50);
        assert_eq!(list, "fffffffffffffffe 50\n0000000000000001 250");
        let s = with(&[
            (keys::AUDIO_OFFSET_MS, "-100"),
            (keys::CHANNEL_AUDIO_OFFSETS, &list),
        ]);
        assert_eq!(with_channel_offset(&s, a, -100), "fffffffffffffffe 50");
    }

    #[test]
    fn the_channel_list_is_capped_and_skips_what_it_cannot_read() {
        let mut s = Settings::new();
        for i in 0..CHANNEL_OFFSETS_MAX as i64 + 5 {
            let list = with_channel_offset(&s, ChannelKey(i), 100);
            s = with(&[(keys::CHANNEL_AUDIO_OFFSETS, &list)]);
        }
        let list = s.get(keys::CHANNEL_AUDIO_OFFSETS).to_owned();
        assert_eq!(list.lines().count(), CHANNEL_OFFSETS_MAX);
        assert!(list.len() < 4096, "inside a setting's size limit");
        assert_eq!(
            audio_offset(&s, Some(ChannelKey(0))),
            0,
            "the oldest is gone"
        );
        let newest = CHANNEL_OFFSETS_MAX as i64 + 4;
        assert_eq!(audio_offset(&s, Some(ChannelKey(newest))), 100);
        let junk = "News\nSport\n0000000000000001 9000\n0000000000000001 -2147483648\n\
                    0000000000000001 33\nzz 50\n1 50\n\
                    0000000000000002 -50";
        let s = with(&[(keys::CHANNEL_AUDIO_OFFSETS, junk)]);
        assert_eq!(audio_offset(&s, Some(ChannelKey(1))), 0);
        assert_eq!(audio_offset(&s, Some(ChannelKey(2))), -50);
        assert_eq!(
            with_channel_offset(&s, ChannelKey(3), 0),
            "0000000000000002 -50"
        );
    }

    #[test]
    fn settings_offer_every_mode_and_every_offset_step() {
        use crate::panels::settings::{spec, Kind};
        locale();
        let values = |key| match spec(key).map(|s| s.kind) {
            Some(Kind::Choice(options)) => options.iter().map(|(v, _)| *v).collect::<Vec<_>>(),
            _ => panic!("{key} is a choice"),
        };
        let modes: Vec<_> = DisplayMode::ALL.iter().map(|m| m.value()).collect();
        assert_eq!(values(keys::PLAYER_DISPLAY_MODE), modes);
        let steps: Vec<String> = (-AUDIO_OFFSET_MAX_MS..=AUDIO_OFFSET_MAX_MS)
            .step_by(AUDIO_OFFSET_STEP_MS as usize)
            .map(|ms| ms.to_string())
            .collect();
        assert_eq!(values(keys::AUDIO_OFFSET_MS), steps);
        let s = with(&[(keys::AUDIO_OFFSET_MS, "250")]);
        assert_eq!(s.display(keys::AUDIO_OFFSET_MS), "+250 ms");
        assert_eq!(Settings::new().display(keys::PLAYER_DISPLAY_MODE), "Normal");
        assert!(
            !Settings::new().set(keys::AUDIO_OFFSET_MS, "25".into()),
            "not a step"
        );
    }

    /// Settings > Playback has the default offset, Appearance > Player the display mode;
    /// picking either stores it.
    #[test]
    fn the_settings_tree_has_the_default_offset_and_the_display_mode() {
        use crate::data::{fake::FakeData, Command, Output};
        use crate::input::Key;
        use crate::panels::testing::press;
        use crate::panels::SettingsPanel;
        locale();
        let data = FakeData::new(0, 0);
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        let go = |panel: &mut SettingsPanel, label: &str| -> Output {
            let page = panel.panel().page();
            let at = page.rows().iter().position(|r| r.label() == label);
            let at = at.unwrap_or_else(|| panic!("{label} on {}", page.title()));
            let moves = at as isize - page.focus() as isize;
            let key = if moves < 0 { Key::Up } else { Key::Down };
            press(panel, &data, &vec![key; moves.unsigned_abs()]);
            press(panel, &data, &[Key::Ok])
        };
        go(&mut panel, "Playback");
        let rows = panel.panel().page().rows();
        let row = rows.iter().find(|r| r.label() == "Default audio offset");
        assert_eq!(row.and_then(|r| r.value_text()), Some("0 ms"));
        go(&mut panel, "Default audio offset");
        assert_eq!(panel.panel().page().rows().len(), 41);
        let set = |key: &str, value: &str| {
            Output::command(Command::SetSetting {
                key: key.to_owned(),
                value: value.to_owned(),
            })
        };
        assert_eq!(go(&mut panel, "+150 ms"), set(keys::AUDIO_OFFSET_MS, "150"));
        press(&mut panel, &data, &[Key::Back]);
        go(&mut panel, "Appearance");
        go(&mut panel, "Player");
        go(&mut panel, "Display mode");
        assert_eq!(
            go(&mut panel, "Fill"),
            set(keys::PLAYER_DISPLAY_MODE, "fill")
        );
    }

    #[test]
    fn sleep_choices_and_time_left_read_in_minutes() {
        locale();
        assert_eq!(sleep_label(0), "Off");
        assert_eq!(sleep_label(90), "90 min");
        assert_eq!(sleep_left_label(30 * 60), "30 min left");
        assert_eq!(sleep_left_label(61), "2 min left", "rounded up");
        assert_eq!(sleep_left_label(1), "1 min left");
    }
}
