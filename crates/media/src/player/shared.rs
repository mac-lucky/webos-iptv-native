//! player::shared — the engine's cross-thread transport, callback and clock state. Every field
//! replaces a `volatile` global from playback.c and is an atomic or a Mutex (never a bare value).
//! One long-lived `static SHARED` in mod.rs outlives every start/stop
//! cycle and is *reset*, never freed. Native callbacks additionally carry the exact `Load` epoch:
//! retirement drains an event already inside that epoch and rejects every later event, so stable
//! storage is not mistaken for permission to mutate the next playback.
use tv_net::stream::HttpStream;
use std::ffi::CString;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// **The names the CONTAINER gives its audio tracks**, in file order.
///
/// Published once by the demuxer when it opens a stream (`ff`), read by the in-player track menu.
/// Both `Vec`s are dense — a track the file does not name contributes an EMPTY string rather than
/// being skipped — because position is the whole join: the N-th entry is the N-th stream of that
/// type. Skipping unnamed tracks would silently shift every name after the first untagged one onto
/// its neighbour, which is worse than showing none.
#[derive(Default)]
pub struct TrackNames {
    pub audio: Vec<String>,
    /// Each audio track's `language` tag (ISO 639-2 in Matroska/MP4: `eng`, `ger`), `""` when the
    /// file has none — dense and in file order, like the names.
    pub audio_langs: Vec<String>,
}

impl TrackNames {
    /// `Default`, but callable from `Shared::new`, which is a `const fn`.
    pub const fn new() -> Self {
        Self { audio: Vec::new(), audio_langs: Vec::new() }
    }
    /// The name of the `i`-th audio stream in file order, or `""`.
    pub fn audio(&self, i: i32) -> &str {
        usize::try_from(i).ok().and_then(|i| self.audio.get(i)).map(String::as_str).unwrap_or("")
    }
}

/// **One chapter of the open file**, as its container lists it (Matroska `Chapters`, MP4 chapter
/// track): where it starts on the content timeline and what the file calls it.
///
/// `title` is the file's own text, from a stranger's file — show it, never log it. Empty when the
/// chapter has no title. Published by the demuxer when it opens a part ([`super::chapters`]),
/// sorted by `start_ms`, an empty list for a file without chapters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chapter {
    pub start_ms: i64,
    pub title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClockHold {
    /// A freshly loaded pipeline has not been started yet.
    Initial,
    /// An in-place seek paused the old timeline and is priming the new one.
    Seek,
    /// The viewer explicitly paused a running clock.
    User,
    /// The viewer resumed a queued stream; feeding is open, but the physical clock remains held
    /// until both decoder lanes and the measured runway are ready.
    ResumePrime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimeKind {
    Fresh,
    Resume,
}

impl ClockHold {
    fn prime_kind(self) -> Option<PrimeKind> {
        match self {
            Self::Initial | Self::Seek => Some(PrimeKind::Fresh),
            Self::ResumePrime => Some(PrimeKind::Resume),
            Self::User => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClockPhase {
    Running,
    PauseIssuing {
        token: u64,
        hold: ClockHold,
    },
    Held {
        hold: ClockHold,
    },
    PlayIssuing {
        token: u64,
        hold: ClockHold,
        resume_acb: bool,
    },
    Stopping,
}

#[derive(Clone, Copy, Debug)]
struct ClockState {
    phase: ClockPhase,
    next_token: u64,
    /// Accepted user Pause ownership. This is intentionally inside the actuator mutex: the
    /// transport atomic is a hot-path mirror, not a second authority over whether Play may issue.
    user_held: bool,
    /// User Resume was accepted while a seek or initial hold still owned the native clock.
    /// The eventual Play must also resume ACB; losing that second half produces video/audio drift.
    resume_acb_pending: bool,
}

impl ClockState {
    const fn new() -> Self {
        Self {
            phase: ClockPhase::Running,
            next_token: 0,
            user_held: false,
            resume_acb_pending: false,
        }
    }

    fn token(&mut self) -> u64 {
        self.next_token = self
            .next_token
            .checked_add(1)
            .expect("clock transition token space exhausted");
        self.next_token
    }
}

/// issue #74 D.1: whether the epoch's Starfish `Load` call has returned yet. Mirrors, on the Rust
/// side, the `g_load_returned` flag `starfish.c`'s `sf_ready_object()` now requires alongside
/// `SMP_READY()` — every session begins `InFlight`, and `threads::load_thread` flips it to
/// `Returned` right after the real, synchronous `sf_load` call comes back, before
/// anything else runs. `pump.rs`'s `loadCompleted` arm requires
/// `Shared::native_load_returned` before it may even poll `sf_is_load_completed` — that poll is
/// itself a concurrent Starfish call and used to be exactly what raced `Load`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadCall {
    InFlight,
    /// `at` is when the flip happened (`mark_native_load_returned`'s own `Instant::now()`) — the
    /// zero point for the OTHER half of issue #74 D.1's budget: `pump.rs` bounds "Load never
    /// returns" from `load_issued_at`, and separately bounds "Load returned but loadCompleted
    /// never arrives" from this timestamp, on a fresh `NATIVE_LOAD_BUDGET` window rather than
    /// whatever was left of the first one.
    Returned {
        at: Instant,
    },
}

/// Ownership of the callback function installed in one native Starfish `Load`.
///
/// [`Shared`] survives reloads, while the library may deliver an event on its own thread after
/// teardown has started. Each Starfish object address is now session-unique, but Rust still needs
/// a generation check and drain before resetting this process-long state. A check without this mutex
/// still has a check/use race against [`Shared::reset_session`]: a callback could validate the
/// old generation, get descheduled, and then write into the freshly reset next session. Holding
/// this lock for the complete callback makes retirement a drain barrier as well as a token check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeSessionPhase {
    Idle,
    Active {
        epoch: u32,
        /// Type-0 presentation callbacks are admitted only for the current media timeline. An
        /// in-place seek closes this while old decoded frames are flushed, then the first real
        /// post-seek keyframe reopens it.
        presentation_gate: NativePresentationGate,
        /// See [`LoadCall`]. Epoch-scoped exactly like `presentation_gate`: a late `load_thread`
        /// from a SUPERSEDED session can never flip this for a newer one, because
        /// `mark_native_load_returned`/`native_load_returned` both require `epoch == active`.
        load_call: LoadCall,
        /// When this epoch's `Load` was issued — for the "native: Load returned after Nms" log.
        load_issued_at: Instant,
    },
    /// Firmware's synchronous UNLOADCOMPLETED callback was observed. This is independent lifecycle
    /// evidence, not a producer barrier: native callback admission is closed and drained in the C
    /// interposer before the main thread retires this phase and considers D1.
    Unloaded {
        epoch: u32,
    },
    /// Teardown gave up on this epoch's `Load` while it was still in flight (the D.1.4 budget
    /// had fired and the media thread had not returned). The native object belongs to no engine
    /// any more: it is held by `PlayerAdapter`'s abandoned-Load slot until `sf_load` returns and
    /// the main thread releases it. Only the firmware's UNLOADCOMPLETED is admitted (that is the
    /// release's own lifecycle evidence); every other callback is dropped, and no new native
    /// session may begin, because the C seam still owns exactly this one object.
    Abandoned {
        epoch: u32,
    },
    /// A real stop handed this epoch's ready object to the OFF-FRAME release (`engine`'s
    /// `PendingRelease`): its `Unload` is running on a worker while the main thread has already
    /// reset the session for whatever comes next. The admission rule is `Abandoned`'s — only the
    /// firmware's UNLOADCOMPLETED is admitted, recorded in `unload_completed` rather than as a move
    /// to `Unloaded` so that [`Shared::reset_session`] (which preserves this phase) cannot erase the
    /// evidence the release then reads — and no new native session may begin until the release has
    /// retired it.
    Releasing {
        epoch: u32,
        unload_completed: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativePresentationGate {
    Armed,
    Disarmed,
    /// The first post-discontinuity AU is inside `sf_feed`. Firmware can publish the buffer to a
    /// GStreamer worker before Feed returns, so retain the newest position callback until the
    /// caller learns whether that exact AU was accepted.
    PendingArm {
        latched: Option<i64>,
    },
}

#[derive(Clone, Copy, Debug)]
struct NativeSessionState {
    next_epoch: u32,
    phase: NativeSessionPhase,
}

impl NativeSessionState {
    const fn new() -> Self {
        Self {
            next_epoch: 0,
            phase: NativeSessionPhase::Idle,
        }
    }

    fn next(&mut self) -> u32 {
        self.next_epoch = self.next_epoch.wrapping_add(1);
        if self.next_epoch == 0 {
            self.next_epoch = 1;
        }
        self.next_epoch
    }
}

/// What provenance/lifecycle effect one native callback carries. Keeping this typed here makes
/// `UNLOADCOMPLETED` a reducer transition rather than a magic event number interpreted by the
/// teardown code after the fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeEventClass {
    Presentation,
    UnloadCompleted,
    Other,
}

/// Whose state a native callback may write; see [`Shared::with_native_session_scoped`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventScope {
    Live,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseCompletion {
    Accepted,
    Refused,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekPause {
    Issue(u64),
    AlreadyHeld,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserPause {
    Issue(u64),
    AlreadyHeld,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserResume {
    Issue(u64),
    Deferred,
    Prime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayCompletion {
    Accepted { resume_acb: bool },
    Refused,
    Stale,
}


/// What playback is actually doing, as one value the UI can render from.
///
/// This replaces the old single `seeking` boolean, which was only ever set by `request_seek` —
/// so `player::loading()` was **false for the whole initial load** and the `Spinner` that has sat
/// in `player_hud.rs` all along never fired on first play. The HUD drew a live-looking transport
/// at 0:00 / -0:00 instead, which is the half of the frozen-HUD report that is not blocking I/O.
///
/// Derived once per frame in `pump::set_state` from signals the workers already publish; no
/// new cross-thread plumbing. `Playing` alone means "a frame is on the panel"; `Paused` (a paused
/// player with no picture and nothing in flight) is neither busy nor playing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PlaybackState {
    /// no engine
    Idle = 0,
    /// what to play is still being resolved above the media layer (the application publishes it
    /// while it asks its addons; the media layer never sets it)
    Resolving = 1,
    /// engine started, pipeline not yet loaded — the HTTP GET + Starfish `Load` window
    Connecting = 2,
    /// loaded and fed, but no frame has been presented yet
    Buffering = 3,
    /// a seek is resolving (reopen → prime → Play)
    Seeking = 4,
    /// frames on the panel
    Playing = 5,
    /// the producer died (a failed open / no video stream). Terminal until teardown.
    Error = 6,
    /// the viewer is paused on a session that has no picture yet and the pump has no work in
    /// flight for one (`pump::paused_and_idle`): not `Buffering` (nothing is being waited on, so
    /// no spinner) and not `Playing` (no frame is on the panel, so no `is_playing`, no
    /// `PlaybackStarted`). The HUD draws its ordinary paused transport.
    Paused = 7,
}

/// How long a viewer's audio pick may stand as "the reason for the next rebuild" before the
/// rebuild is assumed to have been refused.
pub const AUDIO_PICK_WINDOW_MS: i64 = 30_000;

/// See [`Shared::audio_switch`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioSwitch {
    None = 0,
    /// the pick queued a rebuild that has not reloaded yet
    Picked = 1,
    /// the reload for the pick is under way; ends at the first presented frame
    Reloading = 2,
}

impl AudioSwitch {
    pub fn from_u8(v: u8) -> AudioSwitch {
        match v {
            1 => AudioSwitch::Picked,
            2 => AudioSwitch::Reloading,
            _ => AudioSwitch::None,
        }
    }
}

impl PlaybackState {
    pub fn from_u8(v: u8) -> PlaybackState {
        match v {
            1 => PlaybackState::Resolving,
            2 => PlaybackState::Connecting,
            3 => PlaybackState::Buffering,
            4 => PlaybackState::Seeking,
            5 => PlaybackState::Playing,
            6 => PlaybackState::Error,
            7 => PlaybackState::Paused,
            _ => PlaybackState::Idle,
        }
    }
    /// "something is happening and the user should see a spinner, not a dead transport".
    pub fn is_busy(self) -> bool {
        matches!(
            self,
            PlaybackState::Resolving
                | PlaybackState::Connecting
                | PlaybackState::Buffering
                | PlaybackState::Seeking
        )
    }
    /// The one-line status the HUD shows beside the spinner. A `&'static CStr` (the house idiom
    /// for UI strings) so the draw path allocates nothing — this is read 60x/second for the whole
    /// load window, and indefinitely in `Error`.
    pub fn caption(self) -> &'static std::ffi::CStr {
        match self {
            PlaybackState::Resolving => tv_platform::i18n::msg::widgets_status_preparing_c(),
            PlaybackState::Connecting => tv_platform::i18n::msg::widgets_status_connecting_c(),
            PlaybackState::Buffering => tv_platform::i18n::msg::widgets_status_buffering_c(),
            PlaybackState::Seeking => tv_platform::i18n::msg::widgets_status_seeking_c(),
            PlaybackState::Error => tv_platform::i18n::msg::widgets_status_failed_c(),
            _ => c"",
        }
    }
}

pub struct Shared {
    /// Synchronized lifecycle/provenance for callbacks from the current native `Load`.
    native_session: Mutex<NativeSessionState>,
    // library callback thread (K) -> main (M)
    pub playpos_ns: AtomicI64, // g_playpos_ns
    // the presented frame's fed PTS (0-based, raw `num` from the type=0 callback). The feed
    // loop throttles on max_fed_pts - pres_fed so it stays ~MAX_FEED_AHEAD ahead of the
    // decoder (feeding further overfills the 4K HEVC DPB/CPB and stalls the sink).
    pub pres_fed: AtomicI64,
    pub frames: AtomicI32, // bf_frames
    /// **Has this SESSION ever put a picture on the panel?** Set by the frame-presented callback
    /// beside `frames`, cleared ONLY by [`Shared::reset_session`] — so it survives a seek, which
    /// `frames` deliberately does not: `pump` zeroes `frames` as *part of applying* a seek, which
    /// makes "we have never shown a frame" and "we just seeked" indistinguishable through it. The
    /// HUD's one rule for which surface owns the "pipeline is working" read-out keys on this bit;
    /// see `appkit::player_hud::busy_surface`. Monotone within a session (false→true only), which is
    /// what lets two readers in one frame sample it independently without disagreeing.
    pub seen_frame: AtomicBool,
    /// **The raw pts of the first audio AU the demuxer pushed since its last open or seek**.
    /// Written by `ff::demux` BEFORE that push, so the feed thread that pops the AU
    /// sees it. The audio lane uses it to tell the real start of a late audio track (an MKV
    /// audio `Delay` of several seconds) from stale pre-seek audio, which looks the same — far
    /// ahead of the video — but never carries this pts (see `engine::feed_audio_lane`). `i64::MIN`
    /// before one, since a real pts may be negative.
    pub first_audio_pts: AtomicI64,
    /// **Why the NEXT rebuild is happening, when the viewer's audio-track pick is the cause**: the
    /// read-out beside the load spinner says "Switching audio…" instead of "Buffering…". A
    /// [`AudioSwitch`] value. `Picked` (set by `player::request_audio_track` once its reload is
    /// queued) becomes `Reloading` when the engine reloads (`reset_session_for_reload`) and
    /// `None` at that session's first presented frame or at a full `reset_session` (a failure
    /// clears nothing by itself). A
    /// pick whose rebuild never lands (the server refused it) is not left standing: `Picked`
    /// expires after [`AUDIO_PICK_WINDOW_MS`], so a later seek's reload is not named a switch.
    pub audio_switch: AtomicU8,
    /// `vclock_ms` of the pick that set [`Self::audio_switch`] to `Picked`.
    pub audio_pick_ms: AtomicI64,
    pub load_completed: AtomicBool,          // bf_loaded signal
    pub media_id: Mutex<Option<CString>>,    // bf_mediaId (captured once)
    pub source_info: Mutex<Option<Vec<u8>>>, // sourceInfoRaw, VERBATIM incl NUL
    /// What the demuxer read of a VP9 stream's colour (PQ tag, mastering display, light level). Set
    /// only for VP9, cleared per session; `pump` writes it into `source_info` before ACB is sent it.
    pub container_hdr: Mutex<Option<super::vp9_hdr::ContainerHdr>>,

    // main/pump (M) -> library callback thread (K)
    pub pts_shift: AtomicI64, // g_pts_shift
    // content-time offset added to the displayed position: playpos = fed - pts_shift + this. 0
    // for a file whose PTS already IS content time; a stream that restarts 0-based at an offset
    // carries that offset here.
    pub disp_base: AtomicI64,
    /// demux (D) -> main: an HLS VOD stream's playlist time at the first segment the demuxer
    /// opened, ns, else -1. The feed's rebase then reads positions in PLAYLIST time (a catch-up
    /// programme's own clock) instead of the segments' arbitrary MPEG-TS timestamps.
    pub hls_vod_base_ns: AtomicI64,

    // main/pump (M) -> demux (D)
    // The demux seek target in content-ns (-1 = none). The pump publishes it and the demux
    // thread consumes it with an av_seek_frame between two reads — it is the ENTIRE direct-play
    // seek mechanism, and also carries the resume offset armed before the first read. Nothing
    // interrupts the demuxer to make this happen; a `seek_byte` reopen trigger and a `next_url`
    // used to sit alongside it for that, and neither could ever fire (see ff.rs).
    pub seek_to_ns: AtomicI64,
    // the in-place seek's target content-ns, for the feed's rebase guard (drop stale drifted
    // keyframes). Distinct from seek_to_ns, which the demuxer consumes as it seeks. -1 = none.
    pub seek_target_ns: AtomicI64,
    // UI loading state: true from a seek request until playback resumes at the new position (prime→
    // Play). The HUD shows a spinner + freezes the playhead at `seek_display_ns` so it doesn't
    // wobble through the seek/rebase. -1 display = not loading.
    pub seeking: AtomicBool,
    pub seek_display_ns: AtomicI64,
    // NATIVE audio-track switch: the 0-based audio stream index to feed. `desired` (-1 = the
    // first stream of the declared codec) is read by the demuxer to pick the Nth audio stream and
    // PERSISTS across seeks/reloads (reset only on a new item, not in reset_session). Whether a
    // reload is pending is `Transport::audio_reload`, which reset_session does not touch.
    pub desired_audio_idx: AtomicI32,
    // the derived UI state (a `PlaybackState`), published by the pump once a frame and read by
    // the HUD. Written only on the main thread; atomic because it is read from the draw path.
    pub pb_state: AtomicU8,
    // demux (D) -> main (M): the producer died before publishing a duration, so the EOS path
    // (which needs `duration_ns > 0`) can never fire and the player would sit on a black screen
    // forever. The pump turns this into `PlaybackState::Error` so the HUD can say so.
    pub demux_failed: AtomicBool,
    /// The media transport failed after open, possibly after frames were already presented.
    /// Unlike `demux_failed`, this is not gated on a zero-frame start: a truncated live transfer
    /// is an error rather than a successful early EOF.
    pub demux_io_failed: AtomicBool,
    /// WHY the demuxer found nothing to feed, when the answer is "the stream itself": it carries
    /// audio streams and no video stream. `ff: no video stream` alone reads as a demuxer fault;
    /// this lets the read-out name the file. Set by the demux thread beside `demux_failed`.
    pub demux_no_video: AtomicBool,
    /// `sf_load` returned 0 — the pipeline REFUSED the Load payload.
    ///
    /// Without this the pump has no way to learn it: `load_thread` logged the result and dropped
    /// it, `loadCompleted` never arrives, and `Stage::Loading` is never left — so the player sits
    /// on a spinner forever with no error and no timeout. A rejected payload is the most likely
    /// shape of a webOS-5-specific failure (a key the newer pipeline will not accept), which makes
    /// this exactly the case that must be visible.
    pub load_failed: AtomicBool,
    /// issue #74 D.1.4: set alongside `load_failed` ONLY by the two `NATIVE_LOAD_BUDGET` expiry
    /// arms in `pump.rs` — never by a synchronous Load refusal (`sf_load` returning 0) or a
    /// `type=18` pipeline refusal, both of which set `load_failed` alone. This is what lets
    /// `error_shape` tell a k5lp-style hang apart from an ordinary firmware refusal on the wire,
    /// as `FailureKind::LoadTimeout` rather than the generic `FailureKind::TvPipeline`.
    pub load_timed_out: AtomicBool,
    /// The pipeline took the video decoder back from an ESTABLISHED session: `type=18 num=600`
    /// (`"VDEC resource is released by policy action"`) after a picture. Measured on the dev set
    /// (webOS 4, 2026-10-13): a direct play left paused until the screensaver came on got this
    /// with NO `LIFECYCLE: background`, and every later Play was refused for good. Set on the
    /// callback thread, taken once by the app loop ([`super::take_decoder_revoked`]), which parks
    /// the session exactly as an app switch does and rebuilds it on the viewer's next press.
    pub decoder_revoked: AtomicBool,
    /// issue #74 D.1: the epoch this session last logged "native: loadCompleted while Load in
    /// flight — deferring" for — 0 = never. `epoch` is monotonic and never 0 for a real session
    /// ([`NativeSessionState::next`] skips 0 on wraparound), so comparing against this is enough
    /// to log the deferral exactly once per session without needing an `Engine` field: the pump's
    /// `loadCompleted` arm can be entered many times while `Load` is in flight, and only the
    /// first should log.
    pub native_load_deferred_logged_epoch: AtomicU32,
    /// issue #74 D.1: the elapsed milliseconds `mark_native_load_returned` measured at the moment
    /// it flipped `load_call` to `Returned` — captured there (by `threads::load_thread`) rather
    /// than re-derived on a later pump tick, since a later tick would over-count by however long
    /// the pump took to run again. `pump.rs` reads this once, for the "native: Load returned
    /// after Nms" log, exactly when it observes the deferral epoch marker above matching its own.
    pub native_load_elapsed_ms: AtomicU64,

    /// **The container's own per-track names** (D -> M): the only authority on what a track is
    /// called, since a playlist describes a stream, never its tracks.
    ///
    /// `audio` in FILE order, so position N is the N-th audio stream - the ordinal a track-menu
    /// row selects. Empty strings for a file that tags nothing, and an
    /// empty Vec until a demuxer has opened. See `ff::stream_name`.
    pub track_names: Mutex<TrackNames>,
    /// The file's chapters (D -> M), published beside [`Self::track_names`] and cleared with them.
    pub chapters: Mutex<Vec<Chapter>>,
    /// The demux thread is inside `av_seek_frame` (D -> M). A Matroska file's FIRST seek reads its
    /// Cues (at the END of the file, ~102 GB into a UHD remux) and seeks back: three range reopens
    /// before the call returns, seconds on a slow debrid host. The pump's stuck watchdog does not
    /// count that time — retrying or reloading into it only aborts a reopen that was landing.
    pub demux_seeking: AtomicBool,
    /// Running estimate of one range reopen's latency on this playback's server, in ms (0 = no
    /// sample yet). Written by `ff::seek_cb` on the demux thread only; the pump scales the stuck
    /// watchdog by it (`pump::seek_stuck_ms`). Kept across a reload: same server.
    pub reopen_ms_est: AtomicU32,

    // demux (D) -> main (M)
    pub file_size: AtomicI64, // g_file_size
    /// The DECODED FRAME SIZE, published by the demuxer once the video stream is known.
    ///
    /// Exists for the webOS 5+ exported window, whose `SDL_webOSSetExportedWindow(id, src, dst)`
    /// wants the frame you are FEEDING as `src` and the on-screen rect as `dst` — the pair is what
    /// expresses scaling. webOS 4's `AcbAPI_setDisplayWindow` takes only a destination, so nothing
    /// needed this before and the exported path was written passing the authoring canvas (1920x1080)
    /// for both. That is wrong for every 4K direct play, which is most of them.
    ///
    /// 0 until the demuxer has opened the stream; readers must treat 0 as "not known yet".
    pub video_w: AtomicI32,
    pub video_h: AtomicI32,
    /// Coherent `{w,h}` publication for actuator consumers. The individual fields above remain
    /// diagnostic mirrors; reading them independently can manufacture a raster no stream owned.
    video_raster: AtomicU64,
    /// Coherent display aspect from sourceInfo, including non-square pixels. Zero
    /// until reported; overlays temporarily fall back to the demuxer's coded raster.
    pub video_aspect: AtomicU64,
    /// Decoder-reported source frame rate in thousandths of a frame per second. 0 means the
    /// sourceInfo callback has not supplied one; unlike `FRAMEREADY`, this is stream metadata and
    /// therefore does not mistake the firmware's ~5 Hz position tick for video cadence.
    pub video_fps_milli: AtomicI64,
    pub duration_ns: AtomicI64, // was g_mkv.duration_ns (published)
    /// Latest normalized timestamps successfully enqueued for each elementary stream, in movie
    /// PTS (consumers apply the display-base rule). These are content-time facts (not queue byte
    /// counts); `-1` means the lane has not produced an AU in this session/seek.
    pub video_tail_ns: AtomicI64,
    pub audio_tail_ns: AtomicI64,
    /// One synchronized authority for native Pause/Play issuance. FFI is called outside this
    /// mutex through a reserve/complete token protocol.
    clock: Mutex<ClockState>,
    // set once the pipeline has drained to true end-of-stream (EOS pushed AND the last fed frame
    // has been presented). app.rs polls player::ended() to tear the player down at the credits.
    pub ended: AtomicBool,

    // close-to-interrupt handle: raw ptr to the Engine-owned HttpStream box, so
    // the pump/teardown can close(fd) to unblock a blocked recv. The box outlives
    // the worker threads (Engine drops after join), so the ptr stays valid.
    pub hs_ptr: AtomicPtr<HttpStream>,

    // ---- diagnostics mirror (`app::diagnostics`) -------------------------------------------------
    // Values the render path (and the opted-in handled-error snapshot) need that live on the
    // Engine, republished from the pump.
    // The render path holds no `&mut PlayerAdapter` and so cannot reach the `Engine` at all (when
    // the slot was a `static mut` it could, and a second live borrow was instant UB) — so it reads
    // these instead.
    //
    // STRICTLY ONE-WAY: written by the pump and seam, read only for observation. Nothing in the
    // playback state machine may ever branch on one, or a diagnostic becomes load-bearing.
    /// `Stage` as u8 — where the bind/play sequence has got to. Unlike the expensive queue
    /// mirrors below, this scalar is always current enough for a terminal error report even when
    /// Stats for Nerds is closed.
    pub dg_stage: AtomicU8,
    /// Starfish callbacks seen this session. A count of 0 with a completed Load is the pipeline
    /// never speaking to us — the sharpest single symptom there is. The TYPE of the last callback
    /// is deliberately not kept: the numbering shifts between webOS 4 and 5+ above 0x1c, so a
    /// displayed type would be a confident lie on the firmware we cannot test. `dg_cb_err` latches
    /// the one type that means the same on both.
    pub dg_cb_count: AtomicU32,
    /// Why the VIDEO feeder is where it is, taken at each of `feed_stream`'s exit points.
    /// 0 none yet · 1 accepting · 2 BufferFull · 3 refused · 4 waiting for a frame (feed-ahead
    /// throttle) · 5 queue empty.
    ///
    /// Supersedes a bare last-reply byte, which says strictly less: "BufferFull" and "the throttle
    /// held us back" and "there was nothing to send" are three different faults with three
    /// different fixes, and a reply byte cannot tell them apart because the last two never reach a
    /// `Feed()` call at all. `queue empty` vs `BufferFull` is what splits a dead PRODUCER from a
    /// dead SINK.
    pub dg_feed_state: AtomicU8,
    /// The first Starfish error callback (`ty == 18`) of this session, and the callback INDEX it
    /// arrived at. Sticky: a later healthy callback must not erase the error that explains the
    /// session, and the index says whether it was immediate or after a long healthy run.
    pub dg_cb_err: AtomicI32,
    pub dg_cb_err_at: AtomicU32,
    /// The firmware's own error NUMBER (`num`) of that same first `ty == 18` callback — the
    /// `601` of `type=18 num=601 Resource Allocation Error`. Latched together with `dg_cb_err`
    /// (first one wins, so a later callback cannot overwrite the explanation) and cleared where
    /// that clears. Already clamped to the `i32` range: it is the one number a handled `tv_pipeline`
    /// report carries about the refusal, never the callback's string. A SYNCHRONOUS Load refusal
    /// (`sf_load` returning 0) has no number to latch — Starfish's `Load` hands back a bare
    /// success flag, and the other refusals inside `sf_load` are the seam's own lifecycle gates —
    /// so a session that failed that way leaves this at 0 ("none recorded").
    pub dg_cb_err_num: AtomicI32,
    /// The media GET's HTTP status and the bytes actually delivered to the demuxer. Written by the
    /// DEMUX thread — the third writer of this block. 0/0 = no connection was ever made, which is
    /// a different failure from a connection that answered 401.
    pub dg_http_status: AtomicI32,
    pub dg_net_rx: AtomicI64,
    /// WHY the media transfer died, when it was a curl transfer dying mid-body (issue #504): the
    /// libcurl code (0 = none recorded), the transfer's age in ms (-1 = unknown) and whether the
    /// reopen that follows a failed read was refused as well. Written by the demux thread beside
    /// `demux_io_failed` (`ff::frame_read_failed`), read by the handled playback error as a
    /// class and an age bucket — never as these numbers.
    pub dg_transport_rc: AtomicI32,
    pub dg_transport_age_ms: AtomicI64,
    pub dg_transport_reopen_failed: AtomicBool,
    /// SDL ticks when `loadCompleted` landed, and when `frames` last CHANGED. A photograph has no
    /// time axis: "Load completed, 0 frames" is innocent at 2 s and damning at 4 minutes, and the
    /// panel cannot tell the difference without these. Stamped in the pump, never in `app::diagnostics` —
    /// a stats-local timer would start when the panel is OPENED, so a four-minute hang would
    /// photograph as twelve seconds.
    pub dg_load_at: AtomicU32,
    pub dg_frame_at: AtomicU32,
    /// **The video plane's own cadence** — the three counters behind the heartbeat's `vtick=`/`vgap=`.
    ///
    /// They exist because every other frame number in this app is about the GRAPHICS plane. `fps=`
    /// counts our GL swaps and `worstframe=` times our own draw, and both stay a flat 60/0.5 ms
    /// through playback that visibly stutters: the decoded picture is composited by the TV on a
    /// plane we never touch. The one place the pipeline tells us it put a frame on that plane is
    /// the `ty == 0` callback, so that is where these are stamped.
    ///
    /// Written on the LIBRARY thread (`sf_on_event`), drained on the main thread
    /// ([`crate::player::vplane_take`]) — hence a drained COUNT rather than a delta of
    /// [`frames`](Self::frames), which a seek resets underneath a reader.
    ///
    /// `dg_vpres_at` is the previous presentation's stamp and MUST be zeroed wherever `frames` is
    /// (session reset, and the post-seek re-count in the pump): the pipeline legitimately shows
    /// nothing across a seek, and a gap measured over that pause is the harness reporting the seek
    /// as a stutter.
    pub dg_vpres_ct: AtomicU32,
    pub dg_vpres_at: AtomicU32,
    pub dg_vpres_gap: AtomicU32,
    /// Bytes queued in each AU lane at the last tick, against `engine::aq_caps`.
    pub dg_aq_video: AtomicI64,
    pub dg_aq_audio: AtomicI64,
    /// High-water fed PTS per lane. Their DIFFERENCE is the sharpest instantaneous answer to
    /// "video plays but there is no sound": a snapshot of the fed COUNTS cannot see a lane that
    /// stopped advancing, because a large total stays large. A skew that grows without bound is
    /// the audio lane starving; a skew near zero says both lanes are keeping up and the fault is
    /// downstream of us.
    pub dg_fed_v_pts: AtomicI64,
    pub dg_fed_a_pts: AtomicI64,
    /// The codec strings this session actually put in the Starfish Load payload, as small codes
    /// (0 = none/not built; video: `engine::LoadVideo::diag_code`, audio: `stream::AudioFeed::payload`).
    ///
    /// Codes rather than strings so they need no lock on the render path — and the PAYLOAD's view
    /// rather than a re-derivation, because the whole class of bug here is the payload disagreeing
    /// with the stream. `dg_load_a == 0` is `needAudio:false`, which is a complete answer to
    /// "there is no sound": the pipeline was never asked for any.
    pub dg_load_v: AtomicU8,
    pub dg_load_a: AtomicU8,
    /// The Load declared a dual-layer Dolby Vision stream as its base layer alone, so the demuxer
    /// drops its RPU and enhancement-layer NAL units (`ff::strip_dv_nals`). Written at every Load.
    pub dv_strip_layers: AtomicBool,
    /// `vp_place` return, and the size it was called with. `i32::MIN` = never called.
    pub dg_place_rv: AtomicI32,
    pub dg_placed_w: AtomicI32,
    pub dg_placed_h: AtomicI32,
}

impl Shared {
    /// The container's track names ([`Shared::track_names`]) for a READER on the main thread. A
    /// panicked writer cannot leave the names half-built (they are replaced whole), so a poisoned
    /// lock still holds a usable value and the reader takes it rather than panic.
    pub fn track_names_lenient(&self) -> std::sync::MutexGuard<'_, TrackNames> {
        self.track_names.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub const fn new() -> Self {
        Shared {
            native_session: Mutex::new(NativeSessionState::new()),
            dg_stage: AtomicU8::new(0),
            dg_cb_count: AtomicU32::new(0),
            dg_feed_state: AtomicU8::new(0),
            dg_cb_err: AtomicI32::new(0),
            dg_cb_err_at: AtomicU32::new(0),
            dg_cb_err_num: AtomicI32::new(0),
            dg_http_status: AtomicI32::new(0),
            dg_transport_rc: AtomicI32::new(0),
            dg_transport_age_ms: AtomicI64::new(-1),
            dg_transport_reopen_failed: AtomicBool::new(false),
            dg_net_rx: AtomicI64::new(0),
            dg_load_at: AtomicU32::new(0),
            dg_frame_at: AtomicU32::new(0),
            dg_vpres_ct: AtomicU32::new(0),
            dg_vpres_at: AtomicU32::new(0),
            dg_vpres_gap: AtomicU32::new(0),
            dg_aq_video: AtomicI64::new(0),
            dg_aq_audio: AtomicI64::new(0),
            dg_fed_v_pts: AtomicI64::new(0),
            dg_fed_a_pts: AtomicI64::new(0),
            dg_load_v: AtomicU8::new(0),
            dg_load_a: AtomicU8::new(0),
            dv_strip_layers: AtomicBool::new(false),
            dg_place_rv: AtomicI32::new(i32::MIN),
            dg_placed_w: AtomicI32::new(0),
            dg_placed_h: AtomicI32::new(0),
            playpos_ns: AtomicI64::new(0),
            pres_fed: AtomicI64::new(0),
            frames: AtomicI32::new(0),
            seen_frame: AtomicBool::new(false),
            first_audio_pts: AtomicI64::new(i64::MIN),
            audio_switch: AtomicU8::new(AudioSwitch::None as u8),
            audio_pick_ms: AtomicI64::new(0),
            load_completed: AtomicBool::new(false),
            media_id: Mutex::new(None),
            source_info: Mutex::new(None),
            container_hdr: Mutex::new(None),
            pts_shift: AtomicI64::new(0),
            disp_base: AtomicI64::new(0),
            hls_vod_base_ns: AtomicI64::new(-1),
            seek_to_ns: AtomicI64::new(-1),
            seek_target_ns: AtomicI64::new(-1),
            seeking: AtomicBool::new(false),
            seek_display_ns: AtomicI64::new(-1),
            desired_audio_idx: AtomicI32::new(-1),
            pb_state: AtomicU8::new(PlaybackState::Idle as u8),
            demux_failed: AtomicBool::new(false),
            demux_io_failed: AtomicBool::new(false),
            demux_no_video: AtomicBool::new(false),
            load_failed: AtomicBool::new(false),
            load_timed_out: AtomicBool::new(false),
            decoder_revoked: AtomicBool::new(false),
            native_load_deferred_logged_epoch: AtomicU32::new(0),
            native_load_elapsed_ms: AtomicU64::new(0),
            track_names: Mutex::new(TrackNames::new()),
            chapters: Mutex::new(Vec::new()),
            demux_seeking: AtomicBool::new(false),
            reopen_ms_est: AtomicU32::new(0),
            file_size: AtomicI64::new(0),
            video_w: AtomicI32::new(0),
            video_h: AtomicI32::new(0),
            video_raster: AtomicU64::new(0),
            video_aspect: AtomicU64::new(0),
            video_fps_milli: AtomicI64::new(0),
            duration_ns: AtomicI64::new(0),
            video_tail_ns: AtomicI64::new(-1),
            audio_tail_ns: AtomicI64::new(-1),
            clock: Mutex::new(ClockState::new()),
            ended: AtomicBool::new(false),
            hs_ptr: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    /// Enter the only state in which a Starfish `Load` may publish callbacks.
    ///
    /// `None` rejects an overlapping native session instead of silently retagging two live
    /// objects with one process-global owner.
    pub fn begin_native_session(&self) -> Option<u32> {
        let mut state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state.phase != NativeSessionPhase::Idle {
            return None;
        }
        let epoch = state.next();
        state.phase = NativeSessionPhase::Active {
            epoch,
            presentation_gate: NativePresentationGate::Armed,
            load_call: LoadCall::InFlight,
            load_issued_at: Instant::now(),
        };
        Some(epoch)
    }

    /// Mark `epoch`'s Starfish `Load` call as returned (issue #74 D.1) — called by
    /// `threads::load_thread` immediately after the real, blocking `sf_load` call comes back.
    /// Returns the elapsed time since the session's
    /// Load was issued, for the "native: Load returned after Nms" log — `None` when `epoch` no
    /// longer owns the `Active` phase (a superseded/already-retired session; nothing to mark).
    pub fn mark_native_load_returned(&self, epoch: u32) -> Option<Duration> {
        let mut state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match &mut state.phase {
            NativeSessionPhase::Active {
                epoch: active,
                load_call,
                load_issued_at,
                ..
            } if epoch != 0 && *active == epoch => {
                *load_call = LoadCall::Returned {
                    at: Instant::now(),
                };
                Some(load_issued_at.elapsed())
            }
            _ => None,
        }
    }

    /// Publish a synchronous refusal only while this exact epoch owns the active session.
    /// Hold the retirement/reset barrier across check and write, as callback admission does.
    pub fn publish_native_load_failure(&self, epoch: u32) -> bool {
        let state = self.native_session.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(state.phase, NativeSessionPhase::Active { epoch: active, .. } if epoch != 0 && active == epoch) {
            return false;
        }
        self.load_failed.store(true, Ordering::Release);
        true
    }

    /// Whether `epoch`'s Starfish `Load` call has returned yet. Required by `pump.rs`'s
    /// `loadCompleted` arm before it may even poll `sf_is_load_completed` (issue #74: that poll is
    /// itself a concurrent Starfish call, and on v0.6.0 it was the FIRST thing that arm did while
    /// `Load` was still executing on the load thread). Epoch-scoped exactly like
    /// `mark_native_load_returned`: a late thread from a SUPERSEDED session can never open this
    /// gate for a newer one, because only `epoch == active` can ever read `Returned` here.
    pub fn native_load_returned(&self, epoch: u32) -> bool {
        let state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        matches!(
            state.phase,
            NativeSessionPhase::Active {
                epoch: active,
                load_call: LoadCall::Returned { .. },
                ..
            } if epoch != 0 && active == epoch
        )
    }

    /// How long ago `epoch`'s Starfish `Load` call returned — `None` when `epoch` no longer owns
    /// the `Active` phase, OR when its Load has not returned yet (still `LoadCall::InFlight`,
    /// which the OTHER budget window — [`native_load_elapsed`] — covers).
    ///
    /// This is the second half of issue #74 D.1's budget (`load-returned-log-is-conditional-on-
    /// loadcompleted-and-unbounded-after`): `native_load_elapsed` is measured from
    /// `load_issued_at` and never resets, so by the time Load actually returns it may already
    /// read close to `NATIVE_LOAD_BUDGET` — reusing it to bound the WAIT FOR `loadCompleted`
    /// would leave that second wait almost no budget of its own on a Load that took a while to
    /// return. This gives that wait a fresh full window, timed from the moment Load returned.
    pub fn native_load_returned_elapsed(&self, epoch: u32) -> Option<Duration> {
        let state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match state.phase {
            NativeSessionPhase::Active {
                epoch: active,
                load_call: LoadCall::Returned { at },
                ..
            } if epoch != 0 && active == epoch => Some(at.elapsed()),
            _ => None,
        }
    }

    /// How long `epoch`'s Load has been in flight (or took, once returned), for `pump.rs`'s
    /// D.1.4 timeout — `None` once `epoch` no longer owns the `Active` phase (retired/superseded;
    /// there is nothing left to time out).
    pub fn native_load_elapsed(&self, epoch: u32) -> Option<Duration> {
        let state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match state.phase {
            NativeSessionPhase::Active {
                epoch: active,
                load_issued_at,
                ..
            } if epoch != 0 && active == epoch => Some(load_issued_at.elapsed()),
            _ => None,
        }
    }

    /// Test-only injection point for `pump.rs`'s D.1.4 budget arm (`load-budget-expiry-path-
    /// itself-has-no-test`, a fix-wave finding): rewinds `epoch`'s recorded `load_issued_at` by
    /// `by`, so `native_load_elapsed` reads at least `by` older without a real 20s wait. Returns
    /// `false` when `epoch` no longer owns the `Active` phase (nothing to backdate) — callers
    /// must treat that as a precondition failure, not a silent no-op, per this project's
    /// silent-instrument rule (AGENTS.md).
    #[cfg(all(any(test, feature = "test-support"), feature = "hostsim"))]
    pub fn test_backdate_native_load_issued(&self, epoch: u32, by: Duration) -> bool {
        let mut state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match &mut state.phase {
            NativeSessionPhase::Active {
                epoch: active,
                load_issued_at,
                ..
            } if epoch != 0 && *active == epoch => {
                *load_issued_at = load_issued_at.checked_sub(by).unwrap_or(*load_issued_at);
                true
            }
            _ => false,
        }
    }

    /// Test-only injection point for `pump.rs`'s D.1.4 budget arm, second half
    /// (`load-returned-log-is-conditional-on-loadcompleted-and-unbounded-after`): rewinds
    /// `epoch`'s recorded `LoadCall::Returned` timestamp by `by`, so `native_load_returned_elapsed`
    /// reads at least `by` older without a real wait. Requires `epoch` to already have returned
    /// (`LoadCall::Returned`) — `false` otherwise, including when it is still `InFlight` or the
    /// phase is no longer `Active`, per this project's silent-instrument rule (AGENTS.md).
    #[cfg(all(any(test, feature = "test-support"), feature = "hostsim"))]
    pub fn test_backdate_native_load_returned(&self, epoch: u32, by: Duration) -> bool {
        let mut state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match &mut state.phase {
            NativeSessionPhase::Active {
                epoch: active,
                load_call: LoadCall::Returned { at },
                ..
            } if epoch != 0 && *active == epoch => {
                *at = at.checked_sub(by).unwrap_or(*at);
                true
            }
            _ => false,
        }
    }

    /// Retire one exact native session and wait for every callback which already entered it.
    ///
    /// Ready-object teardown calls this after `Unload` has returned and the C interposer gate has
    /// closed and drained. Startup failure paths also retire the Rust epoch here: either no native
    /// object was constructed, or construction succeeded but arming the C gate failed, cleared
    /// dispatch readiness and retained the object in process-long quarantine. That latter path
    /// admitted no native callbacks and does not claim an Unload or native-gate drain. The Active
    /// arm also keeps this primitive independently testable without fabricating firmware callbacks.
    pub fn retire_native_session(&self, epoch: u32) -> bool {
        let mut state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let owns = matches!(
            state.phase,
            NativeSessionPhase::Active { epoch: active, .. }
                | NativeSessionPhase::Unloaded { epoch: active }
                | NativeSessionPhase::Abandoned { epoch: active }
                | NativeSessionPhase::Releasing { epoch: active, .. }
                if epoch != 0 && active == epoch
        );
        if !owns {
            return false;
        }
        state.phase = NativeSessionPhase::Idle;
        true
    }

    /// Hand `epoch`'s ready object to the off-frame release (see
    /// [`NativeSessionPhase::Releasing`]). Taking the lock is the drain barrier for a callback
    /// already inside the session; from here only UNLOADCOMPLETED is admitted. `false` when `epoch`
    /// does not own the `Active` phase (the caller then releases synchronously, as before).
    pub fn begin_native_release(&self, epoch: u32) -> bool {
        self.leave_active(epoch, NativeSessionPhase::Releasing { epoch, unload_completed: false })
    }

    /// Hand `epoch`'s still-in-flight Load to the abandoned-Load release (see
    /// [`NativeSessionPhase::Abandoned`]). `false` when `epoch` does not own the `Active` phase.
    pub fn abandon_native_session(&self, epoch: u32) -> bool {
        self.leave_active(epoch, NativeSessionPhase::Abandoned { epoch })
    }

    /// Move `epoch`'s `Active` phase to `to`; `false` (nothing changed) when `epoch` does not own it.
    fn leave_active(&self, epoch: u32, to: NativeSessionPhase) -> bool {
        let mut state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !matches!(state.phase, NativeSessionPhase::Active { epoch: active, .. } if epoch != 0 && active == epoch)
        {
            return false;
        }
        state.phase = to;
        true
    }

    /// Whether a timed-out Load is parked ([`NativeSessionPhase::Abandoned`]): the native object
    /// is still owned by a `Load` that has not returned, so every native start is refused.
    pub fn native_load_abandoned(&self) -> bool {
        matches!(
            self.native_session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .phase,
            NativeSessionPhase::Abandoned { .. }
        )
    }

    /// Test-only: drop an abandoned phase that a failing test left behind, which
    /// [`reset_session`](Self::reset_session) deliberately preserves.
    #[cfg(all(any(test, feature = "test-support"), feature = "hostsim"))]
    pub fn test_force_native_idle(&self) {
        self.native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .phase = NativeSessionPhase::Idle;
    }

    /// Whether this exact object crossed firmware's synchronous unload-complete callback path.
    /// This is one mandatory lifecycle assertion, separate from the native interposer's admission
    /// and in-flight proof. A missing/renumbered callback quarantines the object rather than D1.
    pub fn native_unload_completed(&self, epoch: u32) -> bool {
        matches!(
            self.native_session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .phase,
            NativeSessionPhase::Unloaded { epoch: e }
                | NativeSessionPhase::Releasing { epoch: e, unload_completed: true }
                if e == epoch
        )
    }

    /// Fence pre-seek presentations while retaining non-frame Load/error callbacks. Locking is a
    /// drain barrier for a type-0 callback already inside the old timeline.
    pub fn begin_native_media_discontinuity(&self, epoch: u32) -> bool {
        let mut state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match &mut state.phase {
            NativeSessionPhase::Active {
                epoch: active,
                presentation_gate,
                ..
            } if epoch != 0 && *active == epoch => {
                *presentation_gate = NativePresentationGate::Disarmed;
                true
            }
            _ => false,
        }
    }

    /// Admit presentation callbacks again after the new timeline's first keyframe is identified.
    #[cfg(any(test, feature = "test-support"))]
    pub fn arm_native_presentations(&self, epoch: u32) -> bool {
        let mut state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match &mut state.phase {
            NativeSessionPhase::Active {
                epoch: active,
                presentation_gate,
                ..
            } if epoch != 0 && *active == epoch => {
                *presentation_gate = NativePresentationGate::Armed;
                true
            }
            _ => false,
        }
    }

    /// Start a two-phase callback admission around the exact post-seek keyframe Feed. No lock is
    /// held across FFI: firmware may re-enter synchronously on another release even though the
    /// audited 4.10.2 path currently dispatches through a GStreamer worker.
    pub fn begin_native_presentation_probe(&self, epoch: u32) -> bool {
        let mut state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match &mut state.phase {
            NativeSessionPhase::Active {
                epoch: active,
                presentation_gate,
                ..
            } if epoch != 0 && *active == epoch => {
                *presentation_gate = NativePresentationGate::PendingArm { latched: None };
                true
            }
            _ => false,
        }
    }

    /// Commit an accepted Feed and replay at most the newest presentation which raced its return.
    pub fn commit_native_presentation_probe(
        &self,
        epoch: u32,
        replay: impl FnOnce(i64),
    ) -> bool {
        let mut state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let NativeSessionPhase::Active {
            epoch: active,
            presentation_gate,
            ..
        } = &mut state.phase
        else {
            return false;
        };
        if epoch == 0 || *active != epoch {
            return false;
        }
        let NativePresentationGate::PendingArm { latched } = *presentation_gate else {
            return false;
        };
        *presentation_gate = NativePresentationGate::Armed;
        if let Some(num) = latched {
            replay(num);
        }
        true
    }

    /// Discard callbacks observed while a BufferFull/error Feed was outstanding and keep the old
    /// timeline fenced. The retained AU will open a fresh probe on its next attempt.
    pub fn reject_native_presentation_probe(&self, epoch: u32) -> bool {
        let mut state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match &mut state.phase {
            NativeSessionPhase::Active {
                epoch: active,
                presentation_gate,
                ..
            } if epoch != 0
                && *active == epoch
                && matches!(presentation_gate, NativePresentationGate::PendingArm { .. }) =>
            {
                *presentation_gate = NativePresentationGate::Disarmed;
                true
            }
            _ => false,
        }
    }

    /// Run an event only while its firmware-provided callback context owns this session.
    pub fn with_native_session<R>(
        &self,
        epoch: u32,
        class: NativeEventClass,
        num: i64,
        event: impl FnOnce() -> R,
    ) -> Option<R> {
        self.with_native_session_scoped(epoch, class, num, |_| event())
    }

    /// [`with_native_session`](Self::with_native_session), telling the event whose state it may
    /// write: [`EventScope::Live`] for the session that owns the process-long diagnostics,
    /// [`EventScope::Release`] for the lifecycle evidence of an object already handed to a release
    /// (the main thread reset that state for the NEXT session before this callback arrived).
    pub fn with_native_session_scoped<R>(
        &self,
        epoch: u32,
        class: NativeEventClass,
        num: i64,
        event: impl FnOnce(EventScope) -> R,
    ) -> Option<R> {
        let mut state = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let NativeSessionPhase::Abandoned { epoch: abandoned } = state.phase {
            // The release of an abandoned object needs its own UNLOADCOMPLETED evidence; nothing
            // else from that object may reach the process-long state a later session will own.
            if epoch == 0 || abandoned != epoch || class != NativeEventClass::UnloadCompleted {
                return None;
            }
            let result = event(EventScope::Release);
            state.phase = NativeSessionPhase::Unloaded { epoch };
            return Some(result);
        }
        if let NativeSessionPhase::Releasing { epoch: releasing, unload_completed } = &mut state.phase {
            // The same rule for an object a real stop handed to the off-frame release: the main
            // thread has already reset the state this callback would write into for the NEXT
            // session, so only the release's own lifecycle evidence gets through.
            if epoch == 0 || *releasing != epoch || class != NativeEventClass::UnloadCompleted {
                return None;
            }
            let result = event(EventScope::Release);
            *unload_completed = true;
            return Some(result);
        }
        let NativeSessionPhase::Active {
            epoch: active,
            presentation_gate,
            ..
        } = &mut state.phase
        else {
            return None;
        };
        if epoch == 0 || *active != epoch {
            return None;
        }
        if class == NativeEventClass::Presentation {
            match presentation_gate {
                NativePresentationGate::Armed => {}
                NativePresentationGate::Disarmed => return None,
                NativePresentationGate::PendingArm { latched } => {
                    *latched = Some(num);
                    return None;
                }
            }
        }
        let result = event(EventScope::Live);
        if class == NativeEventClass::UnloadCompleted {
            state.phase = NativeSessionPhase::Unloaded { epoch };
        }
        Some(result)
    }

    pub fn publish_video_raster(&self, width: i32, height: i32) {
        let packed = (u64::from(width as u32) << 32) | u64::from(height as u32);
        self.video_w.store(width, Ordering::Relaxed);
        self.video_h.store(height, Ordering::Relaxed);
        self.video_raster.store(packed, Ordering::Release);
    }

    pub fn video_raster(&self) -> (i32, i32) {
        let packed = self.video_raster.load(Ordering::Acquire);
        ((packed >> 32) as u32 as i32, packed as u32 as i32)
    }

    /// [`Self::reset_session`] for a RELOAD of the same item — everything a fresh Load must not
    /// inherit is cleared; the file's duration survives. The demuxer re-publishes duration when it reopens, a few hundred ms
    /// later, and until then a zero here is a playbar drawn at position ÷ 0 (`engine::teardown`
    /// has the account).
    pub fn reset_session_for_reload(&self) {
        let duration_ns = self.duration_ns.load(Ordering::Relaxed);
        self.reset_session_inner(true);
        self.duration_ns.store(duration_ns, Ordering::Relaxed);
    }

    /// Everything here describes one engine's session and must not outlive it.
    pub fn reset_session(&self) {
        self.reset_session_inner(false);
    }

    fn reset_session_inner(&self, for_reload: bool) {
        // This lock is held through the whole reset. A callback that entered before retirement
        // finishes first; one arriving afterwards sees no owner. It can therefore never validate
        // session A and then publish into the zeroed/reused storage of session B.
        let mut native = self
            .native_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // An ABANDONED native object is not this engine's session: it outlives the engine by
        // design until its `Load` returns and `engine::reap_abandoned_load` releases it. Clearing
        // it here would let the next session begin while the C seam still owns that object.
        // The same holds for an object handed to the off-frame release: its Unload is still running
        // and the release retires the phase itself once it has the evidence.
        if !matches!(
            native.phase,
            NativeSessionPhase::Abandoned { .. } | NativeSessionPhase::Releasing { .. }
        ) {
            native.phase = NativeSessionPhase::Idle;
        }
        // the diagnostics mirror is per-session too — a stale bind outcome from the last item is
        // exactly the misleading answer the read-out exists to avoid
        self.dg_stage.store(0, Ordering::Relaxed);
        self.dg_cb_count.store(0, Ordering::Relaxed);
        self.dg_feed_state.store(0, Ordering::Relaxed);
        // A latched error MUST be cleared here, or one bad session paints every later healthy
        // playback red for the life of the process.
        self.dg_cb_err.store(0, Ordering::Relaxed);
        self.dg_cb_err_at.store(0, Ordering::Relaxed);
        self.dg_cb_err_num.store(0, Ordering::Relaxed);
        self.dg_http_status.store(0, Ordering::Relaxed);
        self.dg_transport_rc.store(0, Ordering::Relaxed);
        self.dg_transport_age_ms.store(-1, Ordering::Relaxed);
        self.dg_transport_reopen_failed.store(false, Ordering::Relaxed);
        self.dg_net_rx.store(0, Ordering::Relaxed);
        self.dg_load_at.store(0, Ordering::Relaxed);
        self.dg_frame_at.store(0, Ordering::Relaxed);
        self.dg_vpres_ct.store(0, Ordering::Relaxed);
        self.dg_vpres_at.store(0, Ordering::Relaxed);
        self.dg_vpres_gap.store(0, Ordering::Relaxed);
        self.dg_aq_video.store(0, Ordering::Relaxed);
        self.dg_aq_audio.store(0, Ordering::Relaxed);
        self.dg_fed_v_pts.store(0, Ordering::Relaxed);
        self.dg_fed_a_pts.store(0, Ordering::Relaxed);
        self.dg_load_v.store(0, Ordering::Relaxed);
        self.dg_load_a.store(0, Ordering::Relaxed);
        self.dg_place_rv.store(i32::MIN, Ordering::Relaxed);
        self.dg_placed_w.store(0, Ordering::Relaxed);
        self.dg_placed_h.store(0, Ordering::Relaxed);
        self.playpos_ns.store(0, Ordering::Relaxed);
        self.pres_fed.store(0, Ordering::Relaxed);
        self.frames.store(0, Ordering::Relaxed);
        // a fresh session has shown nothing yet — keep this adjacent to `frames` in all three
        // places (declaration, `new`, here): a reload that forgot the bit would silently suppress
        // the centred read-out for the rest of the app's life.
        self.seen_frame.store(false, Ordering::Relaxed);
        self.first_audio_pts.store(i64::MIN, Ordering::Relaxed);
        // an audio pick's rebuild has begun (a reload keeps the pick's meaning for this session);
        // any other reset ends it
        let next = match AudioSwitch::from_u8(self.audio_switch.load(Ordering::Relaxed)) {
            AudioSwitch::Picked | AudioSwitch::Reloading if for_reload => AudioSwitch::Reloading,
            _ => AudioSwitch::None,
        };
        self.audio_switch.store(next as u8, Ordering::Relaxed);
        self.load_completed.store(false, Ordering::Relaxed);
        // issue #74 D.1: both are only ever cleared by the loadCompleted arm that consumes them
        // (pump.rs), so a session torn down after logging the deferral but before that arm runs
        // would otherwise leave them set — a later session's "native: Load returned after Nms"
        // could then report a PREVIOUS session's duration, or skip its own deferral log because
        // the epoch marker still matched. Clear both here, unconditionally, like every other
        // per-session diagnostic above.
        self.native_load_deferred_logged_epoch
            .store(0, Ordering::Relaxed);
        self.native_load_elapsed_ms.store(0, Ordering::Relaxed);
        *self.media_id.lock().unwrap() = None;
        *self.source_info.lock().unwrap() = None;
        *self.container_hdr.lock().unwrap() = None;
        self.pts_shift.store(0, Ordering::Relaxed);
        self.disp_base.store(0, Ordering::Relaxed);
        self.hls_vod_base_ns.store(-1, Ordering::Relaxed);
        self.seek_to_ns.store(-1, Ordering::Relaxed);
        self.seek_target_ns.store(-1, Ordering::Relaxed);
        self.seeking.store(false, Ordering::Relaxed);
        self.seek_display_ns.store(-1, Ordering::Relaxed);
        // NB: desired_audio_idx is NOT reset here — it persists across seeks/reloads so a
        // native audio-track choice survives seeking. It is reset on a new item.
        self.pb_state
            .store(PlaybackState::Idle as u8, Ordering::Relaxed);
        self.demux_failed.store(false, Ordering::Relaxed);
        self.demux_io_failed.store(false, Ordering::Relaxed);
        self.demux_no_video.store(false, Ordering::Relaxed);
        self.load_failed.store(false, Ordering::Relaxed);
        self.load_timed_out.store(false, Ordering::Relaxed);
        self.decoder_revoked.store(false, Ordering::Relaxed);
        // The track names describe the FILE the last demuxer had open. A reload-based seek
        // re-opens the same stream and re-publishes the same names, but a reload onto another
        // stream would otherwise leave the old file's names under its track list.
        *self.track_names.lock().unwrap() = TrackNames::new();
        self.chapters.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.demux_seeking.store(false, Ordering::Relaxed);
        if !for_reload {
            self.reopen_ms_est.store(0, Ordering::Relaxed);
        }
        self.file_size.store(0, Ordering::Relaxed);
        self.video_w.store(0, Ordering::Relaxed);
        self.video_h.store(0, Ordering::Relaxed);
        self.video_raster.store(0, Ordering::Release);
        self.video_aspect.store(0, Ordering::Release);
        self.video_fps_milli.store(0, Ordering::Relaxed);
        self.duration_ns.store(0, Ordering::Relaxed);
        self.video_tail_ns.store(-1, Ordering::Relaxed);
        self.audio_tail_ns.store(-1, Ordering::Relaxed);
        self.reset_clock();
        self.ended.store(false, Ordering::Relaxed);
        self.hs_ptr.store(std::ptr::null_mut(), Ordering::Release);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn reset_clock_for_test(&self) {
        self.reset_clock();
    }

    /// Put a newly constructed native pipeline into its real pre-Play state. This happens before
    /// either worker is spawned, so no user command can mistake "loaded but not started" for a
    /// running clock. `user_held` is the durable viewer intent carried across an engine reload;
    /// `seek_preroll` means that intent is temporarily allowed to decode one frame.
    pub fn arm_initial_clock_hold(&self, user_held: bool, seek_preroll: bool) -> bool {
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        if clock.phase != ClockPhase::Running {
            return false;
        }
        clock.phase = ClockPhase::Held { hold: ClockHold::Initial };
        clock.user_held = user_held && !seek_preroll;
        clock.resume_acb_pending = user_held && seek_preroll;
        true
    }

    /// A start that armed the initial hold but failed before installing an Engine owns no native
    /// clock. Return to the no-session state so retrying the same item cannot inherit `Held`
    /// forever.
    pub fn cancel_clock_session_start(&self) {
        self.reset_clock();
    }

    #[cfg(all(any(test, feature = "test-support"), feature = "hostsim"))]
    pub fn ensure_initial_clock_hold_for_test(&self) {
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        if clock.phase == ClockPhase::Running {
            clock.phase = ClockPhase::Held { hold: ClockHold::Initial };
        }
    }

    /// Reserve a viewer Pause. If a seek or initial hold already owns a physical hold, only the
    /// orthogonal user hold is added and no redundant native Pause is issued.
    pub fn prepare_user_pause(&self) -> Option<UserPause> {
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        if clock.phase == ClockPhase::Stopping {
            return None;
        }
        if clock.user_held {
            return Some(UserPause::AlreadyHeld);
        }
        if matches!(clock.phase, ClockPhase::Held { .. }) {
            clock.user_held = true;
            clock.resume_acb_pending = false;
            return Some(UserPause::AlreadyHeld);
        }
        if clock.phase != ClockPhase::Running {
            return None;
        }
        let token = clock.token();
        clock.phase = ClockPhase::PauseIssuing { token, hold: ClockHold::User };
        Some(UserPause::Issue(token))
    }

    pub fn complete_user_pause(&self, token: u64, accepted: bool) -> PauseCompletion {
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        if clock.phase != (ClockPhase::PauseIssuing { token, hold: ClockHold::User }) {
            return PauseCompletion::Stale;
        }
        if accepted {
            clock.phase = ClockPhase::Held { hold: ClockHold::User };
            clock.user_held = true;
            clock.resume_acb_pending = false;
            PauseCompletion::Accepted
        } else {
            clock.phase = ClockPhase::Running;
            PauseCompletion::Refused
        }
    }

    /// Reserve the native Pause that makes an in-place seek safe. A seek requested from an
    /// existing user hold reuses that physical Pause, but temporarily transfers ownership to the
    /// seek prime so exactly one landed frame may be decoded before the user hold is restored.
    pub fn prepare_seek_pause(&self) -> Option<SeekPause> {
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        match clock.phase {
            ClockPhase::Running => {
                let token = clock.token();
                clock.phase = ClockPhase::PauseIssuing { token, hold: ClockHold::Seek };
                Some(SeekPause::Issue(token))
            }
            ClockPhase::Held { .. } => {
                if clock.user_held {
                    clock.user_held = false;
                    clock.resume_acb_pending = true;
                }
                clock.phase = ClockPhase::Held { hold: ClockHold::Seek };
                Some(SeekPause::AlreadyHeld)
            }
            ClockPhase::PauseIssuing { .. } | ClockPhase::PlayIssuing { .. } | ClockPhase::Stopping => None,
        }
    }

    /// A viewer Pause that lands on a clock a seek or an Initial hold already owns marks it
    /// `user_held`, and `reserve_prime_play` refuses a Play on a user-held clock: the keyframe
    /// would be fed through the preroll's open gate and never be allowed to present. When the pump
    /// opens a preroll for such a Pause it hands the hold back to the seek prime exactly as
    /// [`Self::prepare_seek_pause`] does for a seek dispatched from a user hold. `false` when the
    /// clock is not held (transitioning or running): the caller tries again next tick.
    pub fn hand_hold_to_seek_prime(&self) -> bool {
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        let ClockPhase::Held { .. } = clock.phase else {
            return false;
        };
        if clock.user_held {
            clock.user_held = false;
            clock.resume_acb_pending = true;
        }
        clock.phase = ClockPhase::Held { hold: ClockHold::Seek };
        true
    }

    pub fn complete_seek_pause(&self, token: u64, accepted: bool) -> PauseCompletion {
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        if clock.phase != (ClockPhase::PauseIssuing { token, hold: ClockHold::Seek }) {
            return PauseCompletion::Stale;
        }
        if accepted {
            clock.phase = ClockPhase::Held { hold: ClockHold::Seek };
            PauseCompletion::Accepted
        } else {
            clock.phase = ClockPhase::Running;
            PauseCompletion::Refused
        }
    }

    /// Accept the viewer's Resume intent. Queued streams transfer an ordinary user hold to an
    /// explicit resume-prime hold: the caller opens feeding, while `try_prime` remains the sole
    /// owner of the eventual Play + ACB Resume. A seek or initial hold already holding the clock
    /// keeps it and only records the deferred ACB half.
    pub fn prepare_user_resume(&self, queued_stream: bool) -> Option<UserResume> {
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        if clock.phase == ClockPhase::Stopping || !clock.user_held {
            return None;
        }
        let ClockPhase::Held { hold } = clock.phase else {
            return None;
        };
        if hold != ClockHold::User {
            clock.user_held = false;
            clock.resume_acb_pending = true;
            return Some(UserResume::Deferred);
        }
        if queued_stream {
            clock.phase = ClockPhase::Held { hold: ClockHold::ResumePrime };
            clock.user_held = false;
            clock.resume_acb_pending = true;
            return Some(UserResume::Prime);
        }
        let token = clock.token();
        clock.phase = ClockPhase::PlayIssuing { token, hold, resume_acb: true };
        Some(UserResume::Issue(token))
    }

    /// Which prime currently owns a held clock, if a prime may release it.
    pub fn prime_kind(&self) -> Option<PrimeKind> {
        let clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        let ClockPhase::Held { hold } = clock.phase else {
            return None;
        };
        hold.prime_kind()
    }

    /// Final check-and-reserve for `sf_play`, under the same mutex every transition takes, so
    /// there is no check/use gap between the last read and the native Play call.
    pub fn reserve_prime_play(&self, expected_kind: PrimeKind) -> Option<u64> {
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        if clock.user_held {
            return None;
        }
        let ClockPhase::Held { hold } = clock.phase else {
            return None;
        };
        if hold.prime_kind() != Some(expected_kind) {
            return None;
        }
        let token = clock.token();
        let resume_acb = clock.resume_acb_pending;
        clock.phase = ClockPhase::PlayIssuing { token, hold, resume_acb };
        Some(token)
    }

    pub fn complete_prime_play(&self, token: u64, accepted: bool) -> PlayCompletion {
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        let ClockPhase::PlayIssuing { token: active, hold, resume_acb } = clock.phase else {
            return PlayCompletion::Stale;
        };
        if active != token {
            return PlayCompletion::Stale;
        }
        if accepted {
            clock.phase = ClockPhase::Running;
            clock.user_held = false;
            clock.resume_acb_pending = false;
            PlayCompletion::Accepted { resume_acb }
        } else {
            clock.phase = ClockPhase::Held { hold };
            if hold == ClockHold::User {
                clock.user_held = true;
            }
            PlayCompletion::Refused
        }
    }

    pub fn stop_clock(&self) {
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        clock.phase = ClockPhase::Stopping;
        clock.user_held = false;
        clock.resume_acb_pending = false;
    }

    fn reset_clock(&self) {
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        // Session identity changes, token identity does not restart. A completion from the
        // retired session may still arrive after reset; preserving this counter makes it stale
        // instead of letting token 1 settle the next session's token 1.
        let next_token = clock.next_token;
        *clock = ClockState::new();
        clock.next_token = next_token;
    }
}

/// `Transport::preroll_volume` while the sink is not muted (a saved volume is 0..=100).
const PREROLL_VOLUME_NONE: i32 = i32::MIN;

/// UI-facing transport state. Main-thread-only in practice (the run loop + pump +
/// player_hud all run on M), but exposed as atomics so app.rs / player_hud.rs read
/// it with plain .load()/.store(). Replaces the #[no_mangle] transport globals.
pub struct Transport {
    pub started: AtomicBool, // bf_started
    /// A viewer's audio-track pick (`player::request_audio_track`) the pump still owes a reload
    /// for. Survives a seek reload (the pick is still owed); a stop clears it.
    pub audio_reload: AtomicBool,
    pub paused: AtomicBool,  // pl_paused
    pub resume_pend: AtomicBool, // resumePausePending
    /// The viewer remains logically paused while a seek is allowed to feed, Play one landed frame,
    /// then re-pause. Keeping this separate prevents the UI/user intent from lying during preroll.
    seek_preroll: AtomicBool,
    /// The seek preroll above was opened by the PUMP, not by a caller, and the pump closes it: the
    /// target (ns) the seek was dispatched at, `-1` when the preroll is absent or a caller's.
    /// Transport-wide on purpose: a reload (`reset_for_reload`) replaces the Engine but keeps the
    /// viewer's Pause and this ownership, so the landing is settled on whichever Engine is live.
    seek_preroll_owned: AtomicI64,
    /// The sink volume the paused preroll's Play window muted (`player::mute_for_preroll`) saved for
    /// its restore, `PREROLL_VOLUME_NONE` when the pipeline is not muted. The volume is PER PIPELINE:
    /// a stop unloads it (`reset` forgets this with it) and a reload's fresh Load starts at the
    /// default again, so only a still-live pipeline ever needs the restore
    /// (`player::restore_preroll_volume`: resume, the accepted re-pause, and the pump's per-tick sweep).
    preroll_volume: AtomicI32,
    pub hud_until: AtomicU32,  // pl_hud_until (SDL ticks)
    pub scrub_ns: AtomicI64,   // pl_scrub_ns (-1 = not scrubbing)
    pub seek_to_ns: AtomicI64, // g_seek_to_ns (UI seek request, -1 = none)
    // Seek requests received since the pump last APPLIED one. seek_to_ns only ever holds the
    // newest target, so without this counter a coalesced burst is indistinguishable from a
    // single tap after the fact — the pump reports `coalesced=` from it (see pump.rs).
    pub seek_reqs: AtomicU32,
}

impl Transport {
    pub const fn new() -> Self {
        Transport {
            started: AtomicBool::new(false),
            audio_reload: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            resume_pend: AtomicBool::new(false),
            seek_preroll: AtomicBool::new(false),
            seek_preroll_owned: AtomicI64::new(-1),
            preroll_volume: AtomicI32::new(PREROLL_VOLUME_NONE),
            hud_until: AtomicU32::new(0),
            scrub_ns: AtomicI64::new(-1),
            seek_to_ns: AtomicI64::new(-1),
            seek_reqs: AtomicU32::new(0),
        }
    }

    /// Publish a native-accepted user transport transition to the feed gate. Every production
    /// writer goes through this method; refused native transitions never enter it.
    pub fn commit_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    pub fn begin_paused_seek(&self) -> bool {
        if !self.paused.load(Ordering::Acquire) {
            return false;
        }
        self.seek_preroll.store(true, Ordering::Release);
        true
    }

    pub fn seek_preroll_active(&self) -> bool {
        self.seek_preroll.load(Ordering::Acquire)
    }

    /// Feeding is an actuator concern, not the user-visible pause intent. A paused seek is the one
    /// bounded exception: it must decode a landed frame while the transport still says Paused.
    pub fn feed_allowed(&self) -> bool {
        !self.paused.load(Ordering::Acquire) || self.seek_preroll_active()
    }

    /// Open the paused-seek preroll for a seek the PUMP dispatched under a viewer Pause, and take
    /// ownership of closing it (`pump::settle_paused_seek`). A preroll a caller already opened (the
    /// foreground restore) stays the caller's: `false`. Re-opening an owned one just retargets it
    /// (a coalesced tap). Not paused: nothing to hold, `false`.
    pub fn own_paused_seek(&self, target_ns: i64) -> bool {
        if self.seek_preroll_owned.load(Ordering::Acquire) >= 0 {
            self.seek_preroll_owned.store(target_ns.max(0), Ordering::Release);
            return true;
        }
        if self.seek_preroll_active() || !self.begin_paused_seek() {
            return false;
        }
        self.seek_preroll_owned.store(target_ns.max(0), Ordering::Release);
        true
    }

    /// The target of the preroll the pump owns, if it owns one.
    pub fn owned_paused_seek(&self) -> Option<i64> {
        let target = self.seek_preroll_owned.load(Ordering::Acquire);
        (target >= 0 && self.seek_preroll_active()).then_some(target)
    }

    /// Closing the preroll ends any ownership of it, so a failed re-pause (which leaves the preroll
    /// open) keeps the pump retrying, and every other closer (Resume, an abandoned seek, a stop)
    /// drops it without the pump re-pausing a film the viewer has since released.
    pub fn finish_seek_preroll(&self) {
        self.seek_preroll.store(false, Ordering::Release);
        self.seek_preroll_owned.store(-1, Ordering::Release);
        self.resume_pend.store(false, Ordering::Release);
    }

    /// The paused preroll muted the sink; `saved` (0..=100) is what [`take_preroll_volume`]
    /// returns. A second mute (a reload's fresh pipeline inside one open preroll) keeps the first
    /// saved value (always the pipeline default today: the volume is never read back).
    ///
    /// [`take_preroll_volume`]: Transport::take_preroll_volume
    pub fn note_preroll_muted(&self, saved: i32) {
        let _ = self.preroll_volume.compare_exchange(
            PREROLL_VOLUME_NONE,
            saved.clamp(0, 100),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Whether a mute is outstanding.
    pub fn preroll_muted(&self) -> bool {
        self.preroll_volume.load(Ordering::Acquire) != PREROLL_VOLUME_NONE
    }

    /// Take the volume to restore, clearing the mute. `None` when nothing is muted.
    pub fn take_preroll_volume(&self) -> Option<i32> {
        match self.preroll_volume.swap(PREROLL_VOLUME_NONE, Ordering::AcqRel) {
            PREROLL_VOLUME_NONE => None,
            v => Some(v),
        }
    }

    /// An engine reload is not a new playback and must not erase viewer Pause or the paused-seek
    /// handoff. It only retires engine-local transport mailboxes.
    ///
    /// **`scrub_ns` is NOT among them any more** (restructure §2.3, phase 9). The scrub preview is
    /// the `PlayerScreen`'s own `scrub.ns` and this atomic is the PUBLISHED copy of it, so a store
    /// here was a second owner writing over the first — and it was redundant besides: every seek
    /// that can cause a reload is asked for by that screen, which retires its own preview in the
    /// same step (`commit_scrub`; through phase 12 `app::playback::commit_seek` did it instead,
    /// which was the same cross-owner write one level down). The one reload nobody asks for (the
    /// OS taking the screen) delivers `PlayerScreen::transport_reset(true)` from the loop's
    /// background arm.
    pub fn reset_for_reload(&self) {
        self.started.store(false, Ordering::Relaxed);
        self.seek_to_ns.store(-1, Ordering::Relaxed);
        self.seek_reqs.store(0, Ordering::Relaxed);
    }

    /// reset on stop (mirrors the transport tail of stop_bufferfeed).
    pub fn reset(&self) {
        self.started.store(false, Ordering::Relaxed);
        self.audio_reload.store(false, Ordering::Relaxed);
        self.paused.store(false, Ordering::Release);
        self.resume_pend.store(false, Ordering::Relaxed);
        self.seek_preroll.store(false, Ordering::Relaxed);
        self.seek_preroll_owned.store(-1, Ordering::Relaxed);
        // The pipeline a mute belonged to is unloaded with the stop; the next one starts unmuted.
        self.preroll_volume.store(PREROLL_VOLUME_NONE, Ordering::Relaxed);
        // …and not `scrub_ns`, for `reset_for_reload`'s reason: a new session is a fresh
        // `PlayerScreen` (or, on an auto-advance chain, `start_playback`'s in-place reset of the
        // live one), and both publish `Scrub::IDLE` themselves.
        self.seek_to_ns.store(-1, Ordering::Relaxed);
        self.seek_reqs.store(0, Ordering::Relaxed);
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
/// **`Bound` is the ACB bind, and the webOS 5 path SKIPS it** — `VP_EXPORTED` goes
/// `Playing -> Streaming` directly, because there is no bind sequence to be in the middle of.
/// So `stage >= Bound` means "bound or later" on webOS 4 and "is Streaming" on webOS 5; a new
/// test against it has to decide which of those it wants.
pub enum Stage {
    Loading = 0,
    Playing,
    Bound,
    Streaming,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_readers_never_observe_a_pair_no_stream_published() {
        let shared = Shared::new();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..20_000 {
                    shared.publish_video_raster(1_280, 720);
                    shared.publish_video_raster(1_920, 1_080);
                }
            });
            for _ in 0..20_000 {
                assert!(matches!(
                    shared.video_raster(),
                    (0, 0) | (1_280, 720) | (1_920, 1_080)
                ));
            }
        });
    }

    #[test]
    fn initial_prime_is_an_explicit_hold_that_fences_user_resume() {
        let s = Shared::new();
        assert!(s.arm_initial_clock_hold(false, false));
        assert_eq!(s.prepare_user_pause(), Some(UserPause::AlreadyHeld));
        assert_eq!(
            s.prepare_user_resume(true),
            Some(UserResume::Deferred)
        );
        let play = s
            .reserve_prime_play(PrimeKind::Fresh)
            .expect("the media-prime lane, not user Resume, owns the first Play");
        assert_eq!(
            s.complete_prime_play(play, true),
            PlayCompletion::Accepted { resume_acb: true }
        );
    }

    #[test]
    fn foreground_play_releases_both_the_pause_mirror_and_initial_user_hold() {
        let s = Shared::new();
        assert!(s.arm_initial_clock_hold(true, false));
        assert_eq!(
            s.prepare_user_resume(true),
            Some(UserResume::Deferred),
        );
        assert!(
            s.reserve_prime_play(PrimeKind::Fresh)
                .is_some(),
            "the foreground Play event must clear the clock reducer's user hold, not only TX",
        );
    }

    #[test]
    fn a_user_pause_before_progressive_load_completed_fences_the_initial_play() {
        let s = Shared::new();
        assert!(s.arm_initial_clock_hold(false, false));
        assert_eq!(s.prepare_user_pause(), Some(UserPause::AlreadyHeld));
        assert!(
            s.reserve_prime_play(PrimeKind::Fresh)
                .is_none(),
            "the progressive load-complete fast path must not Play through viewer Pause",
        );

        assert_eq!(
            s.prepare_user_resume(true),
            Some(UserResume::Deferred)
        );
        assert!(
            s.reserve_prime_play(PrimeKind::Fresh)
                .is_some(),
            "the same initial hold must become playable after the viewer resumes",
        );
    }

    #[test]
    fn a_stream_resume_does_not_issue_play_before_feed_reopens() {
        let s = Shared::new();
        let pause = match s.prepare_user_pause() {
            Some(UserPause::Issue(token)) => token,
            other => panic!("unexpected Pause reservation: {other:?}"),
        };
        assert_eq!(
            s.complete_user_pause(pause, true),
            PauseCompletion::Accepted
        );

        assert_eq!(
            s.prepare_user_resume(true),
            Some(UserResume::Prime),
            "a queued stream must reopen feeding and earn a balanced prime before native Play",
        );
        assert_eq!(s.prime_kind(), Some(PrimeKind::Resume));
    }

    #[test]
    fn a_second_pause_during_resume_prime_fences_play_until_the_second_resume() {
        let s = Shared::new();
        let pause = match s.prepare_user_pause() {
            Some(UserPause::Issue(token)) => token,
            other => panic!("unexpected Pause reservation: {other:?}"),
        };
        assert_eq!(
            s.complete_user_pause(pause, true),
            PauseCompletion::Accepted
        );
        assert_eq!(s.prepare_user_resume(true), Some(UserResume::Prime));
        assert_eq!(s.prepare_user_pause(), Some(UserPause::AlreadyHeld));
        assert!(
            s.reserve_prime_play(PrimeKind::Resume)
                .is_none(),
            "the second viewer Pause must fence a ready ResumePrime",
        );
        assert_eq!(
            s.prepare_user_resume(true),
            Some(UserResume::Deferred),
            "the existing ResumePrime certificate survives the second viewer Resume",
        );
        let play = s
            .reserve_prime_play(PrimeKind::Resume)
            .expect("the second Resume releases the same prime once its media is ready");
        assert_eq!(
            s.complete_prime_play(play, true),
            PlayCompletion::Accepted { resume_acb: true }
        );
    }

    #[test]
    fn a_local_sample_without_stream_queues_keeps_immediate_resume() {
        let s = Shared::new();
        let pause = match s.prepare_user_pause() {
            Some(UserPause::Issue(token)) => token,
            other => panic!("unexpected Pause reservation: {other:?}"),
        };
        assert_eq!(
            s.complete_user_pause(pause, true),
            PauseCompletion::Accepted
        );
        assert!(matches!(
            s.prepare_user_resume(false),
            Some(UserResume::Issue(_))
        ));
    }

    #[test]
    fn a_seek_transfers_resume_prime_to_the_fresh_timeline_certificate() {
        let s = Shared::new();
        let pause = match s.prepare_user_pause() {
            Some(UserPause::Issue(token)) => token,
            other => panic!("unexpected Pause reservation: {other:?}"),
        };
        assert_eq!(
            s.complete_user_pause(pause, true),
            PauseCompletion::Accepted
        );
        assert_eq!(s.prepare_user_resume(true), Some(UserResume::Prime));
        assert_eq!(s.prepare_seek_pause(), Some(SeekPause::AlreadyHeld));
        assert_eq!(s.prime_kind(), Some(PrimeKind::Fresh));
        assert!(
            s.reserve_prime_play(PrimeKind::Resume)
                .is_none(),
            "a stale ResumePrime snapshot cannot Play through a newer seek",
        );
        assert!(
            s.reserve_prime_play(PrimeKind::Fresh)
                .is_some(),
            "the seek's fresh-timeline certificate now owns Play",
        );
    }

    #[test]
    fn a_paused_non_seek_reload_stays_held_but_a_seek_preroll_may_decode_its_still() {
        let ordinary_reload = Shared::new();
        assert!(ordinary_reload.arm_initial_clock_hold(true, false));
        assert!(
            ordinary_reload
                .reserve_prime_play(PrimeKind::Fresh)
                .is_none(),
            "preserving Pause across a non-seek reload must not authorize native Play",
        );

        let seek_reload = Shared::new();
        assert!(seek_reload.arm_initial_clock_hold(true, true));
        let play = seek_reload
            .reserve_prime_play(PrimeKind::Fresh)
            .expect("the explicit one-frame seek preroll owns this Play");
        assert_eq!(
            seek_reload.complete_prime_play(play, true),
            PlayCompletion::Accepted { resume_acb: true },
            "the preroll Play must carry the deferred ACB resume",
        );
    }

    #[test]
    fn a_paused_seek_reuses_the_physical_hold_and_resumes_acb_only_at_prime() {
        let s = Shared::new();
        let pause = match s.prepare_user_pause() {
            Some(UserPause::Issue(token)) => token,
            other => panic!("unexpected Pause reservation: {other:?}"),
        };
        assert_eq!(
            s.complete_user_pause(pause, true),
            PauseCompletion::Accepted
        );
        assert_eq!(s.prepare_seek_pause(), Some(SeekPause::AlreadyHeld));
        let play = s
            .reserve_prime_play(PrimeKind::Fresh)
            .expect("seek prime owns the reused hold");
        assert_eq!(
            s.complete_prime_play(play, true),
            PlayCompletion::Accepted { resume_acb: true }
        );
    }

    /// **Every `dg_*` mirror field must have a real writer.**
    ///
    /// The whole diagnostics mirror is write-here-read-there by construction, so a field that is
    /// declared, cleared in `reset_session`, and read by the panel compiles, runs, and is silently
    /// WRONG — there is no unused-field warning for it and no other test can see it. That is not
    /// hypothetical: `dg_place_rv`, `dg_placed_w`, `dg_placed_h` and `dg_splice` shipped exactly
    /// like that. `place_rv` stayed at its `i32::MIN` "never called" sentinel forever, so the
    /// read-out rendered `Placed: not placed` in DANGER bold on every webOS 5+ television — a
    /// fabricated fault, on the one firmware family we cannot test, aimed at the wrong layer.
    ///
    /// Source-level rather than behavioural because the writers live behind the ACB/Starfish seam,
    /// which does not exist on the host. `reset_session` is excluded deliberately: clearing a field
    /// is not writing it, and treating it as one is what let this through.
    #[test]
    fn every_diagnostics_mirror_field_has_a_writer() {
        const SHARED_SRC: &str = include_str!("shared.rs");
        // ff.rs is the THIRD writer of this block — the demux thread stamps the HTTP status and
        // the bytes it received. A writer file missing from this list reads exactly like a field
        // with no writer, so adding one here is part of adding one there.
        const WRITERS: [&str; 4] = [
            include_str!("pump.rs"),
            include_str!("engine.rs"),
            include_str!("mod.rs"),
            include_str!("../ff.rs"),
        ];

        let declared: Vec<&str> = SHARED_SRC
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pub dg_"))
            .filter_map(|l| l.split(':').next())
            .collect();
        assert!(
            declared.len() >= 10,
            "found only {} dg_ fields — the parse broke",
            declared.len()
        );
        // rustfmt may wrap `SHARED.dg_field.store(...)` between either dot.  Whitespace carries no
        // source-level meaning here, so remove it before looking for the writer expression.
        let writers: Vec<String> = WRITERS
            .iter()
            .map(|src| src.chars().filter(|c| !c.is_whitespace()).collect())
            .collect();
        for f in declared {
            let written = writers.iter().any(|src| {
                src.contains(&format!("dg_{f}.store(")) || src.contains(&format!("dg_{f}.fetch_"))
            });
            assert!(written, "dg_{f} is declared and read but NOTHING writes it — the panel would print its sentinel as fact");
        }
    }

    /// The one invariant that cannot be recovered from if it breaks, and breaks SILENTLY: a session
    /// boundary must forget this session's picture. `seen_frame` is set once, off-thread, and is
    /// never cleared anywhere else — so a `reset_session` that dropped it would leave it true for
    /// the rest of the process's life, and every subsequent cold start would show only the 12px
    /// transport mark instead of the centred read-out. Graded together with `frames`, because the
    /// two are only meaningful as a pair (see `Shared::seen_frame`).
    #[test]
    fn reset_session_forgets_this_sessions_picture() {
        let s = Shared::new();
        assert!(
            !s.seen_frame.load(Ordering::Relaxed),
            "a fresh session has shown nothing"
        );
        s.seen_frame.store(true, Ordering::Relaxed);
        s.frames.store(9, Ordering::Relaxed);
        s.demux_io_failed.store(true, Ordering::Relaxed);
        s.dg_transport_rc.store(56, Ordering::Relaxed);
        s.dg_transport_age_ms.store(60_000, Ordering::Relaxed);
        s.dg_transport_reopen_failed.store(true, Ordering::Relaxed);
        s.reset_session();
        assert!(
            !s.seen_frame.load(Ordering::Relaxed),
            "a reload/stop blanks the plane — say so"
        );
        assert_eq!(
            s.frames.load(Ordering::Relaxed),
            0,
            "and the two must be reset together"
        );
        assert!(
            !s.demux_io_failed.load(Ordering::Relaxed),
            "a new session must not inherit an I/O failure"
        );
        assert_eq!(
            (
                s.dg_transport_rc.load(Ordering::Relaxed),
                s.dg_transport_age_ms.load(Ordering::Relaxed),
                s.dg_transport_reopen_failed.load(Ordering::Relaxed),
            ),
            (0, -1, false),
            "nor the cause of the last session's transfer failure"
        );
    }
}
