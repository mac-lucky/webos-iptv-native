//! **IPTV playback: the API the application shell drives.** One [`Playback`] owns the player
//! machine and adapter (`player::machine::Player`, `player::adapter::PlayerAdapter`) and speaks
//! the shell's vocabulary: play a channel live, play a programme from the archive, seek in it,
//! return to live, pause, pick an audio track, move the picture, and poll one [`Status`].
//!
//! Every method runs on the main thread (the adapter holds the `MainThread` token), and
//! [`Playback::tick`] runs once per frame: it drives the engine (`player::pump`) and then the
//! policies that live above it.
//!
//! - **Zap.** [`Playback::play_live`] tears down whatever plays (the engine's off-frame release)
//!   and starts the next channel; the time to its first frame is logged per start
//!   (`zap: first frame after N ms`) and reported in [`Status::first_frame_ms`].
//! - **Stall watchdog.** Once a channel has shown a frame, [`STALL_MS`] without the playhead
//!   moving (not paused) restarts it: live at the live edge, catch-up at its position. A failure
//!   after the first frame (a dead live playlist, a dropped connection) is a stall too. At most
//!   [`MAX_RECOVERIES`] restarts in a row, until [`STALL_MS`] of playback earns the budget back;
//!   then the failure stands.
//! - **Catch-up** ([`Catchup`]). The shell builds the URLs (`iptv::catchup`): the exact-programme
//!   window when the server has one, and a shift URL builder for the rest. A window that does not
//!   load falls back to shift; shift answers a fixed span (3 h on Flussonic), so a longer
//!   programme is played in parts, each requested from where the last one ended; a redirect to
//!   the "no archive" stub is [`State::NoArchive`]; at the programme's end playback returns to
//!   live or reports [`Event::ProgrammeEnded`] ([`AtEnd`]). Positions are programme time.
//! - **Suspend.** [`Playback::suspend`] releases the pipeline and keeps what plays (the app in
//!   the background; done by itself when the TV takes the decoder back, [`Event::DecoderTaken`]);
//!   [`Playback::resume`] starts it again, live at its edge, catch-up at its position.
//! - **Preview.** The guide's preview is an ordinary playback in a rectangle
//!   ([`Playback::set_video_rect`]); going full screen moves the picture without restarting it.
//!
//! The URLs carry tokens: nothing here logs one, and [`Catchup`]'s `Debug` prints none.

use crate::headers::RequestHeaders;
use crate::player::{self, adapter::PlayerAdapter, machine::Player, video_geometry::Viewport, PlaybackState};
use crate::session::{PlaybackSession, PlaybackStart, StartRefusal};
use crate::stream::{AudioFeed, SoftwareAudio};
use std::sync::Arc;
use tv_base::task::MainThread;

/// No playhead movement for this long, playing and not paused, is a stall.
pub const STALL_MS: u32 = 10_000;
/// Restarts in a row without a frame between them before the failure is shown.
pub const MAX_RECOVERIES: u32 = 3;
/// A failed playback is restarted after this, times the restarts already made.
pub const FAIL_RETRY_MS: u32 = 2_000;
/// How much archive one shift request answers (Flussonic's fixed window from `utc`).
pub const SHIFT_SPAN_S: i64 = 3 * 3600;
/// A catch-up of the programme on air that comes this close to now has caught up with live.
pub const LIVE_EDGE_S: i64 = 60;
/// A part that ends this close to the programme's end is its end (the last segment's tail).
const PART_END_SLACK_S: i64 = 10;

/// A URL whose playlist starts at the given Unix second (the shell fills its shift template,
/// `?utc={utc}&lutc={lutc}` or the channel's own, with that start).
pub type ShiftUrl = Arc<dyn Fn(i64) -> String + Send + Sync>;

/// What happens when a catch-up programme reaches its end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AtEnd {
    /// Play the channel live ([`Event::ReturnedToLive`]).
    #[default]
    ReturnToLive,
    /// Report [`Event::ProgrammeEnded`] and keep playing whatever the archive goes on with; the
    /// shell starts the next programme from its guide.
    Continue,
}

/// A programme to play from the archive.
#[derive(Clone)]
pub struct Catchup {
    /// The programme, Unix seconds.
    pub start: i64,
    pub end: i64,
    /// Where to begin, seconds into the programme.
    pub offset_secs: i64,
    /// The exact-programme URL (a Flussonic window `<name>-<start>-<duration>.m3u8`, or the
    /// channel's own catch-up template filled for the programme): its playlist time 0 is `start`.
    pub window: Option<String>,
    /// Shift-style URLs for any start time: the fallback for a window that fails, and every part
    /// of a programme longer than one shift span. `None`: the window is all there is.
    pub shift: Option<ShiftUrl>,
    /// The channel's live stream, for the return to live.
    pub live_url: String,
    /// The channel's request headers (`#EXTVLCOPT` / `#EXTHTTP`: user agent, referrer), sent on
    /// every archive and live request.
    pub headers: RequestHeaders,
    pub at_end: AtEnd,
}

impl std::fmt::Debug for Catchup {
    /// Never a URL.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catchup")
            .field("start", &self.start)
            .field("end", &self.end)
            .field("offset_secs", &self.offset_secs)
            .field("window", &self.window.is_some())
            .field("shift", &self.shift.is_some())
            .field("headers", &self.headers)
            .field("at_end", &self.at_end)
            .finish()
    }
}

/// What is playing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Idle,
    Live,
    Catchup,
}

/// The playback's state, as the OSD shows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Idle,
    /// Opening the stream (probe, Load).
    Connecting,
    /// Loaded, waiting for the first picture (or a restart's).
    Buffering,
    /// A seek is landing.
    Seeking,
    Playing,
    /// The viewer paused.
    Paused,
    /// The archive has no recording for the programme (the server's "no archive" stub).
    NoArchive,
    /// The pipeline is released and the playback kept ([`Playback::suspend`]).
    Suspended,
    /// The playback failed; [`Playback::error`] has the wording.
    Failed(player::FailureKind),
}

/// The stream's facts for the OSD badges. Zero or empty until the stream is probed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// FFmpeg's codec names (`h264`, `hevc`; `aac`, `ac3`, `mp2`).
    pub video_codec: String,
    pub audio_codec: String,
    pub audio_channels: u32,
}

/// Where a catch-up programme is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatchupPosition {
    /// The programme, Unix seconds.
    pub start: i64,
    pub end: i64,
    /// Into the programme, ms (what History saves and a restart resumes at).
    pub position_ms: i64,
    pub duration_ms: i64,
}

/// One poll of the playback.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub mode: Mode,
    pub state: State,
    pub paused: bool,
    pub media: MediaInfo,
    /// Catch-up only.
    pub catchup: Option<CatchupPosition>,
    /// The current start's time to first frame, ms, once it has one.
    pub first_frame_ms: Option<u32>,
}

/// Something that happened, once ([`Playback::take_event`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The current start presented its first frame after this many ms.
    FirstFrame { ms: u32 },
    /// The watchdog restarted a stalled playback.
    Recovered,
    /// A catch-up programme reached its end and the channel now plays live.
    ReturnedToLive,
    /// A catch-up programme reached its end ([`AtEnd::Continue`]).
    ProgrammeEnded,
    /// The TV took the video decoder back (another app, a source switch): the playback is
    /// suspended until [`Playback::resume`].
    DecoderTaken,
}

/// One audio track of the playing stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioTrack {
    /// What [`Playback::select_audio_track`] takes.
    pub index: usize,
    pub codec: String,
    pub channels: u32,
    /// The `language` tag (`pol`, `rus`, `eng`), empty when the stream has none.
    pub language: String,
    /// The container's name for it, if any.
    pub name: String,
    pub selected: bool,
    /// This set can play it (natively, or decoded to PCM).
    pub playable: bool,
}

/// The playback the shell drives (see the module doc).
pub struct Playback {
    pl: Player,
    pa: PlayerAdapter,
    now_ms: u32,
    tracks: tv_base::tracks::TrackPrefs,
    current: Option<Current>,
    /// When the current start began (monotonic, so its time to first frame does not depend on
    /// the frame clock having ticked yet).
    started: std::time::Instant,
    first_frame_ms: Option<u32>,
    watchdog: Watchdog,
    events: std::collections::VecDeque<Event>,
    /// Released by [`Playback::suspend`]; a catch-up programme keeps its position in
    /// [`CatchupPlay::resume_s`].
    suspended: bool,
}

/// At most this many unread events are kept (the oldest is dropped).
const EVENTS_CAP: usize = 8;

enum Current {
    Live { url: String, headers: RequestHeaders },
    Catchup(Box<CatchupPlay>),
}

/// A catch-up programme being played.
struct CatchupPlay {
    req: Catchup,
    /// Which URL the current part came from.
    source: Source,
    /// The current part's playlist time 0, seconds into the programme.
    part_s: i64,
    /// The window failed and shift took over.
    fell_back: bool,
    /// [`Event::ProgrammeEnded`] was reported.
    end_reported: bool,
    /// The redirect went to the no-archive stub.
    no_archive: bool,
    /// Where a suspended programme resumes, seconds into it.
    resume_s: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Window,
    Shift,
}

impl Playback {
    /// Take the main-thread token and bind the video plane (`player::acb_init`, which the app then
    /// must not call itself). Once at boot, after `tv_net::net::global_init()`,
    /// `tv_media::ff::boot()` and the port's video sink install.
    pub fn new(mt: MainThread) -> Playback {
        player::acb_init(&mt);
        Playback {
            pl: Player::new(),
            pa: PlayerAdapter::new(mt),
            now_ms: 0,
            tracks: tv_base::tracks::TrackPrefs::default(),
            current: None,
            started: std::time::Instant::now(),
            first_frame_ms: None,
            watchdog: Watchdog::default(),
            events: std::collections::VecDeque::new(),
            suspended: false,
        }
    }

    /// The viewer's audio languages, first preferred first (Settings > Playback): a new start
    /// with several audio tracks begins on the best one.
    pub fn set_audio_languages(&mut self, languages: Vec<String>) {
        self.tracks = tv_base::tracks::TrackPrefs { audio: languages };
    }

    /// **Play `url` live**, sending `headers` (the channel's own, or none) on every request:
    /// whatever plays is torn down first (a zap). In the guide's preview, call
    /// [`Playback::set_video_rect`] before or after; the same channel going full screen is only a
    /// rect change, not another start.
    pub fn play_live(&mut self, url: &str, headers: RequestHeaders) {
        self.watchdog = Watchdog::default();
        self.start(url, &headers, None);
        self.current = Some(Current::Live { url: url.to_owned(), headers });
    }

    /// **Play a programme from the archive** (see [`Catchup`]).
    pub fn play_catchup(&mut self, c: Catchup) {
        let duration = (c.end - c.start).max(0);
        let offset = c.offset_secs.clamp(0, duration.saturating_sub(1).max(0));
        player::log(&format!(
            "catchup: programme {duration}s long from {offset}s (window={} shift={})",
            c.window.is_some(),
            c.shift.is_some()
        ));
        let part = part_for(&c, false, offset);
        let mut cp = Box::new(CatchupPlay {
            req: c,
            source: Source::Window,
            part_s: 0,
            fell_back: false,
            end_reported: false,
            no_archive: false,
            resume_s: offset,
        });
        let Some(url) = part.and_then(|p| cp.enter(p)) else {
            player::log("catchup: no archive URL for this programme");
            self.stop();
            return;
        };
        let (at, headers) = (offset - cp.part_s, cp.req.headers.clone());
        self.current = Some(Current::Catchup(cp));
        self.watchdog = Watchdog::default();
        self.start(&url, &headers, Some(at));
    }

    /// Seek a catch-up programme to `position_ms` from its start (live streams do not seek). Inside
    /// the current part it is a segment seek of the playlist; elsewhere (another 3 h part) a new
    /// request.
    pub fn seek(&mut self, position_ms: i64) {
        let Some(Current::Catchup(cp)) = self.current.as_mut() else {
            return;
        };
        let duration = (cp.req.end - cp.req.start).max(0);
        let target = (position_ms / 1000).clamp(0, duration.saturating_sub(1).max(0));
        let part_len_s = player::duration_ns() / 1_000_000_000;
        if part_len_s > 0 && target >= cp.part_s && target < cp.part_s + part_len_s {
            player::request_seek((target - cp.part_s) * 1_000_000_000);
            return;
        }
        let Some(url) = part_for(&cp.req, cp.fell_back, target).and_then(|p| cp.enter(p)) else { return };
        cp.end_reported = false;
        let (at, headers) = (target - cp.part_s, cp.req.headers.clone());
        player::log(&format!("catchup: seek to {target}s needs part {}s", cp.part_s));
        self.start(&url, &headers, Some(at));
    }

    /// Leave a catch-up programme for the channel's live stream.
    pub fn go_live(&mut self) {
        if let Some(Current::Catchup(cp)) = &self.current {
            let (url, headers) = (cp.req.live_url.clone(), cp.req.headers.clone());
            self.play_live(&url, headers);
        }
    }

    /// Pause or resume; `false` when the pipeline refused (the state is unchanged).
    pub fn set_paused(&mut self, paused: bool) -> bool {
        if paused == player::transport::paused() {
            return true;
        }
        if paused {
            player::pause(&mut self.pa)
        } else {
            player::resume(&mut self.pa)
        }
    }

    /// The playing stream's audio tracks (empty until it is probed).
    pub fn audio_tracks(&self) -> Vec<AudioTrack> {
        let ps = &self.pl.session;
        let Some(probed) = ps.probed() else { return Vec::new() };
        let selected = ps.audio_track();
        let (caps, sw) = (tv_platform::devcaps::caps(), SoftwareAudio::detected());
        probed
            .audio
            .iter()
            .enumerate()
            .map(|(i, a)| AudioTrack {
                index: i,
                codec: a.codec.clone(),
                channels: u32::try_from(a.channels).unwrap_or(0),
                language: a.language.clone(),
                name: a.name.clone(),
                selected: i32::try_from(i).is_ok_and(|i| i == selected),
                playable: AudioFeed::decide(&a.codec, a.channels, caps, sw).rank() < 2,
            })
            .collect()
    }

    /// Switch to audio track `index` ([`AudioTrack::index`]): the stream reloads at its position.
    /// `Err` for a track this set cannot play; the current one keeps playing.
    pub fn select_audio_track(&mut self, index: usize) -> Result<(), StartRefusal> {
        let track = i32::try_from(index).map_err(|_| StartRefusal::AudioUnsupported { codec: String::new(), channels: 0 })?;
        player::request_probed_audio_track(&mut self.pl.session, track)
    }

    /// Move the picture: `None` full screen, else a rectangle of the 1920x1080 plane.
    pub fn set_video_rect(&mut self, rect: Option<Viewport>) {
        player::set_video_rect(rect);
    }

    /// **Suspend** (the app goes to the background; the TV took the decoder back): the pipeline
    /// is released and what plays is kept. [`Playback::resume`] starts it again.
    pub fn suspend(&mut self) {
        if self.suspended || self.current.is_none() {
            return;
        }
        if let Some(Current::Catchup(cp)) = self.current.as_mut() {
            cp.resume_s = programme_s(cp, &self.pl.session);
        }
        self.suspended = true;
        self.first_frame_ms = None;
        player::log("playback: suspended");
        player::stop_bufferfeed(&mut self.pl.session, &mut self.pa);
    }

    /// Start a suspended playback again: live at its edge, a catch-up programme at its position.
    pub fn resume(&mut self) {
        if !std::mem::replace(&mut self.suspended, false) {
            return;
        }
        player::log("playback: resumed");
        match self.current.as_mut() {
            Some(Current::Live { url, headers }) => {
                let (url, headers) = (url.clone(), headers.clone());
                self.start(&url, &headers, None);
            }
            Some(Current::Catchup(cp)) => {
                let Some(url) = cp.enter((cp.source, cp.part_s)) else { return };
                let (at, headers) = (cp.resume_s - cp.part_s, cp.req.headers.clone());
                self.start(&url, &headers, Some(at));
            }
            None => {}
        }
    }

    /// Stop playing.
    pub fn stop(&mut self) {
        self.current = None;
        self.suspended = false;
        self.first_frame_ms = None;
        self.watchdog = Watchdog::default();
        player::stop_bufferfeed(&mut self.pl.session, &mut self.pa);
    }

    /// **Once per frame**: drive the engine, then the watchdog and the catch-up policy. Returns the
    /// video plane's bound edge (`Some` only when it changed), for the app's present gate.
    pub fn tick(&mut self, now_ms: u32) -> Option<bool> {
        self.now_ms = now_ms;
        self.pl.set_now(now_ms);
        self.pa.poll_release();
        player::pump(&mut self.pl.session, &mut self.pa, now_ms);
        let edge = player::observe_video_plane(&mut self.pl, &mut self.pa);
        if player::take_decoder_revoked() && !self.suspended && self.current.is_some() {
            // Restarting would fight whoever took it: wait for the app to resume.
            player::log("playback: the TV took the video decoder back");
            self.suspend();
            self.push(Event::DecoderTaken);
        }
        if !self.suspended {
            self.after_pump(now_ms);
        }
        edge
    }

    /// How soon [`Playback::tick`] must run again, ms: every display frame (16) while anything
    /// plays, connects, restarts or releases, since each tick feeds the decoder a bounded slice
    /// and drives the watchdog; `None` while idle or suspended.
    pub fn tick_interval_ms(&self) -> Option<u32> {
        ((self.current.is_some() && !self.suspended) || self.pa.has_pending_release()).then_some(16)
    }

    /// One poll.
    pub fn status(&self) -> Status {
        let ps = &self.pl.session;
        let decl = ps.stream();
        let (w, h) = player::video_raster();
        let (w, h) = if w > 0 && h > 0 { (w as u32, h as u32) } else { (u32::from(decl.raster.0), u32::from(decl.raster.1)) };
        let media = MediaInfo {
            width: w,
            height: h,
            fps: decl.fps,
            video_codec: decl.vcodec.clone(),
            audio_codec: decl.acodec.clone(),
            audio_channels: u32::try_from(decl.audio_channels).unwrap_or(0),
        };
        let (mode, catchup, no_archive) = match &self.current {
            None => (Mode::Idle, None, false),
            Some(Current::Live { .. }) => (Mode::Live, None, false),
            Some(Current::Catchup(cp)) => {
                let duration_s = (cp.req.end - cp.req.start).max(0);
                let pos_ms = if self.suspended { cp.resume_s * 1000 } else { programme_ms(cp, ps) };
                let pos_ms = pos_ms.clamp(0, duration_s * 1000);
                let at = CatchupPosition { start: cp.req.start, end: cp.req.end, position_ms: pos_ms, duration_ms: duration_s * 1000 };
                (Mode::Catchup, Some(at), cp.no_archive)
            }
        };
        let paused = player::transport::paused();
        let state = match player::state(ps) {
            _ if self.current.is_none() => State::Idle,
            _ if self.suspended => State::Suspended,
            _ if no_archive => State::NoArchive,
            PlaybackState::Error => State::Failed(player::error_now(ps).kind),
            PlaybackState::Idle | PlaybackState::Resolving | PlaybackState::Connecting => State::Connecting,
            PlaybackState::Buffering => State::Buffering,
            PlaybackState::Seeking => State::Seeking,
            PlaybackState::Paused => State::Paused,
            PlaybackState::Playing if paused => State::Paused,
            PlaybackState::Playing => State::Playing,
        };
        Status { mode, state, paused, media, catchup, first_frame_ms: self.first_frame_ms }
    }

    /// The wording of a [`State::Failed`] playback.
    pub fn error(&self) -> Option<player::ErrorShape> {
        matches!(self.status().state, State::Failed(_)).then(|| player::error_now(&self.pl.session))
    }

    /// The oldest event not yet taken.
    pub fn take_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// The machine and the adapter, for what the app loop owns beside this API (the sandbox
    /// repair, the lifecycle's suspend).
    pub fn parts(&mut self) -> (&mut Player, &mut PlayerAdapter) {
        (&mut self.pl, &mut self.pa)
    }

    // ---- internals ------------------------------------------------------------------------

    /// Start `url`: live (`archive` `None`) at its edge, an archive part at `Some(seconds)` of its
    /// playlist time. The archive position is armed here and nowhere else, even at 0, so an EVENT
    /// playlist that is still growing (the programme on air) plays from there rather than from its
    /// live edge. Every start is probed (no declaration), so the arm lands before its Load.
    fn start(&mut self, url: &str, headers: &RequestHeaders, archive: Option<i64>) {
        crate::hls::note_failure(None);
        let mut start = PlaybackStart::new(url);
        start.headers = headers.clone();
        start.tracks = Some(self.tracks.clone());
        self.started = std::time::Instant::now();
        self.first_frame_ms = None;
        self.watchdog.restarted(self.now_ms);
        match player::begin_playback(&mut self.pl.session, &mut self.pa, start) {
            Ok(outcome) => {
                if let Some(at_s) = archive {
                    player::engine::arm_seek(at_s.max(0) * 1_000_000_000);
                }
                player::log(&format!("zap: {} start {outcome:?}", if archive.is_some() { "catch-up" } else { "live" }));
            }
            Err(r) => player::log(&format!("zap: start refused ({r:?})")),
        }
    }

    fn push(&mut self, e: Event) {
        if self.events.len() >= EVENTS_CAP {
            self.events.pop_front();
        }
        self.events.push_back(e);
    }

    fn after_pump(&mut self, now: u32) {
        if self.current.is_none() {
            return;
        }
        if self.first_frame_ms.is_none() && player::seen_frame() {
            let ms = u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX);
            self.first_frame_ms = Some(ms);
            player::log(&format!("zap: first frame after {ms} ms"));
            self.push(Event::FirstFrame { ms });
        }
        let st = player::state(&self.pl.session);
        if st == PlaybackState::Error && self.first_frame_ms.is_none() && self.catchup_failed() {
            return;
        }
        if self.catchup_policy() {
            return;
        }
        let obs = Observation {
            now,
            failed: st == PlaybackState::Error,
            frame: self.first_frame_ms.is_some(),
            held: is_hold(player::transport::paused(), player::ended(), matches!(self.current, Some(Current::Catchup(_)))),
            position_ns: player::intended_pos_ns(&self.pl.session),
        };
        if self.watchdog.observe(obs) == Verdict::Restart {
            self.recover();
        }
    }

    /// A catch-up start failed before its first picture: the no-archive stub, or (before the
    /// programme ever showed one) a window that does not load, which shift takes over. `true`
    /// when this handled it; anything else is the watchdog's.
    fn catchup_failed(&mut self) -> bool {
        let had_frames = self.watchdog.had_frames;
        let Some(Current::Catchup(cp)) = self.current.as_mut() else { return false };
        if cp.no_archive {
            return true;
        }
        let failure = crate::hls::last_failure();
        if failure == Some(crate::hls::HlsError::NoArchive) {
            player::log("catchup: no archive for this programme");
            cp.no_archive = true;
            player::stop_bufferfeed(&mut self.pl.session, &mut self.pa);
            return true;
        }
        if !had_frames && cp.source == Source::Window && cp.req.shift.is_some() && !cp.fell_back {
            cp.fell_back = true;
            let offset = programme_s(cp, &self.pl.session);
            if let Some(url) = part_for(&cp.req, true, offset).and_then(|p| cp.enter(p)) {
                player::log(&format!(
                    "catchup: the window did not load ({}); falling back to shift",
                    failure.map_or_else(|| "no HLS failure".to_owned(), |e| e.to_string())
                ));
                let (at, headers) = (offset - cp.part_s, cp.req.headers.clone());
                self.start(&url, &headers, Some(at));
                return true;
            }
        }
        false
    }

    /// The programme's end, a part's end, and catching up with live. `true` when it restarted or
    /// left the playback.
    fn catchup_policy(&mut self) -> bool {
        let Some(Current::Catchup(cp)) = self.current.as_mut() else { return false };
        if cp.no_archive {
            return true;
        }
        let pos_s = programme_s(cp, &self.pl.session);
        let now_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let view = PartView {
            pos_s,
            part_s: cp.part_s,
            part_len_s: player::duration_ns() / 1_000_000_000,
            part_ended: player::ended(),
            now_unix,
            end_reported: cp.end_reported,
            have_frame: self.first_frame_ms.is_some(),
        };
        let step = next_step(&cp.req, view);
        match step {
            Step::Play => false,
            Step::ProgrammeEnded => {
                cp.end_reported = true;
                player::log("catchup: the programme ended");
                self.push(Event::ProgrammeEnded);
                false
            }
            Step::ReturnToLive => {
                player::log("catchup: the programme ended; back to live");
                let (url, headers) = (cp.req.live_url.clone(), cp.req.headers.clone());
                self.play_live(&url, headers);
                self.push(Event::ReturnedToLive);
                true
            }
            Step::NextPart(at_s) => {
                // Exactly where the last part ended, whatever span the server answered.
                let Some(url) = cp.enter((Source::Shift, at_s)) else { return false };
                let headers = cp.req.headers.clone();
                player::log(&format!("catchup: part ended at {at_s}s; requesting the next from there"));
                self.start(&url, &headers, Some(0));
                true
            }
        }
    }

    /// The watchdog's restart: live at its edge, catch-up at its position.
    fn recover(&mut self) {
        match self.current.as_mut() {
            Some(Current::Live { url, headers }) => {
                let (url, headers) = (url.clone(), headers.clone());
                player::log("watchdog: no picture progress; restarting the channel at its live edge");
                self.start(&url, &headers, None);
            }
            Some(Current::Catchup(cp)) => {
                let pos_s = programme_s(cp, &self.pl.session);
                let Some(url) = cp.enter((cp.source, cp.part_s)) else { return };
                let (at, headers) = (pos_s - cp.part_s, cp.req.headers.clone());
                player::log(&format!("watchdog: no picture progress; restarting the programme at {pos_s}s"));
                self.start(&url, &headers, Some(at));
            }
            None => return,
        }
        self.push(Event::Recovered);
    }
}

// ---- the policies, pure ---------------------------------------------------------------------

/// The playhead in programme time, ms: the current part's start plus the intended position in
/// its playlist (an in-flight seek's target wins).
fn programme_ms(cp: &CatchupPlay, ps: &PlaybackSession) -> i64 {
    cp.part_s * 1000 + player::intended_pos_ns(ps).max(0) / 1_000_000
}

/// [`programme_ms`] in whole seconds.
fn programme_s(cp: &CatchupPlay, ps: &PlaybackSession) -> i64 {
    programme_ms(cp, ps) / 1000
}

/// Whether a playhead standing still is on purpose (the watchdog waits): the viewer paused, or a
/// catch-up played to its end (its policy decides what follows). A live stream that ended (a
/// server's clean EOF, a packager writing ENDLIST) is a stall like any other.
fn is_hold(paused: bool, ended: bool, catchup: bool) -> bool {
    paused || (ended && catchup)
}

/// Which part plays a programme from `offset_s`, as `(source, seconds into the programme of its
/// playlist's time 0)`: the window for all of it unless it failed (`fell_back`) or there is none,
/// else the shift part holding `offset_s` (parts of [`SHIFT_SPAN_S`] from the programme's start).
fn part_for(c: &Catchup, fell_back: bool, offset_s: i64) -> Option<(Source, i64)> {
    if c.window.is_some() && (!fell_back || c.shift.is_none()) {
        return Some((Source::Window, 0));
    }
    c.shift.as_ref().map(|_| (Source::Shift, offset_s.max(0) / SHIFT_SPAN_S * SHIFT_SPAN_S))
}

/// The URL of the part `source` from `part_s`.
fn url_of(c: &Catchup, source: Source, part_s: i64) -> Option<String> {
    match source {
        Source::Window => c.window.clone(),
        Source::Shift => c.shift.as_ref().map(|shift| shift(c.start + part_s)),
    }
}

impl CatchupPlay {
    /// Make `(source, part_s)` the part playing, for a new request: its URL.
    fn enter(&mut self, (source, part_s): (Source, i64)) -> Option<String> {
        let url = url_of(&self.req, source, part_s)?;
        (self.source, self.part_s, self.no_archive) = (source, part_s, false);
        Some(url)
    }
}

/// What the catch-up policy sees each tick.
#[derive(Clone, Copy, Debug)]
struct PartView {
    /// The playhead, seconds into the programme.
    pos_s: i64,
    /// Where the current part's playlist starts, seconds into the programme.
    part_s: i64,
    /// How long the current part's playlist is, seconds (0: not known).
    part_len_s: i64,
    /// The current part's playlist has played to its end.
    part_ended: bool,
    now_unix: i64,
    end_reported: bool,
    have_frame: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Play,
    ProgrammeEnded,
    ReturnToLive,
    /// Request the archive from this many seconds into the programme.
    NextPart(i64),
}

fn next_step(c: &Catchup, v: PartView) -> Step {
    let duration = (c.end - c.start).max(0);
    let finished = v.have_frame && (v.pos_s >= duration || (v.part_ended && v.pos_s >= duration - PART_END_SLACK_S));
    // A part that ended before the programme did: the shift span ran out (a programme longer
    // than one request answers), or the programme is on air and the archive stops at now. One
    // that ended where it began has nothing after it.
    let short_part = v.part_ended && !finished;
    let caught_up = c.start + v.pos_s >= v.now_unix - LIVE_EDGE_S;
    let no_next = c.shift.is_none() || v.pos_s <= v.part_s;
    if finished || (short_part && (caught_up || no_next)) {
        return match c.at_end {
            AtEnd::ReturnToLive => Step::ReturnToLive,
            AtEnd::Continue if v.end_reported => Step::Play,
            AtEnd::Continue if short_part => Step::ReturnToLive,
            AtEnd::Continue => Step::ProgrammeEnded,
        };
    }
    if short_part {
        // From where the part's playlist ended (the playhead stops a little short of it).
        let end = if v.part_len_s > 0 { v.part_s + v.part_len_s } else { v.pos_s };
        return Step::NextPart(end.max(v.pos_s));
    }
    Step::Play
}

/// What the watchdog sees each tick.
#[derive(Clone, Copy, Debug)]
struct Observation {
    now: u32,
    /// The engine is in its terminal `Error`.
    failed: bool,
    /// The current start has presented a frame.
    frame: bool,
    /// The playhead stands still on purpose: the viewer paused, or the stream played to its end.
    held: bool,
    /// The intended playhead (an in-flight seek's target wins).
    position_ns: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Wait,
    Restart,
}

/// The stall watchdog: armed once the channel has shown a frame, it restarts a playback whose
/// playhead stands still for [`STALL_MS`] or that failed, at most [`MAX_RECOVERIES`] times in a
/// row without a frame between.
#[derive(Clone, Copy, Debug, Default)]
struct Watchdog {
    /// This channel (or programme) has presented a frame at some point.
    had_frames: bool,
    last_pos: i64,
    /// When the playhead last moved, or the watchdog was last (re)started.
    since: u32,
    /// Restarts in a row: cleared only by [`STALL_MS`] of playhead movement after one.
    recoveries: u32,
    /// Since when the playhead has been moving without a stall.
    moving_since: Option<u32>,
}

impl Watchdog {
    /// A start (first or a restart) began at `now`.
    fn restarted(&mut self, now: u32) {
        self.since = now;
        self.last_pos = i64::MIN;
        self.moving_since = None;
    }

    fn observe(&mut self, o: Observation) -> Verdict {
        self.had_frames |= o.frame;
        if !self.had_frames {
            return Verdict::Wait;
        }
        let moving = o.frame && !o.failed && o.position_ns != self.last_pos;
        if moving {
            // One frame is not a recovery: the budget comes back after a stall's worth of play.
            let since = *self.moving_since.get_or_insert(o.now);
            if o.now.wrapping_sub(since) >= STALL_MS {
                self.recoveries = 0;
            }
        }
        if o.held {
            self.moving_since = None;
        }
        if moving || (o.held && !o.failed) {
            self.last_pos = o.position_ns;
            self.since = o.now;
            return Verdict::Wait;
        }
        let due = if o.failed { FAIL_RETRY_MS.saturating_mul(self.recoveries + 1) } else { STALL_MS };
        if o.now.wrapping_sub(self.since) >= due {
            // A stall ends the healthy streak, restarted or not.
            self.moving_since = None;
            if self.recoveries < MAX_RECOVERIES {
                self.recoveries += 1;
                self.since = o.now;
                return Verdict::Restart;
            }
        }
        Verdict::Wait
    }
}

#[cfg(test)]
#[path = "playback_tests.rs"]
mod tests;

#[cfg(all(test, feature = "hostsim"))]
#[path = "playback_e2e_tests.rs"]
mod e2e;
