use std::cell::{Cell, RefCell};
use std::rc::Rc;

use iptv::model::{ChannelKey, Programme};
use iptv::refresh::Transport;
use tv_screens::data::fake::FakeData;
use tv_screens::data::{Group, Logos, ScreenMessage};
use tv_screens::input::{Hold, Press};
use tv_screens::lists::PREVIEW_RECT;

use super::*;
use crate::input::REMOTE;
use crate::player::Stream as Open;

/// Every player call, as text; `ends` makes the next poll end the archive programme, as the
/// player does at its end (it goes live by itself).
#[derive(Clone, Default)]
struct Calls(Rc<RefCell<Vec<String>>>, Rc<Cell<bool>>);

impl Calls {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.borrow_mut())
    }
}

impl Player for Calls {
    fn open(&mut self, stream: &Open<'_>, rect: Option<Rect>) {
        let (what, channel) = match stream {
            Open::Live(c) => ("live", c),
            Open::Catchup { channel, .. } => ("archive", channel),
        };
        let line = format!("open {what} {} {rect:?}", channel.name);
        self.0.borrow_mut().push(line);
    }
    fn set_rect(&mut self, rect: Option<Rect>) {
        self.0.borrow_mut().push(format!("rect {rect:?}"));
    }
    fn seek(&mut self, position_secs: i64) {
        self.0.borrow_mut().push(format!("seek {position_secs}"));
    }
    fn go_live(&mut self) {
        self.0.borrow_mut().push("go live".into());
    }
    fn poll(&mut self, _now_ms: u64, playback: &mut Playback) -> bool {
        self.1.take() && playback.catchup.take().is_some()
    }
    fn set_paused(&mut self, paused: bool) {
        self.0.borrow_mut().push(format!("paused {paused}"));
    }
    fn select_audio_track(&mut self, index: usize) {
        self.0.borrow_mut().push(format!("track {index}"));
    }
}

/// A screen that notes what reaches it and answers OK with `reply`; the test keeps a handle to
/// steer it.
#[derive(Clone, Default)]
struct Probe(Rc<ProbeState>);

#[derive(Default)]
struct ProbeState {
    name: &'static str,
    seen: Rc<RefCell<Vec<String>>>,
    rect: Cell<Option<Rect>>,
    editing: Cell<bool>,
    closing: Cell<bool>,
    gone: Cell<bool>,
    /// Starts animating out when it answers Close, as a side panel does.
    slides: Cell<bool>,
    wake: Cell<Option<u64>>,
    reply: RefCell<Output>,
}

impl Probe {
    fn note(&self, what: String) {
        self.0
            .seen
            .borrow_mut()
            .push(format!("{} {what}", self.0.name));
    }
}

impl Screen for Probe {
    fn hold(&self, _: Key) -> Hold {
        Hold::Once
    }
    fn key(&mut self, key: Key, _: Press, _: &dyn TvData, _: u64) -> Output {
        self.note(format!("key {key:?}"));
        if key != Key::Ok {
            return Output::default();
        }
        let out = self.0.reply.take();
        if self.0.slides.get() && out.navigate.contains(&Navigate::Close) {
            self.0.closing.set(true);
        }
        out
    }
    fn text(&mut self, text: &str, _: &dyn TvData) -> Output {
        self.note(format!("text {text}"));
        Output::default()
    }
    fn editing(&self) -> bool {
        self.0.editing.get()
    }
    fn tick(&mut self, _: &dyn TvData, now_ms: u64) -> Output {
        self.note(format!("tick {now_ms}"));
        self.0.wake.set(None);
        Output::default()
    }
    fn next_wake(&self, _: u64) -> Option<u64> {
        self.0.wake.get()
    }
    fn video_rect(&self) -> Option<Rect> {
        self.0.rect.get()
    }
    fn message(&mut self, message: ScreenMessage, _: &dyn TvData) {
        self.note(format!("message {message:?}"));
    }
    fn closing(&self) -> bool {
        self.0.closing.get()
    }
    fn gone(&self) -> bool {
        self.0.gone.get()
    }
    fn draw(&mut self, _: Painter, _: &dyn Measure, _: &dyn Logos) {}
}

const NOW: i64 = 1_700_000_000;

/// The screens read their strings from the locale, fixed to English here as boot sets the TV's.
fn locale() {
    tv_platform::i18n::initialize(tv_platform::i18n::Preference::En, true);
}

struct Sim {
    shell: Shell,
    calls: Calls,
    seen: Rc<RefCell<Vec<String>>>,
    t: u64,
}

impl Sim {
    /// A started shell over 12 synthetic channels, nothing played yet: the guide is up.
    fn new() -> Self {
        locale();
        let calls = Calls::default();
        let mut shell = Shell::new(
            Box::new(FakeData::new(NOW, 12)),
            Box::new(calls.clone()),
            None,
            LogoCache::new(None),
        );
        shell.start(0);
        calls.take();
        Sim {
            shell,
            calls,
            seen: Rc::default(),
            t: 1_000,
        }
    }

    /// Stack a probe named `name`.
    fn push(&mut self, name: &'static str) -> Probe {
        let probe = Probe(Rc::new(ProbeState {
            name,
            seen: self.seen.clone(),
            ..ProbeState::default()
        }));
        self.shell.stack.push(Layer::new(Box::new(probe.clone())));
        self.shell.settle(self.t);
        probe
    }

    fn seen(&self) -> Vec<String> {
        std::mem::take(&mut self.seen.borrow_mut())
    }

    fn edge(&mut self, name: &str, edge: Edge) {
        let &(_, sym, wcode) = REMOTE.iter().find(|r| r.0 == name).unwrap();
        self.shell.key(KeyEvent {
            sym,
            wcode,
            edge,
            at_ms: self.t as u32,
        });
    }

    fn press(&mut self, name: &str) {
        self.edge(name, Edge::Down);
        self.t += 50;
        self.edge(name, Edge::Up);
        self.run(500);
    }

    fn long(&mut self, name: &str) {
        self.edge(name, Edge::Down);
        self.run(1_100);
        self.edge(name, Edge::Up);
        self.run(500);
    }

    /// Advance `ms` through every wake on the way, with animation frames.
    fn run(&mut self, ms: u64) {
        let end = self.t + ms;
        while self.t < end {
            let next = match self.shell.needs_frame() {
                true => self.t + 16,
                false => self.shell.next_wake(self.t).unwrap_or(end),
            };
            self.t = next.clamp(self.t + 1, end);
            self.shell.tick(self.t);
            if self.shell.needs_frame() {
                self.shell.update(0.016);
                self.shell.dirty = false;
            }
        }
    }

    /// How many screens are open over the player.
    fn open(&self) -> usize {
        self.shell.top().map_or(0, |i| i + 1)
    }

    fn playing(&mut self) -> Playback {
        self.shell.store.playback_mut().clone()
    }

    fn key(&self, i: usize) -> ChannelKey {
        self.shell.store.channels(&Group::All)[i].key
    }

    /// Play channel `i` live where the screens put the video.
    fn live(&mut self, i: usize) {
        let stream = Stream::Live(self.key(i));
        self.shell.play(stream, None, self.t);
        self.shell.settle(self.t);
    }

    fn catchup(&mut self, channel: ChannelKey, programme: &Programme, offset_secs: i64) {
        let stream = Stream::Catchup {
            channel,
            programme: programme.clone(),
            offset_secs,
        };
        self.shell.play(stream, None, self.t);
        self.shell.settle(self.t);
    }
}

fn programme(title: &str) -> Programme {
    Programme {
        start: NOW - 7_200,
        stop: NOW - 5_400,
        title: title.into(),
        ..Programme::default()
    }
}

fn rect(x: i32) -> Rect {
    Rect {
        x,
        y: 0,
        w: 640,
        h: 360,
    }
}

#[test]
fn keys_reach_the_topmost_open_screen_and_messages_the_one_under_it() {
    let mut s = Sim::new();
    let _under = s.push("under");
    let sliding = s.push("sliding");
    sliding.0.closing.set(true);
    let over = s.push("over");
    *over.0.reply.borrow_mut() = Output {
        message: Some(ScreenMessage::ManageFavorites),
        ..Output::close()
    };
    s.press("OK");
    assert_eq!(s.seen(), ["over key Ok", "under message ManageFavorites"]);
    assert_eq!(
        s.shell.stack.len(),
        3,
        "over went at once, sliding animates on"
    );
    s.press("Down");
    assert_eq!(
        s.seen(),
        ["under key Down"],
        "a closing screen gets no keys"
    );
    sliding.0.gone.set(true);
    s.shell.update(0.016);
    assert_eq!(s.shell.stack.len(), 2);
}

#[test]
fn a_screen_that_animates_out_stays_drawn_until_it_is_gone() {
    let mut s = Sim::new();
    let panel = s.push("panel");
    panel.0.slides.set(true);
    *panel.0.reply.borrow_mut() = Output::close();
    s.press("OK");
    assert_eq!((s.open(), s.shell.stack.len()), (1, 2));
    s.press("Down");
    assert_eq!(
        s.seen(),
        ["panel key Ok"],
        "the guide under it has the keys"
    );
    panel.0.gone.set(true);
    s.shell.update(0.016);
    assert_eq!(s.shell.stack.len(), 1);
}

#[test]
fn the_video_goes_where_the_topmost_screen_that_places_it_wants() {
    let mut s = Sim::new();
    let guide = s.shell.rect;
    assert!(guide.is_some(), "the guide's preview");
    let a = s.push("a");
    assert_eq!(s.shell.rect, guide, "a leaves it to the guide");
    a.0.rect.set(Some(rect(1)));
    s.shell.settle(s.t);
    let b = s.push("b");
    assert_eq!(s.shell.rect, Some(rect(1)));
    b.0.rect.set(Some(rect(2)));
    s.shell.settle(s.t);
    s.shell.settle(s.t);
    assert_eq!(
        s.calls.take(),
        [
            format!("rect {:?}", Some(rect(1))),
            format!("rect {:?}", Some(rect(2)))
        ],
        "moved only when it changed"
    );
    b.0.closing.set(true);
    s.shell.settle(s.t);
    assert_eq!(
        s.shell.rect,
        Some(rect(1)),
        "a closing screen places nothing"
    );
    s.shell.navigate(Navigate::Player, None);
    s.shell.settle(s.t);
    assert_eq!(s.shell.rect, None, "nothing open: full screen");
}

#[test]
fn the_keyboard_is_up_while_the_screen_in_front_edits() {
    let mut s = Sim::new();
    let field = s.push("field");
    assert_eq!(s.shell.take_text_input(), None);
    field.0.editing.set(true);
    s.press("Down");
    assert_eq!(s.shell.take_text_input(), Some(true));
    assert!(s.shell.typing());
    s.shell.text("news", s.t);
    assert_eq!(s.seen(), ["field key Down", "field text news"]);
    // Typing keys while the keyboard is up are text, not remote keys ('q' would be Back).
    s.shell.key(KeyEvent {
        sym: u32::from(b'q'),
        wcode: 20,
        edge: Edge::Down,
        at_ms: s.t as u32,
    });
    assert!(s.seen().is_empty());
    // A screen opened over it takes the keyboard down; text with it down raised it.
    let over = s.push("over");
    assert_eq!(s.shell.take_text_input(), Some(false));
    over.0.editing.set(true);
    s.shell.text("x", s.t);
    assert_eq!(
        s.shell.take_text_input(),
        None,
        "already up: not raised again"
    );
    assert!(s.shell.typing());
    s.shell.navigate(Navigate::Player, None);
    s.shell.settle(s.t);
    assert_eq!(
        s.shell.take_text_input(),
        Some(false),
        "gone with the screens"
    );
}

#[test]
fn only_the_screens_that_are_due_are_ticked() {
    let mut s = Sim::new();
    let a = s.push("a");
    let _b = s.push("b");
    a.0.wake.set(Some(s.t + 300));
    s.shell.settle(s.t);
    let at = s.t + 300;
    assert!(s.shell.next_wake(s.t) <= Some(at));
    s.run(1_000);
    assert_eq!(s.seen(), [format!("a tick {at}")]);
    // A playback change shows on every screen.
    s.live(2);
    s.run(10);
    assert_eq!(s.seen().len(), 2);
}

#[test]
fn nothing_played_opens_the_guide_and_back_from_the_player_reopens_it() {
    let mut s = Sim::new();
    assert_eq!(s.open(), 1);
    assert!(s.shell.rect.is_some(), "the guide's preview rectangle");
    s.press("OK");
    s.press("OK");
    assert_eq!(s.open(), 0, "full screen");
    s.press("Back");
    assert_eq!(s.open(), 1);
    assert!(s.shell.rect.is_some());
}

#[test]
fn ok_previews_then_goes_full_screen_without_restarting() {
    let mut s = Sim::new();
    let preview = s.shell.rect;
    s.press("OK");
    assert_eq!(s.calls.take(), [format!("open live Channel 1 {preview:?}")]);
    assert_eq!(s.playing().channel, Some(s.key(0)));
    s.press("OK");
    assert_eq!(s.calls.take(), ["rect None"], "moved, not reopened");
    assert_eq!(s.open(), 0);
}

#[test]
fn catch_up_plays_inside_the_current_rectangle() {
    let mut s = Sim::new();
    let preview = s.shell.rect;
    let programme = programme("Earlier");
    s.catchup(s.key(0), &programme, 60);
    assert_eq!(
        s.calls.take(),
        [format!("open archive Channel 1 {preview:?}")]
    );
    let pb = s.playing();
    assert_eq!((pb.catchup, pb.position_secs), (Some(programme), 60));
    s.shell.run(Command::TogglePause, s.t);
    s.shell.run(Command::SeekTo { position_secs: 90 }, s.t);
    assert_eq!(s.calls.take(), ["paused true", "seek 90"]);
    s.shell.run(Command::ReturnToLive, s.t);
    assert_eq!(
        s.calls.take(),
        ["go live"],
        "the player leaves the archive itself"
    );
    assert_eq!(s.playing().catchup, None);
    assert_eq!(
        (s.open(), s.shell.rect),
        (1, preview),
        "still over the guide"
    );
}

#[test]
fn the_channel_list_opens_groups_on_left_and_takes_the_picked_group() {
    let mut s = Sim::new();
    s.press("OK");
    s.press("OK");
    s.press("Left");
    assert_eq!(
        (s.open(), s.shell.rect),
        (1, None),
        "the list over the picture"
    );
    s.press("Left");
    assert_eq!(s.open(), 2, "the groups panel");
    // Favorites, All channels, History, News, Sport, Movies: from All down to News.
    s.press("Down");
    s.press("Down");
    s.press("OK");
    assert_eq!(s.open(), 1, "the list is back in front");
    s.run(2_000);
    assert_eq!(s.shell.stack.len(), 1, "the groups panel has slid out");
    // News is channels 1, 4, 7 and 10: the second row is Channel 4.
    s.calls.take();
    s.press("Down");
    s.press("OK");
    assert_eq!(s.calls.take(), ["open live Channel 4 None"]);
}

#[test]
fn the_preview_list_moves_the_video_and_back_returns_it() {
    let mut s = Sim::new();
    s.press("OK");
    s.press("OK");
    s.calls.take();
    s.long("Left");
    assert_eq!(s.shell.rect, Some(PREVIEW_RECT));
    assert_eq!(s.calls.take(), [format!("rect {:?}", Some(PREVIEW_RECT))]);
    s.press("Down");
    s.press("OK");
    assert_eq!(
        s.calls.take(),
        [format!("open live Channel 2 {:?}", Some(PREVIEW_RECT))]
    );
    s.press("Back");
    assert_eq!(s.open(), 0);
    assert_eq!(s.calls.take(), ["rect None"]);
    assert_eq!(s.playing().channel, Some(s.key(1)));
}

#[test]
fn long_ok_opens_the_context_menu_which_writes_favourites() {
    let mut s = Sim::new();
    s.long("OK");
    assert_eq!(s.open(), 2, "the menu over the guide");
    s.press("OK");
    assert!(s.shell.store.is_favorite(s.key(0)));
    assert!(s.shell.toast.is_some());
    assert_eq!(
        (s.open(), s.shell.stack.len()),
        (1, 2),
        "the menu slides out"
    );
    s.run(4_000);
    assert_eq!(s.shell.stack.len(), 1);
    assert!(s.shell.toast.is_none());
}

#[test]
fn manage_favorites_from_the_menu_reaches_the_guide_under_it() {
    let mut s = Sim::new();
    s.long("OK");
    // Add to favorites, Programme description, Watch from start, Manage favorites.
    for _ in 0..3 {
        s.press("Down");
    }
    s.press("OK");
    assert_eq!(s.open(), 1, "the guide is back in front");
    // In Manage favorites OK marks the channel instead of playing it.
    s.press("OK");
    assert!(s.shell.store.is_favorite(s.key(0)));
    assert_eq!(s.playing().channel, None);
}

#[test]
fn settings_reach_storage_the_remote_and_the_player() {
    let mut s = Sim::new();
    let set = |s: &mut Sim, key: &str, value: &str| {
        let (key, value) = (key.to_owned(), value.to_owned());
        s.shell.run(Command::SetSetting { key, value }, s.t);
    };
    set(&mut s, keys::LONG_PRESS_MS, "800");
    assert_eq!(s.shell.remote.long_ms, 800);
    assert_eq!(s.shell.settings.get(keys::LONG_PRESS_MS), "800");
    set(&mut s, keys::PLAYER_INFO_SECS, "10");
    assert_eq!(s.shell.bottom.screen.settings.panel_timeout_ms, 10_000);
    assert_eq!(
        s.shell.store.settings().get(keys::PLAYER_INFO_SECS),
        "5",
        "fake data keeps none"
    );
}

#[test]
fn search_raises_the_keyboard_and_drops_it_on_close() {
    let mut s = Sim::new();
    s.shell.navigate(Navigate::Search, None);
    s.shell.settle(s.t);
    assert_eq!(s.shell.take_text_input(), Some(true));
    s.shell.text("news", s.t);
    s.press("Del");
    s.press("Back");
    assert_eq!(s.shell.take_text_input(), Some(false));
    s.press("Back");
    assert_eq!(s.open(), 1, "back to the guide");
}

#[test]
fn navigate_player_closes_every_screen_and_the_keyboard_with_them() {
    let mut s = Sim::new();
    s.shell.navigate(Navigate::Search, None);
    s.shell.settle(s.t);
    s.shell.navigate(Navigate::Player, None);
    s.shell.settle(s.t);
    assert_eq!(s.open(), 0);
    assert_eq!(s.shell.rect, None);
    assert!(!s.shell.typing, "the keyboard went with the search");
}

#[test]
fn every_play_keeps_the_zap_group_while_it_lists_the_channel() {
    let mut s = Sim::new();
    let news = Group::Playlist("News".into()); // channels 1, 4, 7 and 10
    let play = |s: &mut Sim, i: usize, group: Option<&Group>| {
        let stream = Stream::Live(s.key(i));
        let group = group.cloned();
        s.shell.run(Command::Play { stream, group }, s.t);
        s.shell.bottom.screen.group().clone()
    };
    assert_eq!(play(&mut s, 3, Some(&news)), news);
    assert_eq!(play(&mut s, 6, None), news, "channel 7 is in News too");
    assert_eq!(play(&mut s, 4, None), Group::All, "channel 5 is not");
    assert_eq!(play(&mut s, 1, Some(&news)), Group::All, "nor is channel 2");
}

#[test]
fn a_channel_picked_in_search_zaps_on_from_itself() {
    let mut s = Sim::new();
    let news = Some(Group::Playlist("News".into()));
    let stream = Stream::Live(s.key(3));
    s.shell.run(
        Command::Play {
            stream,
            group: news,
        },
        s.t,
    );
    s.shell.navigate(Navigate::Player, None);
    s.shell.navigate(Navigate::Search, None);
    s.shell.settle(s.t);
    s.shell.text("Channel 5", s.t);
    s.run(500);
    s.calls.take();
    s.press("Down");
    s.press("OK");
    assert_eq!(s.open(), 0, "full screen");
    assert_eq!(s.calls.take(), ["open live Channel 5 None"]);
    // Up zaps through every channel from channel 5, not back to the first of News.
    s.press("Up");
    assert_eq!(s.calls.take(), ["open live Channel 6 None"]);
}

#[test]
fn a_message_with_no_screen_under_it_reaches_the_player() {
    let mut s = Sim::new();
    s.live(0);
    s.shell.navigate(Navigate::Player, None);
    let sport = Group::Playlist("Sport".into());
    let news = Group::Playlist("News".into());
    for (group, zaps) in [(news.clone(), news), (sport, Group::All)] {
        let picker = s.push("picker");
        *picker.0.reply.borrow_mut() = Output {
            message: Some(ScreenMessage::SetGroup(group)),
            ..Output::close()
        };
        s.press("OK");
        assert_eq!(s.shell.bottom.screen.group(), &zaps);
    }
}

#[test]
fn manage_favorites_over_the_player_opens_the_guide_in_that_mode() {
    let mut s = Sim::new();
    s.live(0);
    s.shell.navigate(Navigate::Player, None);
    let menu = s.push("menu");
    menu.0.slides.set(true);
    *menu.0.reply.borrow_mut() = Output {
        message: Some(ScreenMessage::ManageFavorites),
        ..Output::close()
    };
    s.press("OK");
    assert_eq!(s.open(), 1, "the guide, under the menu sliding out");
    assert_eq!(s.shell.stack.len(), 2);
    assert!(s.shell.stack[1].screen.closing());
    // In Manage favorites OK marks the channel instead of going full screen.
    s.press("OK");
    assert!(s.shell.store.is_favorite(s.key(0)));
    assert_eq!(s.open(), 1);
}

#[test]
fn an_idle_player_sleeps_until_an_event() {
    let mut s = Sim::new();
    s.press("OK");
    s.press("OK");
    s.run(10_000);
    assert_eq!(s.shell.next_wake(s.t), None);
    assert!(!s.shell.needs_frame());
    assert_eq!(
        s.shell.store.recent_channels(),
        [s.key(0)],
        "watched long enough"
    );
    // A held OK wakes the shell at the long-press threshold.
    s.edge("OK", Edge::Down);
    assert_eq!(s.shell.next_wake(s.t), Some(s.t + 500));
}

#[test]
fn a_history_pick_plays_full_screen_and_closes_the_screens() {
    let mut s = Sim::new();
    let programme = programme("Watched");
    // Watch it 60 s in from the guide's preview, then save: History has it.
    s.catchup(s.key(0), &programme, 60);
    s.run(31_000);
    let h = s.shell.store.history();
    assert_eq!(
        (h.len(), h[0].position_secs),
        (1, 60),
        "saved while it plays"
    );
    s.live(1);
    s.shell.navigate(Navigate::History, Some(0));
    s.shell.settle(s.t);
    assert_eq!(s.open(), 2);
    let preview = s.shell.rect;
    s.calls.take();
    s.press("OK");
    assert_eq!(s.open(), 0, "the guide went too");
    assert_eq!(
        s.calls.take(),
        [
            format!("open archive Channel 1 {preview:?}"),
            "rect None".into()
        ]
    );
    assert_eq!(s.playing().position_secs, 60, "resumed where it stopped");
}

#[test]
fn leaving_an_archive_programme_saves_its_position() {
    let mut s = Sim::new();
    s.catchup(s.key(1), &programme("Left early"), 300);
    assert!(s.shell.store.history().is_empty());
    s.shell.run(Command::ReturnToLive, s.t);
    let h = s.shell.store.history();
    assert_eq!((h.len(), h[0].position_secs), (1, 300));
}

#[test]
fn an_archive_programme_played_to_its_end_is_finished_in_history_and_live_plays() {
    let mut s = Sim::new();
    let channel = s.key(0);
    s.catchup(channel, &programme("To the end"), 1_700);
    s.calls.1.set(true);
    s.run(6_000);
    let h = s.shell.store.history();
    assert_eq!(
        (h.len(), h[0].position_secs),
        (1, 1_800),
        "watched to the end"
    );
    assert_eq!(s.playing().catchup, None);
    assert_eq!(
        s.shell.store.recent_channels(),
        [channel],
        "the live channel counts"
    );
    assert!(
        s.calls.take().iter().all(|c| !c.starts_with("open live")),
        "no second zap"
    );
}

/// Serves a one-channel playlist at any URL.
struct OneChannel;

impl Transport for OneChannel {
    fn get(
        &self,
        _url: &str,
        _headers: &[String],
        _budget: std::time::Duration,
        _cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<iptv::refresh::Response, iptv::refresh::NetError> {
        let m3u = "#EXTM3U\n#EXTINF:-1,Only\nhttp://host/only.m3u8\n";
        Ok(iptv::refresh::Response {
            status: 200,
            validators: Default::default(),
            location: None,
            body: Box::new(std::io::Cursor::new(m3u.as_bytes().to_vec())),
        })
    }
}

fn one_channel() -> Box<dyn Transport> {
    Box::new(OneChannel)
}

#[test]
fn the_refresher_waits_for_a_playlist_url_and_stops_when_it_is_cleared() {
    let dir = std::env::temp_dir().join(format!("webosiptv-shell-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    locale();
    let mut shell = Shell::new(
        Box::new(crate::store::open_at(&dir)),
        Box::new(Calls::default()),
        Some(Box::new(one_channel)),
        LogoCache::new(None),
    );
    shell.start(0);
    assert!(shell.refresher.is_none(), "no URL: idle");
    let set = |shell: &mut Shell, url: &str| {
        let key = keys::PLAYLIST_URL.to_owned();
        shell.run(
            Command::SetSetting {
                key,
                value: url.into(),
            },
            0,
        );
    };
    set(&mut shell, "http://host/list.m3u");
    assert!(shell.refresher.is_some());
    let mut t = 0;
    while shell.store.channels(&Group::All).is_empty() && t < 10_000 {
        t += 50;
        std::thread::sleep(std::time::Duration::from_millis(50));
        shell.tick(t);
    }
    let all = shell.store.channels(&Group::All);
    let names: Vec<_> = all.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Only"]);
    set(&mut shell, "");
    assert!(shell.refresher.is_none());
    drop(shell);
    let _ = std::fs::remove_dir_all(&dir);
}
