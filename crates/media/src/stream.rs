//! **What a stream IS, as the Load must declare it** - and the two decisions that follow from it
//! before a native object is built: how its Dolby Vision is presented, and whether its audio can
//! be fed to the television's decoder at all.
//!
//! LG's buffer-feed pipeline is told the codecs, the frame rate and the sink envelope in the Load
//! payload, BEFORE the first access unit is fed, and it builds its decoders from that declaration
//! rather than from the bitstream. So the facts here must be known at start, and the container is
//! the only authority (a playlist's metadata is not, and an M3U gives none): unless the caller hands
//! in a declaration of its own with [`crate::session::PlaybackStart`], the player PROBES the stream
//! with the bundled demuxer first (`ff::probe`, run off-frame by `player::probe`) and derives the
//! declaration from what it read ([`derive_declaration`]). The demuxer then refuses a file that
//! disagrees with what was declared (`ff::demux`), because a Load that says one codec over another
//! codec's bitstream is a black screen with nothing in the log.

/// The declared shape of one playback. All-empty is the local-sample feeds' baseline (an H.264
/// video-only declaration).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamDeclaration {
    /// The video codec in FFmpeg's lowercase naming (`h264`, `hevc`, `av1`, `vp9`). Empty means
    /// H.264, the baseline the static payloads declare.
    pub vcodec: String,
    /// The audio codec of the track to feed, FFmpeg's lowercase naming (`aac`, `ac3`, `eac3`,
    /// `mp2`, ...). Empty: the stream has no audio track to feed.
    pub acodec: String,
    /// That track's channel count; 0 when unknown.
    pub audio_channels: i64,
    /// That track's sample rate in Hz; 0 when unknown.
    pub audio_sample_rate: i64,
    /// The video frame rate; 0.0 when unknown.
    pub fps: f64,
    /// The coded raster `(width, height)`; 0 on an axis nobody knows.
    pub raster: (u16, u16),
    /// The video's FFmpeg profile id (`AV_PROFILE_*`); -99 (FFmpeg's unknown) or 0 when not read.
    pub vprofile: i32,
    /// What the container says about Dolby Vision.
    pub dovi: Dovi,
    /// The audio track is Dolby Atmos (E-AC3 JOC).
    pub atmos: bool,
}

/// A stream's **Dolby Vision** layering, read together, because the only question worth asking of
/// it is a joint one: **is the base layer, alone, a correct picture?** The buffer-feed pipeline
/// feeds one elementary stream to a decoder that has never heard of an RPU, so the base layer is
/// exactly what reaches the panel unless the Load declares the stream as Dolby Vision.
///
/// All zero is "nothing said", which is also what a non-DV stream produces - see
/// [`Dovi::base_layer_unusable`] for why silence must never convict.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Dovi {
    /// The stream carries Dolby Vision at all (a `dvcC`/`dvvC` configuration record).
    pub present: bool,
    /// The profile: 5 / 7 / 8 ...; 0 means unknown.
    pub profile: i64,
    /// The base layer's signal compatibility id: 0 none (P5) / 1 HDR10 / 2 SDR / 4 HLG.
    pub bl_compat: i64,
    /// An enhancement layer is present (P7).
    pub el_present: bool,
}

impl Dovi {
    pub const NONE: Dovi = Dovi { present: false, profile: 0, bl_compat: 0, el_present: false };

    /// PURE: **true when the base layer on its own is not a picture the panel shows correctly** when
    /// the pipeline was never told it is Dolby Vision: an enhancement layer (P7: the other layer
    /// never arrives) or no cross-compatible base layer (P5's IPT-PQ, decoded as ordinary HEVC
    /// with a pink-green cast). A `profile` of 0 never convicts: silence is indistinguishable from
    /// an SDR file.
    pub fn base_layer_unusable(&self) -> bool {
        if !self.present {
            return false;
        }
        self.el_present || self.profile == 5 || (self.profile > 0 && self.bl_compat == 0)
    }

    /// PURE: **how this stream will be presented** - the one predicate behind both the refusal and
    /// the Load payload's `DolbyHdrInfo` node, so the two can never disagree.
    ///
    /// - not `present` → [`DvPresentation::NotDv`];
    /// - `el_present` (or Profile 7) -> its BASE LAYER only ([`DvPresentation::BaseLayer`]): the
    ///   demuxer drops every RPU (NAL 62) and enhancement-layer (NAL 63) unit (`ff::DvStrip`) and
    ///   the Load declares plain HEVC. The base layer's own VUI and SEI carry its transfer (PQ for
    ///   P7's compatibility id 6 and for 1, SDR for 2, HLG for 4), which the decoder reads in-band;
    ///   there is no payload key for it to repeat. An enhancement layer over a base layer with no
    ///   compatible signal (id 0) has nothing to fall back to and is refused;
    /// - no node will be sent (capability not a definite `Supported`, non-HEVC video, an unknown
    ///   profile, or `signal` withheld on an unusable base layer) → refuse iff the base layer is
    ///   unusable, else play the base layer;
    /// - otherwise declare it, which is what makes the panel enter Dolby Vision mode (libpf sets
    ///   `dolby-vision=TRUE` on the mere presence of the node - decompiled from this television's
    ///   own `CustomPipeline::parseOptionStringSpi`).
    pub fn presentation(
        &self,
        signal: bool,
        capability: tv_platform::devcaps::dv::DvCapability,
        video_is_hevc: bool,
    ) -> DvPresentation {
        if !self.present {
            return DvPresentation::NotDv;
        }
        if self.el_present || self.profile == 7 {
            // P7's base layer is HDR10 by definition (id 6), even where a muxer wrote 0.
            let base_usable = self.profile == 7 || matches!(self.bl_compat, 1 | 2 | 4 | 6);
            return if base_usable && video_is_hevc { DvPresentation::BaseLayer } else { DvPresentation::Refuse("dual-layer") };
        }
        let declare = capability == tv_platform::devcaps::dv::DvCapability::Supported
            && video_is_hevc
            && (signal || !self.base_layer_unusable());
        if !declare || self.profile <= 0 {
            return if self.base_layer_unusable() {
                DvPresentation::Refuse("no cross-compatible base layer")
            } else {
                DvPresentation::NotDv
            };
        }
        DvPresentation::Declare(DolbyHdrInfo {
            profile_id: self.profile,
            track_type: "single",
            // Never `"all"`: with `trackType: "dual"` that sets `dv-dual-svp`, the secure video
            // path this app cannot satisfy.
            encryption_type: "clear",
        })
    }

    /// [`Self::presentation`] from the cached platform answer and the boot-latched diagnostic
    /// signal. Resolved once per playback start; reloads keep the value they were started with.
    pub fn presentation_now(&self, video_is_hevc: bool) -> DvPresentation {
        self.presentation(!dv_withheld(), tv_platform::devcaps::dv::capability(), video_is_hevc)
    }
}

/// The `option.externalStreamingInfo.contents.DolbyHdrInfo` node of the Starfish Load payload.
/// `profileId` must be a JSON integer (`getInt`); the other two are strings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DolbyHdrInfo {
    pub profile_id: i64,
    /// `"single"` or `"dual"`.
    pub track_type: &'static str,
    /// `"clear"`.
    pub encryption_type: &'static str,
}

/// What [`Dovi::presentation`] decided.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DvPresentation {
    /// Not Dolby Vision, or not identifiably so: play it as ordinary HEVC, declare nothing.
    NotDv,
    /// Play it with this node spliced into the Load payload.
    Declare(DolbyHdrInfo),
    /// A dual-layer stream played as its base layer alone: RPU and enhancement-layer NAL units are
    /// dropped by the demuxer and nothing Dolby is declared (Profile 7 as HDR10).
    BaseLayer,
    /// The base layer alone is a wrong picture and the node cannot be sent: refuse the start.
    Refuse(&'static str),
}

impl DvPresentation {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::NotDv => "base-layer",
            Self::Declare(_) => "declare",
            Self::BaseLayer => "base-layer-only",
            Self::Refuse(_) => "refuse",
        }
    }

    /// The demuxer must drop the RPU and enhancement-layer NAL units.
    pub fn strips_layers(&self) -> bool {
        matches!(self, Self::BaseLayer)
    }

    /// The node to splice into the Load payload, if any.
    pub fn declared(&self) -> Option<DolbyHdrInfo> {
        match self {
            Self::Declare(d) => Some(*d),
            _ => None,
        }
    }

    /// Why the start is refused, if it is.
    pub fn refusal(&self) -> Option<&'static str> {
        match self {
            Self::Refuse(why) => Some(why),
            _ => None,
        }
    }
}

tv_base::devtrig::latched_flag!(
    /// `/tmp/webosiptv-dvnonode` - keep a declared Dolby Vision playback but send **no**
    /// `DolbyHdrInfo` node. Diagnostic only, applied at the payload: the picture is expected to be
    /// WRONG for a Profile 5 stream; this knob judges cadence, not colour.
    pub fn dv_node_suppressed = "dvnonode";
);

tv_base::devtrig::latched_flag!(
    /// `/tmp/webosiptv-nodv` - **withhold the Dolby Vision declaration**, for a bisect. A
    /// withheld Profile 5 is refused (its base layer is unusable), so this bisects "declared vs
    /// refused"; [`dv_node_suppressed`] is the finer instrument.
    pub fn dv_withheld = "nodv";
);

/// **How the selected audio track reaches the speakers**, decided at start from the declaration
/// and the television's own codec table (`tv_platform::devcaps`). One decision, so the Load
/// payload, the demuxer's framing and the refusal can never disagree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioFeed {
    /// The stream has no audio track: a video-only Load, played silently.
    Silent,
    /// The television decodes the track as demuxed (AAC ADTS-framed by the demuxer). `payload`
    /// is LG's name for it in the Load (`"AC3 PLUS"` for E-AC3, never `"EAC3"`: the wrong
    /// string leaves the audio ES unconfigured and, through `audioSync`, stalls the video).
    Native { payload: &'static str, diag: u8 },
    /// **Decoded on the CPU and fed as PCM** (`ff::adec`, ported from PlxNative): MPEG-1 Layer II,
    /// which the buffer feed has no name for, MP3 on a set without an MPEG audio row, any track
    /// past its codec's channel ceiling, and on webOS 10+, whose table devcaps empties, every
    /// track the build decodes (AAC, AAC-LATM, AC-3, E-AC-3 too); decoded by the bundled libavcodec and resampled to S16LE 48 kHz, `channels` of them
    /// (`ff::adec::sink_channels`: stereo); the Load declares `"PCM"` with that `pcmInfo`. Chosen
    /// when the decoder bound at boot (`ff::adec::available`).
    SoftPcm { source: String, channels: u32 },
    /// **The television cannot decode this track** - a codec the pipeline has no decoder for (or
    /// a listed one beyond its measured channel ceiling) and the build cannot decode either, or any
    /// such track when the software decoder did not bind. Nothing else re-encodes it: a start that reaches this decision is
    /// refused with an explicit error (`player::FailureKind::AudioUnsupported`) instead of a Load
    /// whose audio clock never starts.
    NeedsSoftwareDecode { codec: String, channels: i64 },
}

/// The software audio path this build can run, as the feed decision needs it: decode -> PCM
/// (`ff::adec::available()`); a test passes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SoftwareAudio {
    /// The software decode -> PCM path (`ff::adec`) bound at boot.
    pub pcm: bool,
}

impl SoftwareAudio {
    /// No software path: the television's own decoders only.
    pub const NONE: SoftwareAudio = SoftwareAudio { pcm: false };

    /// What this device can do, as detected at `ff::boot`.
    pub fn detected() -> SoftwareAudio {
        SoftwareAudio { pcm: crate::ff::adec::available() }
    }
}

impl AudioFeed {
    /// The decision for `codec` with `channels` on a set whose table is `caps`, with the software
    /// path `sw` this build can run.
    pub fn decide(codec: &str, channels: i64, caps: &tv_platform::devcaps::Caps, sw: SoftwareAudio) -> AudioFeed {
        let codec = codec.to_ascii_lowercase();
        if codec.is_empty() {
            return AudioFeed::Silent;
        }
        // LG's buffer-feed vocabulary (libpf `CustomPipeline::parseOptionStringSpi`); the diag
        // code is `SHARED.dg_load_a`'s. Anything else has no name the pipeline accepts.
        let named = match codec.as_str() {
            "ac3" => Some(("AC3", 1)),
            "eac3" => Some(("AC3 PLUS", 2)),
            "aac" => Some(("AAC", 3)),
            "mp3" => Some(("MP3", 7)),
            "ac4" => Some(("AC4", 8)),
            _ => None,
        };
        match named {
            // AC-4 has no table row (and no channel count without a decoder): the firmware rule.
            Some((payload, diag)) if codec == "ac4" && caps.ac4 => AudioFeed::Native { payload, diag },
            Some((payload, diag)) if caps.audio_supports(&codec, channels) => AudioFeed::Native { payload, diag },
            // A codec the pipeline has no name for (MP2), one the set's table does not list (MP3
            // without the MPEG row, everything on webOS 10+) or lists below the track's channels,
            // decoded here and fed as PCM.
            _ if sw.pcm && crate::ff::adec::is_soft(&codec) => {
                AudioFeed::SoftPcm { channels: crate::ff::adec::sink_channels(channels), source: codec }
            }
            _ => AudioFeed::NeedsSoftwareDecode { codec, channels },
        }
    }

    /// What the track becomes on its way to the television, for a track menu: `""` as it is, else
    /// the format it is fed as.
    pub fn fed_as(&self) -> &'static str {
        match self {
            AudioFeed::SoftPcm { .. } => "PCM",
            _ => "",
        }
    }

    /// How good a feed this is, for choosing among a file's audio tracks: 0 the television
    /// decodes it as demuxed, 1 it plays through the software path, 2 it cannot play at all.
    /// `Silent` is not a track.
    pub fn rank(&self) -> u8 {
        match self {
            AudioFeed::Native { .. } | AudioFeed::Silent => 0,
            AudioFeed::SoftPcm { .. } => 1,
            AudioFeed::NeedsSoftwareDecode { .. } => 2,
        }
    }

    /// The Load payload's audio codec name and the diagnostics code, when the television decodes
    /// the track.
    pub fn payload(&self) -> Option<(&'static str, u8)> {
        match self {
            AudioFeed::Native { payload, diag } => Some((payload, *diag)),
            AudioFeed::SoftPcm { .. } => Some(("PCM", 6)),
            AudioFeed::Silent | AudioFeed::NeedsSoftwareDecode { .. } => None,
        }
    }
}

/// **What the probe read off the container** (`ff::probe`): every video and audio stream, as
/// plain data, in FILE order - the order `PlaybackStart::audio_track` and the demuxer's
/// `desired_audio_idx` count audio streams in. Filled by the FFmpeg side, consumed by the pure
/// [`derive_declaration`], so the decision is testable without a demuxer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbedStreams {
    /// The video stream libavformat ranks best (`av_find_best_stream`), if any.
    pub video: Option<ProbedVideo>,
    /// Every audio stream, file order.
    pub audio: Vec<ProbedAudio>,
    /// The container's duration, nanoseconds; 0 when it does not say.
    pub duration_ns: i64,
    /// Where the container's timeline starts, nanoseconds; 0 when it does not say. A file starts
    /// at ~0; a live MPEG-TS joined mid-stream starts wherever its encoder's clock stands.
    pub start_ns: i64,
}

/// Does a fresh start need its first keyframe rebased to fed-pts 0? The TV's buffer-feed
/// pipeline starts its clock at 0 on every Load; a stream whose timeline starts far from 0 (a live
/// channel at ~3581 s) is fed "an hour in the future" otherwise and is never presented.
pub fn needs_fresh_rebase(start_ns: i64) -> bool {
    start_ns > FRESH_REBASE_AFTER_NS
}

/// Timelines that start later than this are rebased (files start within a second or two).
pub const FRESH_REBASE_AFTER_NS: i64 = 10_000_000_000;

/// One probed video stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbedVideo {
    /// FFmpeg's codec name (`avcodec_get_name`): `h264`, `hevc`, `av1`, `vp9`, ...
    pub codec: String,
    pub width: i32,
    pub height: i32,
    /// `AVStream.avg_frame_rate`, else `r_frame_rate`; 0.0 when neither is known.
    pub fps: f64,
    /// The stream-level Dolby Vision configuration record, read as [`Dovi`].
    pub dovi: Dovi,
    /// FFmpeg's profile id for the stream (`AVCodecParameters.profile`); -99 when unknown.
    pub profile: i32,
}

/// One probed audio stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbedAudio {
    /// FFmpeg's codec name: `aac`, `ac3`, `eac3`, `mp2`, ...
    pub codec: String,
    /// `ch_layout.nb_channels`; 0 when unknown.
    pub channels: i64,
    pub sample_rate: i64,
    /// The container flags it as the default track (`AV_DISPOSITION_DEFAULT`).
    pub default: bool,
    /// E-AC3 JOC (Dolby Atmos): `codecpar.profile`, which only the E-AC-3 decoder sets and the
    /// bundled build does not carry, else the probe's own read of the first frame's `addbsi`
    /// (`ff::eac3_joc`).
    pub atmos: bool,
    /// The container's track name (`title`/`name` tag), for a track menu.
    pub name: String,
    /// The `language` tag, if any.
    pub language: String,
    /// The container flags it forced (`AV_DISPOSITION_FORCED`).
    pub forced: bool,
}

/// **Why a probe produced no declaration.** Each is an explicit refusal of the start
/// (`session::StartRefusal::Probe`), never a Load and never a spinner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeFailure {
    /// The bundled FFmpeg is not loaded (or is not the ABI this build was made for), or the probe
    /// thread could not be started.
    Unavailable,
    /// The stream would not open: transport, HTTP status, or a container libavformat rejects.
    Open,
    /// The container opened but its streams could not be read (`avformat_find_stream_info`).
    StreamInfo,
    /// The stream carries no video track.
    NoVideo,
    /// The probe did not finish inside `player::probe::PROBE_BUDGET`.
    Timeout,
    /// The link answered something that is plainly not a video (`ff::probe`'s sniff of the first
    /// bytes): the addon picked the wrong file in a torrent, or the service answered a page.
    NotVideo(NotVideo),
}

/// **What a stream answered instead of a video**, told apart by its first bytes (or, when they
/// say nothing, its `Content-Type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotVideo {
    /// PNG, JPEG, GIF, WebP: an image that sat in the torrent.
    Image,
    /// ZIP or RAR.
    Archive,
    /// JSON or plain text (an `.nfo`, a subtitle, a service's text answer).
    Document,
    /// An HTML page. The one kind worth the waiting retries: a debrid link that is not ready yet
    /// can answer a page for a moment.
    WebPage,
}

/// PURE: **the declaration a probed stream is Loaded with**, and the 0-based audio stream to feed
/// (`None`: no audio track at all).
///
/// The audio track is the caller's `audio_track` when the file has it. Otherwise it is chosen,
/// in this order: the best FEED (`feed_of(track).rank()`: a codec the television decodes, then one
/// the software path plays, then one nothing can play), then the track the container flags as
/// default, then an Atmos (E-AC-3 JOC) track, then file order. "Default first" alone would pick
/// an undecodable track over the AC-3 one beside it and refuse a stream that plays; "first
/// decodable" alone would ignore the muxer's choice of language among equally playable tracks.
///
/// With an audio language order (`prefs.audio`, Settings > Playback), a track's place in it comes
/// right after "can it play at all": a Polish track the software path plays beats a Russian one
/// the television decodes, but never one nothing can play (that would refuse a stream that plays).
/// The pick is made here, before the Load, so the declaration (codec, channels, Atmos) is the
/// chosen track's and no reload follows.
pub fn derive_declaration(
    probed: &ProbedStreams,
    audio_track: Option<i32>,
    prefs: &tv_base::tracks::TrackPrefs,
    feed_of: impl Fn(&ProbedAudio) -> AudioFeed,
) -> Result<(StreamDeclaration, Option<i32>), ProbeFailure> {
    let video = probed.video.as_ref().ok_or(ProbeFailure::NoVideo)?;
    let axis = |v: i32| u16::try_from(v.max(0)).unwrap_or(u16::MAX);
    let explicit = audio_track
        .and_then(|t| usize::try_from(t).ok())
        .filter(|&t| t < probed.audio.len());
    let chosen = explicit.or_else(|| {
        probed
            .audio
            .iter()
            .enumerate()
            .min_by_key(|(i, a)| {
                let rank = feed_of(a).rank();
                (rank == 2, prefs.audio_rank(&a.language), rank, !a.default, !a.atmos, *i)
            })
            .map(|(i, _)| i)
    });
    let mut decl = StreamDeclaration {
        vcodec: video.codec.to_ascii_lowercase(),
        fps: if video.fps.is_finite() && video.fps > 0.0 { video.fps } else { 0.0 },
        raster: (axis(video.width), axis(video.height)),
        vprofile: video.profile,
        dovi: video.dovi,
        ..StreamDeclaration::default()
    };
    if let Some(a) = chosen.map(|i| &probed.audio[i]) {
        decl.acodec = a.codec.to_ascii_lowercase();
        decl.audio_channels = a.channels.max(0);
        decl.audio_sample_rate = a.sample_rate.max(0);
        decl.atmos = a.atmos;
    }
    Ok((decl, chosen.map(|i| i as i32)))
}


#[cfg(test)]
mod tests {
    use super::*;
    use tv_platform::devcaps::dv::DvCapability;
    use tv_platform::devcaps::Caps;
    use tv_base::tracks::TrackPrefs;

    const PCM: SoftwareAudio = SoftwareAudio { pcm: true };

    #[test]
    fn the_pipelines_own_codecs_are_native_and_everything_else_needs_a_software_decoder() {
        let c = Caps::assumed();
        assert_eq!(AudioFeed::decide("eac3", 6, &c, SoftwareAudio::NONE).payload(), Some(("AC3 PLUS", 2)));
        assert_eq!(AudioFeed::decide("ac3", 6, &c, SoftwareAudio::NONE).payload(), Some(("AC3", 1)));
        assert_eq!(AudioFeed::decide("AAC", 2, &c, SoftwareAudio::NONE).payload(), Some(("AAC", 3)), "codec names are case-insensitive");
        for codec in ["mp2", "mp3", "dts", "truehd", "flac", "opus", "pcm_s16le"] {
            assert!(
                matches!(AudioFeed::decide(codec, 2, &c, SoftwareAudio::NONE), AudioFeed::NeedsSoftwareDecode { .. }),
                "{codec}"
            );
            assert_eq!(AudioFeed::decide(codec, 2, &c, SoftwareAudio::NONE).payload(), None, "{codec} is never declared");
        }
        assert_eq!(AudioFeed::decide("", 0, &c, SoftwareAudio::NONE), AudioFeed::Silent);
    }

    /// A listed codec past its measured channel ceiling is not the decoder's: the software path
    /// decodes it to stereo when the build can, and it is refused when it cannot.
    #[test]
    fn a_track_past_its_codecs_ceiling_needs_a_software_decoder() {
        let mut c = Caps::assumed();
        c.audio_channels.insert("ac3".into(), 6);
        assert_eq!(AudioFeed::decide("ac3", 6, &c, PCM), AudioFeed::Native { payload: "AC3", diag: 1 });
        assert_eq!(AudioFeed::decide("ac3", 8, &c, PCM), AudioFeed::SoftPcm { source: "ac3".into(), channels: 2 });
        assert_eq!(AudioFeed::decide("ac3", 8, &c, SoftwareAudio::NONE), AudioFeed::NeedsSoftwareDecode { codec: "ac3".into(), channels: 8 });
    }

    /// **MP3 and AC-4 go to the television's own decoders**, measured on an LG C1 (webOS 6.4):
    /// `"MP3"` played from Matroska, MP4, MPEG-TS and AVI, `"AC4"` from an ATSC 3.0 capture. MP3 is
    /// native where the table lists an `MPEG` audio row (read as `mp3`), AC-4 where `caps.ac4`
    /// says libpf has its caps; otherwise MP3 is decoded to PCM and AC-4 refused. MP2 declared as
    /// `"MP3"` presented nothing there, so it is decoded to PCM.
    #[test]
    fn mp3_and_ac4_are_native_where_the_set_takes_them_and_mp2_is_pcm() {
        let mut c1 = Caps::assumed();
        c1.audio = "aac,ac3,eac3,mp3".into();
        c1.audio_channels.insert("mp3".into(), 2);
        c1.ac4 = true;
        assert_eq!(AudioFeed::decide("mp3", 2, &c1, PCM), AudioFeed::Native { payload: "MP3", diag: 7 });
        assert_eq!(AudioFeed::decide("ac4", 0, &c1, PCM), AudioFeed::Native { payload: "AC4", diag: 8 });
        assert_eq!(AudioFeed::decide("mp2", 2, &c1, PCM), AudioFeed::SoftPcm { source: "mp2".into(), channels: 2 });
        let old = Caps::assumed();
        assert_eq!(AudioFeed::decide("mp3", 2, &old, PCM), AudioFeed::SoftPcm { source: "mp3".into(), channels: 2 });
        assert!(matches!(AudioFeed::decide("ac4", 0, &old, PCM), AudioFeed::NeedsSoftwareDecode { .. }));
    }

    /// **A set whose feed takes no compressed audio** (webOS 10+, where devcaps drops every audio
    /// row): every codec the build decodes becomes stereo PCM, the rest is refused, and without the
    /// decoder every track is refused.
    #[test]
    fn a_set_without_audio_rows_decodes_every_track_here() {
        let mut c = Caps::assumed();
        c.audio.clear();
        c.ac4 = false;
        let es = PCM;
        for (codec, ch) in [("aac", 2), ("AC3", 6), ("eac3", 6), ("mp2", 2), ("mp3", 2), ("aac_latm", 2)] {
            let want = AudioFeed::SoftPcm { source: codec.to_ascii_lowercase(), channels: 2 };
            assert_eq!(AudioFeed::decide(codec, ch, &c, es), want, "{codec}");
        }
        for codec in ["dts", "ac4", "truehd", "opus"] {
            assert!(matches!(AudioFeed::decide(codec, 2, &c, es), AudioFeed::NeedsSoftwareDecode { .. }), "{codec}");
        }
        let no_decoder = SoftwareAudio::NONE;
        assert!(matches!(AudioFeed::decide("ac3", 6, &c, no_decoder), AudioFeed::NeedsSoftwareDecode { .. }));
        assert_eq!(AudioFeed::decide("", 0, &c, es), AudioFeed::Silent);
    }

    /// MP2 is decoded and declared as PCM (stereo: MPEG audio carries at most two channels, and
    /// `ff::adec` downmixes or upmixes to them) when the build can decode it; without the decoder
    /// it stays refused; the TV's own codecs never take the software path.
    #[test]
    fn mp2_becomes_pcm_when_the_build_can_decode_it() {
        let caps = Caps::assumed();
        assert_eq!(AudioFeed::decide("mp2", 2, &caps, SoftwareAudio::NONE), AudioFeed::NeedsSoftwareDecode { codec: "mp2".into(), channels: 2 });
        for (ch, want) in [(1, 2), (2, 2), (0, 2), (6, 2)] {
            assert_eq!(AudioFeed::decide("MP2", ch, &caps, PCM), AudioFeed::SoftPcm { source: "mp2".into(), channels: want }, "{ch}ch");
        }
        let pcm = AudioFeed::decide("mp2", 2, &caps, PCM);
        assert_eq!((pcm.payload(), pcm.rank(), pcm.fed_as()), (Some(("PCM", 6)), 1, "PCM"), "the Load declares PCM, below a native track");
        for (codec, native) in [("aac", "AAC"), ("ac3", "AC3"), ("eac3", "AC3 PLUS")] {
            assert_eq!(AudioFeed::decide(codec, 6, &caps, PCM).payload().map(|p| p.0), Some(native), "{codec} stays native");
        }
        assert!(matches!(AudioFeed::decide("flac", 2, &caps, PCM), AudioFeed::NeedsSoftwareDecode { .. }), "a codec this path does not take");
        assert_eq!((AudioFeed::Native { payload: "AAC", diag: 3 }.rank(), AudioFeed::NeedsSoftwareDecode { codec: "x".into(), channels: 2 }.rank()), (0, 2));
    }

    #[test]
    fn dolby_vision_is_declared_only_where_it_is_affirmatively_supported() {
        let p5 = Dovi { present: true, profile: 5, bl_compat: 0, el_present: false };
        let p8 = Dovi { present: true, profile: 8, bl_compat: 1, el_present: false };
        assert!(p5.presentation(true, DvCapability::Supported, true).declared().is_some());
        assert_eq!(p5.presentation(true, DvCapability::Unknown, true).refusal(), Some("no cross-compatible base layer"));
        assert_eq!(p8.presentation(true, DvCapability::Unsupported, true), DvPresentation::NotDv, "P8's base layer is HDR10");
        assert_eq!(Dovi::NONE.presentation(true, DvCapability::Supported, true), DvPresentation::NotDv);
        assert!(p5.presentation(true, DvCapability::Supported, false).refusal().is_some(), "never declared over non-HEVC");
        let native = p8.presentation(true, DvCapability::Supported, true);
        assert_eq!(native.declared(), Some(DolbyHdrInfo { profile_id: 8, track_type: "single", encryption_type: "clear" }));
        assert!(!native.strips_layers(), "a Profile 8 stream is fed untouched");
    }

    /// Profile 7 (and any enhancement layer): the base layer alone, whatever the set answers, for
    /// every compatible base layer (6 HDR10 on Blu-ray, 1 HDR10, 2 SDR, 4 HLG).
    #[test]
    fn profile_7_plays_its_base_layer() {
        let dual = |profile, bl_compat| Dovi { present: true, profile, bl_compat, el_present: true };
        for compat in [6, 1, 2, 4] {
            for cap in [DvCapability::Supported, DvCapability::Unknown] {
                let p = dual(7, compat).presentation(true, cap, true);
                assert_eq!(p, DvPresentation::BaseLayer, "compat {compat}");
                assert!(p.strips_layers() && p.declared().is_none());
            }
        }
        assert_eq!(dual(7, 0).presentation(true, DvCapability::Supported, true), DvPresentation::BaseLayer, "P7's base layer is HDR10 by definition");
        assert_eq!(dual(4, 2).presentation(true, DvCapability::Unsupported, true), DvPresentation::BaseLayer, "an SDR base layer");
        assert_eq!(dual(9, 0).presentation(true, DvCapability::Supported, true).refusal(), Some("dual-layer"), "no compatible base layer");
        assert_eq!(dual(7, 6).presentation(true, DvCapability::Supported, false).refusal(), Some("dual-layer"), "never over non-HEVC");
    }

    /// The audio language order ranks right after "can it play at all": a Polish track beats the
    /// default Russian one, but a Polish track nothing can play never refuses a stream that plays.
    #[test]
    fn the_language_order_ranks_after_playability() {
        let pl = TrackPrefs { audio: vec!["PL".into(), "RU".into()] };
        let mut p = probed(Some("hevc"), &[("ac3", 6, true), ("eac3", 6, false)]);
        (p.audio[0].language, p.audio[1].language) = ("rus".into(), "pol".into());
        let (d, t) = derive_declaration(&p, None, &pl, feed(SoftwareAudio::NONE)).unwrap();
        assert_eq!((t, d.acodec.as_str()), (Some(1), "eac3"), "declared for the Polish track");
        p.audio[1].codec = "truehd".into();
        assert_eq!(derive_declaration(&p, None, &pl, feed(SoftwareAudio::NONE)).unwrap().1, Some(0), "unplayable Polish loses");
    }

    /// An E-AC-3 JOC (Atmos) track beats an MP2 track the software path would decode to PCM, and
    /// among equally playable tracks Atmos wins after the muxer's default.
    #[test]
    fn an_atmos_track_is_preferred_over_soft_decoded_mp2() {
        let video = ProbedVideo { codec: "hevc".into(), width: 3840, height: 2160, fps: 25.0, dovi: Dovi::NONE, profile: -99 };
        let track = |codec: &str, atmos: bool, default: bool| ProbedAudio { codec: codec.into(), channels: 6, sample_rate: 48000, atmos, default, ..ProbedAudio::default() };
        let mut caps = Caps::assumed();
        caps.audio = "aac,ac3,eac3".into();
        let feed = |a: &ProbedAudio| AudioFeed::decide(&a.codec, a.channels, &caps, PCM);
        let probed = ProbedStreams { video: Some(video.clone()), audio: vec![track("mp2", false, true), track("eac3", true, false)], ..ProbedStreams::default() };
        let (decl, t) = derive_declaration(&probed, None, &TrackPrefs::default(), feed).unwrap();
        assert_eq!((t, decl.acodec.as_str(), decl.atmos), (Some(1), "eac3", true));
        let probed = ProbedStreams { video: Some(video), audio: vec![track("ac3", false, false), track("eac3", true, false)], ..ProbedStreams::default() };
        assert_eq!(derive_declaration(&probed, None, &TrackPrefs::default(), feed).unwrap().1, Some(1), "Atmos before file order");
    }

    /// A live channel's timeline starts wherever its encoder's clock stands (~3581 s in the beta's
    /// device log), and nothing was presented until it was rebased; a file's ~0 start never is.
    #[test]
    fn only_a_timeline_far_from_zero_is_rebased() {
        assert!(needs_fresh_rebase(3_581_400_000_000));
        assert!(!needs_fresh_rebase(0));
        assert!(!needs_fresh_rebase(1_400_000_000), "a file starting 1.4 s in is fed as it is");
        assert!(!needs_fresh_rebase(FRESH_REBASE_AFTER_NS));
    }

    fn probed(video: Option<&str>, audio: &[(&str, i64, bool)]) -> ProbedStreams {
        ProbedStreams {
            video: video.map(|c| ProbedVideo {
                codec: c.into(),
                width: 3840,
                height: 2160,
                fps: 24000.0 / 1001.0,
                dovi: Dovi { present: true, profile: 8, bl_compat: 1, el_present: false },
                profile: -99,
            }),
            start_ns: 0,
            audio: audio
                .iter()
                .map(|&(codec, channels, default)| ProbedAudio {
                    codec: codec.into(),
                    channels,
                    sample_rate: 48_000,
                    default,
                    ..ProbedAudio::default()
                })
                .collect(),
            ..ProbedStreams::default()
        }
    }

    fn feed(sw: SoftwareAudio) -> impl Fn(&ProbedAudio) -> AudioFeed {
        let mut caps = Caps::assumed();
        caps.audio = "aac,ac3,eac3".into();
        move |a: &ProbedAudio| AudioFeed::decide(&a.codec, a.channels, &caps, sw)
    }

    /// The declaration is the probed video stream's codec, raster, frame rate and Dolby Vision
    /// record, plus the chosen audio track's codec and channel count.
    #[test]
    fn the_declaration_is_derived_from_the_probed_streams() {
        let p = probed(Some("HEVC"), &[("eac3", 6, true)]);
        let (d, track) = derive_declaration(&p, None, &TrackPrefs::default(), feed(SoftwareAudio::NONE)).unwrap();
        assert_eq!(track, Some(0));
        assert_eq!(
            d,
            StreamDeclaration {
                vcodec: "hevc".into(),
                acodec: "eac3".into(),
                audio_channels: 6,
                audio_sample_rate: 48_000,
                fps: 24000.0 / 1001.0,
                raster: (3840, 2160),
                vprofile: -99,
                dovi: Dovi { present: true, profile: 8, bl_compat: 1, el_present: false },
                atmos: false,
            }
        );
        let (silent, none) = derive_declaration(&probed(Some("h264"), &[]), None, &TrackPrefs::default(), feed(SoftwareAudio::NONE)).unwrap();
        assert_eq!((silent.acodec.as_str(), none), ("", None), "no audio stream: a video-only declaration");
        assert_eq!(
            derive_declaration(&probed(None, &[("aac", 2, true)]), None, &TrackPrefs::default(), feed(SoftwareAudio::NONE)),
            Err(ProbeFailure::NoVideo),
            "an audio-only stream is an explicit refusal, not a Load"
        );
        let mut odd = probed(Some("h264"), &[]);
        odd.video.as_mut().unwrap().fps = f64::NAN;
        odd.video.as_mut().unwrap().width = 70_000;
        let (d, _) = derive_declaration(&odd, None, &TrackPrefs::default(), feed(SoftwareAudio::NONE)).unwrap();
        assert_eq!((d.fps, d.raster.0), (0.0, u16::MAX), "an unreadable rate is unknown, a huge raster saturates");
    }

    /// The audio track: the caller's when the file has it, else the best feed, then the
    /// container's default, then file order.
    #[test]
    fn the_audio_track_prefers_a_playable_feed_then_the_default_then_file_order() {
        let p = probed(Some("hevc"), &[("truehd", 8, true), ("mp2", 2, false), ("ac3", 6, false), ("ac3", 2, true)]);
        let pick = |p: &ProbedStreams, t: Option<i32>, sw| derive_declaration(p, t, &TrackPrefs::default(), feed(sw)).unwrap();
        assert_eq!(pick(&p, None, SoftwareAudio::NONE).1, Some(3), "a playable default beats a playable non-default");
        let (d, t) = pick(&p, Some(1), PCM);
        assert_eq!((t, d.acodec.as_str(), d.audio_channels), (Some(1), "mp2", 2), "the caller's pick is kept, even if it is not the best");
        assert_eq!(pick(&p, Some(9), SoftwareAudio::NONE).1, Some(3), "a pick past the last track falls back to the automatic choice");
        let mp2_only = probed(Some("hevc"), &[("truehd", 8, true), ("mp2", 2, false)]);
        assert_eq!(pick(&mp2_only, None, PCM).1, Some(1), "the decodable MP2 beats the unplayable default");
        assert_eq!(pick(&mp2_only, None, SoftwareAudio::NONE).1, Some(0), "nothing plays: the default, which the session then refuses");
    }

    /// A DVB restream: MP2 default, then an AC-3 track. The TV decodes the AC-3, so it is fed
    /// natively; the MP2 is decoded only when the viewer picks it.
    #[test]
    fn a_natively_playable_track_beats_a_soft_decoded_one() {
        let dvb = probed(Some("h264"), &[("mp2", 2, true), ("ac3", 6, false)]);
        let (d, t) = derive_declaration(&dvb, None, &TrackPrefs::default(), feed(PCM)).unwrap();
        assert_eq!((t, d.acodec.as_str()), (Some(1), "ac3"));
        let (d, t) = derive_declaration(&dvb, Some(0), &TrackPrefs::default(), feed(PCM)).unwrap();
        assert_eq!((t, d.acodec.as_str(), d.audio_channels), (Some(0), "mp2", 2), "the viewer may still pick the MP2");
    }
}
