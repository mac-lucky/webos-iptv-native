use std::rc::Rc;

use iptv::model::ChannelKey;
use iptv::vod::{VodItem, VodProgress};

use super::*;
use crate::data::fake::{FakeData, Mono};
use crate::data::{Playback, VodPlaying};
use crate::draw::Piece;
use crate::player::transport::COMMIT_MS;

const NOW: i64 = 1_700_000_000;

fn locale() {
    tv_platform::i18n::initialize(tv_platform::i18n::Preference::En, true);
}

fn data() -> FakeData {
    locale();
    let mut d = FakeData::new(NOW, 3);
    d.vod = fake::items("");
    d
}

fn key_of(d: &FakeData, title: &str) -> ChannelKey {
    d.vod
        .iter()
        .find(|v| v.title == title || v.label() == title)
        .map(|v| v.key)
        .unwrap()
}

fn settings() -> PlayerSettings {
    PlayerSettings::default()
}

fn press(s: &mut VodScreen, d: &FakeData, keys: &[Key]) -> Output {
    let mut out = Output::default();
    for &k in keys {
        out = s.key(k, Press::Short, d, 0);
    }
    out
}

fn texts(pieces: &[Piece]) -> Vec<String> {
    pieces
        .iter()
        .filter_map(|p| match p {
            Piece::Text { s, .. } => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

fn titles(c: &Catalog, i: usize) -> Vec<String> {
    c.list(i).map(|e| e.title().to_owned()).collect()
}

#[test]
fn movies_list_all_then_their_groups_in_order() {
    let d = data();
    let c = Catalog::build(&d, VodSection::Movies);
    let names: Vec<_> = c.categories.iter().map(|c| c.name().to_owned()).collect();
    assert_eq!(names, ["All", "Drama", "Action", "Comedy", "Romance"]);
    assert_eq!(c.list_len(0), 8);
    assert_eq!(
        titles(&c, 1),
        ["Northern Lights", "Quiet Hours", "Last Train Home"]
    );
    let Some(Entry::Movie(m)) = c.entry(2, 0) else {
        panic!("a movie");
    };
    assert_eq!((m.title.as_str(), m.year), ("Harbour City", Some(2011)));
}

/// The parental lock covers a movie as it would the channel it was listed as: any of its groups,
/// or its key. A covered movie is left out of every category, Continue watching too, until the
/// PIN opens it.
#[test]
fn locked_groups_and_keys_leave_their_movies_out() {
    let mut d = data();
    let quiet = key_of(&d, "Quiet Hours");
    let harbour = key_of(&d, "Harbour City");
    if let Some(paper) = d.vod.iter_mut().find(|v| v.title == "Paper Moons") {
        paper.groups.push("Adult".into());
    }
    let mut s = crate::panels::Settings::new();
    assert!(s.set(crate::panels::keys::PARENTAL_GROUPS, "Action\nAdult".into()));
    assert!(s.set(crate::panels::keys::PARENTAL_CHANNELS, quiet.hex()));
    d.parental = crate::parental::Parental::from_settings(&s);
    d.vod_progress = vec![VodProgress::new(harbour, 600, 5_400, NOW)];
    let c = Catalog::build(&d, VodSection::Movies);
    let names: Vec<_> = c.categories.iter().map(|c| c.name().to_owned()).collect();
    assert_eq!(names, ["All", "Drama", "Romance", "Comedy"]);
    assert_eq!(
        titles(&c, 0),
        [
            "Northern Lights",
            "Summer Rain",
            "Last Train Home",
            "The Long Road"
        ]
    );
    assert!(d.vod.iter().any(|v| v.key == harbour && d.vod_locked(v)));
    // The PIN opened Action for the session: its movies are back, the started one first.
    d.parental.open(crate::data::Lock::Group("Action".into()));
    let c = Catalog::build(&d, VodSection::Movies);
    assert_eq!(c.categories[0], Category::Continue);
    assert_eq!(titles(&c, 0), ["Harbour City"]);
    assert!(titles(&c, 1).contains(&"Iron Valley".to_owned()));
    assert!(!titles(&c, 1).contains(&"Quiet Hours".to_owned()));
}

#[test]
fn continue_watching_comes_first_latest_first_and_skips_the_watched() {
    let mut d = data();
    let quiet = key_of(&d, "Quiet Hours");
    let paper = key_of(&d, "Paper Moons");
    let road = key_of(&d, "The Long Road");
    d.vod_progress = vec![
        VodProgress::new(quiet, 600, 5_400, NOW - 50),
        VodProgress::new(paper, 1_200, 5_400, NOW - 10),
        VodProgress::new(road, 5_390, 5_400, NOW),
    ];
    let c = Catalog::build(&d, VodSection::Movies);
    assert_eq!(c.categories[0], Category::Continue);
    assert_eq!(titles(&c, 0), ["Paper Moons", "Quiet Hours"]);
    assert_eq!(c.categories[1], Category::All);
    assert_eq!(Mark::of(c.progress(road)), Mark::Watched);
    let Mark::Started(frac, left) = Mark::of(c.progress(quiet)) else {
        panic!("started");
    };
    assert!((frac - 600.0 / 5_400.0).abs() < 1e-4);
    assert_eq!(left, "1:20:00 left");
    assert_eq!(Mark::of(None), Mark::None);
}

#[test]
fn shows_are_series_by_season_and_episode() {
    let mut d = data();
    // Out of order in the playlist, and one more episode of the first show elsewhere.
    d.vod.swap(8, 9);
    let c = Catalog::build(&d, VodSection::Shows);
    let names: Vec<_> = c.categories.iter().map(|c| c.name().to_owned()).collect();
    assert_eq!(names, ["All", "Crime", "Documentary", "Lifestyle"]);
    assert_eq!(
        titles(&c, 0),
        ["Harbour Watch", "The Archive", "Kitchen Stories"]
    );
    let Some(Entry::Series(s)) = c.entry(0, 0) else {
        panic!("a series");
    };
    assert_eq!(s.len(), 5);
    let seasons: Vec<_> = s.seasons.iter().map(Season::name).collect();
    assert_eq!(seasons, ["Season 1", "Season 2"]);
    let first: Vec<_> = s.seasons[0].episodes.iter().map(|e| e.label()).collect();
    assert_eq!(first, ["S01 E01 Pilot", "S01 E02 Low Tide", "S01 E03 Fog"]);
    let Some(Entry::Series(k)) = c.entry(0, 1) else {
        panic!("a series");
    };
    assert_eq!(k.name, "The Archive");
    assert!(Catalog::build(&FakeData::new(NOW, 1), VodSection::Shows)
        .entries
        .is_empty());
}

#[test]
fn unnumbered_episodes_are_a_last_season() {
    locale();
    let mut d = FakeData::new(NOW, 1);
    let ep = |n: &str, season, episode| {
        Rc::new(VodItem {
            key: ChannelKey::of_url(&format!("http://vod.example/{n}")),
            kind: iptv::vod::VodKind::Episode,
            title: n.into(),
            series: Some("Show".into()),
            season,
            episode,
            ..VodItem::default()
        })
    };
    d.vod = vec![
        (*ep("extra", None, None)).clone(),
        (*ep("b", Some(2), Some(1))).clone(),
        (*ep("a", Some(1), Some(1))).clone(),
    ];
    let c = Catalog::build(&d, VodSection::Shows);
    let Some(Entry::Series(s)) = c.entry(0, 0) else {
        panic!("a series");
    };
    let seasons: Vec<_> = s.seasons.iter().map(Season::name).collect();
    assert_eq!(seasons, ["Season 1", "Season 2", "Episodes"]);
}

#[test]
fn ok_on_a_new_movie_plays_it_and_puts_its_player_in_front() {
    let d = data();
    let mut s = VodScreen::new(VodSection::Movies, &d, settings());
    assert_eq!((s.focus(), s.category()), (Focus::Categories, 0));
    assert_eq!(s.title(), "Movies");
    let out = press(&mut s, &d, &[Key::Right, Key::Down, Key::Ok]);
    let key = key_of(&d, "Harbour City");
    assert_eq!(
        out.commands,
        [Command::PlayVod {
            item: key,
            offset_secs: 0
        }]
    );
    assert!(
        out.navigate.is_empty(),
        "the section stays, its player in front"
    );
    let player = s.player().expect("the player is up");
    assert_eq!(player.title(), "Harbour City");
    assert!(player.shown());
}

#[test]
fn a_started_movie_asks_to_resume_or_start_over() {
    let mut d = data();
    let key = key_of(&d, "Northern Lights");
    d.vod_progress = vec![VodProgress::new(key, 754, 6_000, NOW)];
    let mut s = VodScreen::new(VodSection::Movies, &d, settings());
    // Continue watching first, but the section opens on All.
    assert_eq!(s.catalog().categories[s.category()], Category::All);
    let out = press(&mut s, &d, &[Key::Right, Key::Ok]);
    assert!(out.commands.is_empty());
    let prompt = s.prompt().expect("the resume prompt");
    let rows: Vec<_> = prompt
        .panel
        .page()
        .rows()
        .iter()
        .map(|r| r.label().to_owned())
        .collect();
    assert_eq!(rows, ["Resume from 12:34", "Play from start"]);
    assert_eq!(s.hold(Key::Down), PanelKey::hold(Key::Down));
    let out = press(&mut s, &d, &[Key::Ok]);
    assert_eq!(
        out.commands,
        [Command::PlayVod {
            item: key,
            offset_secs: 754
        }]
    );
    assert!(s.prompt().is_none() && s.player().is_some());

    let mut s = VodScreen::new(VodSection::Movies, &d, settings());
    press(&mut s, &d, &[Key::Right, Key::Ok]);
    let out = press(&mut s, &d, &[Key::Down, Key::Ok]);
    assert_eq!(
        out.commands,
        [Command::PlayVod {
            item: key,
            offset_secs: 0
        }]
    );

    let mut s = VodScreen::new(VodSection::Movies, &d, settings());
    press(&mut s, &d, &[Key::Right, Key::Ok]);
    let out = press(&mut s, &d, &[Key::Back]);
    assert!(out.commands.is_empty() && s.prompt().is_none() && s.player().is_none());
    assert_eq!(s.focus(), Focus::Items, "Back on the prompt keeps the list");
}

#[test]
fn back_walks_out_to_the_guide() {
    let d = data();
    let mut s = VodScreen::new(VodSection::Movies, &d, settings());
    press(&mut s, &d, &[Key::Right, Key::Down, Key::Down]);
    assert_eq!((s.focus(), s.item()), (Focus::Items, 2));
    press(&mut s, &d, &[Key::Back]);
    assert_eq!(s.focus(), Focus::Categories);
    press(&mut s, &d, &[Key::Down]);
    assert_eq!(
        (s.category(), s.item()),
        (1, 0),
        "a new category starts at its top"
    );
    assert_eq!(
        press(&mut s, &d, &[Key::Back]),
        Output::navigate(Navigate::Guide)
    );
    assert_eq!(s.hold(Key::Down), Hold::Repeat);
    assert_eq!(s.hold(Key::Ok), Hold::Once);
}

#[test]
fn a_series_opens_on_its_next_episode_and_seasons_switch_on_the_left() {
    let mut d = data();
    let first = key_of(&d, "S01 E01 Pilot");
    d.vod_progress = vec![VodProgress::new(first, 2_990, 3_000, NOW)];
    let mut s = VodScreen::new(VodSection::Shows, &d, settings());
    assert_eq!(s.title(), "Shows");
    press(&mut s, &d, &[Key::Right, Key::Ok]);
    assert_eq!(s.title(), "Harbour Watch");
    assert_eq!(
        (s.focus(), s.season(), s.episode()),
        (Focus::Episodes, 0, 1),
        "the first episode was watched"
    );
    press(&mut s, &d, &[Key::Left, Key::Down]);
    assert_eq!((s.focus(), s.season(), s.episode()), (Focus::Seasons, 1, 0));
    let out = press(&mut s, &d, &[Key::Right, Key::Down, Key::Ok]);
    assert_eq!(
        out.commands,
        [Command::PlayVod {
            item: key_of(&d, "S02 E02 Undertow"),
            offset_secs: 0
        }]
    );
    assert_eq!(
        s.player().unwrap().title(),
        "Harbour Watch S02 E02 Undertow"
    );

    let mut s = VodScreen::new(VodSection::Shows, &d, settings());
    press(&mut s, &d, &[Key::Right, Key::Ok, Key::Back]);
    assert_eq!((s.focus(), s.series().is_none()), (Focus::Items, true));
    assert_eq!(s.title(), "Shows");
}

#[test]
fn refresh_keeps_the_focus_on_the_same_movie() {
    let mut d = data();
    let mut s = VodScreen::new(VodSection::Movies, &d, settings());
    press(&mut s, &d, &[Key::Right, Key::Down, Key::Down, Key::Down]);
    let focused = s
        .catalog()
        .entry(s.category(), s.item())
        .unwrap()
        .title()
        .to_owned();
    assert_eq!(focused, "Paper Moons");
    // A movie stopped part way: Continue watching appears above All.
    let key = key_of(&d, "Paper Moons");
    d.vod_progress = vec![VodProgress::new(key, 300, 6_000, NOW)];
    s.refresh(&d);
    assert_eq!(s.catalog().categories[s.category()], Category::All);
    let focused = s.catalog().entry(s.category(), s.item()).unwrap().title();
    assert_eq!(focused, "Paper Moons");
    // Everything gone: back to the categories, and the view says so.
    d.vod.clear();
    s.refresh(&d);
    assert_eq!(s.focus(), Focus::Categories);
    let mut out = Vec::new();
    view::build(&s, &Mono, &mut out);
    assert!(texts(&out).contains(&"This playlist has no movies".to_owned()));
}

#[test]
fn the_view_shows_titles_years_and_marks() {
    let mut d = data();
    let key = key_of(&d, "Northern Lights");
    let done = key_of(&d, "Harbour City");
    d.vod_progress = vec![
        VodProgress::new(key, 600, 6_000, NOW),
        VodProgress::new(done, 5_990, 6_000, NOW),
    ];
    let mut s = VodScreen::new(VodSection::Movies, &d, settings());
    press(&mut s, &d, &[Key::Right]);
    let mut out = Vec::new();
    view::build(&s, &Mono, &mut out);
    let shown = texts(&out);
    for want in [
        "Movies",
        "Continue watching",
        "All",
        "Northern Lights",
        "2019 \u{b7} Drama",
        "1:30:00 left",
        "Watched",
    ] {
        assert!(shown.iter().any(|t| t == want), "{want:?} in {shown:?}");
    }
    let posters = out
        .iter()
        .filter(|p| matches!(p, Piece::Logo { .. }))
        .count();
    assert_eq!(posters, ITEM_ROWS);
}

fn playing(item: ChannelKey, position: i64, duration: i64) -> Playback {
    Playback {
        position_secs: position,
        vod: Some(VodPlaying {
            item,
            title: "x".into(),
            duration_secs: duration,
            ended: false,
        }),
        ..Playback::default()
    }
}

#[test]
fn the_player_seeks_in_one_burst_pauses_and_stops_on_back() {
    let mut d = data();
    let key = key_of(&d, "Northern Lights");
    let mut s = VodScreen::new(VodSection::Movies, &d, settings());
    press(&mut s, &d, &[Key::Right, Key::Ok]);
    d.playback = playing(key, 600, 6_000);
    s.tick(&d, 10);
    let r = s.player().unwrap().readout().clone();
    assert_eq!((r.elapsed.as_str(), r.total.as_str()), ("10:00", "1:40:00"));
    assert!((r.knob - 0.1).abs() < 1e-4);
    assert_eq!(s.hold(Key::Right), Hold::Repeat);
    // Two taps forward, one back: one seek once the presses stop.
    for k in [Key::Right, Key::Right, Key::Left] {
        assert!(s.key(k, Press::Short, &d, 100).commands.is_empty());
    }
    let skip = settings().skip_forward_secs * 2 - settings().skip_back_secs;
    assert_eq!(s.player().unwrap().shown_position(), 600 + skip);
    assert!(s.player().unwrap().readout().offset.starts_with("[+"));
    assert_eq!(s.next_wake(100), Some(100 + COMMIT_MS));
    let out = s.tick(&d, 100 + COMMIT_MS);
    assert_eq!(
        out.commands,
        [Command::SeekTo {
            position_secs: 600 + skip
        }]
    );
    // OK with nothing pending pauses; Back hides the panel, then stops.
    let out = s.key(Key::Ok, Press::Short, &d, 2_000);
    assert_eq!(out.commands, [Command::TogglePause]);
    let out = s.key(Key::Back, Press::Short, &d, 2_100);
    assert!(out.commands.is_empty() && !s.player().unwrap().shown());
    let out = s.key(Key::Back, Press::Short, &d, 2_200);
    assert_eq!(out.commands, [Command::StopVod]);
    assert!(s.player().is_none(), "the section is in front again");
}

#[test]
fn the_player_never_seeks_past_the_end_or_before_the_length_is_known() {
    let mut d = data();
    let key = key_of(&d, "Northern Lights");
    let mut s = VodScreen::new(VodSection::Movies, &d, settings());
    press(&mut s, &d, &[Key::Right, Key::Ok]);
    d.playback = playing(key, 10, 0);
    s.tick(&d, 10);
    s.key(Key::Right, Press::Short, &d, 20);
    assert!(s.player().unwrap().burst().is_none(), "no length yet");
    d.playback = playing(key, 5_990, 6_000);
    s.tick(&d, 30);
    for _ in 0..5 {
        s.key(Key::FastForward, Press::Repeat, &d, 40);
    }
    let guard = crate::player::transport::END_GUARD_SECS;
    assert_eq!(s.player().unwrap().shown_position(), 6_000 - guard);
}

#[test]
fn an_ended_or_replaced_item_gives_the_section_back() {
    let mut d = data();
    let key = key_of(&d, "Northern Lights");
    let mut s = VodScreen::new(VodSection::Movies, &d, settings());
    press(&mut s, &d, &[Key::Right, Key::Ok]);
    d.playback = playing(key, 5_000, 6_000);
    assert!(s.tick(&d, 10).commands.is_empty());
    d.playback.vod.as_mut().unwrap().ended = true;
    assert_eq!(s.tick(&d, 20).commands, [Command::StopVod]);
    assert!(s.player().is_none());

    press(&mut s, &d, &[Key::Ok]);
    d.playback = Playback {
        channel: Some(d.channels[0].key),
        ..Playback::default()
    };
    let out = s.tick(&d, 30);
    assert!(out.commands.is_empty() && out.redraw);
    assert!(s.player().is_none(), "a live channel took over");
}

#[test]
fn an_idle_section_sleeps_and_the_player_panel_times_out() {
    let mut d = data();
    let key = key_of(&d, "Northern Lights");
    let mut s = VodScreen::new(VodSection::Movies, &d, settings());
    assert_eq!(s.next_wake(0), None);
    press(&mut s, &d, &[Key::Right, Key::Ok]);
    let timeout = settings().panel_timeout_ms;
    assert_eq!(s.next_wake(0), Some(timeout));
    d.playback = playing(key, 0, 6_000);
    let out = s.tick(&d, timeout);
    assert!(out.redraw && !s.player().unwrap().shown());
    assert_eq!(s.next_wake(timeout), None);
    // Hidden, the panel draws nothing over the video.
    let mut out = Vec::new();
    s.player().unwrap().build(&Mono, &mut out);
    assert!(out.is_empty());
    s.key(Key::Up, Press::Short, &d, timeout + 1);
    let mut out = Vec::new();
    s.player().unwrap().build(&Mono, &mut out);
    assert!(texts(&out).contains(&"Northern Lights".to_owned()));
}
