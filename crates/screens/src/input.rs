//! Remote keys to actions: short and long presses, and accelerated repeat on held keys.
//!
//! Pure and clock-free. The event loop maps each SDL key event with [`Key::from_sdl`] and feeds
//! [`Remote`] its down, repeat and up edges with a millisecond timestamp, then calls
//! [`Remote::tick`] no later than [`Remote::wait_ms`] says. The screen in front chooses how each
//! key behaves while held ([`Hold`]). One key is tracked at a time: a new key-down abandons the
//! previous key, whose key-up was lost.

use std::os::raw::c_uint;

/// A remote key the screens react to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Ok,
    Back,
    ChannelUp,
    ChannelDown,
    /// 0 to 9.
    Digit(u8),
    Red,
    Green,
    Yellow,
    Blue,
    Play,
    Pause,
    PlayPause,
    Rewind,
    FastForward,
    /// The TV keyboard's delete and clear-all keys, sent as key events while it is up.
    Backspace,
    Clear,
}

impl Key {
    /// The key for an SDL keycode and webOS key code (the scancode field): the LG remote's
    /// measured codes (`tv_ui::consts`). The simulator hands desktop keys over already
    /// translated to remote codes, except Esc and F7/F8/F9 (rewind, play/pause, fast-forward),
    /// which are matched here.
    pub fn from_sdl(sym: c_uint, scancode: c_uint) -> Option<Key> {
        use tv_ui::consts as c;
        let either = |code| sym == code || scancode == code;
        Some(match sym {
            c::SDLK_UP => Key::Up,
            c::SDLK_DOWN => Key::Down,
            c::SDLK_LEFT => Key::Left,
            c::SDLK_RIGHT => Key::Right,
            c::SDLK_BACKSPACE => Key::Backspace,
            c::SDLK_CLEAR => Key::Clear,
            s if c::is_ok(s) => Key::Ok,
            _ if c::is_back(sym, scancode) => Key::Back,
            _ => match scancode {
                c::WCODE_CH_UP_KEY | c::WCODE_CH_UP_ALT => Key::ChannelUp,
                c::WCODE_CH_DOWN_KEY | c::WCODE_CH_DOWN_ALT => Key::ChannelDown,
                c::SCANCODE_1..=c::SCANCODE_9 => Key::Digit((scancode - c::SCANCODE_1 + 1) as u8),
                c::SCANCODE_0 => Key::Digit(0),
                c::WCODE_RED => Key::Red,
                c::WCODE_GREEN => Key::Green,
                c::WCODE_YELLOW => Key::Yellow,
                c::WCODE_BLUE => Key::Blue,
                c::SCANCODE_F7 => Key::Rewind,
                c::SCANCODE_F8 => Key::PlayPause,
                c::SCANCODE_F9 => Key::FastForward,
                c::WCODE_PAUSE => Key::Pause,
                _ if either(c::WCODE_PAUSE_ALT) => Key::Pause,
                _ if either(c::WCODE_PLAY)
                    || either(c::WCODE_PLAY_ALT_A)
                    || either(c::WCODE_PLAY_ALT_B) =>
                {
                    Key::Play
                }
                _ if either(c::WCODE_PLAYPAUSE) => Key::PlayPause,
                _ if either(c::WCODE_REWIND) => Key::Rewind,
                _ if either(c::WCODE_FASTFORWARD) => Key::FastForward,
                _ => return None,
            },
        })
    }
}

/// How a key behaves while held. The screen in front picks it per key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// [`Press::Short`] on key-down; holding does nothing more.
    Once,
    /// [`Press::Short`] on key-down, then [`Press::Repeat`] faster the longer it is held: list
    /// navigation.
    Repeat,
    /// [`Press::Short`] on a release before the threshold; [`Press::Long`] once at the threshold
    /// while still held, and nothing on that release.
    Long,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    Short,
    Long,
    Repeat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Action {
    pub key: Key,
    pub press: Press,
}

/// Default long-press threshold.
pub const LONG_MS: u32 = 500;
/// Held this long, a [`Hold::Repeat`] key starts repeating.
const REPEAT_DELAY_MS: u32 = 400;
/// The first repeat interval; each repeat shortens the next by `REPEAT_STEP_MS` down to
/// `REPEAT_MIN_MS`, full speed after about 0.6 s of repeating.
const REPEAT_START_MS: u32 = 160;
const REPEAT_STEP_MS: u32 = 20;
const REPEAT_MIN_MS: u32 = 40;
/// The platform's own auto-repeat stopped this long ago without a key-up: the key-up was lost.
/// Only checked once a repeat edge has been seen, since some keys (OK on the LG remote) never
/// auto-repeat.
const LOST_MS: u32 = 350;
/// A [`Hold::Repeat`] key with no platform repeat this long after key-down counts as released:
/// without that heartbeat a lost key-up would scroll a list forever.
const SILENT_MS: u32 = 1000;

#[derive(Clone, Copy, Debug)]
struct Held {
    key: Key,
    hold: Hold,
    since: u32,
    /// Milliseconds after `since` when [`Remote::tick`] acts next: the long press or the next
    /// repeat. `None` once nothing is left to fire.
    due: Option<u32>,
    repeats: u32,
    /// The last platform repeat edge.
    beat: Option<u32>,
}

impl Held {
    fn due_in(&self, now: u32) -> Option<u32> {
        self.due
            .map(|d| d.saturating_sub(now.wrapping_sub(self.since)))
    }

    /// Milliseconds until the key counts as released without its key-up; 0 once it does.
    fn lost_in(&self, now: u32) -> Option<u32> {
        match self.beat {
            Some(b) => Some(LOST_MS.saturating_sub(now.wrapping_sub(b))),
            None if self.hold == Hold::Repeat => {
                Some(SILENT_MS.saturating_sub(now.wrapping_sub(self.since)))
            }
            None => None,
        }
    }
}

/// The input layer. `long_ms` is the long-press threshold setting.
#[derive(Clone, Debug)]
pub struct Remote {
    pub long_ms: u32,
    held: Option<Held>,
}

impl Default for Remote {
    fn default() -> Self {
        Self {
            long_ms: LONG_MS,
            held: None,
        }
    }
}

impl Remote {
    /// A key went down at `now` (ms). A platform repeat goes to [`repeat`](Self::repeat) instead.
    pub fn down(&mut self, key: Key, now: u32, hold: Hold) -> Option<Action> {
        let due = match hold {
            Hold::Once => None,
            Hold::Repeat => Some(REPEAT_DELAY_MS),
            Hold::Long => Some(self.long_ms),
        };
        self.held = Some(Held {
            key,
            hold,
            since: now,
            due,
            repeats: 0,
            beat: None,
        });
        (hold != Hold::Long).then_some(Action {
            key,
            press: Press::Short,
        })
    }

    /// Whether `key` is down: its key-down came and its key-up has not.
    pub fn holds(&self, key: Key) -> bool {
        self.held.is_some_and(|h| h.key == key)
    }

    /// Drop the held key without an action: its key-up will not come here (the TV keyboard
    /// rose over it and takes the remote's keys).
    pub fn forget(&mut self) {
        self.held = None;
    }

    /// The platform's own auto-repeat for a held key: proof it is still held, never an action.
    pub fn repeat(&mut self, key: Key, now: u32) {
        if let Some(h) = self.held.as_mut().filter(|h| h.key == key) {
            h.beat = Some(now);
        }
    }

    pub fn up(&mut self, key: Key, now: u32) -> Option<Action> {
        let h = self.held.take_if(|h| h.key == key)?;
        // A release past the threshold is long even if no tick ran in between.
        let long = now.wrapping_sub(h.since) >= self.long_ms;
        (h.hold == Hold::Long && h.due.is_some()).then_some(Action {
            key,
            press: if long { Press::Long } else { Press::Short },
        })
    }

    /// Fire what is due at `now`: a long press at its threshold, the next repeat, or the release
    /// of a key whose key-up was lost.
    pub fn tick(&mut self, now: u32) -> Option<Action> {
        let h = self.held.as_mut()?;
        if h.lost_in(now) == Some(0) {
            let (key, released) = (h.key, h.beat.unwrap_or(now));
            return self.up(key, released);
        }
        if h.due_in(now)? > 0 {
            return None;
        }
        let held_ms = now.wrapping_sub(h.since);
        let press = match h.hold {
            Hold::Long => {
                h.due = None;
                Press::Long
            }
            _ => {
                let gap = REPEAT_START_MS
                    .saturating_sub(h.repeats.saturating_mul(REPEAT_STEP_MS))
                    .max(REPEAT_MIN_MS);
                h.repeats = h.repeats.saturating_add(1);
                // From now, not from the missed due time: a late frame drops beats, never bursts.
                h.due = Some(held_ms.saturating_add(gap));
                Press::Repeat
            }
        };
        Some(Action { key: h.key, press })
    }

    /// How long the loop may wait before the next [`tick`](Self::tick) matters; `None` when
    /// nothing is pending, so an idle screen sleeps until the next event.
    pub fn wait_ms(&self, now: u32) -> Option<u32> {
        let h = self.held.as_ref()?;
        h.due_in(now).into_iter().chain(h.lost_in(now)).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(key: Key, press: Press) -> Option<Action> {
        Some(Action { key, press })
    }

    /// Every action from ticking each millisecond in `from..to`, with its time.
    fn run(r: &mut Remote, from: u32, to: u32) -> Vec<(u32, Action)> {
        (from..to)
            .filter_map(|t| r.tick(t).map(|a| (t, a)))
            .collect()
    }

    /// [`run`] while the platform auto-repeats `key` every 50 ms, as a held key does.
    fn run_held(r: &mut Remote, key: Key, from: u32, to: u32) -> Vec<(u32, Action)> {
        (from..to)
            .filter_map(|t| {
                if t % 50 == 0 {
                    r.repeat(key, t);
                }
                r.tick(t).map(|a| (t, a))
            })
            .collect()
    }

    #[test]
    fn long_key_released_early_is_short() {
        let mut r = Remote::default();
        assert_eq!(r.down(Key::Ok, 1000, Hold::Long), None);
        assert!(run(&mut r, 1000, 1499).is_empty());
        assert_eq!(r.up(Key::Ok, 1499), act(Key::Ok, Press::Short));
        assert_eq!(r.tick(3000), None);
    }

    #[test]
    fn long_key_held_fires_long_once_at_the_threshold() {
        let mut r = Remote::default();
        r.down(Key::Back, 0, Hold::Long);
        assert_eq!(
            run(&mut r, 0, 3000),
            vec![(
                500,
                Action {
                    key: Key::Back,
                    press: Press::Long
                }
            )]
        );
        assert_eq!(r.up(Key::Back, 3000), None);
    }

    #[test]
    fn release_past_the_threshold_is_long_without_a_tick() {
        let mut r = Remote::default();
        r.down(Key::Left, 0, Hold::Long);
        assert_eq!(r.up(Key::Left, 650), act(Key::Left, Press::Long));
    }

    #[test]
    fn threshold_is_a_setting() {
        let mut r = Remote {
            long_ms: 800,
            ..Remote::default()
        };
        r.down(Key::Ok, 0, Hold::Long);
        assert_eq!(
            run(&mut r, 0, 2000),
            vec![(
                800,
                Action {
                    key: Key::Ok,
                    press: Press::Long
                }
            )]
        );
    }

    #[test]
    fn once_fires_on_down_and_never_again() {
        let mut r = Remote::default();
        assert_eq!(
            r.down(Key::Digit(7), 0, Hold::Once),
            act(Key::Digit(7), Press::Short)
        );
        assert!(run(&mut r, 0, 5000).is_empty());
        assert_eq!(r.up(Key::Digit(7), 5000), None);
    }

    #[test]
    fn held_arrow_repeats_after_a_delay_and_accelerates_to_a_floor() {
        let mut r = Remote::default();
        assert_eq!(
            r.down(Key::Down, 0, Hold::Repeat),
            act(Key::Down, Press::Short)
        );
        let beats = run_held(&mut r, Key::Down, 0, 2000);
        assert!(beats.iter().all(|(_, a)| *a
            == Action {
                key: Key::Down,
                press: Press::Repeat
            }));
        assert_eq!(beats[0].0, REPEAT_DELAY_MS);
        let gaps: Vec<u32> = beats.windows(2).map(|w| w[1].0 - w[0].0).collect();
        assert_eq!(gaps[0], REPEAT_START_MS);
        assert!(gaps.windows(2).all(|g| g[1] <= g[0]));
        assert_eq!(*gaps.last().unwrap(), REPEAT_MIN_MS);
        assert_eq!(r.up(Key::Down, 2000), None);
        assert_eq!(r.tick(2100), None);
    }

    #[test]
    fn a_late_frame_drops_repeats_instead_of_bursting() {
        let mut r = Remote::default();
        r.down(Key::Up, 0, Hold::Repeat);
        r.repeat(Key::Up, 1990);
        assert_eq!(r.tick(2000), act(Key::Up, Press::Repeat));
        assert_eq!(r.tick(2001), None);
    }

    #[test]
    fn a_repeating_key_without_platform_repeats_stops_after_a_second() {
        let mut r = Remote::default();
        r.down(Key::Down, 0, Hold::Repeat);
        let beats = run(&mut r, 0, 5000);
        assert!(!beats.is_empty() && beats.iter().all(|&(t, _)| t < SILENT_MS));
        assert_eq!(r.wait_ms(5000), None);
    }

    #[test]
    fn a_new_key_abandons_the_held_one() {
        let mut r = Remote::default();
        r.down(Key::Ok, 0, Hold::Long);
        assert_eq!(
            r.down(Key::Down, 100, Hold::Repeat),
            act(Key::Down, Press::Short)
        );
        assert_eq!(r.up(Key::Ok, 200), None);
        assert_eq!(r.tick(450), None);
        assert_eq!(r.tick(500), act(Key::Down, Press::Repeat));
    }

    #[test]
    fn silence_after_platform_repeats_releases_a_lost_key() {
        let mut r = Remote::default();
        r.down(Key::Right, 0, Hold::Repeat);
        r.repeat(Key::Right, 380);
        r.repeat(Key::Left, 700); // another key's repeat is not a heartbeat
        let beats = run(&mut r, 0, 3000);
        assert!(beats.iter().all(|&(t, _)| t < 380 + LOST_MS));
        assert_eq!(r.wait_ms(3000), None);

        r.down(Key::Ok, 5000, Hold::Long);
        r.repeat(Key::Ok, 5100);
        assert_eq!(r.tick(5100 + LOST_MS), act(Key::Ok, Press::Short));
    }

    #[test]
    fn a_key_that_never_repeats_is_held_until_its_key_up() {
        let mut r = Remote::default();
        r.down(Key::Ok, 0, Hold::Long);
        assert_eq!(r.tick(400), None);
        assert_eq!(r.wait_ms(400), Some(100));
        r.tick(500);
        assert_eq!(r.wait_ms(10_000), None);
    }

    #[test]
    fn wait_ms_is_the_nearest_of_due_and_lost() {
        let mut r = Remote::default();
        assert_eq!(r.wait_ms(0), None);
        r.down(Key::Down, 0, Hold::Repeat);
        assert_eq!(r.wait_ms(100), Some(300));
        r.repeat(Key::Down, 200);
        assert_eq!(r.wait_ms(300), Some(100));
        assert_eq!(r.wait_ms(1000), Some(0));
    }

    #[test]
    fn the_clock_may_wrap() {
        let mut r = Remote::default();
        let t = u32::MAX - 100;
        r.down(Key::Ok, t, Hold::Long);
        assert_eq!(r.tick(t.wrapping_add(499)), None);
        assert_eq!(r.tick(t.wrapping_add(500)), act(Key::Ok, Press::Long));
    }

    #[test]
    fn lg_remote_and_desktop_keys_map() {
        use tv_ui::consts as c;
        // SDL keycode of a key without a character: its scancode | 1 << 30.
        let k = |scancode: c_uint| scancode | 1 << 30;
        for (sym, scancode, key) in [
            (c::SDLK_UP, 82, Key::Up),
            (c::SDLK_LEFT, 80, Key::Left),
            (c::SDLK_RETURN, 40, Key::Ok),
            (k(c::WCODE_BACK), c::WCODE_BACK, Key::Back),
            (c::SDLK_ESCAPE, 41, Key::Back),
            (k(c::WCODE_CH_UP_KEY), c::WCODE_CH_UP_KEY, Key::ChannelUp),
            (0, c::WCODE_CH_DOWN_KEY, Key::ChannelDown),
            (k(c::WCODE_CH_UP_ALT), c::WCODE_CH_UP_ALT, Key::ChannelUp),
            (0, c::WCODE_CH_DOWN_ALT, Key::ChannelDown),
            ('4' as c_uint, 33, Key::Digit(4)),
            ('0' as c_uint, 39, Key::Digit(0)),
            (0, c::WCODE_PLAY, Key::Play),
            (0, c::WCODE_PAUSE, Key::Pause),
            (0, c::WCODE_PLAYPAUSE, Key::PlayPause),
            (0, c::WCODE_REWIND, Key::Rewind),
            (0, c::WCODE_FASTFORWARD, Key::FastForward),
            (0, 486, Key::Red),
            (0, 487, Key::Green),
            (0, 488, Key::Yellow),
            (0, 489, Key::Blue),
            (k(65), 65, Key::PlayPause),
            (c::SDLK_BACKSPACE, 42, Key::Backspace),
            (c::SDLK_CLEAR, 156, Key::Clear),
        ] {
            assert_eq!(
                Key::from_sdl(sym, scancode),
                Some(key),
                "sym {sym:#x} scancode {scancode}"
            );
        }
        assert_eq!(Key::from_sdl('x' as c_uint, 27), None);
        assert_eq!(Key::from_sdl(0, 0), None);
    }
}
