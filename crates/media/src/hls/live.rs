//! **The live window, as pure arithmetic**: where a live playback starts, when the media playlist
//! is reloaded, where the next segment is after a reload, and when a playlist that has stopped
//! growing is declared dead. No I/O and no clock of its own - every function takes `now` in
//! milliseconds from whatever clock the caller runs ([`super::source::Clock`]), so the tests drive
//! them with a fake one.
//!
//! - **Start** (RFC 8216 §6.3.3): no segment that begins less than three target durations from the
//!   end of the playlist. [`live_start_index`] walks back from the end until the segments after the
//!   start add up to at least that much.
//! - **Reload** (§6.3.4): at least one target duration after the last load BEGAN when that load
//!   changed the playlist, half of one when it did not ([`Schedule::reload_due_ms`]).
//! - **The next segment** is found by media sequence number, never by position: a reload slides the
//!   window, and `EXT-X-MEDIA-SEQUENCE` says by how much ([`locate`]). A client that fell behind the
//!   window (its next segment already expired) jumps to the oldest segment still listed.
//! - **A dead playlist** is one that has added no segment for [`STALL_TARGETS`] target durations
//!   (and at least [`STALL_FLOOR_MS`]): an explicit failure, never an endless wait.

use super::playlist::MediaPlaylist;

/// How many target durations behind the live edge a playback starts (RFC 8216 §6.3.3's minimum).
pub const START_TARGETS: u64 = 3;

/// A live playlist that has added no segment for this many target durations is dead. Twice
/// ExoPlayer's 3.5 (its `PlaylistStuckException`): a TV that stalls a live channel is worse than
/// one that waits a few seconds longer for a slow packager.
pub const STALL_TARGETS: u64 = 7;

/// ... and never sooner than this, whatever a tiny target duration says.
pub const STALL_FLOOR_MS: u64 = 15_000;

/// The index of the first segment a live playback plays: the latest one from which at least
/// [`START_TARGETS`] target durations of media remain. The first segment when the playlist holds
/// less than that.
pub fn live_start_index(p: &MediaPlaylist) -> usize {
    let want = p.target_duration_us.saturating_mul(START_TARGETS);
    let mut tail = 0u64;
    for (i, s) in p.segments.iter().enumerate().rev() {
        tail += s.duration_us;
        if tail >= want {
            return i;
        }
    }
    0
}

/// The index of the VOD segment that contains playlist time `at_us` (the last one for a time past
/// the end).
pub fn vod_index_at(p: &MediaPlaylist, at_us: u64) -> usize {
    let mut start = 0u64;
    for (i, s) in p.segments.iter().enumerate() {
        if at_us < start + s.duration_us {
            return i;
        }
        start += s.duration_us;
    }
    p.segments.len().saturating_sub(1)
}

/// The playlist time at which segment `index` begins, microseconds.
pub fn start_of(p: &MediaPlaylist, index: usize) -> u64 {
    p.segments.iter().take(index).map(|s| s.duration_us).sum()
}

/// Where the segment with sequence number `next` is in the current playlist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locate {
    /// At this index.
    At(usize),
    /// It has expired: the window moved past it. Play from the oldest listed segment (index 0),
    /// `skipped` segments later than planned.
    Behind { skipped: u64 },
    /// Not listed yet.
    Ahead,
}

pub fn locate(p: &MediaPlaylist, next: u64) -> Locate {
    if next < p.media_sequence {
        if p.segments.is_empty() {
            return Locate::Ahead;
        }
        return Locate::Behind { skipped: p.media_sequence - next };
    }
    match usize::try_from(next - p.media_sequence) {
        Ok(i) if i < p.segments.len() => Locate::At(i),
        _ => Locate::Ahead,
    }
}

/// The reload and stall bookkeeping of one live playlist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    target_ms: u64,
    /// When the last load BEGAN.
    load_began_ms: u64,
    /// Whether that load changed the playlist.
    changed: bool,
    /// When the playlist last grew (its end sequence advanced), or was first loaded.
    grew_ms: u64,
    end_sequence: u64,
}

impl Schedule {
    /// The schedule after the first load, which began at `began_ms`.
    pub fn first(p: &MediaPlaylist, began_ms: u64) -> Schedule {
        Schedule {
            target_ms: (p.target_duration_us / 1000).max(1),
            load_began_ms: began_ms,
            changed: true,
            grew_ms: began_ms,
            end_sequence: p.end_sequence(),
        }
    }

    /// Record a reload that began at `began_ms` and produced `p`; `changed` is whether its text
    /// differs from the last one's.
    pub fn reloaded(&mut self, p: &MediaPlaylist, began_ms: u64, changed: bool) {
        self.target_ms = (p.target_duration_us / 1000).max(1);
        self.load_began_ms = began_ms;
        self.changed = changed;
        if p.end_sequence() > self.end_sequence {
            self.end_sequence = p.end_sequence();
            self.grew_ms = began_ms;
        }
    }

    /// Record a reload that began at `began_ms` and failed: retried after half a target duration,
    /// as for an unchanged playlist, and it does not count as growth.
    pub fn failed(&mut self, began_ms: u64) {
        self.load_began_ms = began_ms;
        self.changed = false;
    }

    /// When the next reload may begin (RFC 8216 §6.3.4).
    pub fn reload_due_ms(&self) -> u64 {
        let wait = if self.changed { self.target_ms } else { self.target_ms.div_ceil(2) };
        self.load_began_ms.saturating_add(wait)
    }

    /// How long the playlist may go without growing before it is dead.
    pub fn stall_limit_ms(&self) -> u64 {
        self.target_ms.saturating_mul(STALL_TARGETS).max(STALL_FLOOR_MS)
    }

    /// Has the playlist stopped growing for good, as of `now_ms`?
    pub fn stalled(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.grew_ms) >= self.stall_limit_ms()
    }

    /// Milliseconds since the playlist last grew.
    pub fn since_growth_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.grew_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::playlist::{parse, Playlist};

    fn live(seq: u64, durations: &[f64], target: u64) -> MediaPlaylist {
        let mut text = format!("#EXTM3U\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:{seq}\n");
        for (i, d) in durations.iter().enumerate() {
            text.push_str(&format!("#EXTINF:{d},\ns{}.ts\n", seq + i as u64));
        }
        match parse(&text, "https://cdn.example/live.m3u8").unwrap() {
            Playlist::Media(m) => m,
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_live_start_is_three_target_durations_behind_the_edge() {
        // Ten 6 s segments: the last three hold exactly 18 s, so play starts at the 8th (index 7).
        let p = live(100, &[6.0; 10], 6);
        assert_eq!(live_start_index(&p), 7);
        // Shorter segments than the target: walk back until 18 s are left.
        let p = live(0, &[4.0; 10], 6);
        assert_eq!(live_start_index(&p), 5, "5 x 4 s = 20 s >= 18 s, 4 x 4 s = 16 s is not");
        // A playlist shorter than three targets starts at its first segment.
        assert_eq!(live_start_index(&live(0, &[6.0, 6.0], 6)), 0);
        assert_eq!(live_start_index(&live(0, &[], 6)), 0);
    }

    #[test]
    fn vod_positions_map_to_segments() {
        let p = live(0, &[10.0, 10.0, 5.0], 10);
        assert_eq!(vod_index_at(&p, 0), 0);
        assert_eq!(vod_index_at(&p, 9_999_999), 0);
        assert_eq!(vod_index_at(&p, 10_000_000), 1);
        assert_eq!(vod_index_at(&p, 24_000_000), 2);
        assert_eq!(vod_index_at(&p, 99_000_000), 2, "past the end: the last segment");
        assert_eq!(start_of(&p, 2), 20_000_000);
    }

    #[test]
    fn the_next_segment_is_found_by_sequence_across_window_slides() {
        let p = live(1000, &[6.0; 5], 6); // 1000..=1004
        assert_eq!(locate(&p, 1003), Locate::At(3));
        assert_eq!(locate(&p, 1005), Locate::Ahead);
        // The next reload slid the window by two: the same next segment is two places earlier.
        let slid = live(1002, &[6.0; 5], 6); // 1002..=1006
        assert_eq!(locate(&slid, 1005), Locate::At(3));
        // Fell behind: 998 and 999 expired before they were fetched.
        assert_eq!(locate(&p, 998), Locate::Behind { skipped: 2 });
        assert_eq!(locate(&live(5, &[], 6), 1), Locate::Ahead, "an empty window has nothing to jump to");
    }

    #[test]
    fn reloads_follow_rfc_8216_6_3_4_on_a_fake_clock() {
        let p = live(0, &[6.0; 4], 6);
        let mut s = Schedule::first(&p, 10_000);
        assert_eq!(s.reload_due_ms(), 16_000, "one target duration after the first load began");
        // An unchanged reload at 16 s: retry after half a target.
        s.reloaded(&p, 16_000, false);
        assert_eq!(s.reload_due_ms(), 19_000);
        // A changed reload at 19 s (one new segment): a whole target again.
        let grown = live(1, &[6.0; 4], 6);
        s.reloaded(&grown, 19_000, true);
        assert_eq!(s.reload_due_ms(), 25_000);
        // A failed reload retries after half a target and is not growth.
        s.failed(25_000);
        assert_eq!(s.reload_due_ms(), 28_000);
        assert_eq!(s.since_growth_ms(28_000), 9_000);
        // An odd target rounds the half up: never a reload sooner than the RFC allows.
        let odd = live(0, &[5.0], 5);
        let mut t = Schedule::first(&odd, 0);
        t.reloaded(&odd, 0, false);
        assert_eq!(t.reload_due_ms(), 2_500);
    }

    #[test]
    fn a_playlist_that_stops_growing_is_declared_dead_and_a_slide_alone_is_not_growth() {
        let p = live(0, &[6.0; 4], 6);
        let mut s = Schedule::first(&p, 0);
        assert_eq!(s.stall_limit_ms(), 42_000, "seven 6 s targets");
        // Reloads that change the text but add nothing (a rewritten title) do not reset the clock.
        for at in (6_000..42_000).step_by(6_000) {
            s.reloaded(&p, at, true);
            assert!(!s.stalled(at), "{at}");
        }
        assert!(s.stalled(42_000));
        // A segment added just in time resets it.
        let mut s = Schedule::first(&p, 0);
        s.reloaded(&live(1, &[6.0; 4], 6), 40_000, true);
        assert!(!s.stalled(42_000));
        assert!(s.stalled(82_000));
        // A tiny target never declares death before the floor.
        let tiny = live(0, &[1.0], 1);
        let s = Schedule::first(&tiny, 0);
        assert!(!s.stalled(14_999));
        assert!(s.stalled(STALL_FLOOR_MS));
    }
}
