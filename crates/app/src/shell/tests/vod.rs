//! Movies and Shows in the running app: a section opens from the main menu in place
//! of the guide, a movie plays full screen through the player's VOD seam, its seek bar and pause
//! reach the player, and its position is kept every half minute, when it stops, when something
//! else plays and when the app goes to the background; an item played to its end is watched.

use iptv::vod::VodKind;
use tv_screens::data::VodSection;

use super::*;

/// A started shell over synthetic channels and the synthetic movies and shows.
fn sim() -> Sim {
    locale();
    let calls = Calls::default();
    let mut data = FakeData::new(NOW, 12);
    data.vod = tv_screens::vod::fake::items("");
    let mut shell = Shell::new(
        Box::new(data),
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

/// The player calls since the last look, without the rectangle moves.
fn told(s: &Sim) -> Vec<String> {
    let calls = s.calls.take().into_iter();
    calls.filter(|c| !c.starts_with("rect")).collect()
}

fn first_movie(s: &Sim) -> ChannelKey {
    let vod = s.shell.store.vod();
    vod.iter().find(|v| v.kind == VodKind::Movie).unwrap().key
}

/// Open Movies and play the first movie of All.
fn play_first_movie(s: &mut Sim) -> ChannelKey {
    s.shell.navigate(Navigate::Vod(VodSection::Movies), None);
    s.shell.settle(s.t);
    s.press("Right");
    s.press("OK");
    first_movie(s)
}

/// The player reports the movie's length, as `poll` does once it opened.
fn opened(s: &mut Sim, duration_secs: i64) {
    let pb = s.shell.store.playback_mut();
    pb.vod.as_mut().unwrap().duration_secs = duration_secs;
    s.shell.playback_moved = true;
    s.run(20);
}

#[test]
fn the_main_menu_opens_movies_in_place_of_the_guide() {
    let mut s = sim();
    assert_eq!(s.open(), 1, "the guide");
    s.shell.navigate(Navigate::MainMenu, None);
    s.shell.settle(s.t);
    let labels: Vec<String> = {
        let cx = PanelCtx {
            data: &*s.shell.store,
            settings: &s.shell.settings,
        };
        let menu = ContextMenu::main(&cx);
        menu.panel()
            .page()
            .rows()
            .iter()
            .map(|r| r.label().to_owned())
            .collect()
    };
    assert!(labels.contains(&"Movies".to_owned()) && labels.contains(&"Shows".to_owned()));
    s.shell.navigate(Navigate::Vod(VodSection::Movies), None);
    s.shell.settle(s.t);
    assert_eq!(s.open(), 1, "the section replaced the guide and the menu");
    assert_eq!(
        s.shell.rect, None,
        "no preview: the video, when it plays, is full screen"
    );
}

#[test]
fn a_movie_plays_seeks_pauses_and_keeps_its_position_when_stopped() {
    let mut s = sim();
    let key = play_first_movie(&mut s);
    assert_eq!(told(&s), ["open vod Northern Lights at 0 None"]);
    let pb = s.playing();
    assert_eq!(pb.channel, None);
    assert_eq!(pb.vod.as_ref().map(|v| v.item), Some(key));
    assert_eq!(
        pb.vod.as_ref().map(|v| v.title.as_str()),
        Some("Northern Lights")
    );

    opened(&mut s, 6_000);
    s.shell.store.playback_mut().position_secs = 600;
    s.shell.playback_moved = true;
    s.run(20);
    s.press("Right");
    s.run(1_000);
    let skip = PlayerSettings::default().skip_forward_secs;
    assert_eq!(told(&s), [format!("seek {}", 600 + skip)]);
    assert_eq!(s.playing().position_secs, 600 + skip);
    s.press("OK");
    assert_eq!(told(&s), ["paused true"]);
    assert!(s.playing().paused);

    // Back hides the panel, Back again stops: the position is kept, the section is back.
    s.press("Back");
    s.press("Back");
    assert_eq!(told(&s), ["stop"]);
    assert_eq!(s.playing(), Playback::default());
    let kept = s.shell.store.vod_progress(key).expect("kept");
    assert_eq!(
        (kept.position_secs, kept.duration_secs),
        ((600 + skip) as u32, 6_000)
    );
    assert_eq!(s.open(), 1, "the section");
    // Reopened, the movie asks to resume.
    s.press("OK");
    assert!(told(&s).is_empty(), "the prompt first");
    s.press("OK");
    assert_eq!(
        told(&s),
        [format!("open vod Northern Lights at {} None", 600 + skip)]
    );
}

#[test]
fn the_position_is_saved_while_playing_on_suspend_and_when_live_takes_over() {
    let mut s = sim();
    let key = play_first_movie(&mut s);
    opened(&mut s, 6_000);
    s.shell.store.playback_mut().position_secs = 120;
    s.run(31_000);
    assert_eq!(
        s.shell.store.vod_progress(key).map(|p| p.position_secs),
        Some(120),
        "every half minute"
    );
    s.shell.store.playback_mut().position_secs = 300;
    s.shell.suspend();
    assert_eq!(
        s.shell.store.vod_progress(key).map(|p| p.position_secs),
        Some(300)
    );
    s.shell.store.playback_mut().position_secs = 360;
    s.live(0);
    assert_eq!(
        s.shell.store.vod_progress(key).map(|p| p.position_secs),
        Some(360)
    );
    assert_eq!(s.playing().vod, None);
    s.run(100);
    assert_eq!(s.open(), 1, "the section is back over the live channel");
}

#[test]
fn an_item_played_to_its_end_is_watched_and_gives_the_section_back() {
    let mut s = sim();
    let key = play_first_movie(&mut s);
    opened(&mut s, 6_000);
    told(&s);
    let pb = s.shell.store.playback_mut();
    pb.position_secs = 5_940;
    pb.vod.as_mut().unwrap().ended = true;
    s.shell.playback_moved = true;
    s.run(50);
    assert_eq!(told(&s), ["stop"]);
    let kept = s.shell.store.vod_progress(key).unwrap();
    assert!(kept.watched);
    assert_eq!(s.playing().vod, None);
}

/// A movie plays at the default audio offset, not at the one of the channel that played before.
#[test]
fn a_movie_plays_at_the_default_audio_offset() {
    let mut s = sim();
    let own = format!("{} 250", s.key(1).hex());
    let set = Command::SetSetting {
        key: keys::CHANNEL_AUDIO_OFFSETS.into(),
        value: own,
    };
    s.shell.run(set, s.t);
    s.live(1);
    assert_eq!(told(&s)[0], "audio offset 250");
    play_first_movie(&mut s);
    assert_eq!(
        told(&s),
        ["audio offset 0", "open vod Northern Lights at 0 None"]
    );
}

/// The parental lock covers a movie of a locked group: asked for by key, it does not play.
#[test]
fn a_locked_movie_does_not_play() {
    let mut s = sim();
    let key = first_movie(&s);
    s.shell.navigate(Navigate::Vod(VodSection::Movies), None);
    s.shell.settle(s.t);
    let lock = Command::SetSetting {
        key: keys::PARENTAL_GROUPS.into(),
        value: "Drama".into(),
    };
    s.shell.run(lock, s.t);
    told(&s);
    let play = Command::PlayVod {
        item: key,
        offset_secs: 0,
    };
    s.shell.run(play.clone(), s.t);
    assert!(told(&s).is_empty());
    assert!(s.playing().vod.is_none());
    // Opened by the PIN for the session, it plays.
    s.shell
        .run(Command::Unlock(Lock::Group("Drama".into())), s.t);
    s.shell.run(play, s.t);
    assert_eq!(told(&s), ["open vod Northern Lights at 0 None"]);
}

#[test]
fn a_seek_or_a_pause_without_vod_or_catchup_does_nothing() {
    let mut s = sim();
    s.live(1);
    told(&s);
    s.shell.run(Command::SeekTo { position_secs: 10 }, s.t);
    s.shell.run(Command::TogglePause, s.t);
    s.shell.run(Command::StopVod, s.t);
    assert!(told(&s).is_empty());
    // A VOD item that is gone does not play.
    s.shell.run(
        Command::PlayVod {
            item: ChannelKey(42),
            offset_secs: 0,
        },
        s.t,
    );
    assert!(told(&s).is_empty());
    assert!(s.playing().vod.is_none());
}
