//! Remote-driven navigation: the whole shell over the synthetic channels, driven key by key the
//! way the remote does (short presses, long presses past the threshold, held keys that repeat),
//! with what the viewer sees checked after every step: the screen in front, where its focus is
//! and where the video is. The tests walk the remote's key map (keys nothing implements yet are
//! ignored tests at the end), plus the guide Left/Back sequence of the C1 trial. `cargo xtask
//! tv-replay` plays such a sequence on a television.

use std::any::Any;

use iptv::model::Timestamp;
use tv_screens::guide::GuideScreen;
use tv_screens::lists::{ChannelList, Column, History, Mode};
use tv_screens::panels::{ContextMenu, Search, SettingsPanel, SidePanel};
use tv_screens::player::model::TileKind;
use tv_screens::player::{Overlay, PlayerScreen};
use tv_screens::time::hhmm;
use tv_ui::consts::{WCODE_FASTFORWARD, WCODE_PAUSE, WCODE_PLAY, WCODE_PLAYPAUSE, WCODE_REWIND};

use super::*;

/// The media keys, which only a remote has (the simulator's [`REMOTE`] table leaves them out).
const MEDIA: [(&str, u32); 5] = [
    ("Play", WCODE_PLAY),
    ("Pause", WCODE_PAUSE),
    ("PlayPause", WCODE_PLAYPAUSE),
    ("Rewind", WCODE_REWIND),
    ("FF", WCODE_FASTFORWARD),
];

/// The screen in front.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Top {
    /// The player with nothing up.
    Player,
    /// The info panel without its tile row, after a zap.
    Banner,
    Info,
    Playback,
    Card,
    Guide,
    /// The guide with the focus in its groups panel.
    GuideGroups,
    /// The channel list, in overlay or preview mode, focus in this column.
    List(Mode, Column),
    History,
    Search,
    /// A side menu: the channel, group, player or main menu (the focus names its title).
    Menu,
    Settings,
}

/// Where the video is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Video {
    Full,
    /// The guide's preview, top left.
    Guide,
    /// The channel list's preview mode, top right.
    List,
    /// The guide's picture-in-picture window, bottom right.
    Pip,
}

impl Video {
    fn rect(self) -> Option<Rect> {
        let guide = || GuideScreen::new(&FakeData::new(NOW, 1), Group::All);
        match self {
            Video::Full => None,
            Video::Guide => guide().video_rect(),
            Video::List => Some(PREVIEW_RECT),
            Video::Pip => {
                let mut pip = guide();
                pip.set_pip(true);
                pip.video_rect()
            }
        }
    }
}

/// What the viewer sees.
#[derive(Clone, Debug, PartialEq)]
struct View {
    top: Top,
    /// The focus in words: channel, programme start, group, tile, button or menu row.
    focus: String,
    /// `None`: full screen.
    rect: Option<Rect>,
}

/// A [`Sim`] driven step by step, keeping the steps for the failure message.
struct Walk {
    s: Sim,
    trail: Vec<String>,
    /// BACK reaches the app only when released, down and up together, as webOS 25 hands it
    /// over.
    back_on_release: bool,
}

impl Walk {
    /// The app as started with nothing played: the guide is up.
    fn new() -> Self {
        Walk {
            s: Sim::new(),
            trail: Vec::new(),
            back_on_release: false,
        }
    }

    /// Channel `i` playing live with every screen closed.
    fn playing(i: usize) -> Self {
        Walk::new().live(i)
    }

    /// With `c5`, BACK handed over on release, as the C5 (webOS 25) does.
    fn c5(mut self, c5: bool) -> Self {
        self.back_on_release = c5;
        self
    }

    fn live(mut self, i: usize) -> Self {
        self.s.live(i);
        self.s.shell.navigate(Navigate::Player, None);
        self.s.shell.settle(self.s.t);
        // Past the info banner a channel started shows, and watched long enough to count.
        self.s.run(10_000);
        self
    }

    /// One remote step: `"OK"` a short press, `"long OK"` held past the long-press threshold,
    /// `"hold Down 2000"` held that many milliseconds (the key repeats).
    fn step(&mut self, step: &str) {
        let words: Vec<&str> = step.split_whitespace().collect();
        let (key, held_ms) = match words[..] {
            [key] => (key, None),
            ["long", key] => (key, Some(1_100)),
            ["hold", key, ms] => (key, Some(ms.parse().unwrap())),
            _ => panic!("bad step {step:?}"),
        };
        if key == "Back" && self.back_on_release {
            self.s.run(held_ms.unwrap_or(50));
            self.edge(key, Edge::Down);
            self.edge(key, Edge::Up);
        } else {
            self.edge(key, Edge::Down);
            match held_ms {
                Some(ms) => self.s.run(ms),
                None => self.s.t += 50,
            }
            self.edge(key, Edge::Up);
        }
        self.s.run(500);
        self.trail.push(step.to_owned());
    }

    /// `key` held 1100 ms with its key-down handed over at once: what shows 100 ms after the
    /// key-down (checked as [`expect`](Self::expect) does) is still what shows once it is up.
    #[track_caller]
    fn held(&mut self, key: &str, top: Top, focus: &str, video: Video) -> View {
        self.edge(key, Edge::Down);
        self.s.run(100);
        self.trail.push(format!("{key} down 100 ms"));
        let view = self.expect(top, focus, video);
        self.s.run(1_000);
        self.edge(key, Edge::Up);
        self.s.run(500);
        self.trail.push(format!("{key} up at 1100 ms"));
        assert_eq!(self.view(), view, "after {:?}", self.trail);
        view
    }

    fn edge(&mut self, name: &str, edge: Edge) {
        let (sym, wcode) = REMOTE
            .iter()
            .find(|r| r.0 == name)
            .map(|r| (r.1, r.2))
            .or_else(|| MEDIA.iter().find(|m| m.0 == name).map(|m| (0, m.1)))
            .unwrap_or_else(|| panic!("no remote key {name:?}"));
        self.s.shell.key(KeyEvent {
            sym,
            wcode,
            edge,
            at_ms: self.s.t as u32,
        });
    }

    /// Take `step`, then check the screen in front, its focus (`*` stands for any run of
    /// characters) and the video.
    #[track_caller]
    fn go(&mut self, step: &str, top: Top, focus: &str, video: Video) -> View {
        self.step(step);
        self.expect(top, focus, video)
    }

    /// Check what the viewer sees now.
    #[track_caller]
    fn expect(&mut self, top: Top, focus: &str, video: Video) -> View {
        let view = self.view();
        let ok = view.top == top && glob(focus, &view.focus) && view.rect == video.rect();
        let back = if self.back_on_release {
            ", BACK on release"
        } else {
            ""
        };
        assert!(
            ok,
            "after {:?}{back}\n  expected {top:?} {focus:?} {video:?}\n  got      {view:?}",
            self.trail
        );
        view
    }

    fn view(&self) -> View {
        let shell = &self.s.shell;
        let data: &dyn TvData = &*shell.store;
        let (top, focus) = match shell.top() {
            None => player(&shell.bottom.screen, data),
            Some(i) => screen(&*shell.stack[i].screen, data),
        };
        View {
            top,
            focus,
            rect: shell.rect,
        }
    }

    /// The programme the guide focuses.
    fn guide_programme(&self) -> Programme {
        let i = self.s.shell.top().expect("a screen is up");
        let any: &dyn Any = &*self.s.shell.stack[i].screen;
        let guide = any.downcast_ref::<GuideScreen>().expect("the guide is up");
        guide.focus().programme.expect("a programme has the focus")
    }

    /// The player calls since the last look.
    fn calls(&self) -> Vec<String> {
        self.s.calls.take()
    }
}

/// `pattern` with `*` matching any run of characters.
fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    for (i, part) in parts.iter().enumerate() {
        if i + 1 == parts.len() {
            return rest.ends_with(part);
        }
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.is_empty()
}

fn name(data: &dyn TvData, key: ChannelKey) -> String {
    data.channel(key)
        .map_or_else(|| "gone".to_owned(), |c| c.name.clone())
}

fn time(t: Timestamp, data: &dyn TvData) -> String {
    hhmm(t, data.utc_offset_secs())
}

/// The player in front: what plays, the panel up and its focus.
fn player(p: &PlayerScreen, data: &dyn TvData) -> (Top, String) {
    let pb = data.playback();
    let mut playing = pb
        .channel
        .map_or_else(|| "nothing".to_owned(), |k| name(data, k));
    if pb.catchup.is_some() {
        playing.push_str(" (archive)");
    }
    if let Some(n) = p.number() {
        playing.push_str(&format!(" #{}", n.digits));
    }
    match p.overlay() {
        Overlay::Hidden => (Top::Player, playing),
        Overlay::Banner => (Top::Banner, playing),
        Overlay::Info { tile } => {
            let tile = match p.tiles().get(tile).map(|t| &t.kind) {
                Some(TileKind::Guide) => "TV guide".to_owned(),
                Some(TileKind::History) => "History".to_owned(),
                Some(TileKind::Channel(k)) => name(data, *k),
                None => "no tile".to_owned(),
            };
            (Top::Info, tile)
        }
        Overlay::Playback => (Top::Playback, format!("{:?}", p.transport_focus())),
        Overlay::Card { index } => (Top::Card, format!("card {index}")),
    }
}

/// A screen over the player.
fn screen(screen: &dyn Screen, data: &dyn TvData) -> (Top, String) {
    let any: &dyn Any = screen;
    if let Some(g) = any.downcast_ref::<GuideScreen>() {
        let f = g.focus();
        if let Some(group) = f.group {
            return (Top::GuideGroups, group.name().to_owned());
        }
        let channel = f.channel.map_or("no channel", |c| c.name.as_str());
        let at = match (&f.programme, f.live) {
            (_, true) => "live".to_owned(),
            (Some(p), false) => time(p.start, data),
            (None, false) => "no programme".to_owned(),
        };
        let manage = if f.managing { " (manage)" } else { "" };
        return (Top::Guide, format!("{channel} @ {at}{manage}"));
    }
    if let Some(l) = any.downcast_ref::<ChannelList>() {
        let channel = l
            .focused_channel()
            .map_or("no channel", |c| c.name.as_str());
        let epg = l.epg();
        let focus = match l.column() {
            Column::Groups => l
                .groups()
                .and_then(|g| g.focused())
                .map_or("no group".to_owned(), |g| g.name().to_owned()),
            Column::Channels => channel.to_owned(),
            Column::Programmes => match epg.and_then(|e| e.focused()) {
                Some(p) => format!("{channel} > {}", time(p.start, data)),
                None => format!("{channel} > no programme"),
            },
            Column::Days => format!("{channel} > day {}", epg.map_or(0, |e| e.day)),
        };
        return (Top::List(l.mode(), l.column()), focus);
    }
    if let Some(h) = any.downcast_ref::<History>() {
        let focus = match h.focused_entry() {
            Some(e) => format!("{} > {}", name(data, e.channel), e.programme.title),
            None => "empty".to_owned(),
        };
        let delete = if h.on_delete() { " (delete)" } else { "" };
        return (Top::History, focus + delete);
    }
    if let Some(s) = any.downcast_ref::<Search>() {
        let focus = match s.focus().and_then(|i| s.hit(i)) {
            Some(hit) => hit.title().to_owned(),
            None => format!("field {:?}", s.draft()),
        };
        return (Top::Search, focus);
    }
    if let Some(m) = any.downcast_ref::<ContextMenu>() {
        return (Top::Menu, side(m.panel()));
    }
    if let Some(p) = any.downcast_ref::<SettingsPanel>() {
        return (Top::Settings, side(p.panel()));
    }
    panic!("a screen the navigation tests do not know is in front")
}

/// A side panel's page title and focused row.
fn side<Id: Copy + PartialEq>(panel: &SidePanel<Id>) -> String {
    let page = panel.page();
    let row = page.rows().get(page.focus()).map_or("", |r| r.label());
    format!("{} > {row}", page.title())
}

#[test]
fn glob_matches_runs() {
    assert!(glob("Channel 1 @ live", "Channel 1 @ live"));
    assert!(glob("Channel 1 @ *", "Channel 1 @ 21:30"));
    assert!(glob("* > Search", "Menu > Search"));
    assert!(glob("a*c*e", "abcde"));
    assert!(!glob("Channel 1 @ *", "Channel 12 @ live"));
    assert!(!glob("a*c", "abcd"));
    assert!(!glob("live", "live!"));
}

/// The C1 trial (2026-10-19): Left from the live programme opened the groups panel and
/// Left again the main menu, so the grid never went back in time, and every panel toggle moved
/// the preview. Left walks into the past on every channel and opens nothing; Back returns to
/// live, then opens the groups panel over the preview, and Back closes it with the focus where
/// it was (nothing plays; with a channel playing that Back returns to the player).
/// `xtask/replay/guide-left-back.keys` is the same sequence for a television.
#[test]
fn guide_left_walks_into_the_past_and_back_opens_the_groups_panel() {
    for c5 in [false, true] {
        let mut w = Walk::new().c5(c5);
        w.expect(Top::Guide, "Channel 1 @ live", Video::Guide);
        // Channel 3 has no catch-up, as on the C1 trial; Channel 1 has.
        for (down, channel) in [(2, "Channel 3"), (0, "Channel 1")] {
            for i in 0..down {
                let at = format!("Channel {} @ live", i + 2);
                w.go("Down", Top::Guide, &at, Video::Guide);
            }
            w.calls();
            let on_air = w.guide_programme();
            let mut start = on_air.start;
            let earlier = format!("{channel} @ *");
            for _ in 0..3 {
                w.go("Left", Top::Guide, &earlier, Video::Guide);
                let p = w.guide_programme();
                assert_eq!(p.stop, start, "{channel}: the programme just before");
                start = p.start;
            }
            w.go("hold Left 2000", Top::Guide, &earlier, Video::Guide);
            assert!(w.guide_programme().start < start, "held, Left repeats");
            let live = format!("{channel} @ live");
            w.go("Back", Top::Guide, &live, Video::Guide);
            assert_eq!(w.guide_programme(), on_air);
            w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
            w.go("Back", Top::Guide, &live, Video::Guide);
            w.go("Left", Top::Guide, &earlier, Video::Guide);
            w.go("Back", Top::Guide, &live, Video::Guide);
            assert_eq!(w.calls(), Vec::<String>::new(), "the preview never moved");
            w.go("Up", Top::Guide, "Channel * @ live", Video::Guide);
            w.go("Down", Top::Guide, &live, Video::Guide);
            // Back to the first channel for the next round.
            for _ in 0..down {
                w.go("Up", Top::Guide, "Channel * @ live", Video::Guide);
            }
        }
    }
}

#[test]
fn guide_keys() {
    for c5 in [false, true] {
        let mut w = Walk::new().c5(c5);
        w.expect(Top::Guide, "Channel 1 @ live", Video::Guide);
        // Up and Down move through the channels, wrapping; held, they repeat.
        w.go("Down", Top::Guide, "Channel 2 @ live", Video::Guide);
        w.go("Up", Top::Guide, "Channel 1 @ live", Video::Guide);
        w.go("Up", Top::Guide, "Channel 12 @ live", Video::Guide);
        w.go("Down", Top::Guide, "Channel 1 @ live", Video::Guide);
        // CH- and CH+ turn a page.
        w.go("CH-", Top::Guide, "Channel * @ live", Video::Guide);
        let paged = w.view().focus;
        assert!(
            !["Channel 1 @ live", "Channel 2 @ live"].contains(&paged.as_str()),
            "a page down: {paged}"
        );
        w.go("CH+", Top::Guide, "Channel 1 @ live", Video::Guide);
        w.go(
            "hold Down 1500",
            Top::Guide,
            "Channel * @ live",
            Video::Guide,
        );
        assert!(
            !w.view().focus.starts_with("Channel 2 "),
            "held, Down repeats"
        );
        w.go("hold Up 1500", Top::Guide, "Channel * @ live", Video::Guide);
        let top = w.view().focus;
        // Right moves to later programmes (repeating while held), Back to the current time.
        let on_air = w.guide_programme();
        w.go("Right", Top::Guide, "Channel * @ *", Video::Guide);
        assert_eq!(w.guide_programme().start, on_air.stop, "the next programme");
        w.go("hold Right 1500", Top::Guide, "Channel * @ *", Video::Guide);
        w.go("Back", Top::Guide, &top, Video::Guide);
        // Long OK: the channel's menu over the guide; Back closes it.
        w.go(
            "long OK",
            Top::Menu,
            "Channel * > Add to favorites",
            Video::Guide,
        );
        w.go("Back", Top::Guide, &top, Video::Guide);
        // OK previews the channel in the guide; OK again plays it full screen.
        w.go("OK", Top::Guide, &top, Video::Guide);
        assert!(w.s.playing().channel.is_some(), "previewing");
        let playing = w.go("OK", Top::Banner, "Channel *", Video::Full).focus;
        // Back from the player is the guide again; Back at live opens the groups panel and Back
        // there leaves the guide for the player, which goes on with what played when the guide
        // opened (its zap banner still up).
        w.go("Back", Top::Guide, "Channel * @ live", Video::Guide);
        w.go("Down", Top::Guide, "Channel * @ live", Video::Guide);
        w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
        w.go("Back", Top::Banner, &playing, Video::Full);
    }
}

#[test]
fn guide_ok_on_the_past_plays_from_the_archive_only_with_catch_up() {
    for c5 in [false, true] {
        let mut w = Walk::new().c5(c5);
        // Channel 1 has catch-up: OK on the programme before plays it from the archive.
        w.go("Left", Top::Guide, "Channel 1 @ *", Video::Guide);
        w.go("OK", Top::Guide, "Channel 1 @ *", Video::Guide);
        assert!(w.s.playing().catchup.is_some(), "plays in the preview");
        w.go("OK", Top::Banner, "Channel 1 (archive)", Video::Full);
        // Channel 3 has none: OK shows that and plays nothing.
        let mut w = Walk::new().c5(c5);
        w.go("Down", Top::Guide, "Channel 2 @ live", Video::Guide);
        w.go("Down", Top::Guide, "Channel 3 @ live", Video::Guide);
        w.go("Left", Top::Guide, "Channel 3 @ *", Video::Guide);
        w.go("OK", Top::Guide, "Channel 3 @ *", Video::Guide);
        assert_eq!(w.s.playing().channel, None);
        assert!(w.s.toast().is_some(), "says it has no archive");
    }
}

#[test]
fn guide_groups_panel_keys() {
    for c5 in [false, true] {
        let mut w = Walk::new().c5(c5);
        w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
        // Up and Down move; the guide follows the group focused (History is a screen of its own).
        w.go("Up", Top::GuideGroups, "Favorites", Video::Guide);
        w.go("Down", Top::GuideGroups, "All channels", Video::Guide);
        w.go("Down", Top::GuideGroups, "History", Video::Guide);
        w.go("Down", Top::GuideGroups, "News", Video::Guide);
        // OK opens the group: News is channels 1, 4, 7 and 10.
        w.go("OK", Top::Guide, "Channel 1 @ live", Video::Guide);
        w.go("Down", Top::Guide, "Channel 4 @ live", Video::Guide);
        // Left in the panel is the main menu, which Back closes; long Left goes to the top.
        w.go("Back", Top::GuideGroups, "News", Video::Guide);
        w.go("Left", Top::Menu, "Menu > Search", Video::Guide);
        w.go("Back", Top::GuideGroups, "News", Video::Guide);
        w.go("long Left", Top::GuideGroups, "Favorites", Video::Guide);
        // OK on History opens it; Back comes back to the panel.
        w.go("Down", Top::GuideGroups, "All channels", Video::Guide);
        w.go("Down", Top::GuideGroups, "History", Video::Guide);
        w.go("OK", Top::History, "empty", Video::Guide);
        w.go("Back", Top::GuideGroups, "History", Video::Guide);
        // Long OK: the group's menu.
        w.go("Up", Top::GuideGroups, "All channels", Video::Guide);
        w.go("long OK", Top::Menu, "All channels > *", Video::Guide);
        w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
        // With nothing playing Back only closes the panel (no empty player); the guide shows the
        // group last focused. With something playing it returns to the player (guide_keys).
        w.go("Back", Top::Guide, "Channel 1 @ live", Video::Guide);
        w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
        w.go("Back", Top::Guide, "Channel 1 @ live", Video::Guide);
    }
}

#[test]
fn player_keys() {
    for c5 in [false, true] {
        let mut w = Walk::playing(0).c5(c5);
        w.expect(Top::Player, "Channel 1", Video::Full);
        // Up, Down, CH+ and CH- zap; the info banner shows each zap.
        w.go("Up", Top::Banner, "Channel 2", Video::Full);
        w.go("CH+", Top::Banner, "Channel 3", Video::Full);
        w.go("CH-", Top::Banner, "Channel 2", Video::Full);
        w.go("Down", Top::Banner, "Channel 1", Video::Full);
        w.s.run(10_000);
        w.expect(Top::Player, "Channel 1", Video::Full);
        // Long Right: the channel watched before (Channel 2 played long enough to count).
        w.go("Up", Top::Banner, "Channel 2", Video::Full);
        w.s.run(10_000);
        w.go("long Right", Top::Banner, "Channel 1", Video::Full);
        w.s.run(10_000);
        // Long CH+: the programme card; Right steps on, Left back, Back closes it.
        w.go("long CH+", Top::Card, "card 0", Video::Full);
        w.go("Right", Top::Card, "card 1", Video::Full);
        w.go("Left", Top::Card, "card 0", Video::Full);
        w.go("Back", Top::Player, "Channel 1", Video::Full);
        // Digits type a channel number, played once the typing stops.
        w.go("1", Top::Player, "Channel 1 #1", Video::Full);
        w.go("2", Top::Player, "Channel 1 #12", Video::Full);
        w.s.run(3_000);
        w.expect(Top::Banner, "Channel 12", Video::Full);
        w.s.run(10_000);
        // Long Up: History; Back closes it.
        w.go("long Up", Top::History, "empty", Video::Full);
        w.go("Back", Top::Player, "Channel 12", Video::Full);
        // Long OK: the channel's menu.
        w.go("long OK", Top::Menu, "Channel 12 > *", Video::Full);
        w.go("Back", Top::Player, "Channel 12", Video::Full);
        // Back: the guide, Back again its groups panel and Back there the player; long Down:
        // Settings.
        w.go("Back", Top::Guide, "Channel 12 @ live", Video::Guide);
        w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
        w.go("Back", Top::Player, "Channel 12", Video::Full);
        w.go(
            "long Down",
            Top::Settings,
            "Settings > General",
            Video::Full,
        );
        w.go("Back", Top::Player, "Channel 12", Video::Full);
    }
}

#[test]
fn info_and_playback_panels() {
    for c5 in [false, true] {
        let mut w = Walk::playing(0).c5(c5);
        // OK: the info panel; its tile row is TV guide, History, then the recent channels.
        w.go("OK", Top::Info, "TV guide", Video::Full);
        w.go("Right", Top::Info, "History", Video::Full);
        w.go("Left", Top::Info, "TV guide", Video::Full);
        w.go("OK", Top::Guide, "Channel 1 @ live", Video::Guide);
        w.go("OK", Top::Player, "Channel 1", Video::Full);
        w.go("OK", Top::Info, "TV guide", Video::Full);
        w.go("Right", Top::Info, "History", Video::Full);
        w.go("OK", Top::History, "empty", Video::Full);
        w.go("Back", Top::Player, "Channel 1", Video::Full);
        // Down: the player menu; Back closes the info panel.
        w.go("OK", Top::Info, "TV guide", Video::Full);
        w.go("Down", Top::Menu, "Channel 1 > *", Video::Full);
        w.go("Back", Top::Player, "Channel 1", Video::Full);
        w.go("OK", Top::Info, "TV guide", Video::Full);
        w.go("Back", Top::Player, "Channel 1", Video::Full);
        // The panel hides by itself after the timeout.
        w.go("OK", Top::Info, "TV guide", Video::Full);
        w.s.run(6_000);
        w.expect(Top::Player, "Channel 1", Video::Full);

        // On a catch-up programme Up opens the playback panel: the seek bar, then the buttons.
        let key = w.s.key(0);
        w.s.catchup(key, &programme("Earlier"), 60);
        w.calls();
        w.go("OK", Top::Info, "TV guide", Video::Full);
        w.go("Up", Top::Playback, "Seek", Video::Full);
        w.go("Left", Top::Playback, "Seek", Video::Full);
        w.s.run(2_000);
        assert!(
            w.calls().iter().any(|c| c.starts_with("seek ")),
            "Left on the seek bar seeks"
        );
        w.go("Down", Top::Playback, "Button(*)", Video::Full);
        let button = w.view().focus;
        w.go("Right", Top::Playback, "Button(*)", Video::Full);
        assert_ne!(w.view().focus, button, "Right moves along the buttons");
        w.go("Left", Top::Playback, &button, Video::Full);
        w.go("Up", Top::Playback, "Seek", Video::Full);
        w.go("Down", Top::Playback, &button, Video::Full);
        w.go("Down", Top::Info, "TV guide", Video::Full);
        w.go("Up", Top::Playback, "Seek", Video::Full);
        w.go("Back", Top::Player, "Channel 1 (archive)", Video::Full);
        // The media keys: Play/Pause pauses and resumes, Rewind and FF open the panel and seek.
        w.calls();
        w.go("PlayPause", Top::Player, "Channel 1 (archive)", Video::Full);
        w.go("Play", Top::Player, "Channel 1 (archive)", Video::Full);
        w.go("Pause", Top::Player, "Channel 1 (archive)", Video::Full);
        assert_eq!(w.calls(), ["paused true", "paused false", "paused true"]);
        // Back first drops a seek not sent yet, then closes the panel.
        w.go("Rewind", Top::Playback, "Seek", Video::Full);
        w.go("Back", Top::Playback, "Seek", Video::Full);
        w.go("Back", Top::Player, "Channel 1 (archive)", Video::Full);
        assert!(w.calls().iter().all(|c| !c.starts_with("seek ")), "dropped");
        w.go("FF", Top::Playback, "Seek", Video::Full);
        w.s.run(2_000);
        assert!(w.calls().iter().any(|c| c.starts_with("seek ")));
    }
}

/// Left and Right in full screen skip by the skip step, Back on key-down or release: on
/// a live channel with catch-up Left plays the archive that far behind now and Right back to
/// the live edge returns to live; without catch-up a toast says so. Held, Left and Right open
/// the channel list and play the last channel, CH- and CH+ the preview list and the card.
#[test]
fn player_left_and_right_skip_and_their_old_actions_are_held() {
    for c5 in [false, true] {
        let mut w = Walk::playing(0).c5(c5);
        w.calls();
        w.go("Left", Top::Playback, "Seek", Video::Full);
        w.s.run(1_000);
        assert_eq!(w.calls(), ["open time shift Channel 1 None"]);
        let now = {
            let data: &dyn TvData = &*w.s.shell.store;
            data.now()
        };
        let pb = w.s.playing();
        let shifted = pb.catchup.as_ref().map(|p| p.start + pb.position_secs);
        let back = PlayerSettings::default().skip_back_secs;
        assert_eq!(shifted, Some(now - back), "c5 {c5}");
        w.go("Right", Top::Playback, "Seek", Video::Full);
        w.s.run(1_000);
        assert_eq!(w.calls(), ["go live"]);
        assert_eq!(w.s.playing().catchup, None);
        w.go("Back", Top::Player, "Channel 1", Video::Full);

        // Channel 3 keeps no archive.
        w.go("CH+", Top::Banner, "Channel 2", Video::Full);
        w.go("CH+", Top::Banner, "Channel 3", Video::Full);
        w.s.run(10_000);
        w.calls();
        for key in ["Left", "Right"] {
            w.go(key, Top::Player, "Channel 3", Video::Full);
            let toast = w.s.toast();
            assert_eq!(toast.as_deref(), Some("This channel has no catch-up"));
        }
        assert!(w.calls().iter().all(|c| !c.starts_with("open")), "c5 {c5}");

        let list = |mode| Top::List(mode, Column::Channels);
        w.go("long Left", list(Mode::Overlay), "Channel 3", Video::Full);
        w.go("Back", Top::Player, "Channel 3", Video::Full);
        w.go("long CH-", list(Mode::Preview), "Channel 3", Video::List);
        w.go("Back", Top::Player, "Channel 3", Video::Full);
        w.go("long CH+", Top::Card, "card 0", Video::Full);
        w.go("Back", Top::Player, "Channel 3", Video::Full);
        w.go("long Right", Top::Banner, "Channel 1", Video::Full);
    }
}

#[test]
fn channel_list_keys() {
    for c5 in [false, true] {
        let mut w = Walk::playing(0).c5(c5);
        let list = |column| Top::List(Mode::Overlay, column);
        // Long Left: the list over the picture, on the channel playing.
        w.go(
            "long Left",
            list(Column::Channels),
            "Channel 1",
            Video::Full,
        );
        w.go("Down", list(Column::Channels), "Channel 2", Video::Full);
        w.go("Up", list(Column::Channels), "Channel 1", Video::Full);
        w.go(
            "hold Down 1500",
            list(Column::Channels),
            "Channel *",
            Video::Full,
        );
        // Long Left: the top of the list; CH- and CH+ turn a page.
        w.go(
            "long Left",
            list(Column::Channels),
            "Channel 1",
            Video::Full,
        );
        w.go("CH-", list(Column::Channels), "Channel 9", Video::Full);
        w.go("CH+", list(Column::Channels), "Channel 1", Video::Full);
        // Right: the channel's programmes, then the days; Left back one column at a time.
        w.go(
            "Right",
            list(Column::Programmes),
            "Channel 1 > *",
            Video::Full,
        );
        w.go(
            "Down",
            list(Column::Programmes),
            "Channel 1 > *",
            Video::Full,
        );
        w.go(
            "Right",
            list(Column::Days),
            "Channel 1 > day 0",
            Video::Full,
        );
        w.go("Up", list(Column::Days), "Channel 1 > day -1", Video::Full);
        w.go(
            "Left",
            list(Column::Programmes),
            "Channel 1 > *",
            Video::Full,
        );
        w.go("Left", list(Column::Channels), "Channel 1", Video::Full);
        // Left from the channels: the groups panel beside them; OK or Right returns with its group.
        w.go("Left", list(Column::Groups), "All channels", Video::Full);
        w.go("Down", list(Column::Groups), "History", Video::Full);
        w.go("Down", list(Column::Groups), "News", Video::Full);
        w.go("OK", list(Column::Channels), "Channel 1", Video::Full);
        w.go("Down", list(Column::Channels), "Channel 4", Video::Full);
        w.go("Left", list(Column::Groups), "News", Video::Full);
        w.go("Down", list(Column::Groups), "Sport", Video::Full);
        w.go("Right", list(Column::Channels), "Channel 2", Video::Full);
        // Back in the panel: the channels; Back again closes the list.
        w.go("Left", list(Column::Groups), "Sport", Video::Full);
        w.go("Back", list(Column::Channels), "Channel 2", Video::Full);
        w.go("Back", Top::Player, "Channel 1", Video::Full);
        // It opens again on the group picked last. OK plays the channel and closes the list.
        w.go(
            "long Left",
            list(Column::Channels),
            "Channel 2",
            Video::Full,
        );
        w.calls();
        w.go("OK", Top::Banner, "Channel 2", Video::Full);
        assert_eq!(w.calls(), ["open live Channel 2 None"]);
        w.s.run(10_000);
        // Back closes it without playing.
        w.go(
            "long Left",
            list(Column::Channels),
            "Channel 2",
            Video::Full,
        );
        w.go("Down", list(Column::Channels), "Channel 5", Video::Full);
        w.go("Back", Top::Player, "Channel 2", Video::Full);
    }
}

#[test]
fn channel_list_preview_mode() {
    for c5 in [false, true] {
        let mut w = Walk::playing(0).c5(c5);
        let list = |column| Top::List(Mode::Preview, column);
        // Long CH-: the list with the video moved into its preview.
        w.go("long CH-", list(Column::Channels), "Channel 1", Video::List);
        w.calls();
        // The first OK previews, the list stays; OK again on it goes full screen.
        w.go("Down", list(Column::Channels), "Channel 2", Video::List);
        w.go("OK", list(Column::Channels), "Channel 2", Video::List);
        assert_eq!(
            w.calls(),
            [format!("open live Channel 2 {:?}", Some(PREVIEW_RECT))]
        );
        w.go("OK", Top::Banner, "Channel 2", Video::Full);
        // Programmes and days work as in the overlay, with the video kept in the preview.
        w.go("long CH-", list(Column::Channels), "Channel 2", Video::List);
        w.go(
            "Right",
            list(Column::Programmes),
            "Channel 2 > *",
            Video::List,
        );
        w.go(
            "Right",
            list(Column::Days),
            "Channel 2 > day 0",
            Video::List,
        );
        w.go(
            "Left",
            list(Column::Programmes),
            "Channel 2 > *",
            Video::List,
        );
        w.go("Left", list(Column::Channels), "Channel 2", Video::List);
        // Back closes, committing the channel previewed.
        w.go("Down", list(Column::Channels), "Channel 3", Video::List);
        w.go("OK", list(Column::Channels), "Channel 3", Video::List);
        w.go("Back", Top::Banner, "Channel 3", Video::Full);
    }
}

#[test]
fn history_keys() {
    for c5 in [false, true] {
        let mut w = Walk::playing(1).c5(c5);
        // Two archive programmes watched long enough to be kept.
        for (ch, title) in [(0, "First"), (1, "Second")] {
            let key = w.s.key(ch);
            w.s.catchup(key, &programme(title), 60);
            w.s.run(31_000);
        }
        w.s.live(2);
        w.s.run(500);
        // Long Up opens it on the programme watched last.
        w.go("long Up", Top::History, "Channel 2 > Second", Video::Full);
        w.go("Up", Top::History, "Channel 1 > First", Video::Full);
        w.go("Down", Top::History, "Channel 2 > Second", Video::Full);
        w.go("Up", Top::History, "Channel 1 > First", Video::Full);
        // OK resumes the programme full screen, where it was left.
        w.calls();
        w.go("OK", Top::Banner, "Channel 1 (archive)", Video::Full);
        assert_eq!(w.s.playing().position_secs, 60);
        // Back closes History.
        w.s.run(10_000);
        w.go("long Up", Top::History, "Channel * > *", Video::Full);
        w.go("Back", Top::Player, "Channel 1 (archive)", Video::Full);
    }
}

#[test]
fn menus_and_settings() {
    for c5 in [false, true] {
        let mut w = Walk::new().c5(c5);
        // The main menu from the guide's groups panel.
        w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
        w.go("Left", Top::Menu, "Menu > Search", Video::Guide);
        w.go("Down", Top::Menu, "Menu > TV guide", Video::Guide);
        w.go("Down", Top::Menu, "Menu > Channels", Video::Guide);
        w.go("Down", Top::Menu, "Menu > History", Video::Guide);
        w.go("Down", Top::Menu, "Menu > Settings", Video::Guide);
        w.go("Up", Top::Menu, "Menu > History", Video::Guide);
        w.go("Down", Top::Menu, "Menu > Settings", Video::Guide);
        // OK opens Settings; Right or OK opens a section, Left or Back goes up a level.
        w.go("OK", Top::Settings, "Settings > General", Video::Guide);
        w.go("Down", Top::Settings, "Settings > Playlists", Video::Guide);
        w.go("Right", Top::Settings, "Playlists > *", Video::Guide);
        w.go("Left", Top::Settings, "Settings > Playlists", Video::Guide);
        w.go("OK", Top::Settings, "Playlists > *", Video::Guide);
        w.go("Back", Top::Settings, "Settings > Playlists", Video::Guide);
        w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
        // The main menu's TV guide and Channels rows.
        w.go("Left", Top::Menu, "Menu > Search", Video::Guide);
        w.go("Down", Top::Menu, "Menu > TV guide", Video::Guide);
        w.go("OK", Top::Guide, "Channel 1 @ live", Video::Guide);
        w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
        w.go("Left", Top::Menu, "Menu > Search", Video::Guide);
        w.go("Down", Top::Menu, "Menu > TV guide", Video::Guide);
        w.go("Down", Top::Menu, "Menu > Channels", Video::Guide);
        w.go(
            "OK",
            Top::List(Mode::Overlay, Column::Channels),
            "Channel 1",
            Video::Full,
        );
        // With nothing playing Back leaves the list for the guide, not an empty player.
        w.go("Back", Top::Guide, "Channel 1 @ live", Video::Guide);
        // The channel menu: Description opens a page, Left steps back out of it.
        w.go(
            "long OK",
            Top::Menu,
            "Channel 1 > Add to favorites",
            Video::Guide,
        );
        w.go(
            "Down",
            Top::Menu,
            "Channel 1 > Programme description",
            Video::Guide,
        );
        w.go("Right", Top::Menu, "* > ", Video::Guide);
        w.go(
            "Left",
            Top::Menu,
            "Channel 1 > Programme description",
            Video::Guide,
        );
        w.go("Back", Top::Guide, "Channel 1 @ live", Video::Guide);
        // OK on Add to favorites stars the channel and closes the menu.
        w.go(
            "long OK",
            Top::Menu,
            "Channel 1 > Add to favorites",
            Video::Guide,
        );
        w.go("OK", Top::Guide, "Channel 1 @ live", Video::Guide);
        assert_eq!(w.s.toast().as_deref(), Some("Added to favorites"));
        w.go(
            "long OK",
            Top::Menu,
            "Channel 1 > Remove from favorites",
            Video::Guide,
        );
        w.go("Back", Top::Guide, "Channel 1 @ live", Video::Guide);
    }
}

#[test]
fn search_from_the_main_menu() {
    for c5 in [false, true] {
        let mut w = Walk::new().c5(c5);
        w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
        w.go("Left", Top::Menu, "Menu > Search", Video::Guide);
        w.go("OK", Top::Search, "field \"\"", Video::Guide);
        assert_eq!(
            w.s.shell.take_text_input(),
            Some(true),
            "the keyboard is up"
        );
        w.s.shell.text("Channel 5", w.s.t);
        w.s.run(500);
        w.expect(Top::Search, "field \"Channel 5\"", Video::Guide);
        // Down leaves the field for the results; OK plays the hit full screen.
        w.go("Down", Top::Search, "*Channel 5", Video::Guide);
        assert_eq!(w.s.shell.take_text_input(), Some(false));
        w.go("Back", Top::Search, "field \"Channel 5\"", Video::Guide);
        w.go("Down", Top::Search, "*Channel 5", Video::Guide);
        w.calls();
        w.go("OK", Top::Banner, "Channel 5", Video::Full);
        let preview = Video::Guide.rect();
        assert_eq!(
            w.calls(),
            [
                format!("open live Channel 5 {preview:?}"),
                "rect None".into()
            ],
            "opened where the video was, then full screen"
        );
    }
}

#[test]
fn manage_favorites_from_the_channel_menu() {
    for c5 in [false, true] {
        let mut w = Walk::new().c5(c5);
        w.go(
            "long OK",
            Top::Menu,
            "Channel 1 > Add to favorites",
            Video::Guide,
        );
        // Programme description and, when the archive has it, Watch from start come first.
        while w.view().focus != "Channel 1 > Manage favorites" {
            assert!(w.trail.len() < 5, "no Manage favorites row");
            w.go("Down", Top::Menu, "Channel 1 > *", Video::Guide);
        }
        w.go("OK", Top::Guide, "Channel 1 @ live (manage)", Video::Guide);
        // OK stars the channel instead of playing it; Back leaves the mode.
        w.go("OK", Top::Guide, "Channel 1 @ live (manage)", Video::Guide);
        assert!(w.s.shell.store.is_favorite(w.s.key(0)));
        assert_eq!(w.s.playing().channel, None);
        w.go("Back", Top::Guide, "Channel 1 @ live", Video::Guide);
    }
}

/// Down from the info panel: the player menu, with its Picture in picture row (Channel 1
/// playing).
fn open_pip(w: &mut Walk) {
    w.go("OK", Top::Info, "TV guide", Video::Full);
    w.go("Down", Top::Menu, "Channel 1 > *", Video::Full);
    let rows = w.trail.len();
    while w.view().focus != "Channel 1 > Picture in picture" {
        assert!(w.trail.len() < rows + 12, "no Picture in picture row");
        w.go("Down", Top::Menu, "Channel 1 > *", Video::Full);
    }
    w.go("OK", Top::Guide, "Channel 1 @ live", Video::Pip);
}

#[test]
fn picture_in_picture_from_the_player_menu() {
    for c5 in [false, true] {
        let mut w = Walk::playing(0).c5(c5);
        open_pip(&mut w);
        // The keys stay the guide's: OK on another channel plays it in the window, OK on the
        // channel in the window plays it full screen.
        w.go("Down", Top::Guide, "Channel 2 @ live", Video::Pip);
        w.calls();
        w.go("OK", Top::Guide, "Channel 2 @ live", Video::Pip);
        let pip = Video::Pip.rect();
        assert_eq!(w.calls(), [format!("open live Channel 2 {pip:?}")]);
        w.go("OK", Top::Banner, "Channel 2", Video::Full);
        // Back at the current time puts the video back in the preview, Back again opens the groups
        // panel and Back there returns to the player.
        let mut w = Walk::playing(0).c5(c5);
        open_pip(&mut w);
        w.go("Back", Top::Guide, "Channel 1 @ live", Video::Guide);
        w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
        w.go("Back", Top::Player, "Channel 1", Video::Full);
    }
}

/// webOS keeps BACK while it is held and closes the app after a long hold, on the C1 (webOS
/// 6.5) and the C5 (webOS 25) alike, and the C5 hands it to the app only on release.
/// So Back acts once and nothing needs it held: long Down in the player opens Settings, Back in
/// the guide's groups panel returns to the player, the channel list closes with Back from its
/// channels. This walk hands BACK over as the C5 does; `xtask/replay/c5-long-back.keys` takes
/// the same way on a TV.
#[test]
fn the_old_long_back_actions_have_other_keys() {
    let mut w = Walk::playing(0).c5(true);
    let list = |column| Top::List(Mode::Overlay, column);
    w.expect(Top::Player, "Channel 1", Video::Full);
    // Player: Down still zaps (on release, as Up does); long Down opens Settings.
    w.go("Down", Top::Banner, "Channel 12", Video::Full);
    w.go("Up", Top::Banner, "Channel 1", Video::Full);
    w.s.run(10_000);
    w.go(
        "long Down",
        Top::Settings,
        "Settings > General",
        Video::Full,
    );
    w.go("Back", Top::Player, "Channel 1", Video::Full);
    // A held Back is the short Back the system hands over on release: the guide.
    w.go(
        "hold Back 1100",
        Top::Guide,
        "Channel 1 @ live",
        Video::Guide,
    );
    // Guide: Back to live, Back to the groups panel, Back to the player with what played when
    // the guide opened, whatever was previewed since.
    w.go("Down", Top::Guide, "Channel 2 @ live", Video::Guide);
    w.go("OK", Top::Guide, "Channel 2 @ live", Video::Guide);
    w.go("Left", Top::Guide, "Channel 2 @ *", Video::Guide);
    w.go("Back", Top::Guide, "Channel 2 @ live", Video::Guide);
    w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
    w.go("Back", Top::Banner, "Channel 1", Video::Full);
    w.s.run(10_000);
    // Right in the panel goes back to the grid instead; Left is the main menu, with Settings.
    w.go("Back", Top::Guide, "Channel 1 @ live", Video::Guide);
    w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
    w.go("Right", Top::Guide, "Channel 1 @ live", Video::Guide);
    w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
    w.go("Left", Top::Menu, "Menu > Search", Video::Guide);
    w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
    w.go("Back", Top::Player, "Channel 1", Video::Full);
    // Channel list: a held Back in the groups panel is Back to the channels; Back closes.
    w.go(
        "long Left",
        list(Column::Channels),
        "Channel 1",
        Video::Full,
    );
    w.go("Left", list(Column::Groups), "All channels", Video::Full);
    w.go(
        "hold Back 1100",
        list(Column::Channels),
        "Channel 1",
        Video::Full,
    );
    w.go("Back", Top::Player, "Channel 1", Video::Full);
    // Picture in picture: Back puts the video back in the preview, then the same way out.
    open_pip(&mut w);
    w.go("Back", Top::Guide, "Channel 1 @ live", Video::Guide);
    w.go("Back", Top::GuideGroups, "All channels", Video::Guide);
    w.go("Back", Top::Player, "Channel 1", Video::Full);
}

/// Back is never long, even when its key-down comes at once and the key stays down: it acts on
/// the key-down, and the key-up 1.1 s later changes nothing. In the player, the guide's grid and
/// groups panel (playing or not) and the channel list's groups panel and channels.
#[test]
fn a_held_back_is_one_short_back() {
    let list = |column| Top::List(Mode::Overlay, column);
    let mut w = Walk::playing(0);
    w.held("Back", Top::Guide, "Channel 1 @ live", Video::Guide);
    w.held("Back", Top::GuideGroups, "All channels", Video::Guide);
    w.held("Back", Top::Player, "Channel 1", Video::Full);
    w.go(
        "long Left",
        list(Column::Channels),
        "Channel 1",
        Video::Full,
    );
    w.go("Left", list(Column::Groups), "All channels", Video::Full);
    w.held("Back", list(Column::Channels), "Channel 1", Video::Full);
    w.held("Back", Top::Player, "Channel 1", Video::Full);
    // With nothing playing Back in the groups panel only closes it.
    let mut w = Walk::new();
    w.held("Back", Top::GuideGroups, "All channels", Video::Guide);
    w.held("Back", Top::Guide, "Channel 1 @ live", Video::Guide);
}

/// `xtask/replay/c5-long-back.keys` as shipped, step by step on the host the way the C5 hands
/// BACK over, with what each of its shots should show.
#[test]
fn the_c5_replay_script_reaches_each_long_back_action() {
    let script = include_str!("../../../../../xtask/replay/c5-long-back.keys");
    let shots: [(&str, Top, &str, Video); 9] = [
        ("start", Top::Player, "Channel 1", Video::Full),
        ("settings", Top::Settings, "Settings > General", Video::Full),
        ("player", Top::Player, "Channel 1", Video::Full),
        ("guide", Top::Guide, "Channel 1 @ live", Video::Guide),
        ("moved", Top::Guide, "Channel 2 @ *", Video::Guide),
        ("groups", Top::GuideGroups, "All channels", Video::Guide),
        ("player-again", Top::Banner, "Channel 1", Video::Full),
        (
            "list",
            Top::List(Mode::Overlay, Column::Channels),
            "Channel 1",
            Video::Full,
        ),
        ("player-last", Top::Player, "Channel 1", Video::Full),
    ];
    let mut w = Walk::playing(0).c5(true);
    let mut seen = 0;
    for line in script.lines() {
        let step = line.split('#').next().unwrap_or_default().trim();
        match step.strip_prefix("shot ") {
            Some(name) => {
                let (shot, top, focus, video) = shots[seen];
                assert_eq!(shot, name, "the shots in order");
                w.expect(top, focus, video);
                seen += 1;
            }
            None if !step.is_empty() => w.step(step),
            None => {}
        }
    }
    assert_eq!(seen, shots.len());
}

/// The guide's 0-9 row: digits pick a channel by number and the cursor moves there once the
/// typing stops.
#[test]
fn guide_digits_enter_a_channel_number() {
    let mut w = Walk::new();
    w.go("1", Top::Guide, "Channel 1 @ live", Video::Guide);
    w.go("2", Top::Guide, "Channel 1 @ live", Video::Guide);
    w.s.run(3_000);
    w.expect(Top::Guide, "Channel 12 @ live", Video::Guide);
}

// Keys that nothing implements yet: each test is what the key map says, ignored until it is
// built.

#[test]
#[ignore = "remote-keys.md: long OK in the channel list is the context menu; it plays"]
fn channel_list_long_ok_opens_the_context_menu() {
    let mut w = Walk::playing(0);
    let channels = Top::List(Mode::Overlay, Column::Channels);
    w.go("long Left", channels, "Channel 1", Video::Full);
    w.go("Down", channels, "Channel 2", Video::Full);
    w.go(
        "long OK",
        Top::Menu,
        "Channel 2 > Add to favorites",
        Video::Full,
    );
    w.go("Back", channels, "Channel 2", Video::Full);
}

#[test]
#[ignore = "remote-keys.md: long OK in History is a menu (delete the entry, clear all)"]
fn history_long_ok_opens_its_menu() {
    let mut w = Walk::playing(1);
    let key = w.s.key(0);
    w.s.catchup(key, &programme("Watched"), 60);
    w.s.run(31_000);
    w.s.live(1);
    w.s.run(10_000);
    w.go("long Up", Top::History, "Channel 1 > Watched", Video::Full);
    w.go("long OK", Top::Menu, "* > *", Video::Full);
    w.go("Back", Top::History, "Channel 1 > Watched", Video::Full);
}
