//! Picture in picture in the running app: the player menu's row opens the guide full screen and
//! the playing video moves into its corner window through the screens' rectangle, with no
//! second decoder; OK and Back take it out again.

use super::*;

/// The guide's preview and the picture-in-picture window: a quarter of the width, 16:9, in the
/// bottom-right corner.
const PREVIEW: Rect = Rect {
    x: 32,
    y: 24,
    w: 640,
    h: 360,
};
const WINDOW: Rect = Rect {
    x: 1408,
    y: 786,
    w: 480,
    h: 270,
};

fn moved(rect: Option<Rect>) -> Vec<String> {
    vec![format!("rect {rect:?}")]
}

/// Channel 4 full screen with the player menu over it; its Picture in picture row answers what
/// `ContextMenu` answers (the menu's own tests pick the row).
fn menu_over_the_player() -> (Sim, Probe) {
    let mut s = Sim::new();
    s.live(4);
    s.shell.navigate(Navigate::Player, None);
    s.shell.settle(s.t);
    assert_eq!(s.shell.rect, None, "full screen");
    s.calls.take();
    let menu = s.push("menu");
    *menu.0.reply.borrow_mut() = Output::close().and(Output::navigate(Navigate::Pip));
    (s, menu)
}

#[test]
fn the_playing_video_moves_into_the_corner_window_over_the_full_screen_guide() {
    let (mut s, _menu) = menu_over_the_player();
    s.press("OK");
    assert_eq!(s.open(), 1, "the guide alone, full screen");
    assert_eq!(s.shell.rect, Some(WINDOW));
    assert_eq!(s.calls.take(), moved(Some(WINDOW)), "moved, not reopened");
    assert_eq!(s.playing().channel, Some(s.key(4)), "it keeps playing");
    s.run(10_000);
    assert_eq!(s.shell.rect, Some(WINDOW), "browsing time does not end it");
    s.press("Down");
    s.press("Right");
    assert_eq!(
        s.calls.take(),
        Vec::<String>::new(),
        "browsing plays nothing"
    );
}

#[test]
fn ok_on_the_channel_in_the_window_plays_it_full_screen() {
    let (mut s, _menu) = menu_over_the_player();
    s.press("OK");
    s.calls.take();
    s.press("OK");
    assert_eq!(s.open(), 0, "the player in front");
    assert_eq!(s.shell.rect, None);
    assert_eq!(s.calls.take(), moved(None));
    assert_eq!(s.playing().channel, Some(s.key(4)));
}

#[test]
fn back_takes_the_video_back_into_the_preview_then_out_to_full_screen() {
    let (mut s, _menu) = menu_over_the_player();
    s.press("OK");
    s.calls.take();
    s.press("Back");
    assert_eq!(s.open(), 1, "still the guide");
    assert_eq!(s.calls.take(), moved(Some(PREVIEW)));
    s.shell.navigate(Navigate::Pip, None);
    s.shell.settle(s.t);
    assert_eq!(s.calls.take(), moved(Some(WINDOW)));
    // Back to the preview, Back to the groups panel, Back to the player.
    s.press("Back");
    assert_eq!(s.calls.take(), moved(Some(PREVIEW)));
    s.press("Back");
    assert_eq!((s.open(), s.shell.rect), (1, Some(PREVIEW)));
    s.press("Back");
    assert_eq!((s.open(), s.shell.rect), (0, None));
}

#[test]
fn the_plain_guide_opens_with_its_preview() {
    let (mut s, _menu) = menu_over_the_player();
    s.shell.navigate(Navigate::Guide, None);
    s.shell.settle(s.t);
    assert_eq!(s.shell.rect, Some(PREVIEW));
}
