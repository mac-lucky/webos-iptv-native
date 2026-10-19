//! VOD on the playback API: a movie or an episode is its own mode, keeps going through a suspend
//! and a seek before it opened, holds at its end, and never prints its URL.

use super::*;

fn movie() -> Vod {
    Vod {
        url: "http://127.0.0.1:1/vod/film.mkv?token=SECRETTOKEN1".into(),
        headers: RequestHeaders::none(),
        offset_secs: 90,
    }
}

#[test]
fn a_vod_prints_no_url() {
    let shown = format!("{:?}", movie());
    assert!(!shown.contains("127.0.0.1") && !shown.contains("SECRETTOKEN1"), "{shown}");
    assert!(shown.contains("90"));
}

#[test]
fn an_ended_vod_is_a_hold_not_a_stall() {
    assert!(is_hold(false, true, true));
    assert!(!is_hold(false, false, true));
}

/// Driven through the real API with no FFmpeg on the host: the start itself is refused, which is
/// all the mode and the suspend need.
#[cfg(feature = "hostsim")]
#[test]
fn a_vod_is_its_own_mode_through_a_seek_a_suspend_and_a_stop() {
    let _serial = tv_base::testlock::serial();
    // SAFETY: this test thread is the only one driving the player.
    let mut pb = Playback::new(unsafe { MainThread::assume() });
    pb.play_vod(movie());
    pb.tick(0);
    let st = pb.status();
    assert_eq!((st.mode, st.catchup), (Mode::Vod, None));
    assert!(st.vod.is_some());
    pb.seek(30_000);
    pb.go_live();
    assert_eq!(pb.status().mode, Mode::Vod, "a VOD has no live stream to go to");
    pb.suspend();
    let st = pb.status();
    assert_eq!((st.mode, st.state), (Mode::Vod, State::Suspended));
    assert!(!st.vod.unwrap().ended, "a suspended item has not ended");
    pb.resume();
    assert_ne!(pb.status().state, State::Suspended);
    pb.stop();
    let st = pb.status();
    assert_eq!((st.mode, st.state, st.vod), (Mode::Idle, State::Idle, None));
}
