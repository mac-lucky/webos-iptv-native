//! The home navigation, driven by the remote: the home page first, sections over it, Back back
//! to it with the stream stopped, full screen and back to the section.

use super::*;
use tv_screens::data::VodSection;
use tv_screens::home::HomeScreen;
use tv_screens::lists::Column;
use tv_screens::vod::Focus;

/// A shell set to the home navigation before it starts, over the fake data (with the fake VOD
/// catalog when `vod`).
fn home_sim(vod: bool) -> Sim {
    locale();
    let calls = Calls::default();
    let mut data = FakeData::new(NOW, 12);
    if vod {
        data.vod = tv_screens::vod::fake::items("");
    }
    let mut shell = Shell::new(
        Box::new(data),
        Box::new(calls.clone()),
        None,
        LogoCache::new(None),
    );
    shell.set_setting(keys::NAVIGATION, "home".to_owned());
    shell.start(0);
    calls.take();
    Sim {
        shell,
        calls,
        seen: Rc::default(),
        t: 1_000,
    }
}

/// The screen in front, `None` for the full-screen player.
fn front(s: &Sim) -> Option<&dyn std::any::Any> {
    let i = s.shell.top()?;
    let screen: &dyn std::any::Any = &*s.shell.stack[i].screen;
    Some(screen)
}

fn front_is<T: 'static>(s: &Sim) -> bool {
    front(s).is_some_and(|f| f.is::<T>())
}

fn live(s: &Sim) -> bool {
    s.shell.store.playback().channel.is_some()
}

/// Walk the home page's focus to the tile that opens `nav` and press OK.
fn open_tile(s: &mut Sim, nav: Navigate) {
    let at = {
        let home: &HomeScreen = front(s)
            .and_then(|f| f.downcast_ref())
            .expect("the home page");
        home.tiles()
            .iter()
            .position(|t| t.open == nav)
            .expect("a tile for it")
    };
    let focus = |s: &Sim| {
        let home: &HomeScreen = front(s).and_then(|f| f.downcast_ref()).unwrap();
        home.focus()
    };
    let row = |i: usize| i / tv_screens::home::COLUMNS;
    while row(focus(s)) != row(at) {
        s.press(if row(focus(s)) < row(at) {
            "Down"
        } else {
            "Up"
        });
    }
    while focus(s) != at {
        s.press(if focus(s) < at { "Right" } else { "Left" });
    }
    s.press("OK");
    s.run(50);
}

#[test]
fn the_app_opens_on_the_home_page_with_nothing_playing() {
    let s = home_sim(false);
    assert!(front_is::<HomeScreen>(&s));
    assert_eq!(s.shell.stack.len(), 1);
    assert!(!live(&s));
}

#[test]
fn back_from_the_channel_list_returns_home_and_stops_the_stream() {
    let mut s = home_sim(false);
    open_tile(&mut s, Navigate::ChannelList { preview: true });
    assert!(front_is::<ChannelList>(&s));
    assert!(
        s.shell.rect.is_some(),
        "the list's preview shows the picture"
    );
    s.press("OK");
    assert!(live(&s), "OK plays the channel in the preview");
    assert!(front_is::<ChannelList>(&s));
    s.press("Back");
    s.run(500);
    assert!(front_is::<HomeScreen>(&s), "Back returns home");
    assert!(!live(&s), "and the stream stops");
}

#[test]
fn full_screen_takes_the_home_page_away_and_back_returns_to_the_section() {
    let mut s = home_sim(false);
    open_tile(&mut s, Navigate::ChannelList { preview: true });
    s.press("OK");
    s.press("OK");
    s.run(500);
    assert!(
        front(&s).is_none(),
        "the second OK puts the channel full screen"
    );
    assert!(live(&s));
    s.press("Back");
    s.run(500);
    assert!(
        front_is::<ChannelList>(&s),
        "Back on the player opens the list again"
    );
    assert!(
        s.shell.rect.is_some(),
        "in its preview, not over the full screen"
    );
    assert!(live(&s), "the channel plays on in its preview");
    s.press("Back");
    s.run(500);
    assert!(front_is::<HomeScreen>(&s));
    assert!(!live(&s));
}

#[test]
fn back_unwinds_the_channel_groups_before_it_returns_home() {
    let mut s = home_sim(false);
    open_tile(&mut s, Navigate::ChannelList { preview: true });
    let column = |s: &Sim| {
        let l: &ChannelList = front(s).and_then(|f| f.downcast_ref()).expect("the list");
        l.column()
    };
    s.press("Left");
    assert_eq!(column(&s), Column::Groups);
    s.press("Back");
    assert_eq!(
        column(&s),
        Column::Channels,
        "Back returns to the channels first"
    );
    s.press("Back");
    s.run(500);
    assert!(front_is::<HomeScreen>(&s));
}

#[test]
fn back_leaves_the_guide_from_its_groups_panel() {
    let mut s = home_sim(false);
    open_tile(&mut s, Navigate::Guide);
    assert!(front_is::<GuideScreen>(&s));
    s.press("Back");
    let guide: &GuideScreen = front(&s).and_then(|f| f.downcast_ref()).expect("the guide");
    assert!(
        guide.focus().group.is_some(),
        "Back at live opens the groups panel"
    );
    s.press("Back");
    s.run(500);
    assert!(
        front_is::<HomeScreen>(&s),
        "Back from the groups panel returns home"
    );
}

#[test]
fn back_closes_a_series_then_its_column_before_it_returns_home() {
    let mut s = home_sim(true);
    open_tile(&mut s, Navigate::Vod(VodSection::Shows));
    let vod = |s: &Sim| -> (bool, Focus) {
        let v: &VodScreen = front(s).and_then(|f| f.downcast_ref()).expect("the series");
        (v.series().is_some(), v.focus())
    };
    s.press("Right");
    s.press("OK");
    assert_eq!(vod(&s), (true, Focus::Episodes));
    s.press("Back");
    assert_eq!(vod(&s), (false, Focus::Items), "Back closes the series");
    s.press("Back");
    assert_eq!(vod(&s), (false, Focus::Categories));
    s.press("Back");
    s.run(500);
    assert!(front_is::<HomeScreen>(&s));
}

#[test]
fn settings_and_search_close_back_to_the_home_page() {
    let mut s = home_sim(false);
    open_tile(&mut s, Navigate::Settings);
    assert!(front_is::<SettingsPanel>(&s));
    s.press("Back");
    s.run(500);
    assert!(front_is::<HomeScreen>(&s));
    open_tile(&mut s, Navigate::Search);
    assert!(front_is::<Search>(&s));
}

#[test]
pub(super) fn the_navigation_setting_applies_at_once() {
    let mut s = Sim::new();
    assert!(
        front_is::<GuideScreen>(&s),
        "the classic navigation opens on the guide"
    );
    s.shell.set_setting(keys::NAVIGATION, "home".to_owned());
    s.shell.settle(s.t);
    assert!(front_is::<GuideScreen>(&s), "what is open stays open");
    s.press("Back");
    s.press("Back");
    s.run(500);
    assert!(
        front_is::<HomeScreen>(&s),
        "and leads back to the home page under it"
    );
    s.shell.set_setting(keys::NAVIGATION, "classic".to_owned());
    s.shell.settle(s.t);
    assert!(
        front_is::<GuideScreen>(&s),
        "the classic navigation takes the home page away"
    );
    assert!(!s
        .shell
        .stack
        .iter()
        .any(|l| super::super::home::is_home(&*l.screen)));
}

#[test]
fn a_movie_found_from_films_opens_in_films_over_the_home_page() {
    let mut s = home_sim(true);
    open_tile(&mut s, Navigate::Vod(VodSection::Movies));
    assert!(front_is::<VodScreen>(&s));
    s.press("Up");
    s.press("OK");
    s.run(50);
    assert!(front_is::<Search>(&s), "Search heads the categories");
    s.shell.text("harbour", s.t);
    s.press("Down");
    s.press("OK");
    s.run(500);
    let vod: &VodScreen = front(&s)
        .and_then(|f| f.downcast_ref())
        .expect("the movies");
    let Some(tv_screens::vod::Entry::Movie(m)) = vod.catalog().entry(vod.category(), vod.item())
    else {
        panic!("a movie in focus");
    };
    assert_eq!(m.title, "Harbour City");
    assert_eq!(s.shell.stack.len(), 2, "over the home page");
    s.press("Back");
    s.press("Back");
    s.run(500);
    assert!(front_is::<HomeScreen>(&s));
}

#[test]
fn back_from_a_section_finds_the_home_page_on_its_tile() {
    let mut s = home_sim(true);
    open_tile(&mut s, Navigate::Vod(VodSection::Shows));
    s.press("Back");
    s.run(500);
    let home: &HomeScreen = front(&s).and_then(|f| f.downcast_ref()).expect("home");
    assert_eq!(
        home.tiles()[home.focus()].open,
        Navigate::Vod(VodSection::Shows)
    );
}
