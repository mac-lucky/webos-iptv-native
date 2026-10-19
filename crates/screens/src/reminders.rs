//! Programme reminders: "remind me" on a future programme in the guide, a prompt at its start.
//!
//! The reminders live in the settings ([`keys::REMINDERS`]), one per line as
//! `<channel key hex> <start> <title>`, at most [`MAX`] and in start order. The title is kept so
//! the prompt can name the programme even when the guide has moved on; it is cut to a bounded
//! length and stripped of control characters. The shell fires them
//! ([`due`]) and drops what has fired or is long past from the setting.

use iptv::model::{ChannelKey, Timestamp};

use crate::panels::keys;
use crate::panels::settings::Settings;

/// The most reminders kept.
pub const MAX: usize = 32;
/// Bytes of a title kept, cut on a character boundary.
const TITLE_MAX: usize = 64;
/// A reminder found this long after its start (the app was not running) is dropped unfired.
pub const LATE_SECS: i64 = 5 * 60;
/// The latest start a stored line may carry (9999-12-31T23:59:59Z); with no negative ones, the
/// clock arithmetic on a start never overflows, whatever a restored backup holds.
const MAX_START: Timestamp = 253_402_300_799;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reminder {
    pub channel: ChannelKey,
    pub start: Timestamp,
    pub title: String,
}

/// The stored reminders in start order; lines that do not read are skipped.
pub fn parse(value: &str) -> Vec<Reminder> {
    let mut list: Vec<Reminder> = value.lines().filter_map(parse_line).collect();
    list.sort_by_key(|r| r.start);
    list.dedup_by(|a, b| a.channel == b.channel && a.start == b.start);
    list.truncate(MAX);
    list
}

pub fn reminders(settings: &Settings) -> Vec<Reminder> {
    parse(settings.get(keys::REMINDERS))
}

/// The stored form of `list`.
pub fn format(list: &[Reminder]) -> String {
    list.iter()
        .map(|r| format!("{} {} {}", r.channel.hex(), r.start, r.title))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn has(settings: &Settings, channel: ChannelKey, start: Timestamp) -> bool {
    reminders(settings)
        .iter()
        .any(|r| r.channel == channel && r.start == start)
}

/// What a toggle did to the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Toggled {
    Added(String),
    Removed(String),
    /// [`MAX`] reminders are set already.
    Full,
}

/// The value of [`keys::REMINDERS`] with a reminder for `title` on `channel` at `start` added,
/// or removed when it is set.
pub fn toggled(settings: &Settings, channel: ChannelKey, start: Timestamp, title: &str) -> Toggled {
    let mut list = reminders(settings);
    if let Some(i) = list
        .iter()
        .position(|r| r.channel == channel && r.start == start)
    {
        list.remove(i);
        return Toggled::Removed(format(&list));
    }
    if list.len() >= MAX {
        return Toggled::Full;
    }
    list.push(Reminder {
        channel,
        start,
        title: clean_title(title),
    });
    list.sort_by_key(|r| r.start);
    Toggled::Added(format(&list))
}

/// Split `list` at `now`: the reminders whose start has come (in start order) and those still
/// ahead. Of the ones that came, those [`LATE_SECS`] or more past their start are dropped.
pub fn due(list: &[Reminder], now: Timestamp) -> (Vec<Reminder>, Vec<Reminder>) {
    let (came, ahead): (Vec<_>, Vec<_>) = list.iter().cloned().partition(|r| r.start <= now);
    let fire = came
        .into_iter()
        .filter(|r| now.saturating_sub(r.start) < LATE_SECS)
        .collect();
    (fire, ahead)
}

fn parse_line(line: &str) -> Option<Reminder> {
    let mut parts = line.trim().splitn(3, ' ');
    let channel = ChannelKey::from_hex(parts.next()?)?;
    let start = parts
        .next()?
        .parse()
        .ok()
        .filter(|s| (0..=MAX_START).contains(s))?;
    let title = clean_title(parts.next().unwrap_or(""));
    Some(Reminder {
        channel,
        start,
        title,
    })
}

/// One line of at most [`TITLE_MAX`] bytes.
fn clean_title(title: &str) -> String {
    let flat: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    flat[..flat.floor_char_boundary(TITLE_MAX)]
        .trim_end()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panels::settings::{accepts, spec};

    const NOW: Timestamp = 1_700_000_000;

    fn stored(value: &str) -> Settings {
        Settings::from_stored([(keys::REMINDERS, value)])
    }

    #[test]
    fn a_toggle_adds_then_removes() {
        let (a, b) = (ChannelKey(7), ChannelKey(-3));
        let Toggled::Added(one) = toggled(&Settings::new(), a, NOW + 600, "Film\nnight") else {
            panic!("added");
        };
        assert_eq!(one, format!("{} {} Film night", a.hex(), NOW + 600));
        let s = stored(&one);
        assert!(has(&s, a, NOW + 600) && !has(&s, a, NOW));
        let Toggled::Added(two) = toggled(&s, b, NOW + 60, "News") else {
            panic!("added");
        };
        let list = parse(&two);
        assert_eq!(
            list.iter().map(|r| r.channel).collect::<Vec<_>>(),
            [b, a],
            "in start order"
        );
        assert_eq!(list[1].title, "Film night");
        assert_eq!(
            toggled(&stored(&two), b, NOW + 60, "News"),
            Toggled::Removed(one)
        );
    }

    #[test]
    fn the_list_is_capped_and_every_value_fits_the_setting() {
        let title = "\u{17c}".repeat(200);
        let mut s = Settings::new();
        for i in 0..MAX as i64 {
            let Toggled::Added(v) = toggled(&s, ChannelKey(i), NOW + i, &title) else {
                panic!("added {i}");
            };
            assert!(accepts(spec(keys::REMINDERS).unwrap(), &v), "{i} fits");
            assert!(s.set(keys::REMINDERS, v));
        }
        assert_eq!(toggled(&s, ChannelKey(99), NOW, "x"), Toggled::Full);
        let list = reminders(&s);
        assert_eq!(list.len(), MAX);
        assert!(list
            .iter()
            .all(|r| r.title.len() <= TITLE_MAX && r.title.starts_with('\u{17c}')));
    }

    #[test]
    fn bad_lines_are_skipped() {
        let good = format!("{} {} Ok", ChannelKey(1).hex(), NOW);
        let value = format!("junk\n{good}\n0000000000000001 soon Bad\nzz {NOW} Bad\n{good}");
        let list = parse(&value);
        assert_eq!(list.len(), 1, "one good, its duplicate dropped");
        assert_eq!(list[0].title, "Ok");
        assert_eq!(parse("0000000000000002 5")[0].title, "", "no title");
    }

    #[test]
    fn a_start_out_of_the_clocks_range_is_skipped() {
        let value = format!(
            "0000000000000001 {} x\n0000000000000002 -1 x\n0000000000000003 {} x\n0000000000000004 {MAX_START} x",
            i64::MIN,
            i64::MAX
        );
        let list = parse(&value);
        assert_eq!(list.len(), 1, "only the last start reads");
        assert_eq!(list[0].start, MAX_START);
        // Even a list built by hand never overflows the clock arithmetic.
        let r = |start| Reminder {
            channel: ChannelKey(1),
            start,
            title: String::new(),
        };
        let (fire, ahead) = due(&[r(i64::MIN), r(i64::MAX)], NOW);
        assert_eq!((fire, ahead), (vec![], vec![r(i64::MAX)]));
    }

    #[test]
    fn due_fires_what_started_and_drops_what_is_long_past() {
        let r = |start| Reminder {
            channel: ChannelKey(1),
            start,
            title: String::new(),
        };
        let list = [r(NOW - LATE_SECS), r(NOW - 10), r(NOW), r(NOW + 1)];
        let (fire, ahead) = due(&list, NOW);
        assert_eq!(fire, [r(NOW - 10), r(NOW)]);
        assert_eq!(ahead, [r(NOW + 1)]);
    }
}

#[cfg(test)]
mod menu_tests {
    use super::*;
    use crate::data::fake::FakeData;
    use crate::data::{Command, Navigate, Output, TvData};
    use crate::input::Key;
    use crate::panels::testing::press;
    use crate::panels::{ContextMenu, PanelCtx};
    use iptv::model::Programme;

    fn labels(menu: &ContextMenu) -> Vec<String> {
        menu.panel()
            .page()
            .rows()
            .iter()
            .map(|r| r.label().to_owned())
            .collect()
    }

    fn choose(menu: &mut ContextMenu, label: &str) -> Output {
        let i = labels(menu).iter().position(|l| l == label).unwrap();
        let mut keys = vec![Key::Down; i];
        keys.push(Key::Ok);
        press(menu, &FakeData::new(0, 0), &keys)
    }

    #[test]
    fn remind_me_is_offered_on_a_programme_still_to_start() {
        let data = FakeData::new(1_700_000_000, 3);
        let ch = &data.channels[0];
        let (on_air, next) = data.now_next(ch.key, data.now);
        let (on_air, next): (Programme, Programme) = (on_air.unwrap(), next.unwrap());
        let none = Settings::new();
        let cx = PanelCtx {
            data: &data,
            settings: &none,
        };
        let menu = ContextMenu::channel(ch, Some(&on_air), None, &cx);
        assert!(!labels(&menu).contains(&"Remind me".to_owned()), "on air");
        let mut menu = ContextMenu::channel(ch, Some(&next), None, &cx);
        let out = choose(&mut menu, "Remind me");
        let Toggled::Added(value) = toggled(&none, ch.key, next.start, &next.title) else {
            panic!("added");
        };
        assert_eq!(out.toast.as_deref(), Some("Reminder set"));
        assert_eq!(
            out.commands,
            [Command::SetSetting {
                key: keys::REMINDERS.into(),
                value: value.clone()
            }]
        );
        assert_eq!(out.navigate, [Navigate::Close]);

        let set = Settings::from_stored([(keys::REMINDERS, value.as_str())]);
        let cx = PanelCtx {
            data: &data,
            settings: &set,
        };
        let mut menu = ContextMenu::channel(ch, Some(&next), None, &cx);
        let out = choose(&mut menu, "Cancel reminder");
        assert_eq!(out.toast.as_deref(), Some("Reminder cancelled"));
        assert_eq!(
            out.commands,
            [Command::SetSetting {
                key: keys::REMINDERS.into(),
                value: String::new()
            }]
        );
    }
}
