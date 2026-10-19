//! **The seam between the media layer and whoever decides what to play.**
//!
//! Everything a playback needs from above arrives here as plain data, at start, in one
//! [`PlaybackStart`]: the stream URL and the request headers its addon names, where to start, which
//! audio track, and - optionally - what the stream is ([`StreamDeclaration`]). Left
//! out, the player probes the stream itself before it builds the Load (`player::probe`, then
//! [`PlaybackSession::resolve`]); Stremio describes a stream, never its codecs. The device's own
//! capabilities are read from `tv_platform` (`devcaps`, `tv::sandbox`), never passed. Everything the media layer reports back leaves through one
//! [`PlaybackObserver`] the start carries — nothing here knows what an addon, a library or a
//! watch-history is.
//!
//! [`PlaybackSession`] is the main-thread value the player machine owns
//! (`player::machine::Player::session`) and every engine entry point takes: the start it is
//! playing, the two decisions frozen from it when its declaration became known (Dolby Vision
//! presentation and the audio feed, so a reload or a seek never re-reads a capability that answered
//! later), and the refusals the player's `Error` state is derived from when no Engine exists to
//! publish one.
//!
//! A start without a declaration is installed UNRESOLVED: no decision is made, `player::state`
//! reads `Connecting`, and the native start (`engine::start_bufferfeed_tracked`) spawns the probe
//! instead of a Load. When the probe lands, [`PlaybackSession::resolve`] derives the declaration
//! (`stream::derive_declaration`) and makes the same decisions a declared start makes at
//! [`PlaybackSession::begin`]; a probe that fails is a [`StartRefusal::Probe`].

use crate::headers::RequestHeaders;
use crate::stream::{AudioFeed, DvPresentation, ProbeFailure, ProbedStreams, SoftwareAudio, StreamDeclaration};
use std::sync::Arc;

/// One playback, as the caller asks for it.
#[derive(Clone)]
pub struct PlaybackStart {
    /// The stream: `http` or `https`. Carries whatever query its addon put there; never logged.
    pub url: String,
    /// Sent on every request the media transport makes for this stream — the first open, every
    /// Range seek, every redirect hop (credential headers only to the stream's own origin).
    pub headers: RequestHeaders,
    /// Where to start, in nanoseconds of content time (a resume). 0 = the beginning.
    pub start_ns: i64,
    /// The audio stream to feed, as its 0-based position among the file's AUDIO streams. `None`:
    /// the probe's choice (`stream::derive_declaration`), or with a caller's declaration the
    /// first audio stream of the declared codec.
    pub audio_track: Option<i32>,
    /// The viewer's language preferences (Settings > Playback) for a start that names no audio
    /// track: the probe picks the audio track by them before the Load (`stream::
    /// derive_declaration`). `None`: the file's audio choice.
    pub tracks: Option<tv_base::tracks::TrackPrefs>,
    /// What the stream is, as the Load must declare it — an OVERRIDE. `None` (the default, and
    /// what a Stremio stream has): the player probes the stream with the bundled demuxer before
    /// it builds the Load, and derives this from what the container says.
    pub stream: Option<StreamDeclaration>,
    /// A background preview (the detail page's trailer), not a playback: no observer reports and
    /// the preview admission budget (`player::preview`) applies.
    pub preview: bool,
    /// Who hears about this playback's progress and its end. `None`: nobody.
    pub observer: Option<Arc<dyn PlaybackObserver>>,
}

impl PlaybackStart {
    /// A start for `url` from the beginning, with no headers, no track choice and no
    /// declaration (the player probes it).
    pub fn new(url: impl Into<String>) -> PlaybackStart {
        PlaybackStart {
            url: url.into(),
            headers: RequestHeaders::none(),
            start_ns: 0,
            audio_track: None,
            tracks: None,
            stream: None,
            preview: false,
            observer: None,
        }
    }
}

impl std::fmt::Debug for PlaybackStart {
    /// Never the URL (its query may carry an addon's credential) and never header values.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackStart")
            .field("start_ns", &self.start_ns)
            .field("audio_track", &self.audio_track)
            .field("tracks", &self.tracks)
            .field("stream", &self.stream)
            .field("headers", &self.headers)
            .field("preview", &self.preview)
            .field("observer", &self.observer.is_some())
            .finish()
    }
}

/// The playback position as reported to a [`PlaybackObserver`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Content time, milliseconds.
    pub position_ms: i64,
    /// The stream's duration, milliseconds; 0 when the container does not say (live).
    pub duration_ms: i64,
    pub paused: bool,
}

/// Why a playback stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The viewer (or the app) stopped it.
    Stopped,
    /// It played to the end of the stream.
    Ended,
    /// It failed; `player::error_now` says why.
    Failed,
}

/// **What the media layer reports upward**, one value per playback. Every method has an empty
/// default, so an observer implements only what it records. Called from the progress worker
/// (`progress`) or the main thread (the rest); an implementation must not block — a slow one is
/// handed the facts and does its own I/O on its own thread.
pub trait PlaybackObserver: Send + Sync {
    /// The playback's first frame is on the panel, at `position_ms`.
    fn started(&self, _position_ms: i64) {}
    /// Roughly every 10 s while the playback runs (paused or not).
    fn progress(&self, _at: Progress) {}
    /// The playback ended, once. `at` is the last position presented.
    fn stopped(&self, _at: Progress, _reason: StopReason) {}
}

/// Why a start was refused before any native object was built — each is an explicit `Error`
/// state (`player::state`), never a Load that cannot advance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartRefusal {
    /// The television cannot decode the stream's video codec, or its table lists no such decoder.
    VideoUnsupported(String),
    /// Dolby Vision whose base layer alone is a wrong picture, on a set that cannot be told.
    DolbyVision(&'static str),
    /// The selected audio track needs a software decoder ([`AudioFeed::NeedsSoftwareDecode`]).
    AudioUnsupported { codec: String, channels: i64 },
    /// The stream had no declaration and the probe could not produce one.
    Probe(ProbeFailure),
}

/// One line per playback naming the audio track it starts on and why (never a URL):
/// `tracks: audio #1 (pol) by pref`.
fn log_tracks(prefs: &tv_base::tracks::TrackPrefs, probed: &ProbedStreams, audio: Option<i32>, picked: bool) {
    let a = match audio.and_then(|i| Some((i, probed.audio.get(usize::try_from(i).ok()?)?))) {
        None => "audio none".to_string(),
        Some((i, t)) => {
            let lang = if t.language.is_empty() { "und" } else { t.language.as_str() };
            let why = if !picked {
                "by start"
            } else if prefs.audio_rank(&t.language) < prefs.audio.len() {
                "by pref"
            } else {
                "by file"
            };
            format!("audio #{i} ({lang}) {why}")
        }
    };
    tv_base::eventlog::log(&format!("tracks: {a}"));
}

/// The main-thread playback state (see the module doc).
pub struct PlaybackSession {
    start: Option<PlaybackStart>,
    dv: DvPresentation,
    audio: AudioFeed,
    refusal: Option<StartRefusal>,
    /// This playback was refused by the cached device sandbox preflight. No Engine exists.
    pub jail_load_blocked: bool,
    /// The last fresh native start was refused because a timed-out Load is still inside `sf_load`
    /// (an `AbandonedLoad` owns the one native object). `player::state` derives `Error` from it.
    pub native_start_refused: bool,
    /// Read-only publication of the sandbox repair state for the HUD.
    pub repair_status: tv_platform::tv::sandbox::State,
    /// The observer has been told this playback started (first frame), so it is told once.
    started_reported: bool,
    /// What the probe read off the container, once a probed start resolved: the file's audio
    /// tracks (codec, channels, name) for a track menu, which an addon cannot describe.
    probed: Option<ProbedStreams>,
}

impl PlaybackSession {
    /// Nothing playing.
    pub const IDLE: PlaybackSession = PlaybackSession {
        start: None,
        dv: DvPresentation::NotDv,
        audio: AudioFeed::Silent,
        refusal: None,
        jail_load_blocked: false,
        native_start_refused: false,
        repair_status: tv_platform::tv::sandbox::State::Idle,
        started_reported: false,
        probed: None,
    };

    /// Install `start` as this session's playback and freeze its two decisions from the
    /// television's current capability answers. `Err` is a start that must not reach the native
    /// pipeline; the refusal stays on the session (`refusal`) for the `Error` read-out until the
    /// next `begin` or [`clear`](Self::clear).
    ///
    /// A start with no declaration is installed [`unresolved`](Self::unresolved) and decides
    /// nothing yet: the probe's result arrives through [`resolve`](Self::resolve).
    pub fn begin(&mut self, start: PlaybackStart) -> Result<(), StartRefusal> {
        let caps = tv_platform::devcaps::caps();
        self.decide(start, caps, SoftwareAudio::detected(), |d, hevc| d.presentation_now(hevc))
    }

    /// [`begin`](Self::begin) against an explicit codec table, for tests of the layers that read
    /// the session (no Dolby Vision on the stream, no software audio path).
    #[cfg(test)]
    pub(crate) fn begin_against(&mut self, start: PlaybackStart, caps: &tv_platform::devcaps::Caps) -> Result<(), StartRefusal> {
        self.decide(start, caps, SoftwareAudio::NONE, |_, _| DvPresentation::NotDv)
    }

    /// **A probed start's declaration has arrived**: derive it (`stream::derive_declaration`,
    /// which also picks the audio track when the start named none, by the start's
    /// [`tracks`](PlaybackStart::tracks) preferences, logged once as `tracks: ...`), install it on
    /// the start and make the decisions [`begin`](Self::begin) makes for a declared one. `Ok`
    /// carries the audio stream to feed. `Err` is a refusal that stays on the session, exactly as
    /// `begin`'s does - a probe that found no video track among them.
    ///
    /// Only an [`unresolved`](Self::unresolved) start can be resolved; anything else is a stale
    /// probe result for a playback that has since changed, and is ignored (`Ok(None)`).
    pub fn resolve(&mut self, probed: ProbedStreams) -> Result<Option<i32>, StartRefusal> {
        let caps = tv_platform::devcaps::caps();
        self.resolve_against(probed, caps, SoftwareAudio::detected(), |d, hevc| d.presentation_now(hevc))
    }

    fn resolve_against(
        &mut self,
        probed: ProbedStreams,
        caps: &tv_platform::devcaps::Caps,
        sw: SoftwareAudio,
        dv_of: impl Fn(&crate::stream::Dovi, bool) -> DvPresentation,
    ) -> Result<Option<i32>, StartRefusal> {
        if !self.unresolved() {
            return Ok(None);
        }
        let mut start = self.start.take().expect("an unresolved session holds its start");
        let prefs = start.tracks.clone().unwrap_or_default();
        let derived = crate::stream::derive_declaration(&probed, start.audio_track, &prefs, |a| {
            AudioFeed::decide(&a.codec, a.channels, caps, sw)
        });
        let (decl, track) = match derived {
            Ok(d) => d,
            Err(f) => {
                self.start = Some(start);
                return Err(self.refuse_probe(f));
            }
        };
        tv_base::eventlog::log(&format!(
            "session: probed {} {}x{} @{:.3} dv=P{} audio={} {}ch track={track:?} of {}",
            decl.vcodec,
            decl.raster.0,
            decl.raster.1,
            decl.fps,
            decl.dovi.profile,
            if decl.acodec.is_empty() { "-" } else { decl.acodec.as_str() },
            decl.audio_channels,
            probed.audio.len()
        ));
        if start.tracks.is_some() {
            log_tracks(&prefs, &probed, track, start.audio_track.is_none());
        }
        start.stream = Some(decl);
        start.audio_track = track;
        self.decide(start, caps, sw, dv_of)?;
        self.probed = Some(probed);
        Ok(track)
    }

    /// The probe of the installed (unresolved) start failed: refuse it, keeping the start so the
    /// read-out can offer the same start again.
    pub fn refuse_probe(&mut self, failure: ProbeFailure) -> StartRefusal {
        let r = StartRefusal::Probe(failure);
        tv_base::eventlog::log(&format!("session: start refused — {r:?}"));
        self.refusal = Some(r.clone());
        r
    }

    /// The installed start has no declaration yet and has not been refused: the probe owes it
    /// one. `player::state` reads `Connecting` meanwhile.
    pub fn unresolved(&self) -> bool {
        self.refusal.is_none() && self.start.as_ref().is_some_and(|s| s.stream.is_none())
    }

    /// What the probe read off the container (a probed start, once resolved).
    pub fn probed(&self) -> Option<&ProbedStreams> {
        self.probed.as_ref()
    }

    /// [`begin`](Self::begin) against an explicit table, software paths and Dolby Vision answer.
    fn decide(
        &mut self,
        start: PlaybackStart,
        caps: &tv_platform::devcaps::Caps,
        sw: SoftwareAudio,
        dv_of: impl Fn(&crate::stream::Dovi, bool) -> DvPresentation,
    ) -> Result<(), StartRefusal> {
        self.jail_load_blocked = false;
        self.native_start_refused = false;
        self.started_reported = false;
        self.probed = None;
        let Some(decl) = start.stream.as_ref() else {
            // Unresolved: nothing to decide until the probe lands (`resolve`).
            self.start = Some(start);
            self.dv = DvPresentation::NotDv;
            self.audio = AudioFeed::Silent;
            self.refusal = None;
            return Ok(());
        };
        let hevc = matches!(decl.vcodec.to_ascii_lowercase().as_str(), "hevc" | "h265");
        let dv = dv_of(&decl.dovi, hevc);
        let audio = AudioFeed::decide(&decl.acodec, decl.audio_channels, caps, sw);
        let refusal = video_refusal(&decl.vcodec, decl.raster, decl.vprofile, caps)
            .or_else(|| dv.refusal().map(StartRefusal::DolbyVision))
            .or_else(|| match &audio {
                AudioFeed::NeedsSoftwareDecode { codec, channels } => {
                    Some(StartRefusal::AudioUnsupported { codec: codec.clone(), channels: *channels })
                }
                _ => None,
            });
        self.start = Some(start);
        self.dv = dv;
        self.audio = audio;
        self.refusal = refusal.clone();
        match refusal {
            Some(r) => {
                tv_base::eventlog::log(&format!("session: start refused — {r:?}"));
                Err(r)
            }
            None => Ok(()),
        }
    }

    /// Switch the fed audio track (a viewer pick) and re-decide its feed. `Err` leaves the session
    /// on the track it had.
    pub fn select_audio(&mut self, track: i32, codec: &str, channels: i64, atmos: bool) -> Result<(), StartRefusal> {
        let feed = AudioFeed::decide(codec, channels, tv_platform::devcaps::caps(), SoftwareAudio::detected());
        if let AudioFeed::NeedsSoftwareDecode { codec, channels } = &feed {
            return Err(StartRefusal::AudioUnsupported { codec: codec.clone(), channels: *channels });
        }
        if let Some(s) = self.start.as_mut() {
            s.audio_track = Some(track);
            let d = s.stream.get_or_insert_with(StreamDeclaration::default);
            d.acodec = codec.to_ascii_lowercase();
            d.audio_channels = channels;
            d.atmos = atmos;
        }
        self.audio = feed;
        Ok(())
    }

    /// [`select_audio`](Self::select_audio) by track position alone, reading the codec, channel
    /// count and Atmos flag off the probe — what a track menu over a probed stream has. `Err` for
    /// a track the probe did not see is the same refusal an undecodable one gets.
    pub fn select_probed_audio(&mut self, track: i32) -> Result<(), StartRefusal> {
        let Some(a) = self.probed.as_ref().and_then(|p| p.audio.get(usize::try_from(track).ok()?)).cloned() else {
            return Err(StartRefusal::AudioUnsupported { codec: String::new(), channels: 0 });
        };
        self.select_audio(track, &a.codec, a.channels, a.atmos)
    }

    /// Forget the playback (a real stop). Device facts (`repair_status`) stay.
    pub fn clear(&mut self) {
        let repair = self.repair_status;
        *self = PlaybackSession::IDLE;
        self.repair_status = repair;
    }

    /// The playback being played, if any.
    pub fn start(&self) -> Option<&PlaybackStart> {
        self.start.as_ref()
    }

    /// The stream URL; empty when nothing is installed.
    pub fn url(&self) -> &str {
        self.start.as_ref().map_or("", |s| s.url.as_str())
    }

    /// The stream's request headers.
    pub fn headers(&self) -> RequestHeaders {
        self.start.as_ref().map_or_else(RequestHeaders::none, |s| s.headers.clone())
    }

    /// The installed declaration (empty when nothing is installed, or not yet probed: the sample
    /// feeds' baseline).
    pub fn stream(&self) -> StreamDeclaration {
        self.start.as_ref().and_then(|s| s.stream.clone()).unwrap_or_default()
    }

    /// The Dolby Vision presentation frozen at [`begin`](Self::begin).
    pub fn dv(&self) -> DvPresentation {
        self.dv
    }

    /// The audio feed frozen at [`begin`](Self::begin) (or the last [`select_audio`](Self::select_audio)).
    pub fn audio(&self) -> &AudioFeed {
        &self.audio
    }

    /// Why the installed start was refused, if it was.
    pub fn refusal(&self) -> Option<&StartRefusal> {
        self.refusal.as_ref()
    }

    pub fn is_preview(&self) -> bool {
        self.start.as_ref().is_some_and(|s| s.preview)
    }

    /// The chosen audio stream (position among the file's audio streams), `-1` for the default.
    pub fn audio_track(&self) -> i32 {
        self.start.as_ref().and_then(|s| s.audio_track).unwrap_or(-1)
    }

    /// Where the installed start begins, in nanoseconds.
    pub fn start_ns(&self) -> i64 {
        self.start.as_ref().map_or(0, |s| s.start_ns.max(0))
    }

    /// The observer, unless this is a preview (which reports nothing).
    pub fn observer(&self) -> Option<Arc<dyn PlaybackObserver>> {
        self.start.as_ref().filter(|s| !s.preview).and_then(|s| s.observer.clone())
    }

    /// Tell the observer the first frame is up, once per [`begin`](Self::begin).
    pub fn report_started(&mut self, position_ns: i64) {
        if std::mem::replace(&mut self.started_reported, true) {
            return;
        }
        if let Some(o) = self.observer() {
            o.started(position_ns.max(0) / 1_000_000);
        }
    }
}

/// A video codec the pipeline cannot be fed, or that this set's table lists no decoder for, or a
/// `raster` larger than that codec's own row in the table (PlxNative's `video_direct_plays`
/// resolution gate, per codec: its shared `hevc_max` is the MIN over H.264 and HEVC and would
/// refuse a 4K HEVC file on a SoC whose AVC decoder stops at 1080p, and there is no server here to
/// transcode it). An axis that is 0 (unknown, or not stated — every row of an unread table) never
/// convicts.
fn video_refusal(vcodec: &str, raster: (u16, u16), profile: i32, caps: &tv_platform::devcaps::Caps) -> Option<StartRefusal> {
    let codec = vcodec.to_ascii_lowercase();
    // H.264 beyond 8-bit 4:2:0 (FFmpeg's AV_PROFILE_H264_* ids): the TV decodes none of them
    if codec == "h264" {
        let beyond = match profile {
            110 => Some("10-bit"),
            122 => Some("4:2:2"),
            244 | 44 => Some("4:4:4"),
            _ => None,
        };
        if let Some(what) = beyond {
            return Some(StartRefusal::VideoUnsupported(format!("h264 {what}")));
        }
    }
    let refused = match codec.as_str() {
        "" | "h264" => false,
        "hevc" | "h265" => !caps.hevc,
        "av1" => !caps.av1,
        "vp9" => !caps.vp9,
        "vp8" => !caps.vp8,
        // Everything else has no caps in libpf's buffer feed (`CustomPipeline::getVideoCaps`):
        // MPEG-4 Part 2 (Xvid, DivX), MPEG-1/2, H.263, WMV1-3, VC-1 (its caps never negotiate),
        // RealVideo, MJPEG, even where the
        // table lists a decoder for them (the TV's own file player reaches those, this feed cannot).
        _ => true,
    };
    if refused {
        return Some(StartRefusal::VideoUnsupported(codec));
    }
    let (w, h) = (u32::from(raster.0), u32::from(raster.1));
    let (rw, rh, _) = match codec.as_str() {
        "hevc" | "h265" => caps.hevc_row,
        "av1" => caps.av1_row,
        "vp9" => caps.vp9_row,
        "vp8" => caps.vp8_row,
        _ => caps.h264_row,
    };
    let fits = (rw == 0 || w <= rw) && (rh == 0 || h <= rh);
    (!fits).then(|| StartRefusal::VideoUnsupported(format!("{codec} {w}x{h}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{Dovi, DvPresentation};
    use tv_platform::devcaps::Caps;

    fn start(vcodec: &str, acodec: &str, channels: i64) -> PlaybackStart {
        let mut s = PlaybackStart::new("https://cdn.example/v.mkv");
        s.stream = Some(StreamDeclaration {
            vcodec: vcodec.into(),
            acodec: acodec.into(),
            audio_channels: channels,
            ..StreamDeclaration::default()
        });
        s
    }

    fn no_dv(_: &Dovi, _: bool) -> DvPresentation {
        DvPresentation::NotDv
    }

    impl PlaybackSession {
        /// `decide` with no software audio path, the shape these tests were written against.
        fn decide_t(
            &mut self,
            start: PlaybackStart,
            caps: &Caps,
            dv_of: impl Fn(&Dovi, bool) -> DvPresentation,
        ) -> Result<(), StartRefusal> {
            self.decide(start, caps, SoftwareAudio::NONE, dv_of)
        }
    }

    /// Ported from PlxNative's `video_direct_plays` (test `a_source_beyond_the_device_bound_does_
    /// not_direct_play`), per codec: a source larger than its codec's row in the table is refused
    /// before any Load instead of failing as a pipeline error. An axis nobody knows (0) never
    /// convicts, and an unread table (`assumed()`, every row 0) refuses no raster.
    /// 10-bit H.264 (anime Blu-ray encodes with FLAC): the TV's decoder takes 8-bit H.264 only, and
    /// such a Load was refused by the pipeline before any picture (`type=18 num=100`) on every one
    /// of the beta's H.264+FLAC streams. Refused here, with a reason, before the Load.
    #[test]
    fn ten_bit_h264_is_refused_before_the_load() {
        let caps = Caps::assumed();
        assert_eq!(video_refusal("h264", (1920, 1080), 110, &caps), Some(StartRefusal::VideoUnsupported("h264 10-bit".into())));
        assert_eq!(video_refusal("h264", (1920, 1080), 122, &caps), Some(StartRefusal::VideoUnsupported("h264 4:2:2".into())));
        assert_eq!(video_refusal("h264", (1920, 1080), 244, &caps), Some(StartRefusal::VideoUnsupported("h264 4:4:4".into())));
        assert_eq!(video_refusal("h264", (1920, 1080), 100, &caps), None, "High (8-bit) plays");
        assert_eq!(video_refusal("h264", (1920, 1080), -99, &caps), None, "an unknown profile convicts nothing");
        assert_eq!(video_refusal("hevc", (1920, 1080), 2, &caps), None, "HEVC Main 10 plays");
    }

    #[test]
    fn a_source_beyond_its_codecs_row_is_refused_before_the_load() {
        let at = |codec: &str, w: u16, h: u16, caps: &Caps| video_refusal(codec, (w, h), -99, caps);
        assert_eq!(at("hevc", 7680, 4320, &Caps::assumed()), None, "an unread table bounds nothing");
        let mut caps = Caps::assumed();
        caps.h264_row = (1920, 1088, 60);
        caps.hevc_row = (4096, 2176, 60);
        caps.vp9 = true;
        caps.vp9_row = (1920, 1080, 60);
        assert_eq!(at("hevc", 3840, 2160, &caps), None, "4K HEVC beside a 1080p AVC decoder");
        assert_eq!(at("h264", 3840, 1600, &caps), Some(StartRefusal::VideoUnsupported("h264 3840x1600".into())));
        assert_eq!(at("hevc", 7680, 4320, &caps), Some(StartRefusal::VideoUnsupported("hevc 7680x4320".into())));
        assert_eq!(at("vp9", 3840, 2160, &caps), Some(StartRefusal::VideoUnsupported("vp9 3840x2160".into())));
        assert_eq!(at("h264", 0, 0, &caps), None, "an unknown raster is the pipeline's to find out");
        assert_eq!(at("mpeg2video", 720, 576, &caps), Some(StartRefusal::VideoUnsupported("mpeg2video".into())));
    }

    /// VP8 plays where the table lists its decoder and within its row; every codec the buffer feed
    /// does not take stays refused, whatever the table says: MPEG-4 Part 2 (Xvid, DivX), MPEG-2,
    /// MPEG-1, H.263, WMV1-3, VC-1, RealVideo, MJPEG.
    #[test]
    fn vp8_plays_where_the_table_lists_it_and_nothing_else_is_fed() {
        let at = |codec: &str, w: u16, h: u16, caps: &Caps| video_refusal(codec, (w, h), -99, caps);
        let assumed = Caps::assumed();
        assert!(at("vp8", 1280, 720, &assumed).is_some(), "no row read: not claimed");
        let mut c1 = Caps::assumed();
        c1.vp8 = true;
        c1.vp8_row = (1920, 1088, 30);
        assert_eq!(at("vp8", 1920, 1080, &c1), None);
        assert_eq!(at("vp8", 3840, 2160, &c1), Some(StartRefusal::VideoUnsupported("vp8 3840x2160".into())));
        for codec in ["mpeg4", "mpeg2video", "mpeg1video", "h263", "vc1", "wmv3", "wmv2", "msmpeg4v3", "rv40", "mjpeg"] {
            assert_eq!(at(codec, 1280, 720, &c1), Some(StartRefusal::VideoUnsupported(codec.into())), "{codec}");
        }
    }

    /// A probed stream: 1080p HEVC and one default audio track of `codec`.
    fn probed_with(codec: &str, channels: i64) -> ProbedStreams {
        use crate::stream::{ProbedAudio, ProbedVideo};
        ProbedStreams {
            video: Some(ProbedVideo { codec: "hevc".into(), width: 1920, height: 1080, fps: 25.0, dovi: Dovi::NONE, profile: -99 }),
            audio: vec![ProbedAudio { codec: codec.into(), channels, sample_rate: 48_000, default: true, ..Default::default() }],
            ..Default::default()
        }
    }

    const PCM: SoftwareAudio = SoftwareAudio { pcm: true };

    /// A start with no declaration decides nothing until the probe lands; the probe's result then
    /// makes the same decisions a declared start makes, including the MP2 decode to PCM.
    #[test]
    fn an_undeclared_start_is_unresolved_until_the_probe_resolves_it() {
        let caps = Caps::assumed();
        let mut ps = PlaybackSession::IDLE;
        assert_eq!(ps.begin_against(PlaybackStart::new("https://cdn.example/live.m3u8"), &caps), Ok(()));
        assert!(ps.unresolved());
        assert_eq!((ps.audio(), ps.refusal()), (&AudioFeed::Silent, None));
        assert_eq!(ps.resolve_against(probed_with("mp2", 2), &caps, PCM, no_dv), Ok(Some(0)));
        assert!(!ps.unresolved());
        assert_eq!(ps.audio(), &AudioFeed::SoftPcm { source: "mp2".into(), channels: 2 });
        assert_eq!(ps.audio().payload(), Some(("PCM", 6)));
        assert_eq!(ps.stream().vcodec, "hevc");
        assert_eq!(ps.start().unwrap().audio_track, Some(0), "the chosen track is the one the demuxer feeds");
        assert_eq!(ps.probed().map(|p| p.audio.len()), Some(1));
        assert_eq!(ps.resolve_against(probed_with("aac", 2), &caps, PCM, no_dv), Ok(None), "a stale second result changes nothing");
        assert_eq!(ps.audio(), &AudioFeed::SoftPcm { source: "mp2".into(), channels: 2 });

        // Without the software path the same stream is an explicit audio refusal.
        ps.begin_against(PlaybackStart::new("https://cdn.example/live.m3u8"), &caps).unwrap();
        assert_eq!(
            ps.resolve_against(probed_with("mp2", 2), &caps, SoftwareAudio::NONE, no_dv),
            Err(StartRefusal::AudioUnsupported { codec: "mp2".into(), channels: 2 })
        );
    }

    /// **The audio language preference picks the track before the Load**: the declaration is the
    /// Polish track's (E-AC-3), so nothing reloads once it plays; with no preference the file's
    /// default track plays.
    #[test]
    fn a_polish_preference_picks_the_polish_track_before_the_load() {
        use crate::stream::ProbedAudio;
        let mut bilingual = probed_with("ac3", 6);
        bilingual.audio = vec![
            ProbedAudio { codec: "ac3".into(), channels: 6, default: true, language: "rus".into(), ..Default::default() },
            ProbedAudio { codec: "eac3".into(), channels: 6, language: "pol".into(), ..Default::default() },
        ];
        let resolve_with = |tracks: Option<tv_base::tracks::TrackPrefs>| {
            let mut caps = Caps::assumed();
            caps.audio = "aac,ac3,eac3".into();
            let mut ps = PlaybackSession::IDLE;
            let mut start = PlaybackStart::new("https://cdn.example/live.m3u8");
            start.tracks = tracks;
            ps.begin_against(start, &caps).unwrap();
            ps.resolve_against(bilingual.clone(), &caps, SoftwareAudio::NONE, no_dv).unwrap();
            (ps.audio_track(), ps.stream().acodec)
        };
        let pl = tv_base::tracks::TrackPrefs { audio: vec!["PL".into(), "RU".into()] };
        assert_eq!(resolve_with(Some(pl)), (1, "eac3".into()), "the Polish track is declared");
        assert_eq!(resolve_with(Some(tv_base::tracks::TrackPrefs::default())), (0, "ac3".into()), "no order: the default track");
        assert_eq!(resolve_with(None), (0, "ac3".into()));
    }

    /// A probe failure is a refusal on the session — never an unresolved start left spinning —
    /// and the start stays installed so the read-out can offer it again.
    #[test]
    fn a_failed_probe_is_an_explicit_refusal_that_keeps_the_start() {
        let mut ps = PlaybackSession::IDLE;
        ps.begin_against(PlaybackStart::new("https://cdn.example/v.mkv"), &Caps::assumed()).unwrap();
        assert_eq!(ps.refuse_probe(ProbeFailure::Timeout), StartRefusal::Probe(ProbeFailure::Timeout));
        assert!(!ps.unresolved(), "a refused start is not waiting on anything");
        assert!(ps.start().is_some(), "kept for Try again");
        ps.begin_against(PlaybackStart::new("https://cdn.example/v.mkv"), &Caps::assumed()).unwrap();
        let mut audio_only = probed_with("aac", 2);
        audio_only.video = None;
        assert_eq!(
            ps.resolve_against(audio_only, &Caps::assumed(), SoftwareAudio::NONE, no_dv),
            Err(StartRefusal::Probe(ProbeFailure::NoVideo))
        );
        assert_eq!(ps.refusal(), Some(&StartRefusal::Probe(ProbeFailure::NoVideo)));
    }

    #[test]
    fn a_track_past_the_tables_ceiling_is_an_explicit_refusal_not_a_start() {
        let mut caps = Caps::assumed();
        caps.audio_channels.insert("eac3".into(), 6);
        let mut ps = PlaybackSession::IDLE;
        assert_eq!(ps.decide_t(start("hevc", "eac3", 6), &caps, no_dv), Ok(()));
        assert_eq!(ps.audio(), &AudioFeed::Native { payload: "AC3 PLUS", diag: 2 });
        let refused = ps.decide_t(start("hevc", "eac3", 8), &caps, no_dv);
        assert_eq!(refused, Err(StartRefusal::AudioUnsupported { codec: "eac3".into(), channels: 8 }));
        assert_eq!(ps.refusal(), Some(&StartRefusal::AudioUnsupported { codec: "eac3".into(), channels: 8 }));
        ps.clear();
        assert_eq!(ps.refusal(), None, "a stop retires the refusal");
    }

    #[test]
    fn video_and_dolby_vision_refusals_come_before_audio() {
        let caps = Caps::assumed();
        let mut ps = PlaybackSession::IDLE;
        assert_eq!(
            ps.decide_t(start("vp9", "truehd", 8), &caps, no_dv),
            Err(StartRefusal::VideoUnsupported("vp9".into())),
            "the assumed table lists no VP9 decoder"
        );
        assert_eq!(
            ps.decide_t(start("hevc", "eac3", 6), &caps, |_, _| DvPresentation::Refuse("dual-layer")),
            Err(StartRefusal::DolbyVision("dual-layer"))
        );
        assert_eq!(ps.decide_t(start("h264", "aac", 2), &caps, no_dv), Ok(()));
        assert_eq!(ps.audio().payload(), Some(("AAC", 3)));
    }

    #[test]
    fn a_preview_reports_to_nobody_and_debug_prints_no_url() {
        struct Count(std::sync::atomic::AtomicU32);
        impl PlaybackObserver for Count {
            fn started(&self, _: i64) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let seen = Arc::new(Count(std::sync::atomic::AtomicU32::new(0)));
        let mut s = start("h264", "aac", 2);
        s.url = "https://cdn.example/v.mkv?token=secret".into();
        s.observer = Some(seen.clone());
        assert!(!format!("{s:?}").contains("secret"));
        let mut ps = PlaybackSession::IDLE;
        ps.decide_t(s.clone(), &Caps::assumed(), no_dv).unwrap();
        ps.report_started(5_000_000_000);
        ps.report_started(6_000_000_000);
        assert_eq!(seen.0.load(std::sync::atomic::Ordering::Relaxed), 1, "once per start");
        s.preview = true;
        ps.decide_t(s, &Caps::assumed(), no_dv).unwrap();
        ps.report_started(0);
        assert_eq!(seen.0.load(std::sync::atomic::Ordering::Relaxed), 1, "a preview reports nothing");
    }
}
