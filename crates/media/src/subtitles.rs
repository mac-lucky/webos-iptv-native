//! Subtitles: the tracks the demuxer found, the viewer's pick, and the decoded pictures waiting
//! for their time.
//!
//! The demux thread (`ff::subdec`) publishes the subtitle tracks it can render each time it opens
//! a stream, decodes every one of them and pushes each decoded display set here as a [`Cue`] of its
//! track. The main thread asks [`current`] once a frame which cue of the picked track belongs on
//! screen at the presented position and draws it (the application's overlay). Nothing here touches
//! FFmpeg, so the store, its caps and its timing are host-tested as plain data.
//!
//! **Every track is decoded, the pick only chooses what shows.** The demuxer reads ahead of the
//! picture (a live playlist's buffer, tens of seconds of a recording), and a track it skipped has
//! nothing for that stretch: decoding only the picked one would leave a new pick blank until the
//! read-ahead ran out. A DVB display set is a few kilobytes every second or two, so decoding them
//! all costs little, and the caps below bound what is kept whatever the stream sends.
//!
//! **Timing.** A cue is stamped with its packet's demux time (the stream's PTS in ns, the clock the
//! video access units carry). The engine feeds video at `pts + pts_shift` and the sink reports
//! `playpos = fed - pts_shift + disp_base`, so the demux time on screen is `playpos - disp_base`:
//! no offset of its own to keep in step across rebases and seeks.
//!
//! **Untrusted input.** Every picture comes off the network. The decoder side lists at most
//! [`MAX_TRACKS`] tracks and refuses an oversized packet, rect or picture before it allocates
//! ([`MAX_RECTS`], [`MAX_SIDE`], [`MAX_CUE_PIXELS`]); the store holds at most [`MAX_CUES`] cues and
//! [`MAX_STORE_BYTES`] of pixels over all tracks, dropping the oldest first, so a stream that sends
//! pictures faster than they show cannot grow it.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};

/// Most subtitle tracks listed (and decoded) for one stream.
pub const MAX_TRACKS: usize = 8;
/// Most rects (DVB regions) one picture may carry; a broadcast uses one to four.
pub const MAX_RECTS: usize = 8;
/// Longest side of a rect or of the canvas it is placed on (a DVB display definition tops out at
/// 4096).
pub const MAX_SIDE: u32 = 4096;
/// Most pixels in one picture, all rects together: a full HD screen.
pub const MAX_CUE_PIXELS: usize = 1920 * 1080;
/// Most cues held at once, all tracks together. The demuxer runs ahead of the picture, and a
/// broadcast sends a display set every second or two per track.
pub const MAX_CUES: usize = 96;
/// Most bytes of pixels (palette indices) held at once, all tracks together.
pub const MAX_STORE_BYTES: usize = 8 << 20;

/// One subtitle track the app can show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Track {
    /// The `language` tag (`pol`, `eng`), empty when the stream has none.
    pub language: String,
    /// FFmpeg's codec name (`dvb_subtitle`).
    pub codec: String,
    /// Subtitles for the deaf and hard of hearing (the stream says so).
    pub hearing_impaired: bool,
}

/// One rect of a picture, as palette indices, on the stream's canvas.
#[derive(Clone, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// `w * h` palette indices, row by row.
    pub index: Vec<u8>,
    /// Straight-alpha RGBA; an index past the decoder's colours reads transparent.
    pub palette: Box<[[u8; 4]; 256]>,
}

impl std::fmt::Debug for Rect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rect({},{} {}x{})", self.x, self.y, self.w, self.h)
    }
}

impl Rect {
    /// The rect as straight-alpha RGBA, `w * h * 4` bytes.
    pub fn to_rgba(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.index.len() * 4);
        for &i in &self.index {
            out.extend_from_slice(&self.palette[usize::from(i)]);
        }
        out
    }
}

/// One decoded display set of one track: what shows from `start_ns` until `end_ns` or the track's
/// next cue, whichever comes first. No rects is a clear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cue {
    /// The track's position in [`tracks`].
    pub track: usize,
    /// Demux time (ns) it shows from.
    pub start_ns: i64,
    /// Demux time it is gone by; `i64::MAX` until the next cue replaces it.
    pub end_ns: i64,
    /// The canvas the rects are placed on (DVB: 720x576 unless the stream defines another).
    pub canvas: (u32, u32),
    pub rects: Vec<Rect>,
    /// Unique per cue for the process, so a renderer knows when to upload again.
    pub serial: u64,
}

impl Cue {
    fn bytes(&self) -> usize {
        self.rects.iter().map(|r| r.index.len()).sum()
    }
}

/// The cues of every track, in start order.
#[derive(Default)]
pub struct Store {
    cues: Vec<Arc<Cue>>,
    bytes: usize,
}

impl Store {
    /// Add `cue` in start order, then drop the oldest until the caps hold. A cue alone over the
    /// byte cap is refused (`false`).
    pub fn push(&mut self, cue: Cue) -> bool {
        let n = cue.bytes();
        if n > MAX_STORE_BYTES {
            return false;
        }
        let at = self.cues.partition_point(|c| c.start_ns <= cue.start_ns);
        self.cues.insert(at, Arc::new(cue));
        self.bytes += n;
        while self.cues.len() > MAX_CUES || self.bytes > MAX_STORE_BYTES {
            let gone = self.cues.remove(0);
            self.bytes -= gone.bytes();
        }
        true
    }

    /// The cue of `track` on screen at demux time `now`: the track's latest one started by then,
    /// unless it is a clear or has ended. A cue of any track with a later one of its own track
    /// started by `now` is superseded and dropped.
    pub fn active(&mut self, now: i64, track: usize) -> Option<Arc<Cue>> {
        // Asked once a frame: no allocation. Each track's latest cue started by `now`.
        let started = self.cues.partition_point(|c| c.start_ns <= now);
        let mut latest = [None; MAX_TRACKS];
        for (i, c) in self.cues[..started].iter().enumerate() {
            if let Some(slot) = latest.get_mut(c.track) {
                *slot = Some(i);
            }
        }
        let latest_of = |t: usize| latest.get(t).copied().flatten();
        let shown = latest_of(track).map(|i| Arc::clone(&self.cues[i]));
        let mut i = 0;
        self.cues.retain(|c| {
            let kept = i >= started || latest_of(c.track) == Some(i);
            i += 1;
            if !kept {
                self.bytes -= c.bytes();
            }
            kept
        });
        shown.filter(|c| !c.rects.is_empty() && now < c.end_ns)
    }

    pub fn clear(&mut self) {
        self.cues.clear();
        self.bytes = 0;
    }

    pub fn len(&self) -> usize {
        self.cues.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// The picked track: `OFF`, `AUTO` (the preferred one once the tracks are known) or a position in
/// [`tracks`].
const OFF: i32 = -1;
const AUTO: i32 = -2;

static STORE: Mutex<Store> = Mutex::new(Store { cues: Vec::new(), bytes: 0 });
static TRACKS: Mutex<Vec<Track>> = Mutex::new(Vec::new());
/// Bumped whenever [`tracks`] or [`selected`] changes, for a reader that copies them only then.
static TRACKS_GEN: AtomicU64 = AtomicU64::new(0);
static WANT: AtomicI32 = AtomicI32::new(OFF);
static SERIAL: AtomicU64 = AtomicU64::new(0);
/// Show subtitles without a pick: a new playback starts on the preferred track.
static AUTO_ON: AtomicBool = AtomicBool::new(false);
/// The preferred language (any tag [`tv_base::tracks::language_index`] reads), for [`AUTO`].
static AUTO_LANGUAGE: Mutex<String> = Mutex::new(String::new());

fn lock<T>(m: &'static Mutex<T>) -> MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The viewer's setting: whether a playback starts with subtitles on, and in which language
/// first. Read at the next [`begin`].
pub fn set_auto(on: bool, language: &str) {
    AUTO_ON.store(on, Relaxed);
    *lock(&AUTO_LANGUAGE) = language.to_owned();
}

/// A new playback: no tracks, no cues, and the pick back to the setting's.
pub fn begin() {
    lock(&TRACKS).clear();
    lock(&STORE).clear();
    WANT.store(if AUTO_ON.load(Relaxed) { AUTO } else { OFF }, Relaxed);
    TRACKS_GEN.fetch_add(1, Relaxed);
}

/// The renderable subtitle tracks of the stream playing, in the order the menu lists them.
pub fn tracks() -> Vec<Track> {
    lock(&TRACKS).clone()
}

/// Changes whenever [`tracks`] or [`selected`] may have.
pub fn tracks_generation() -> u64 {
    TRACKS_GEN.load(Relaxed)
}

/// The track shown, by position in [`tracks`]; `None` while off (or not decided yet).
pub fn selected() -> Option<usize> {
    usize::try_from(WANT.load(Relaxed)).ok()
}

/// Show track `index` of [`tracks`], or none. Its cues are already decoded: the next frame shows
/// it.
pub fn select(index: Option<usize>) {
    let want = index.and_then(|i| i32::try_from(i).ok()).unwrap_or(OFF);
    WANT.store(want, Relaxed);
    TRACKS_GEN.fetch_add(1, Relaxed);
}

/// The demuxer opened a stream with these renderable `tracks`: publish them, drop every cue (the
/// clock may have moved) and resolve an [`AUTO`] pick. Returns the track picked, if any.
pub(crate) fn opened(tracks: Vec<Track>) -> Option<usize> {
    lock(&STORE).clear();
    let want = WANT.load(Relaxed);
    let pick = match want {
        AUTO if !tracks.is_empty() => Some(preferred_track(&tracks, &lock(&AUTO_LANGUAGE))),
        n => usize::try_from(n).ok().filter(|&i| i < tracks.len()),
    };
    let resolved = pick
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(if want == AUTO { AUTO } else { OFF });
    WANT.store(resolved, Relaxed);
    let mut published = lock(&TRACKS);
    if *published != tracks || resolved != want {
        *published = tracks;
        TRACKS_GEN.fetch_add(1, Relaxed);
    }
    pick
}

/// The track in `language` (a non-hearing-impaired one first), else the first one.
pub fn preferred_track(tracks: &[Track], language: &str) -> usize {
    let Some(want) = tv_base::tracks::language_index(language) else { return 0 };
    let find = |hi: bool| {
        tracks
            .iter()
            .position(|t| t.hearing_impaired == hi && tv_base::tracks::language_index(&t.language) == Some(want))
    };
    find(false).or_else(|| find(true)).unwrap_or(0)
}

/// A decoded display set of one track.
pub(crate) fn push(mut cue: Cue) {
    cue.serial = SERIAL.fetch_add(1, Relaxed) + 1;
    if !lock(&STORE).push(cue) {
        crate::player::log("subtitles: a picture over the store's byte cap was dropped");
    }
}

/// Drop every cue (a seek, a reopen): what shows next comes from the new reads.
pub(crate) fn flush() {
    lock(&STORE).clear();
}

/// The cue on screen now, at the presented position; `None` when nothing shows.
pub fn current() -> Option<Arc<Cue>> {
    let track = selected()?;
    let shared = &crate::player::SHARED;
    let now = shared.playpos_ns.load(Relaxed) - shared.disp_base.load(Relaxed);
    lock(&STORE).active(now, track)
}

/// Every cue held, oldest first, without pruning (the decode tests read what the demuxer stored).
#[cfg(test)]
pub(crate) fn cues_for_test() -> Vec<Arc<Cue>> {
    lock(&STORE).cues.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    fn rect(w: u32, h: u32) -> Rect {
        Rect { x: 0, y: 0, w, h, index: vec![1; (w * h) as usize], palette: Box::new([[255; 4]; 256]) }
    }

    fn cue_of(track: usize, start_s: i64, end_s: Option<i64>, rects: Vec<Rect>) -> Cue {
        Cue {
            track,
            start_ns: start_s * S,
            end_ns: end_s.map_or(i64::MAX, |e| e * S),
            canvas: (720, 576),
            rects,
            serial: (track as u64) * 1000 + start_s as u64,
        }
    }

    fn cue(start_s: i64, end_s: Option<i64>, rects: Vec<Rect>) -> Cue {
        cue_of(0, start_s, end_s, rects)
    }

    #[test]
    fn a_cue_shows_from_its_start_until_its_end_or_the_next_one() {
        let mut st = Store::default();
        st.push(cue(10, Some(14), vec![rect(4, 2)]));
        st.push(cue(12, None, vec![rect(2, 2)]));
        st.push(cue(20, None, vec![])); // a clear
        assert_eq!(st.active(9 * S, 0), None, "nothing before the first start");
        assert_eq!(st.active(10 * S, 0).map(|c| c.serial), Some(10));
        assert_eq!(st.active(12 * S, 0).map(|c| c.serial), Some(12), "the next one replaces it");
        assert_eq!(st.active(19 * S, 0).map(|c| c.serial), Some(12), "no end: until replaced");
        assert_eq!(st.active(20 * S, 0), None, "a clear clears");
        assert_eq!(st.len(), 1, "superseded cues are dropped");
    }

    #[test]
    fn a_cue_past_its_end_shows_nothing_and_late_cues_are_ordered() {
        let mut st = Store::default();
        st.push(cue(30, Some(32), vec![rect(1, 1)]));
        st.push(cue(25, Some(40), vec![rect(1, 1)])); // arrives out of order
        assert_eq!(st.active(26 * S, 0).map(|c| c.serial), Some(25));
        assert_eq!(st.active(31 * S, 0).map(|c| c.serial), Some(30));
        assert_eq!(st.active(33 * S, 0), None, "30 ended at 32, and 25 was superseded");
    }

    #[test]
    fn each_track_keeps_its_own_cues_and_the_pick_shows_one() {
        let mut st = Store::default();
        st.push(cue_of(0, 10, None, vec![rect(1, 1)]));
        st.push(cue_of(1, 11, None, vec![rect(2, 1)]));
        st.push(cue_of(0, 12, None, vec![]));
        assert_eq!(st.active(11 * S, 1).map(|c| c.serial), Some(1011));
        assert_eq!(st.active(11 * S, 0).map(|c| c.serial), Some(10), "track 1 does not replace 0");
        assert_eq!(st.active(13 * S, 1).map(|c| c.serial), Some(1011), "nor does 0's clear clear 1");
        assert_eq!(st.active(13 * S, 0), None);
        assert_eq!(st.len(), 2, "each track keeps only its current cue");
        assert_eq!(st.active(13 * S, 5), None, "a track with no cues shows nothing");
    }

    #[test]
    fn the_store_keeps_its_caps_by_dropping_the_oldest() {
        let mut st = Store::default();
        for i in 0..(MAX_CUES as i64 + 10) {
            st.push(cue(i, None, vec![rect(10, 10)]));
        }
        assert_eq!(st.len(), MAX_CUES);
        assert_eq!(st.bytes(), MAX_CUES * 100);
        assert_eq!(st.active(10 * S - 1, 0), None, "the ten oldest went first");
        assert_eq!(st.active(10 * S, 0).map(|c| c.serial), Some(10));

        let mut st = Store::default();
        let big = 1920 * 1080;
        for i in 0..10 {
            st.push(cue(i, None, vec![rect(1920, 1080)]));
            assert!(st.bytes() <= MAX_STORE_BYTES);
        }
        assert_eq!(st.len(), MAX_STORE_BYTES / big);
        let mut huge = rect(1, 1);
        huge.index = vec![0; MAX_STORE_BYTES + 1];
        assert!(!st.push(cue(99, None, vec![huge])), "one picture over the cap is refused");
    }

    #[test]
    fn rgba_reads_each_index_through_the_palette() {
        let mut r = rect(2, 1);
        r.index = vec![0, 3];
        r.palette[0] = [0, 0, 0, 0];
        r.palette[3] = [10, 20, 30, 255];
        assert_eq!(r.to_rgba(), vec![0, 0, 0, 0, 10, 20, 30, 255]);
    }

    #[test]
    fn the_preferred_track_is_the_language_match_then_the_first() {
        let t = |lang: &str, hi: bool| Track { language: lang.into(), codec: "dvb_subtitle".into(), hearing_impaired: hi };
        let tracks = [t("eng", false), t("pol", true), t("pol", false)];
        assert_eq!(preferred_track(&tracks, "pl"), 2, "the plain Polish one before the SDH one");
        assert_eq!(preferred_track(&tracks, "pol"), 2);
        assert_eq!(preferred_track(&tracks, "de"), 0, "no match: the first");
        assert_eq!(preferred_track(&tracks, ""), 0);
        assert_eq!(preferred_track(&tracks[..2], "pl"), 1, "only an SDH one: that one");
    }

    #[test]
    fn a_pick_follows_the_tracks_and_auto_resolves_once_they_are_known() {
        let _g = tv_base::testlock::serial();
        let t = |lang: &str| Track { language: lang.into(), codec: "dvb_subtitle".into(), hearing_impaired: false };
        set_auto(false, "");
        begin();
        assert_eq!(selected(), None);
        assert_eq!(opened(vec![t("eng"), t("pol")]), None, "off");
        select(Some(1));
        let gen = tracks_generation();
        assert_eq!(opened(vec![t("eng"), t("pol")]), Some(1), "a reopen keeps the pick");
        assert_eq!(tracks_generation(), gen, "and changes nothing the menu shows");
        assert_eq!(opened(vec![t("eng")]), None, "a pick the stream no longer has is off");
        assert_eq!(selected(), None);

        set_auto(true, "pl");
        begin();
        assert_eq!(selected(), None, "auto waits for the tracks");
        assert_eq!(opened(vec![]), None);
        assert_eq!(opened(vec![t("eng"), t("pol")]), Some(1));
        assert_eq!(selected(), Some(1));
        assert_eq!(tracks().len(), 2);
        set_auto(false, "");
        begin();
        assert!(tracks().is_empty());
    }
}
