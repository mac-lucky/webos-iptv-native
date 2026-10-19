//! **Software audio decode -> PCM** - the path behind [`crate::stream::AudioFeed::SoftPcm`].
//!
//! Ported from PlxNative's `media/src/adec.rs`. The IPTV audio the television's buffer feed has no
//! decoder for - MPEG-1 Layer II (a DVB restream's usual audio, which the C1 does not present when
//! declared `"MP3"`), MP3 where the set's table has no MPEG audio row, and on webOS 10+, whose
//! feed takes no compressed audio (`tv_platform::devcaps` drops every row there), every track -
//! AAC, AAC-LATM, AC-3 and E-AC-3 included - is decoded here by the bundled libavcodec and resampled with libswresample to S16LE 48 kHz stereo (any layout:
//! 5.1 and 7.1 are downmixed with swr's default matrix, LFE dropped, mono is upmixed), and the Load
//! declares `"PCM"` with a `pcmInfo` node of exactly that shape. The decode loop is
//! StremioNative's `native/adec.c` (a C shim upstream) in Rust, calling the same
//! libraries as the demuxer by the same absolute paths. Everything goes through FFmpeg calls and
//! AVOptions except the decoded frame's fields, read from the [`AVFrame`] model below, whose
//! offsets are pinned for both pointer widths.
//!
//! # Timestamps
//!
//! Decoded PCM is batched into access units of at least [`PCM_BATCH_MS`], so a decoder that hands
//! back short frames does not cost one Starfish feed each, and each batch is stamped by COUNTING
//! its 48 kHz samples on a [`SampleClock`], anchored at the decoder's first frame pts: strictly
//! monotonic through Matroska's millisecond rounding and MPEG-TS jitter, re-anchored only past
//! [`REANCHOR_NS`] (a gap in the stream). A seek calls [`PcmFeed::reset`].

use super::*;
use std::ptr::null_mut;

// The entry points the demuxer's tables (ff.rs) do not bind, from the same three libraries by the
// same candidate names: `dlopen` of a path `ff::boot` already opened hands back that handle, so
// these resolve in exactly the libraries the demuxer uses, never the television's own FFmpeg.
tv_base::dynlib! {
    adec_avcodec: ["libavcodec-wi.so.63", "libavcodec-wi.63.dylib"] {
        fn avcodec_descriptor_get_by_name(name: *const c_char) -> *const AVCodecDescriptor;
        fn avcodec_find_decoder(id: c_int) -> *const AVCodec;
        fn avcodec_parameters_to_context(ctx: *mut AVCodecContext, par: *const AVCodecParameters) -> c_int;
        fn avcodec_send_packet(ctx: *mut AVCodecContext, pkt: *const AVPacket) -> c_int;
        fn avcodec_receive_frame(ctx: *mut AVCodecContext, frame: *mut AVFrame) -> c_int;
        fn avcodec_flush_buffers(ctx: *mut AVCodecContext);
    }
}
tv_base::dynlib! {
    adec_avutil: ["libavutil-wi.so.61", "libavutil-wi.61.dylib"] {
        fn av_frame_unref(frame: *mut AVFrame);
        fn av_opt_set_q(obj: *mut c_void, name: *const c_char, val: AVRational, search_flags: c_int) -> c_int;
        fn av_channel_layout_default(layout: *mut AVChannelLayout, nb_channels: c_int);
        fn av_channel_layout_copy(dst: *mut AVChannelLayout, src: *const AVChannelLayout) -> c_int;
        fn av_channel_layout_compare(a: *const AVChannelLayout, b: *const AVChannelLayout) -> c_int;
        fn av_channel_layout_uninit(layout: *mut AVChannelLayout);
    }
}
tv_base::dynlib! {
    adec_swr: ["libswresample-wi.so.7", "libswresample-wi.7.dylib"] {
        // The C signature: nine arguments.
        #[allow(clippy::too_many_arguments)]
        fn swr_alloc_set_opts2(
            ps: *mut *mut SwrContext,
            out_layout: *const AVChannelLayout, out_fmt: c_int, out_rate: c_int,
            in_layout: *const AVChannelLayout, in_fmt: c_int, in_rate: c_int,
            log_offset: c_int, log_ctx: *mut c_void,
        ) -> c_int;
        fn swr_init(s: *mut SwrContext) -> c_int;
        fn swr_get_out_samples(s: *mut SwrContext, in_samples: c_int) -> c_int;
        fn swr_convert(s: *mut SwrContext, out: *const *mut u8, out_count: c_int, inp: *const *const u8, in_count: c_int) -> c_int;
        fn swr_free(s: *mut *mut SwrContext);
    }
}

/// libswresample's major. `SwrContext` is opaque and only called, so its API is the whole ABI; the
/// other two libraries' majors are `ff::boot`'s check.
const SWR_MAJOR: c_uint = 7;

pub enum SwrContext {}

/// `AVCodecDescriptor`: only `id`, its first field, is read. Library-owned, never constructed.
#[repr(C)]
pub struct AVCodecDescriptor {
    id: c_int,
}

/// `AVFrame` (FFmpeg 9.0, libavutil/frame.h) through `ch_layout`, the last field read here; the
/// struct goes on past it (`duration`, `alpha_mode`). Allocated by `av_frame_alloc` and only read:
/// NEVER construct or stack-allocate one. Every field up to `ch_layout` is modelled so the
/// compiler lays it out by the target's C ABI, and the asserts below pin the fields read. All but
/// `best_effort_timestamp` are numbers StremioNative's `ci/ffabi-assert.c` proved against the 9.0
/// headers; all seven were checked with `offsetof` against the 9.0.2 headers for armv7 and the
/// 64-bit host.
#[repr(C)]
pub struct AVFrame {
    data: [*mut u8; 8],
    linesize: [c_int; 8],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
    pict_type: c_int,
    sample_aspect_ratio: AVRational,
    pts: i64,
    pkt_dts: i64,
    time_base: AVRational,
    quality: c_int,
    opaque: *mut c_void,
    repeat_pict: c_int,
    sample_rate: c_int,
    buf: [*mut AVBufferRef; 8],
    extended_buf: *mut *mut AVBufferRef,
    nb_extended_buf: c_int,
    side_data: *mut *mut c_void,
    nb_side_data: c_int,
    flags: c_int,
    color_range: c_int,
    color_primaries: c_int,
    color_trc: c_int,
    colorspace: c_int,
    chroma_location: c_int,
    best_effort_timestamp: i64,
    metadata: *mut AVDictionary,
    decode_error_flags: c_int,
    hw_frames_ctx: *mut AVBufferRef,
    opaque_ref: *mut AVBufferRef,
    crop_top: usize,
    crop_bottom: usize,
    crop_left: usize,
    crop_right: usize,
    private_ref: *mut c_void,
    ch_layout: AVChannelLayout,
}
#[cfg(target_pointer_width = "32")]
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(AVFrame, extended_data) == 64);
    assert!(offset_of!(AVFrame, nb_samples) == 76);
    assert!(offset_of!(AVFrame, format) == 80);
    assert!(offset_of!(AVFrame, pts) == 96);
    assert!(offset_of!(AVFrame, sample_rate) == 132);
    assert!(offset_of!(AVFrame, best_effort_timestamp) == 208);
    assert!(offset_of!(AVFrame, ch_layout) == 256);
};
#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(AVFrame, extended_data) == 96);
    assert!(offset_of!(AVFrame, nb_samples) == 112);
    assert!(offset_of!(AVFrame, format) == 116);
    assert!(offset_of!(AVFrame, pts) == 136);
    assert!(offset_of!(AVFrame, sample_rate) == 180);
    assert!(offset_of!(AVFrame, best_effort_timestamp) == 304);
    assert!(offset_of!(AVFrame, ch_layout) == 384);
};

/// `AV_SAMPLE_FMT_S16`, `AV_CHANNEL_ORDER_UNSPEC` / `_NATIVE` and the `AV_CH_LAYOUT_STEREO` mask
/// (FL|FR).
const AV_SAMPLE_FMT_S16: c_int = 1;
const AV_CHANNEL_ORDER_UNSPEC: c_int = 0;
const AV_CHANNEL_ORDER_NATIVE: c_int = 1;
const AV_CH_LAYOUT_STEREO: u64 = 0x3;

/// An empty `AVChannelLayout` (what `av_channel_layout_uninit` leaves).
fn no_layout() -> AVChannelLayout {
    AVChannelLayout { order: AV_CHANNEL_ORDER_UNSPEC, nb_channels: 0, u_mask: 0, opaque: null_mut() }
}

/// The layout the sink is declared ([`PCM_CHANNELS`]).
fn stereo_layout() -> AVChannelLayout {
    AVChannelLayout { order: AV_CHANNEL_ORDER_NATIVE, nb_channels: PCM_CHANNELS as c_int, u_mask: AV_CH_LAYOUT_STEREO, opaque: null_mut() }
}

/// The codecs this path decodes, in FFmpeg's names (`avcodec_get_name`). The bundled build must
/// carry their decoders; `ff_adec_tests` proves each one is there.
pub const SOFT_CODECS: &[&str] = &["mp2", "mp3", "aac", "aac_latm", "ac3", "eac3"];

/// PURE: does this path decode `codec` (FFmpeg's name, any case)?
pub fn is_soft(codec: &str) -> bool {
    SOFT_CODECS.iter().any(|c| c.eq_ignore_ascii_case(codec))
}

/// Every PCM feed is 48 kHz stereo: [`PcmFeed`] resamples, downmixes and upmixes.
pub const PCM_RATE: u32 = 48_000;
pub const PCM_CHANNELS: u32 = 2;
/// The most PCM one packet may decode to: a second of [`PCM_RATE`] stereo S16.
const MAX_PACKET_PCM_BYTES: usize = PCM_RATE as usize * PCM_CHANNELS as usize * 2;
/// The smallest PCM access unit fed, in ms (decoded frames are batched up to it).
pub const PCM_BATCH_MS: u32 = 20;
/// A decode whose container pts is further than this from the sample count's prediction starts a
/// new timeline. Matroska rounds to the millisecond and MPEG-TS jitters by a few: 100 ms is well
/// clear of both and well under any real gap.
pub const REANCHOR_NS: i64 = 100_000_000;

/// PURE: the channels the sink is told for a source of `_src` channels: always [`PCM_CHANNELS`].
/// [`PcmFeed`] downmixes every wider layout (5.1 AC-3, E-AC-3 and AAC) and upmixes mono, and the
/// probed count is no promise anyway: a broadcast switches layout mid-stream (stereo adverts
/// around a 5.1 film).
pub fn sink_channels(_src: i64) -> u32 {
    PCM_CHANNELS
}

/// PURE: the `pcmInfo` object of a Load payload (shape from webosbrew's ss4s SMP driver, as
/// PlxNative fed it).
pub fn pcm_info_json(channels: u32, rate: u32) -> String {
    let mode = match channels {
        1 => "mono",
        _ => "stereo",
    };
    // sampleRate is in kHz, as a number: 48, 44.1.
    let khz = rate as f64 / 1000.0;
    format!(r#"{{"channelMode":"{mode}","format":"S16LE","sampleRate":{khz},"bitsPerSample":16,"layout":"interleaved"}}"#)
}

pub(super) static AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Did the decoder's entry points bind, on the libswresample this build expects? Latched by
/// [`detect`] at `ff::boot`; `false` keeps the explicit `AudioUnsupported` refusal for these codecs.
pub fn available() -> bool {
    AVAILABLE.load(Ordering::Acquire)
}

/// Bind the decoder from the app directory, once the bundled FFmpeg has loaded and passed its
/// major check, and ask for each decoder: a build without them binds the same symbols at the same
/// majors, and would otherwise be found out only by a failed open, past the feed decision.
pub(super) fn detect() -> bool {
    let ok = ABI_OK.load(Ordering::Acquire)
        && load_from(tv_base::paths::app_dir())
        && SOFT_CODECS.iter().all(|c| {
            let present = decoder_present(c);
            if !present {
                crate::player::log(&format!("ff: soft audio: the bundled libavcodec has no {c} decoder"));
            }
            present
        });
    AVAILABLE.store(ok, Ordering::Release);
    crate::player::log(&format!(
        "ff: software audio decode -> PCM {}",
        if ok { "available" } else { "UNAVAILABLE - audio the television cannot decode is refused" }
    ));
    ok
}

/// Bind the three tables above from `dir`, where the demuxer's own tables already loaded the
/// same libraries (`swresample_version` is theirs).
pub(super) fn load_from(dir: &std::path::Path) -> bool {
    let mut ok = true;
    for (what, verdict) in [
        ("avutil", adec_avutil::load(Some(dir))),
        ("swresample", adec_swr::load(Some(dir))),
        ("avcodec", adec_avcodec::load(Some(dir))),
    ] {
        match verdict {
            tv_base::dynlib::Loaded::Ok(_) => {}
            tv_base::dynlib::Loaded::NoLibrary => {
                ok = false;
                crate::player::log(&format!("ff: soft audio: no {what} in the app directory"));
            }
            tv_base::dynlib::Loaded::Incomplete(soname, n) => {
                ok = false;
                crate::player::log(&format!("ff: soft audio: {soname} is missing {n} symbol(s)"));
            }
        }
    }
    if !ok {
        return false;
    }
    // SAFETY: bound above (`ok`), no arguments.
    let major = unsafe { swresample_version() } >> 16;
    if major != SWR_MAJOR {
        crate::player::log(&format!("ff: soft audio: libswresample {major}, want {SWR_MAJOR}"));
    }
    major == SWR_MAJOR
}

/// Does the loaded libavcodec decode `codec` (FFmpeg's name)? Only once [`load_from`] bound the
/// tables.
pub(super) fn decoder_present(codec: &str) -> bool {
    // SAFETY: the two entry points are bound; `c` is NUL-terminated for the call, and a non-null
    // descriptor is a static FFmpeg table entry.
    CString::new(codec).is_ok_and(|c| unsafe {
        let d = avcodec_descriptor_get_by_name(c.as_ptr());
        !d.is_null() && !avcodec_find_decoder((*d).id).is_null()
    })
}

/// PURE: the sample-counting clock of the module doc. Sample indices are absolute (they never
/// restart), so the pts stay monotonic across a re-anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleClock {
    rate: i64,
    base_ns: i64,
    base_sample: i64,
    anchored: bool,
}

impl SampleClock {
    pub fn new(rate: i32) -> SampleClock {
        SampleClock { rate: i64::from(rate.max(1)), base_ns: 0, base_sample: 0, anchored: false }
    }

    /// Content time of absolute sample `s`.
    pub fn ns_of(&self, s: i64) -> i64 {
        let d = i128::from(s - self.base_sample) * 1_000_000_000 / i128::from(self.rate);
        self.base_ns.saturating_add(d.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
    }

    /// A decode stamped `pts_ns` is about to add samples starting at absolute sample `end`. Returns
    /// `true` when it starts a new timeline (one past [`REANCHOR_NS`]; the first one only anchors).
    pub fn on_packet(&mut self, pts_ns: i64, end: i64) -> bool {
        // Saturating: a hostile container can put a timestamp anywhere in i64.
        if self.anchored && pts_ns.saturating_sub(self.ns_of(end)).unsigned_abs() <= REANCHOR_NS as u64 {
            return false;
        }
        let first = !self.anchored;
        self.base_ns = pts_ns;
        self.base_sample = end;
        self.anchored = true;
        !first
    }

    /// Forget the anchor (a seek): the next decode anchors afresh.
    pub fn reset(&mut self) {
        self.anchored = false;
    }
}

/// One PCM access unit for the audio lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmAu {
    pub data: Vec<u8>,
    pub pts_ns: i64,
}

/// PURE: decoded S16 interleaved chunks in, sample-counted access units of at least
/// [`PCM_BATCH_MS`] out (see the module doc).
#[derive(Debug)]
pub struct PcmBatcher {
    frame_bytes: usize,
    clock: SampleClock,
    batch: Vec<u8>,
    /// Absolute index of the first sample in `batch`.
    batch_start: i64,
}

impl PcmBatcher {
    pub fn new(channels: u32) -> PcmBatcher {
        PcmBatcher { frame_bytes: channels.max(1) as usize * 2, clock: SampleClock::new(PCM_RATE as i32), batch: Vec::new(), batch_start: 0 }
    }

    fn min_bytes(&self) -> usize {
        PCM_RATE as usize * PCM_BATCH_MS as usize / 1000 * self.frame_bytes
    }

    /// Queue one decode's output, whose first sample the decoder stamps `pts_ns`.
    pub fn push(&mut self, pcm: &[u8], pts_ns: i64) -> Vec<PcmAu> {
        let mut out = Vec::new();
        if pcm.is_empty() {
            return out;
        }
        let end = self.batch_start + (self.batch.len() / self.frame_bytes) as i64;
        let old_pts = self.clock.ns_of(self.batch_start);
        if self.clock.on_packet(pts_ns, end) && !self.batch.is_empty() {
            // A new timeline: what was queued plays at the old one's time.
            out.push(PcmAu { data: std::mem::take(&mut self.batch), pts_ns: old_pts });
        }
        if self.batch.is_empty() {
            self.batch_start = end;
        }
        self.batch.extend_from_slice(&pcm[..pcm.len() - pcm.len() % self.frame_bytes]);
        if self.batch.len() >= self.min_bytes() {
            out.extend(self.take());
        }
        out
    }

    /// What is queued, however short (the end of the stream).
    pub fn take(&mut self) -> Option<PcmAu> {
        if self.batch.is_empty() {
            return None;
        }
        let pts_ns = self.clock.ns_of(self.batch_start);
        self.batch_start += (self.batch.len() / self.frame_bytes) as i64;
        Some(PcmAu { data: std::mem::take(&mut self.batch), pts_ns })
    }

    /// A seek: drop what is queued; the next decode anchors afresh.
    pub fn reset(&mut self) {
        self.batch_start += (self.batch.len() / self.frame_bytes) as i64;
        self.batch.clear();
        self.clock.reset();
    }
}

/// The decoder, its resampler and its batcher, owned by the demux thread. Every pointer is owned
/// and freed by `Drop`, null or not.
pub struct PcmFeed {
    ctx: *mut AVCodecContext,
    frame: *mut AVFrame,
    /// Built for the input shape below; rebuilt whenever a frame differs ([`PcmFeed::ensure_swr`]).
    swr: *mut SwrContext,
    in_layout: AVChannelLayout,
    in_fmt: c_int,
    in_rate: c_int,
    /// One decode's output: S16LE interleaved, `channels` x [`PCM_RATE`].
    buf: Vec<u8>,
    tb: AVRational,
    batcher: PcmBatcher,
    bad: u32,
    /// A packet has decoded: the track plays, and a bad stretch no longer fails it.
    decoded: bool,
    pub channels: u32,
}

// The FFmpeg objects are touched by the one thread that owns the value.
unsafe impl Send for PcmFeed {}

impl Drop for PcmFeed {
    fn drop(&mut self) {
        // SAFETY: the objects are this value's own; every free tolerates null and nulls the field.
        unsafe {
            avcodec_free_context(&mut self.ctx);
            av_frame_free(&mut self.frame as *mut *mut AVFrame as *mut *mut c_void);
            self.drop_swr();
        }
    }
}

/// Consecutive undecodable packets tolerated before a track that never decoded gives up. One that
/// has played rides a corrupt stretch out instead (a scrambled minute on a restream): the decoder
/// is flushed and the count starts over, as the television's own decoder would conceal it.
const MAX_BAD_PACKETS: u32 = 32;

impl PcmFeed {
    /// Open a decoder for the stream `par` with time base `tb`, producing `channels`, which must be
    /// [`PCM_CHANNELS`].
    ///
    /// # Safety
    /// `par` is a live `AVCodecParameters` of the bundled FFmpeg.
    pub unsafe fn open(par: *const AVCodecParameters, tb: AVRational, channels: u32) -> Result<PcmFeed, String> {
        if !available() {
            return Err("the software audio decoder did not load".into());
        }
        let codec = avcodec_find_decoder((*par).codec_id);
        if codec.is_null() {
            return Err(format!("no decoder for codec id {:#x}", (*par).codec_id));
        }
        if channels != PCM_CHANNELS {
            return Err(format!("{channels} channels asked for; the PCM feed is stereo"));
        }
        let feed = PcmFeed {
            ctx: avcodec_alloc_context3(codec),
            frame: av_frame_alloc() as *mut AVFrame,
            swr: null_mut(),
            in_layout: no_layout(),
            in_fmt: -1,
            in_rate: 0,
            buf: Vec::new(),
            tb,
            batcher: PcmBatcher::new(channels),
            bad: 0,
            decoded: false,
            channels,
        };
        if feed.ctx.is_null() || feed.frame.is_null() {
            return Err("out of memory".into());
        }
        let r = avcodec_parameters_to_context(feed.ctx, par);
        if r < 0 {
            return Err(format!("stream parameters refused ({r})"));
        }
        // The decoder's own pts bookkeeping (skipped samples) is in the stream's time base.
        av_opt_set_q(feed.ctx as *mut c_void, c"pkt_timebase".as_ptr(), tb, 0);
        let r = avcodec_open2(feed.ctx, codec, null_mut());
        if r < 0 {
            return Err(format!("open failed ({r})"));
        }
        Ok(feed)
    }

    /// Decode one packet (null: drain at the end); `fallback_ns` is its container pts in ns, for a
    /// decode the decoder could not stamp.
    ///
    /// # Safety
    /// `pkt` is null or a live `AVPacket` of the bundled FFmpeg.
    pub unsafe fn push(&mut self, pkt: *const AVPacket, fallback_ns: i64) -> Result<Vec<PcmAu>, String> {
        let (r, pts) = self.decode(pkt);
        if r < 0 {
            self.bad += 1;
            if self.bad <= 3 {
                crate::player::log(&format!("ff: soft audio: packet did not decode (r={r}); skipped"));
            }
            if self.bad < MAX_BAD_PACKETS {
                return Ok(Vec::new());
            }
            if !self.decoded {
                return Err(format!("{} packets in a row did not decode (r={r})", self.bad));
            }
            crate::player::log(&format!("ff: soft audio: {} packets in a row did not decode; decoder flushed", self.bad));
            self.reset();
            self.bad = 0;
            return Ok(Vec::new());
        }
        self.bad = 0;
        self.decoded = true;
        // A timestamp that rescales to i64::MIN reads as "no audio yet" downstream: take the fallback.
        let at = if pts == AV_NOPTS_VALUE { i64::MIN } else { av_rescale_q(pts, self.tb, NS_TB) };
        let at = if at == i64::MIN { fallback_ns } else { at };
        Ok(self.batcher.push(&self.buf, at))
    }

    /// Send `pkt` and convert every frame it yields into `buf`. Returns 0 or a negative AVERROR,
    /// and the first frame's pts in the stream time base (`AV_NOPTS_VALUE` when none).
    ///
    /// # Safety
    /// `pkt` is null (drain) or a live packet; called by the thread that owns this opened value.
    unsafe fn decode(&mut self, pkt: *const AVPacket) -> (c_int, i64) {
        self.buf.clear();
        let mut pts = AV_NOPTS_VALUE;
        let r = avcodec_send_packet(self.ctx, pkt);
        if r < 0 && r != AVERROR_EAGAIN && r != AVERROR_EOF {
            return (r, pts);
        }
        let r = loop {
            let r = avcodec_receive_frame(self.ctx, self.frame);
            if r < 0 {
                break r;
            }
            if pts == AV_NOPTS_VALUE {
                pts = (*self.frame).best_effort_timestamp;
            }
            let r = self.append();
            av_frame_unref(self.frame);
            if r < 0 {
                // The rest of this packet would only decode into more of the same: drop it.
                avcodec_flush_buffers(self.ctx);
                return (r, pts);
            }
        };
        (if r == AVERROR_EAGAIN || r == AVERROR_EOF { 0 } else { r }, pts)
    }

    /// Convert the decoded frame onto the end of `buf`. Returns 0 or a negative AVERROR.
    ///
    /// # Safety
    /// `self.frame` holds the frame `avcodec_receive_frame` just returned.
    unsafe fn append(&mut self) -> c_int {
        let f = &*self.frame;
        let r = self.ensure_swr(f);
        if r < 0 {
            return r;
        }
        let max_out = swr_get_out_samples(self.swr, f.nb_samples);
        if max_out < 0 {
            return max_out;
        }
        // A real frame is a few thousand samples at 7.35 kHz or more, and a real packet a few
        // frames: a second's worth, from one frame or from one packet decoded as many (a hostile
        // AAC packet of 4-byte silent blocks expands about 50 MB), is a broken stream and is not
        // allocated.
        let (len, frame_bytes) = (self.buf.len(), self.channels as usize * 2);
        if max_out > PCM_RATE as c_int || len + max_out as usize * frame_bytes > MAX_PACKET_PCM_BYTES {
            return AVERROR_INVALIDDATA;
        }
        self.buf.resize(len + max_out as usize * frame_bytes, 0);
        let dst = self.buf.as_mut_ptr().add(len);
        let n = swr_convert(self.swr, &dst, max_out, f.extended_data as *const *const u8, f.nb_samples);
        self.buf.truncate(len + n.max(0) as usize * frame_bytes);
        n.min(0)
    }

    /// A stream may change layout, format or rate mid-way (a broadcast's stereo adverts around a
    /// 5.1 film; HE-AAC's SBR and PS found in the first frame): swr reads `extended_data` by the
    /// shape it was configured with, so a stale context would read past the frame's planes.
    /// Rebuilt on any change; a failed build is dropped, so the next frame tries again.
    ///
    /// # Safety
    /// `f` is the decoded frame in `self.frame`, its planes valid for the call.
    unsafe fn ensure_swr(&mut self, f: &AVFrame) -> c_int {
        if !self.swr.is_null()
            && f.format == self.in_fmt
            && f.sample_rate == self.in_rate
            && av_channel_layout_compare(&f.ch_layout, &self.in_layout) == 0
        {
            return 0;
        }
        self.drop_swr();
        if f.ch_layout.nb_channels <= 0 || f.sample_rate <= 0 {
            return -libc::EINVAL;
        }
        let r = av_channel_layout_copy(&mut self.in_layout, &f.ch_layout);
        if r < 0 {
            return r;
        }
        self.in_fmt = f.format;
        self.in_rate = f.sample_rate;
        // The frame's own layout, unless it has no channel order or swr cannot remix it (an
        // ambisonic or lopsided one): then the default layout for its channel count.
        let mut native = no_layout();
        av_channel_layout_default(&mut native, f.ch_layout.nb_channels);
        let mut r = -libc::EINVAL;
        if f.ch_layout.order != AV_CHANNEL_ORDER_UNSPEC {
            r = self.build_swr(&f.ch_layout, f);
        }
        if r < 0 && av_channel_layout_compare(&native, &f.ch_layout) != 0 {
            r = self.build_swr(&native, f);
        }
        if r < 0 {
            self.drop_swr();
        }
        r
    }

    /// A resampler from `input` (and the frame's format and rate) to the sink's shape, in `swr`;
    /// left null on failure.
    ///
    /// # Safety
    /// `input` is a valid layout for the call and `f` the decoded frame.
    unsafe fn build_swr(&mut self, input: *const AVChannelLayout, f: &AVFrame) -> c_int {
        let out = stereo_layout();
        let mut r = swr_alloc_set_opts2(&mut self.swr, &out, AV_SAMPLE_FMT_S16, PCM_RATE as c_int, input, f.format, f.sample_rate, 0, null_mut());
        if r >= 0 {
            r = swr_init(self.swr);
        }
        if r < 0 {
            swr_free(&mut self.swr);
        }
        r
    }

    ///
    /// # Safety
    /// only this value touches its resampler; both frees tolerate null.
    unsafe fn drop_swr(&mut self) {
        swr_free(&mut self.swr);
        av_channel_layout_uninit(&mut self.in_layout);
    }

    /// The end of the stream: drain the decoder and hand over the last short batch.
    pub fn finish(&mut self) -> Vec<PcmAu> {
        // SAFETY: a null packet is FFmpeg's drain request.
        let mut out = unsafe { self.push(std::ptr::null(), 0) }.unwrap_or_default();
        out.extend(self.batcher.take());
        out
    }

    /// A seek: flush the decoder, drop the resampler's history, drop the queued batch, re-anchor.
    pub fn reset(&mut self) {
        // SAFETY: the context is this value's own; swr is freed null-safe.
        unsafe {
            avcodec_flush_buffers(self.ctx);
            self.drop_swr();
        }
        self.batcher.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_info_is_the_ss4s_shape() {
        assert_eq!(
            pcm_info_json(2, 48_000),
            r#"{"channelMode":"stereo","format":"S16LE","sampleRate":48,"bitsPerSample":16,"layout":"interleaved"}"#
        );
        assert!(pcm_info_json(2, 44_100).contains(r#""sampleRate":44.1"#));
    }

    #[test]
    fn the_sink_shape_and_the_codec_list() {
        assert!([-1, 0, 1, 2, 6, 8, 24].iter().all(|&n| sink_channels(n) == 2), "always stereo");
        for name in ["mp2", "MP3", "aac", "AAC", "aac_latm", "ac3", "eac3", "EAC3"] {
            assert!(is_soft(name), "{name}");
        }
        // Not FFmpeg codec names (LG's "AC3 PLUS", decoder names), or codecs the build has no
        // decoder for.
        for name in ["", "flac", "dts", "truehd", "ac4", "opus", "aac_fixed", "AC3 PLUS", "latm", "mp2float"] {
            assert!(!is_soft(name), "{name}");
        }
    }

    /// Counting samples gives monotonic, rounding-free times; a container pts within the bound is
    /// trusted to the count, one outside it starts a new timeline.
    #[test]
    fn the_sample_clock_counts_and_reanchors_only_past_the_bound() {
        let mut c = SampleClock::new(48_000);
        assert!(!c.on_packet(10_000_000_000, 0), "the first decode anchors, it does not re-anchor");
        assert_eq!(c.ns_of(0), 10_000_000_000);
        assert_eq!(c.ns_of(48_000), 11_000_000_000);
        assert_eq!(c.ns_of(-256), 10_000_000_000 - 5_333_333, "a sample before the anchor reads as just before it");
        // Matroska's millisecond rounding: 512 samples = 10.666 ms, stamped 11 ms.
        assert!(!c.on_packet(10_011_000_000, 512));
        assert_eq!(c.ns_of(512), 10_010_666_666, "the count wins over the rounded stamp");
        assert!(c.on_packet(20_000_000_000, 1024), "a 10 s jump starts a new timeline");
        assert_eq!(c.ns_of(1024), 20_000_000_000);
        assert!(c.ns_of(1024 + 1152) > c.ns_of(1024));
        c.reset();
        assert!(!c.on_packet(5_000_000_000, 4096), "after a seek the next decode simply anchors");
        assert_eq!(c.ns_of(4096), 5_000_000_000);
    }

    /// Small decodes (40 samples, 0.83 ms) stamped to the millisecond come out as >= 20 ms AUs
    /// whose pts advance by EXACTLY their sample count - no rounding drift, no gap, no overlap.
    #[test]
    fn batches_are_sample_counted_and_continuous() {
        let mut b = PcmBatcher::new(6);
        let chunk = vec![0u8; 40 * 12];
        let mut aus = Vec::new();
        for i in 0..2400i64 {
            // 2 s of audio from 10 s; the container rounds each stamp to the ms.
            let exact = 10_000_000_000 + i * 40 * 1_000_000_000 / 48_000;
            aus.extend(b.push(&chunk, exact / 1_000_000 * 1_000_000));
        }
        aus.extend(b.take());
        assert_eq!(aus[0].pts_ns, 10_000_000_000);
        let mut next = aus[0].pts_ns;
        let mut samples = 0i64;
        for au in &aus {
            assert_eq!(au.data.len() % 12, 0);
            assert_eq!(au.pts_ns, 10_000_000_000 + samples * 1_000_000_000 / 48_000, "pts is the sample count");
            assert!(au.pts_ns >= next);
            next = au.pts_ns;
            samples += (au.data.len() / 12) as i64;
        }
        assert_eq!(samples, 2400 * 40, "every sample is fed exactly once");
        assert!(aus[..aus.len() - 1].iter().all(|a| a.data.len() >= 960 * 12), "AUs are >= 20 ms");
    }

    /// A jump in the container's time (a gap, or a seek the clock was not told about) flushes what
    /// was queued at its own time and starts the new timeline at the new stamp; `reset` (a seek)
    /// drops the queue and anchors at the next decode.
    #[test]
    fn a_new_timeline_reanchors_and_a_seek_drops_the_queue() {
        let mut b = PcmBatcher::new(2);
        assert!(b.push(&[0u8; 400], 1_000_000_000).is_empty());
        let out = b.push(&[0u8; 400], 9_000_000_000);
        assert_eq!(out, vec![PcmAu { data: vec![0u8; 400], pts_ns: 1_000_000_000 }], "the old batch keeps its time");
        assert_eq!(b.take().map(|a| a.pts_ns), Some(9_000_000_000));
        b.push(&[0u8; 400], 9_002_083_333);
        b.reset();
        assert_eq!(b.take(), None, "a seek drops the queue");
        assert!(b.push(&[0u8; 400], 3_000_000_000).is_empty());
        assert_eq!(b.take().map(|a| a.pts_ns), Some(3_000_000_000), "after a seek the next decode anchors");
    }
}
