//! The video player as the shell drives it: open a channel live or a programme from its archive
//! into a rectangle, move it, seek, pause, pick an audio track, and report back what the
//! decoder learnt.
//!
//! The shell decides what plays (it turns the screens' `Command`s into these calls and keeps the
//! screens' `Playback`); a [`Player`] only plays, building stream and archive URLs from the
//! channel itself. `media::MediaPlayer` is the one over `tv_media`.

use iptv::model::{Channel, Programme};
use iptv::vod::VodItem;
use tv_screens::data::{Playback, Rect};
use tv_screens::playback::DisplayMode;

/// What to play. The channel's URLs and catch-up template carry the account's token: a player
/// logs them only through `iptv::redact`.
pub enum Stream<'a> {
    Live(&'a Channel),
    /// `programme` from the channel's archive, `offset_secs` into it. At its end a time shift
    /// (`shift`) is reported ([`Player::take_ended`]) and plays on until the shell opens the
    /// next programme; any other goes live by itself.
    Catchup {
        channel: &'a Channel,
        programme: &'a Programme,
        offset_secs: i64,
        shift: bool,
    },
}

pub trait Player {
    /// Stop what plays and play `stream` in `rect` on the 1920x1080 plane (`None`: full screen).
    fn open(&mut self, stream: &Stream<'_>, rect: Option<Rect>);
    /// Stop what plays and play the movie or episode `item` from `offset_secs` in:
    /// [`seek`](Self::seek) and [`set_paused`](Self::set_paused) act on it, and
    /// [`poll`](Self::poll) reports its position, length and end in `playback.vod`. Its URL
    /// carries the account's token as a channel's does. A player without VOD ignores it.
    fn open_vod(&mut self, _item: &VodItem, _offset_secs: i64, _rect: Option<Rect>) {}
    /// Move the video without restarting it.
    fn set_rect(&mut self, rect: Option<Rect>);
    /// Seek inside the archive programme, seconds from its start.
    fn seek(&mut self, position_secs: i64);
    /// Leave the archive programme for its channel's live stream.
    fn go_live(&mut self);
    fn set_paused(&mut self, paused: bool);
    fn select_audio_track(&mut self, index: usize);
    /// Show subtitle track `index` of `Playback::subtitle_tracks`, or none, without a reload.
    fn select_subtitle_track(&mut self, _index: Option<usize>) {}
    /// Advance to `now_ms` and fold what the decoder reported (position, media info, audio
    /// tracks) into `playback`; at the end of an archive programme that is no time shift the
    /// player goes live by itself and clears `playback.catchup`. Returns whether anything
    /// changed. Called on every loop turn, and by [`next_wake_ms`](Self::next_wake_ms).
    fn poll(&mut self, _now_ms: u64, _playback: &mut Playback) -> bool {
        false
    }
    /// The time shift playing reached the end of its programme since the last call, once.
    fn take_ended(&mut self) -> bool {
        false
    }
    /// When [`poll`](Self::poll) is next needed even if nothing else happens; `None`: only on
    /// events.
    fn next_wake_ms(&self, _now_ms: u64) -> Option<u64> {
        None
    }
    /// The app went to the background: release the decoder, keep what plays.
    fn suspend(&mut self) {}
    /// Back in front (or the TV gave the decoder back): play on, live at its edge, an archive
    /// programme where it was.
    fn resume(&mut self) {}
    /// The viewer pressed a key: a playback the TV took the decoder from tries again.
    fn on_key(&mut self) {}
    /// How the picture fills its rectangle, from now on: what plays and every later start.
    fn set_display_mode(&mut self, _mode: DisplayMode) {}
    /// The audio sync offset, ms (positive: the sound later), from now on; set before a stream
    /// opens, it plays with it from its first sound.
    fn set_audio_offset(&mut self, _offset_ms: i32) {}
    /// Stop playing and release the decoder (the sleep timer).
    fn stop(&mut self) {}
    /// The app is ending: stop and wait until the decoder is released, before the process ends.
    fn shutdown(&mut self) {}
    /// The video is on its own plane under the UI: keep presenting frames while it is.
    fn needs_frames(&self) -> bool {
        false
    }
    /// A message for the viewer (no archive for the programme, a failed playback), once.
    fn take_notice(&mut self) -> Option<String> {
        None
    }
}
