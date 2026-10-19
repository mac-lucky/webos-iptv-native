//! FFmpeg FFI (libavformat / libavcodec / libavutil). The TV ships FFmpeg n3.3
//! (SONAMEs libavformat.so.57 / libavcodec.so.57 / libavutil.so.55 = 57.71.100 /
//! 57.89.100 / 55.58.100); we link stub `.so`s carrying those SONAMEs and the device
//! loads the real libraries at runtime - the same stub trick as SDL/GLES/Starfish.
//!
//! This module is THE media demuxer: robust MKV/MP4/TS demux, HTTP input, and
//! index-based seeking (av_seek_frame). Design record: docs/ffmpeg-demuxer-plan.md.
//! It also owns the encode path and the probe, because every FFmpeg ABI detail - struct offsets,
//! AVOption names, custom AVIO - belongs in one module: the dev capture stream's MPEG1 + MPEG-TS
//! muxer (`venc`, near the end of the file) and the pre-Load stream probe (`probe`, ff_probe.rs).
//! The software decode -> PCM path (`adec`, ff_adec.rs) calls the same bundled libavcodec and
//! libswresample from Rust and owns its own FFmpeg structs.
//! The struct layouts below are the FFmpeg n3.3 ABI, confirmed on-device by the
//! Phase A probe (/tmp/webosiptv-ffprobe logs codec_id/width/height for a known title).
#![allow(dead_code)]
use crate::aq::AuQueue;
use crate::player::threads::SendPtr;
use crate::player::SHARED;
use tv_net::stream::HttpStream;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_long, c_uchar, c_uint, c_void};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Once;

/// Feed audio (es=2) to the pipeline. Cleared by the /tmp/webosiptv-noaudio dev trigger to
/// A/B whether the audio ES (E-AC3/Atmos) is what stalls the sink on 4K HEVC.
static FEED_AUDIO: AtomicBool = AtomicBool::new(true);
/// Does the FFmpeg on this device have the ABI these offsets were written for? Set once by
/// [`boot`]; read by [`demux`]. Starts FALSE so the gate is closed until boot has actually looked
/// - a demux that somehow ran first should refuse, not assume.
static ABI_OK: AtomicBool = AtomicBool::new(false);
/// Did this demux session hand a single video AU to a lane? Cleared by `demux` on entry.
///
/// The tail uses it to tell "ended cleanly" from "ended having produced nothing", which EOF alone
/// cannot express - and which is the difference between a real end-of-file and the player sitting
/// on "Buffering..." forever with no error.
static PUSHED_ANY: AtomicBool = AtomicBool::new(false);
/// Did libswscale load? It is the one FFmpeg library that is NOT required: `RELEASE=1` leaves it
/// out of the package because only the dev capture stream's RGBA->YUV conversion uses it. `venc`
/// checks this rather than relying on being feature-gated out, so the dependency is enforced
/// where it exists instead of somewhere else.
static SWS_OK: AtomicBool = AtomicBool::new(false);
pub fn set_feed_audio(on: bool) {
    FEED_AUDIO.store(on, Ordering::Relaxed);
}
/// Decode the fed track on the CPU and feed PCM of this many channels (`adec::PcmFeed`); 0 = off.
/// Set by `engine::start_bufferfeed` from the session's frozen `AudioFeed::SoftPcm` before it
/// spawns the demuxer, which reads it once, at open - the same contract as [`FEED_AUDIO`].
static SOFT_PCM_CHANNELS: AtomicU32 = AtomicU32::new(0);
pub fn set_soft_pcm(channels: u32) {
    SOFT_PCM_CHANNELS.store(channels, Ordering::Relaxed);
}

// ---- opaque handles (pointer-only, never dereferenced) ----
pub enum AVClass {}
pub enum AVInputFormat {}
pub enum AVIOContext {}
pub enum AVDictionary {}
pub enum AVCodec {}
pub enum AVCodecContext {} // opaque: we only pass the lib-allocated pointer to decode/free
pub enum AVBitStreamFilter {}
pub enum AVBufferRef {}
// AVStream is opaque here: it is 712 bytes with a large "internal but ABI" block, so
// rather than transcribe every field we read only the three we need at their verified
// n3.3 offsets (index +0, time_base +40, codecpar +708). Phase A confirms the offsets.
pub enum AVStream {}
// ---- The FFmpeg 9.0 ABI, on 32-bit ARM. ----
//
// These were a RUNTIME TABLE selected per libavformat major, because the app read the
// television's own FFmpeg and every webOS release ships a different one (55 / 57 / 58 / 59 / 60
// across webOS 2 to 11). They are plain constants again because the app now ships its own
// FFmpeg: exactly one version exists, under a SONAME that cannot collide with the TV's, and
// `ci/ffabi-assert.c` holds every number here against the very headers the shipped libraries
// were built from.
//
// That is the point of bundling. An offset that is merely *probably* right - derived from a
// version string plus an upstream tarball - becomes one the compiler checked.
//
// `ci/ffabi-check.sh dump` re-derives all of them if the bundled version ever changes.
//
// **TWO TABLES, and the axis is POINTER WIDTH rather than FFmpeg version.** The 32-bit arm is the
// television; the 64-bit arm is the desktop simulator (`make sim`), which runs this same code
// against a host build of the SAME FFmpeg 9.0 from the SAME `ci/build-ffmpeg.sh` component list.
// Every difference below is a pointer that got wider or an int64 that moved to keep its
// alignment; nothing here is a version difference, which is exactly why the old runtime
// major-selected table does not come back. `ci/ffabi-assert.c` holds both, `#if`-ed the same way,
// against each build's own headers - so a wrong number is a compile error on the platform it is
// wrong for, and `ci/ffabi-check.sh dump --host` prints the 64-bit half (`--arm-eabi` the 32-bit one without an NDK).
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_INDEX: usize = 4; // NB not 0 - FFmpeg 5.0 put `const AVClass *av_class` first
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_CODECPAR: usize = 12;
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_TIME_BASE: usize = 20;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_INDEX: usize = 8;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_CODECPAR: usize = 16;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_TIME_BASE: usize = 32;
/// `AVStream.metadata` - the container's own per-track tags, which is where a track's NAME lives.
///
/// It is read for one reason and it is a data reason, not a rendering one: **a stream's track
/// names exist only in the file.** An addon describes a stream, not its tracks, so the track menu
/// has nothing to draw but what the container says. Matroska spells the tag `title` and MP4 spells
/// it `name` (`"Forced"`, `"SDH"`, `"Full SDH"`): without it a nine-track MP4 reads as six rows of
/// one language and the viewer cannot tell a forced signs track from a full translation. The file
/// is the only source, and we are already holding it open.
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_METADATA: usize = 72;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_METADATA: usize = 80;
/// `AVStream.disposition` (`AV_DISPOSITION_*` flags), `avg_frame_rate` and `r_frame_rate` - read
/// by the probe (ff_probe.rs) for the default audio track and the declared frame rate. Proven on
/// both widths in `ci/ffabi-assert.c`; the ARM numbers are not the host ones less the pointer
/// growth because the int64 `start_time` after `time_base` is 8-aligned on ARM EABI.
/// `AVStream.discard` (`enum AVDiscard`), right after disposition; proven in `ci/ffabi-assert.c`.
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_DISCARD: usize = 60;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_DISCARD: usize = 68;
const AVDISCARD_DEFAULT: c_int = 0;
const AVDISCARD_ALL: c_int = 48;

/// What the demuxer reads of stream `i`: the played video and audio, nothing else. A discarded
/// stream's samples are skipped by the demuxer, which matters when they are stored apart from the
/// video: an MP4's 14 subtitle tracks laid out one after another made every subtitle sample an
/// HTTP range reopen (~300 ms each) and stalled a 4K stream.
fn discard_for(i: c_int, vi: c_int, ai: c_int) -> c_int {
    if i == vi || i == ai {
        AVDISCARD_DEFAULT
    } else {
        AVDISCARD_ALL
    }
}

#[inline]
unsafe fn set_stream_discard(s: *mut AVStream, v: c_int) {
    *((s as *mut u8).add(OFF_STREAM_DISCARD) as *mut c_int) = v;
}

#[cfg(target_pointer_width = "32")]
const OFF_STREAM_DISPOSITION: usize = 56;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_DISPOSITION: usize = 64;
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_AVG_FRAME_RATE: usize = 76;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_AVG_FRAME_RATE: usize = 88;
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_R_FRAME_RATE: usize = 172;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_R_FRAME_RATE: usize = 204;
/// `AVFormatContext.duration`, in AV_TIME_BASE units. By offset - see the struct's closing note.
#[cfg(target_pointer_width = "32")]
const OFF_FMT_DURATION: usize = 64;
#[cfg(target_pointer_width = "64")]
const OFF_FMT_DURATION: usize = 104;

/// `AVFormatContext.nb_chapters` (`unsigned`) and `chapters` (`AVChapter **`), read once at open
/// for the scrub preview's chapter names. Proven on both widths in `ci/ffabi-assert.c`.
#[cfg(target_pointer_width = "32")]
const OFF_FMT_NB_CHAPTERS: usize = 40;
#[cfg(target_pointer_width = "64")]
const OFF_FMT_NB_CHAPTERS: usize = 72;
#[cfg(target_pointer_width = "32")]
const OFF_FMT_CHAPTERS: usize = 44;
#[cfg(target_pointer_width = "64")]
const OFF_FMT_CHAPTERS: usize = 80;

/// `AVChapter` (avformat.h): plain public fields, the same layout on both widths (`metadata` at
/// +32, sizeof 40 - the trailing pointer is padded to the int64 alignment on ARM EABI).
/// `ci/ffabi-assert.c` pins every field.
#[repr(C)]
pub struct AVChapter {
    pub id: i64,
    pub time_base: AVRational,
    pub start: i64,
    pub end: i64,
    pub metadata: *mut AVDictionary,
}

const _: () = assert!(std::mem::size_of::<AVChapter>() == 40 && std::mem::offset_of!(AVChapter, metadata) == 32);

/// The file's chapters, sorted by start, as plain data: start in ms on the content timeline (the
/// container's start time subtracted, the way `pts_ns` places a packet) and the `title` tag.
/// A chapter with an unusable time base or no start is skipped. The titles are COPIES: the
/// dictionary is freed with the context.
unsafe fn read_chapters(fmt: *mut AVFormatContext) -> Vec<crate::player::Chapter> {
    let n = *((fmt as *const u8).add(OFF_FMT_NB_CHAPTERS) as *const c_uint);
    let list = *((fmt as *const u8).add(OFF_FMT_CHAPTERS) as *const *const *const AVChapter);
    if list.is_null() {
        return Vec::new();
    }
    let st = fmt_start_time(fmt);
    let base_ms = if st == AV_NOPTS_VALUE { 0 } else { st / 1000 };
    let ms_tb = AVRational { num: 1, den: 1000 };
    // ponytail: capped at 1024; no real file has more, and the list is a hostile-input walk.
    let mut out: Vec<_> = (0..n.min(1024) as usize)
        .filter_map(|i| {
            let c = *list.add(i);
            if c.is_null() || (*c).time_base.num <= 0 || (*c).time_base.den <= 0 || (*c).start == AV_NOPTS_VALUE {
                return None;
            }
            let start_ms = av_rescale_q((*c).start, (*c).time_base, ms_tb) - base_ms;
            let title = if (*c).metadata.is_null() {
                None
            } else {
                let e = av_dict_get((*c).metadata, c"title".as_ptr(), std::ptr::null(), 0);
                (!e.is_null() && !(*e).value.is_null())
                    .then(|| std::ffi::CStr::from_ptr((*e).value).to_string_lossy().trim().to_string())
            };
            Some(crate::player::Chapter { start_ms: start_ms.max(0), title: title.unwrap_or_default() })
        })
        .collect();
    out.sort_by_key(|c| c.start_ms);
    out
}

/// `AVDictionaryEntry` - two `char *`. Modelled rather than opaque because the whole point of the
/// call is to read both halves; `ci/ffabi-assert.c` holds the layout.
#[repr(C)]
pub struct AVDictionaryEntry {
    key: *const c_char,
    value: *const c_char,
}

/// The only field this app reads past `AVFormatContext.streams`.
#[inline]
unsafe fn fmt_duration(fmt: *const AVFormatContext) -> i64 {
    *((fmt as *const u8).add(OFF_FMT_DURATION) as *const i64)
}

/// `AVFormatContext.start_time` (AV_TIME_BASE microseconds; AV_NOPTS_VALUE when unknown): the
/// int64 directly before `duration` in every FFmpeg the app bundles (`ci/ffabi-assert.c`).
const OFF_FMT_START_TIME: usize = OFF_FMT_DURATION - 8;

#[inline]
unsafe fn fmt_start_time(fmt: *const AVFormatContext) -> i64 {
    *((fmt as *const u8).add(OFF_FMT_START_TIME) as *const i64)
}

// ---- structs (exact n3.3 field order; 32-bit ARM/AAPCS sizes) ----
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AVRational {
    pub num: c_int,
    pub den: c_int,
}

// sizeof = 72
#[repr(C)]
pub struct AVPacket {
    pub buf: *mut AVBufferRef,            // +0
    pub pts: i64,                         // +8
    pub dts: i64,                         // +16
    pub data: *mut u8,                    // +24
    pub size: c_int,                      // +28
    pub stream_index: c_int,              // +32
    pub flags: c_int,                     // +36
    pub side_data: *mut AVPacketSideData, // +40
    pub side_data_elems: c_int,           // +44
    pub duration: i64,                    // +48
    pub pos: i64,                         // +56
    // The tail changed at FFmpeg 5.0: `convergence_duration` went with
    // FF_API_CONVERGENCE_DURATION, and these three arrived. sizeof 72 -> 80. The app never
    // allocates an AVPacket (av_packet_alloc does), so only the offsets above matter - but the
    // size is asserted anyway, because a short model here would be a silent overread.
    pub opaque: *mut c_void,          // +64
    pub opaque_ref: *mut AVBufferRef, // +68
    pub time_base: AVRational,        // +72
}

/// `AVPacketSideData` - one entry of `AVCodecParameters::coded_side_data`. It was an opaque
/// `enum` here for as long as nothing read it; the Dolby Vision configuration record is the first
/// thing that does.
///
/// sizeof = 12 on 32-bit ARM, proven with `ci/ffabi-assert.c`. `size` is a `size_t`, so `usize`
/// is the field's type on both the target and the host rather than a number that happens to match
/// - the one place in this table where the Rust type carries the ABI instead of a comment.
#[repr(C)]
pub struct AVPacketSideData {
    pub data: *mut u8, // +0
    pub size: usize,   // +4
    pub type_: c_int,  // +8  (enum AVPacketSideDataType)
}

/// `AVDOVIDecoderConfigurationRecord` (libavutil/dovi_meta.h), the payload of an
/// `AV_PKT_DATA_DOVI_CONF` side-data entry - what the mp4 `dvcC`/`dvvC` box and the Matroska
/// `DolbyVisionConfiguration` block carry, handed over by BOTH bundled demuxers.
///
/// **Nine plain `uint8_t`, so this is the rare zero-risk read in this file**: no integers wider
/// than a byte, no pointers, nothing to align and nothing to pad, on any target. Field order and
/// count verified against the vendored FFmpeg 9.0 header, and each offset proven for 32-bit ARM
/// in `ci/ffabi-assert.c` - which is cheap insurance rather than doubt, since the assertions cost
/// nothing and the struct is public ABI that could gain a field at any major.
///
/// The upstream spelling is `AVDOVI...`, not `AVDovi...`, and the difference matters only to anyone
/// grepping the header for it.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct AVDOVIDecoderConfigurationRecord {
    pub dv_version_major: u8,              // +0
    pub dv_version_minor: u8,              // +1
    pub dv_profile: u8,                    // +2
    pub dv_level: u8,                      // +3
    pub rpu_present_flag: u8,              // +4
    pub el_present_flag: u8,               // +5
    pub bl_present_flag: u8,               // +6
    pub dv_bl_signal_compatibility_id: u8, // +7
    pub dv_md_compression: u8,             // +8
}

// sizeof = 136
#[repr(C)]
pub struct AVCodecParameters {
    pub codec_type: c_int,                      // +0
    pub codec_id: c_int,                        // +4
    pub codec_tag: u32,                         // +8
    pub extradata: *mut u8,                     // +12
    pub extradata_size: c_int,                  // +16
    pub coded_side_data: *mut AVPacketSideData, // +20
    pub nb_coded_side_data: c_int,              // +24
    pub format: c_int,                          // +28
    pub bit_rate: i64,                          // +32
    pub bits_per_coded_sample: c_int,           // +40
    pub bits_per_raw_sample: c_int,             // +44
    pub profile: c_int,                         // +48
    pub level: c_int,                           // +52
    pub width: c_int,                           // +56
    pub height: c_int,                          // +60
    pub sample_aspect_ratio: AVRational,        // +64
    pub framerate: AVRational,                  // +72
    pub field_order: c_int,                     // +80
    pub color_range: c_int,                     // +84
    pub color_primaries: c_int,                 // +88
    pub color_trc: c_int,                       // +92
    pub color_space: c_int,                     // +96
    pub chroma_location: c_int,                 // +100
    pub video_delay: c_int,                     // +104
    // FFmpeg 7 DELETED the deprecated `channel_layout`/`channels` pair that lived here and left
    // only this struct. `nb_channels` is the replacement for the `channels` int, and reading the
    // old field is not a compile error on a hand-written model - it is a silent read of whatever
    // now occupies +104. Which is the argument for shipping a known FFmpeg in one sentence.
    pub ch_layout: AVChannelLayout, // +112
    pub sample_rate: c_int,         // +136
    pub block_align: c_int,         // +140
    pub frame_size: c_int,          // +144
    pub initial_padding: c_int,     // +148
    pub trailing_padding: c_int,    // +152
    pub seek_preroll: c_int,        // +156
    pub alpha_mode: c_int,          // +160
}

/// `AVChannelLayout`, in full (24 bytes on both widths), so the fields after it in
/// [`AVCodecParameters`] sit where FFmpeg puts them. The probe reads `nb_channels`. The union that
/// follows `nb_channels` is 8-aligned, so a 4-byte pad sits after it on ARM.
#[repr(C)]
pub struct AVChannelLayout {
    pub order: c_int,        // +0  (enum AVChannelOrder: 1 = NATIVE)
    pub nb_channels: c_int,  // +4
    pub u_mask: u64,         // +8  (union { uint64_t mask; AVChannelCustom *map })
    pub opaque: *mut c_void, // +16
}

// The #[repr(C)] models above, held to the numbers `ci/ffabi-assert.c` proves against the bundled
// headers. The C file proves FFmpeg's layout; these prove OURS is the same one - a model field of
// the wrong type or in the wrong place is then a compile error on the width it is wrong for, not a
// write into the next field. Only the fields the probe and the demuxer read are listed.
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<AVChannelLayout>() == 24);
    assert!(offset_of!(AVChannelLayout, nb_channels) == 4);
    assert!(offset_of!(AVChannelLayout, u_mask) == 8);
    assert!(offset_of!(AVChannelLayout, opaque) == 16);
    assert!(offset_of!(AVCodecParameters, codec_type) == 0);
    assert!(offset_of!(AVCodecParameters, codec_id) == 4);
};
#[cfg(target_pointer_width = "32")]
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(AVCodecParameters, format) == 28);
    assert!(offset_of!(AVCodecParameters, bit_rate) == 32);
    assert!(offset_of!(AVCodecParameters, profile) == 48);
    assert!(offset_of!(AVCodecParameters, width) == 56);
    assert!(offset_of!(AVCodecParameters, framerate) == 72);
    assert!(offset_of!(AVCodecParameters, ch_layout) == 112);
    assert!(offset_of!(AVCodecParameters, sample_rate) == 136);
    assert!(offset_of!(AVCodecParameters, frame_size) == 144);
    assert!(offset_of!(AVCodecParameters, initial_padding) == 148);
    assert!(offset_of!(AVPacket, pts) == 8);
    assert!(offset_of!(AVPacket, data) == 24);
    assert!(offset_of!(AVPacket, size) == 28);
};
#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(AVCodecParameters, format) == 44);
    assert!(offset_of!(AVCodecParameters, bit_rate) == 48);
    assert!(offset_of!(AVCodecParameters, profile) == 64);
    assert!(offset_of!(AVCodecParameters, width) == 72);
    assert!(offset_of!(AVCodecParameters, framerate) == 88);
    assert!(offset_of!(AVCodecParameters, ch_layout) == 128);
    assert!(offset_of!(AVCodecParameters, sample_rate) == 152);
    assert!(offset_of!(AVCodecParameters, frame_size) == 160);
    assert!(offset_of!(AVCodecParameters, initial_padding) == 164);
    assert!(offset_of!(AVPacket, pts) == 8);
    assert!(offset_of!(AVPacket, data) == 24);
    assert!(offset_of!(AVPacket, size) == 32);
};

// AVFormatContext truncated after `filename` - we only ever hold a library-returned pointer and
// read leading fields. NEVER stack-allocate this.
//
// It STOPS at `filename` because that is where the two majors stop agreeing: FFmpeg 4.0 inserted
// `char *url` at +1056, pushing start_time to +1064 and duration to +1072. Everything above is
// identical on 3.3 and 4.x. `duration` - the only field past that point this app reads - is
// reached through [`fmt_duration`] at the offset the runtime ABI table gives, rather than by
// declaring it here at an offset that is right on exactly one of the two.
#[repr(C)]
pub struct AVFormatContext {
    pub av_class: *const AVClass,    // +0
    pub iformat: *mut AVInputFormat, // +4
    pub oformat: *mut c_void,        // +8
    pub priv_data: *mut c_void,      // +12
    pub pb: *mut AVIOContext,        // +16
    pub ctx_flags: c_int,            // +20
    pub nb_streams: c_uint,          // +24
    pub streams: *mut *mut AVStream, // +28
                                     // STOPS HERE. Everything past `streams` shifts between majors - `duration` is at +48 on
                                     // FFmpeg 6, +64 on 9, and +1064 on the n3.3 the televisions ship, because 5.0 deleted
                                     // `filename[1024]` and later releases kept rearranging what replaced it. It is the only field
                                     // beyond this point the app reads, so it is read at OFF_FMT_DURATION rather than declared at
                                     // a position that would be quietly wrong after a version bump.
}

// AVBSFContext is NOT modelled. The app does its own AVCC->Annex-B conversion in Rust, so the
// bitstream-filter API is never driven - only `av_bsf_get_by_name` survives, and only to log
// whether the filters exist. Keeping a #[repr(C)] mirror of a struct nothing reads meant
// re-deriving its offsets at every FFmpeg bump to prove a fact that did not matter.

// ---- externs, bound at RUNTIME by SONAME candidate list (see `dynlib`) ----
//
// These were four `#[link]` directives, which put `libavformat.so.57` and its three siblings in
// DT_NEEDED. That is fatal on any firmware that moved the major: webOS 5.3.1 ships 58/58/56/5,
// 10.2.0 ships 59/59/57/6, 11.2.0 ships 60/60/58/7 - and a missing DT_NEEDED kills the process at
// exec(), before main, before this log file is even open. DT_NEEDED also cannot say "57 OR 58", so
// one binary spanning both eras has to do the resolution itself.
//
// The candidate lists reach past the majors this table describes ON PURPOSE. Opening a
// libavformat 59 does not make the offsets below correct - `boot()` still refuses it - but the
// difference between refusing and never starting is the difference between a UI that browses your
// library and says it cannot play, and a television where nothing happens at all.
//
// One consequence worth knowing: the `cfg(test)` gate is gone. With no link directive the host
// suite links unconditionally, and a test that calls into FFmpeg now fails by taking `dlopen`'s
// None branch on Darwin instead of by failing to link.
tv_base::dynlib! {
    avformat: [
        // The ELF the app SHIPS, and the Mach-O the desktop simulator builds beside it
        // (`HOST=1 ci/build-ffmpeg.sh` - same version, same component list). A candidate
        // LIST rather than a `cfg`: `dynlib!` already tries each name and reports the ones
        // it could not open, and only one of these two can ever exist in an app directory.
        "libavformat-wi.so.63",
        "libavformat-wi.63.dylib",
    ] {
    // Declared beside libavformat's other entry points because that is the library that DEFINES
    // it. Under `#[link]` the final link resolved every name against every library at once and
    // the grouping was cosmetic; `dlsym` searches one handle and its dependency chain, and
    // libavutil does not depend on libavformat - so leaving this in the avutil block reported the
    // whole avutil table as Incomplete on every device, including working ones.
    fn avformat_version() -> c_uint;
    fn avformat_network_init() -> c_int;
    fn avformat_alloc_context() -> *mut AVFormatContext;
    fn avformat_open_input(
        ps: *mut *mut AVFormatContext,
        url: *const c_char,
        fmt: *mut AVInputFormat,
        options: *mut *mut AVDictionary,
    ) -> c_int;
    fn avformat_find_stream_info(ic: *mut AVFormatContext, options: *mut *mut AVDictionary) -> c_int;
    fn av_find_best_stream(
        ic: *mut AVFormatContext,
        type_: c_int,
        wanted: c_int,
        related: c_int,
        decoder_ret: *mut *mut AVCodec,
        flags: c_int,
    ) -> c_int;
    fn av_read_frame(s: *mut AVFormatContext, pkt: *mut AVPacket) -> c_int;
    fn av_seek_frame(s: *mut AVFormatContext, stream_index: c_int, ts: i64, flags: c_int) -> c_int;
    fn avformat_close_input(s: *mut *mut AVFormatContext);
    fn avio_alloc_context(
        buffer: *mut u8,
        buffer_size: c_int,
        write_flag: c_int,
        opaque: *mut c_void,
        read_packet: Option<extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int>,
        write_packet: Option<extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int>,
        seek: Option<extern "C" fn(*mut c_void, i64, c_int) -> i64>,
    ) -> *mut AVIOContext;
    // MPEG-TS mux (dev capture stream, venc section)
    fn avformat_alloc_output_context2(
        ctx: *mut *mut AVFormatContext,
        oformat: *mut c_void,
        format_name: *const c_char,
        filename: *const c_char,
    ) -> c_int;
    fn avformat_new_stream(s: *mut AVFormatContext, c: *const AVCodec) -> *mut AVStream;
    fn avformat_write_header(s: *mut AVFormatContext, options: *mut *mut AVDictionary) -> c_int;
    fn av_write_frame(s: *mut AVFormatContext, pkt: *mut AVPacket) -> c_int;
    fn avio_flush(s: *mut AVIOContext);
    fn avformat_free_context(s: *mut AVFormatContext);
}}
tv_base::dynlib! {
    avcodec: [
        // The ELF the app SHIPS, and the Mach-O the desktop simulator builds beside it
        // (`HOST=1 ci/build-ffmpeg.sh` - same version, same component list). A candidate
        // LIST rather than a `cfg`: `dynlib!` already tries each name and reports the ones
        // it could not open, and only one of these two can ever exist in an app directory.
        "libavcodec-wi.so.63",
        "libavcodec-wi.63.dylib",
    ] {
    fn avcodec_version() -> c_uint;
    fn av_packet_alloc() -> *mut AVPacket;
    fn av_packet_free(pkt: *mut *mut AVPacket);
    fn av_packet_unref(pkt: *mut AVPacket);
    fn avcodec_get_name(id: c_int) -> *const c_char;
    fn av_bsf_get_by_name(name: *const c_char) -> *const AVBitStreamFilter;
    // MPEG1 encode (dev capture stream, venc section)
    fn avcodec_alloc_context3(codec: *const AVCodec) -> *mut AVCodecContext;
    fn avcodec_open2(ctx: *mut AVCodecContext, codec: *const AVCodec, opts: *mut *mut AVDictionary) -> c_int;
    fn avcodec_free_context(ctx: *mut *mut AVCodecContext);
    // Scratch AVCodecParameters for venc's ABI self-check - library-allocated so its true size is
    // never our problem, and `from_context` starts by av_freep-ing par->extradata.
    fn avcodec_parameters_alloc() -> *mut AVCodecParameters;
    fn avcodec_parameters_free(par: *mut *mut AVCodecParameters);
    fn avcodec_find_encoder_by_name(name: *const c_char) -> *const AVCodec;
    fn avcodec_send_frame(ctx: *mut AVCodecContext, frame: *const c_void) -> c_int;
    fn avcodec_receive_packet(ctx: *mut AVCodecContext, pkt: *mut AVPacket) -> c_int;
    fn avcodec_parameters_from_context(par: *mut AVCodecParameters, ctx: *const AVCodecContext) -> c_int;
    fn av_packet_rescale_ts(pkt: *mut AVPacket, tb_src: AVRational, tb_dst: AVRational);
}}
tv_base::dynlib! {
    // avformat_version lives in libavformat, not libavutil - but it was declared here and the
    // loader resolves by symbol, not by header, so it must move to the library that defines it or
    // the whole avutil table reports Incomplete on every device.
    avutil: [
        // The ELF the app SHIPS, and the Mach-O the desktop simulator builds beside it
        // (`HOST=1 ci/build-ffmpeg.sh` - same version, same component list). A candidate
        // LIST rather than a `cfg`: `dynlib!` already tries each name and reports the ones
        // it could not open, and only one of these two can ever exist in an app directory.
        "libavutil-wi.so.61",
        "libavutil-wi.61.dylib",
    ] {
    fn avutil_version() -> c_uint;
    fn av_malloc(size: usize) -> *mut c_void;
    fn av_freep(ptr: *mut c_void);
    fn av_rescale_q(a: i64, bq: AVRational, cq: AVRational) -> i64;
    // MPEG1 encode input frames + option/pixfmt helpers (venc section)
    fn av_frame_alloc() -> *mut c_void;
    fn av_frame_free(frame: *mut *mut c_void);
    fn av_frame_get_buffer(frame: *mut c_void, align: c_int) -> c_int;
    fn av_frame_make_writable(frame: *mut c_void) -> c_int;
    fn av_opt_set(obj: *mut c_void, name: *const c_char, val: *const c_char, search_flags: c_int) -> c_int;
    fn av_get_pix_fmt(name: *const c_char) -> c_int;
    // An integer option by name (the HLS probe budget, `cap_hls_probe`).
    fn av_opt_set_int(obj: *mut c_void, name: *const c_char, val: i64, search_flags: c_int) -> c_int;
    // Container tags. Borrowed, never freed: the entry belongs to the dictionary, which belongs to
    // the AVStream, which `avformat_close_input` frees - so every read must copy before the close.
    fn av_dict_get(
        m: *const AVDictionary,
        key: *const c_char,
        prev: *const AVDictionaryEntry,
        flags: c_int,
    ) -> *const AVDictionaryEntry;
}}
tv_base::dynlib! {
    // libavcodec NEEDs it, and so does the software audio decoder (`adec`, ff_adec.rs): loaded
    // BEFORE libavcodec, by absolute path, for the same reason the other three are.
    swresample: ["libswresample-wi.so.7", "libswresample-wi.7.dylib"] {
    fn swresample_version() -> c_uint;
}}
tv_base::dynlib! {
    swscale: [
        // The ELF the app SHIPS, and the Mach-O the desktop simulator builds beside it
        // (`HOST=1 ci/build-ffmpeg.sh` - same version, same component list). A candidate
        // LIST rather than a `cfg`: `dynlib!` already tries each name and reports the ones
        // it could not open, and only one of these two can ever exist in an app directory.
        "libswscale-wi.so.10",
        "libswscale-wi.10.dylib",
    ] {
    fn sws_getContext(
        src_w: c_int, src_h: c_int, src_fmt: c_int,
        dst_w: c_int, dst_h: c_int, dst_fmt: c_int,
        flags: c_int, src_filter: *mut c_void, dst_filter: *mut c_void, param: *const f64,
    ) -> *mut c_void;
    fn sws_scale(
        c: *mut c_void,
        src_slice: *const *const u8, src_stride: *const c_int,
        src_slice_y: c_int, src_slice_h: c_int,
        dst: *const *mut u8, dst_stride: *const c_int,
    ) -> c_int;
    fn sws_freeContext(c: *mut c_void);
}}

// ---- constants (n3.3) ----
pub const AVMEDIA_TYPE_VIDEO: c_int = 0;
pub const AVMEDIA_TYPE_AUDIO: c_int = 1;
// The codec ids the app compares against are AAC, H264, HEVC, VP9, VP8 and AV1. Their values shift between
// FFmpeg majors, so each is asserted against the bundled headers in ci/ffabi-assert.c.
// AV1 needs no FFmpeg parser or bsf: matroska (V_AV1) and mov (av01) demux it without one, and
// with the av1 parser absent the packets arrive raw. `av1_packet_to_tu` frames the OBUs itself.
// Enabling the parser would pull in cbs_av1 for nothing.
pub const AV_CODEC_ID_AAC: c_int = 0x15002;
/// `AV_DISPOSITION_DEFAULT` - the muxer's default track. Asserted in ci/ffabi-assert.c.
pub const AV_DISPOSITION_DEFAULT: c_int = 1;
/// `AV_DISPOSITION_FORCED` - a track the muxer flags forced. Asserted in ci/ffabi-assert.c.
pub const AV_DISPOSITION_FORCED: c_int = 1 << 6;
/// `AV_PROFILE_EAC3_DDP_ATMOS` (libavcodec/defs.h) - what `codecpar.profile` reads for E-AC3 JOC.
pub const AV_PROFILE_EAC3_DDP_ATMOS: c_int = 30;
pub const AV_CODEC_ID_H264: c_int = 27;
// FF_API_XVMC and FF_API_VOXWARE both died before FFmpeg 6, which is why these differ from the
// values the n3.3 televisions use (28 / 174 / 0x15029).
pub const AV_CODEC_ID_HEVC: c_int = 172;
pub const AV_CODEC_ID_VP9: c_int = 166;
pub const AV_CODEC_ID_AV1: c_int = 222;
/// VP8, the buffer feed's one other video caps. Asserted in ci/ffabi-assert.c like the ids above.
pub const AV_CODEC_ID_VP8: c_int = 139;
pub const AV_PKT_FLAG_KEY: c_int = 1;
pub const AVSEEK_FLAG_BACKWARD: c_int = 1;

/// The next seek lands at or after its target (`player::seek_forward_once`); one-shot.
pub static SEEK_FORWARD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `av_seek_frame`'s flags: the keyframe before the target (a scrub), or the first one at or after
/// it (a skip).
fn seek_flags(forward: bool) -> c_int {
    if forward { 0 } else { AVSEEK_FLAG_BACKWARD }
}
pub const AVERROR_EOF: c_int = -541478725;
/// `AVERROR(EAGAIN)`: the platform's errno (11 on the ARM television and Linux, 35 on a macOS host).
pub const AVERROR_EAGAIN: c_int = -libc::EAGAIN;
/// `AVERROR(EIO)`: unlike EOF, tells libavformat and our producer that the source was truncated by
/// a transport failure.
pub const AVERROR_IO: c_int = -5;
pub const AV_NOPTS_VALUE: i64 = i64::MIN;
pub const AV_TIME_BASE: i64 = 1_000_000;
pub const NS_TB: AVRational = AVRational {
    num: 1,
    den: 1_000_000_000,
};
/// `AV_PKT_DATA_DOVI_CONF` - the side-data type tag whose payload is an
/// [`AVDOVIDecoderConfigurationRecord`]. 29 in FFmpeg 9.0; asserted in `ci/ffabi-assert.c`
/// because `AVPacketSideDataType` is an ordinary sequential enum and every value in it shifts
/// when a member is inserted above.
pub const AV_PKT_DATA_DOVI_CONF: c_int = 29;
/// `AV_PKT_DATA_MASTERING_DISPLAY_METADATA` (an `AVMasteringDisplayMetadata`, 88 bytes) and
/// `AV_PKT_DATA_CONTENT_LIGHT_LEVEL` (an `AVContentLightMetadata`, 8 bytes): 20 and 22 in FFmpeg 9.0,
/// asserted in `ci/ffabi-assert.c` for the same reason as the Dolby Vision tag. The two structs'
/// member offsets are hard-coded in `player::vp9_hdr::{parse_mastering, parse_cll}` and pinned by
/// the same file; a bump that moves them has to change both.
pub const AV_PKT_DATA_MASTERING_DISPLAY_METADATA: c_int = 20;
pub const AV_PKT_DATA_CONTENT_LIGHT_LEVEL: c_int = 22;

// ---- AVStream field accessors, at the constants above. Read by offset rather than modelled:
// the struct is large, mostly internal, and only three fields are wanted. ----
#[inline]
unsafe fn stream_codecpar(s: *mut AVStream) -> *mut AVCodecParameters {
    *((s as *const u8).add(OFF_STREAM_CODECPAR) as *const *mut AVCodecParameters)
}
#[inline]
unsafe fn stream_time_base(s: *mut AVStream) -> AVRational {
    *((s as *const u8).add(OFF_STREAM_TIME_BASE) as *const AVRational)
}

/// PURE: read an [`AVDOVIDecoderConfigurationRecord`] out of the raw side-data bytes.
///
/// Split from the pointer walk in [`dovi_conf`] on purpose - this half is the only half worth
/// testing, and it is testable precisely because it never touches FFmpeg. (A test that had to
/// enter the library would take `dlopen`'s `None` branch on Darwin and pass without executing
/// anything, which is the failure shape `docs/agent-reference.md` warns about by name.)
///
/// A SHORT buffer yields `None` rather than a partial record. FFmpeg allocates these with
/// `av_dovi_alloc` and its own demuxers always write all nine bytes, so this cannot happen today
/// - but the record is public ABI whose size "is not a part of the public ABI" by its own header's
/// admission, and reading nine bytes out of a seven-byte allocation is a heap overread that would
/// report a plausible profile number rather than crash. A longer buffer is fine and expected: a
/// future FFmpeg may append fields, and the nine we read keep their meaning.
fn parse_dovi_conf(bytes: &[u8]) -> Option<AVDOVIDecoderConfigurationRecord> {
    if bytes.len() < 9 {
        return None;
    }
    Some(AVDOVIDecoderConfigurationRecord {
        dv_version_major: bytes[0],
        dv_version_minor: bytes[1],
        dv_profile: bytes[2],
        dv_level: bytes[3],
        rpu_present_flag: bytes[4],
        el_present_flag: bytes[5],
        bl_present_flag: bytes[6],
        dv_bl_signal_compatibility_id: bytes[7],
        dv_md_compression: bytes[8],
    })
}

/// The Dolby Vision configuration record attached to this stream, if the demuxer found one.
///
/// `coded_side_data` is the STREAM-level side data (as opposed to `AVPacket::side_data`, which is
/// per-packet), and both bundled demuxers populate it: `mov` from the `dvcC`/`dvvC` sample-entry
/// box, `matroska` from the `DolbyVisionConfiguration` block-additions element. So this answers
/// "what is this file really" for every container the app direct-plays, without decoding a frame.
unsafe fn dovi_conf(cp: *const AVCodecParameters) -> Option<AVDOVIDecoderConfigurationRecord> {
    let (list, n) = ((*cp).coded_side_data, (*cp).nb_coded_side_data);
    if list.is_null() || n <= 0 {
        return None;
    }
    for i in 0..n as usize {
        let sd = &*list.add(i);
        if sd.type_ != AV_PKT_DATA_DOVI_CONF || sd.data.is_null() {
            continue;
        }
        return parse_dovi_conf(std::slice::from_raw_parts(sd.data, sd.size));
    }
    None
}
/// What the container said about a stream's colour: the three H.273 code points from
/// `AVCodecParameters`, plus the mastering display and content light level from the stream's coded
/// side data when the demuxer found them (Matroska's `Colour` element fills them; each only when the
/// file states it). Read for VP9, whose bitstream cannot carry the transfer function.
unsafe fn container_hdr(cp: *const AVCodecParameters) -> crate::player::vp9_hdr::ContainerHdr {
    use crate::player::vp9_hdr::{parse_cll, parse_mastering, ContainerHdr};
    let mut h = ContainerHdr {
        trc: (*cp).color_trc,
        pri: (*cp).color_primaries,
        mc: (*cp).color_space,
        mastering: None,
        cll: None,
    };
    let (list, n) = ((*cp).coded_side_data, (*cp).nb_coded_side_data);
    if list.is_null() || n <= 0 {
        return h;
    }
    for i in 0..n as usize {
        let sd = &*list.add(i);
        if sd.data.is_null() {
            continue;
        }
        let bytes = std::slice::from_raw_parts(sd.data, sd.size);
        match sd.type_ {
            AV_PKT_DATA_MASTERING_DISPLAY_METADATA => h.mastering = parse_mastering(bytes),
            AV_PKT_DATA_CONTENT_LIGHT_LEVEL => h.cll = parse_cll(bytes),
            _ => {}
        }
    }
    h
}
/// The container's own name for this track - `title` (Matroska and most formats), else `name`
/// (MP4's per-track `udta` name box, which is what FFmpeg's mov demuxer exposes it as).
///
/// Deliberately NOT `handler_name`: MP4 sets it to a constant per media type (`"SubtitleHandler"`,
/// `"SoundHandler"`), so it is the same string on every track and reading it would give the picker
/// nine identical sub-lines instead of none - the failure it exists to fix, dressed as a fix.
///
/// The returned `String` is a COPY. The entry points into the stream's dictionary, which
/// `avformat_close_input` frees.
unsafe fn stream_name(s: *mut AVStream) -> String {
    [c"title", c"name"].into_iter().find_map(|key| stream_tag(s, key)).unwrap_or_default()
}

/// The stream's `language` tag (ISO 639-2 in Matroska/MP4: `rus`, `pol`, `und`), or None when it
/// has none. A COPY, like [`stream_name`]. It names the track in the player's track menus
/// ([`track_names`]).
unsafe fn stream_language(s: *mut AVStream) -> Option<String> {
    stream_tag(s, c"language")
}

/// One metadata tag of the stream, trimmed, or None when absent or empty. A COPY: the entry
/// points into the stream's dictionary, which `avformat_close_input` frees.
unsafe fn stream_tag(s: *mut AVStream, key: &std::ffi::CStr) -> Option<String> {
    let dict = *((s as *const u8).add(OFF_STREAM_METADATA) as *const *const AVDictionary);
    if dict.is_null() {
        return None;
    }
    let e = av_dict_get(dict, key.as_ptr(), std::ptr::null(), 0);
    if e.is_null() || (*e).value.is_null() {
        return None;
    }
    // `to_string_lossy` rather than a strict decode: these are user-authored tags off a
    // stranger's file, and a mis-encoded byte must cost that character, not the whole value.
    let v = std::ffi::CStr::from_ptr((*e).value).to_string_lossy().trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// Every audio track's container name, in file order - the positions `desired_audio_idx`
/// selects by.
///
/// File order is the join, and it is the mapping `nth_audio_stream` already relies on: position N
/// here and the Nth audio stream the demuxer selects name one track, and a file with no tags
/// simply yields empty strings in the right slots.
unsafe fn track_names(fmt: *mut AVFormatContext) -> crate::player::TrackNames {
    let mut names = crate::player::TrackNames::new();
    let streams = (*fmt).streams;
    for i in 0..(*fmt).nb_streams {
        let st = *streams.add(i as usize);
        if (*stream_codecpar(st)).codec_type == AVMEDIA_TYPE_AUDIO {
            names.audio.push(stream_name(st));
            names.audio_langs.push(stream_language(st).unwrap_or_default());
        }
    }
    names
}

#[inline]
unsafe fn stream_index(s: *mut AVStream) -> c_int {
    *((s as *const u8).add(OFF_STREAM_INDEX) as *const c_int)
}

/// The ffmpeg stream index of the first audio stream whose codec matches `want` (the Load
/// payload's audio codec, e.g. "ac3"), or None. `av_find_best_stream` picks the "highest
/// quality" audio (on an 8-track file it chose DTS over the AC3 default) - but the Load payload
/// carries Media[0].audioCodec, so feeding a different-codec track leaves the audio ES
/// unconfigured and, with audioSync, wedges the video (BufferFull forever). Matching the fed
/// track to the payload codec avoids that.
unsafe fn audio_stream_matching(fmt: *mut AVFormatContext, want: &str) -> Option<c_int> {
    if want.is_empty() {
        return None;
    }
    let streams = (*fmt).streams;
    for i in 0..(*fmt).nb_streams {
        let cp = stream_codecpar(*streams.add(i as usize));
        if (*cp).codec_type == AVMEDIA_TYPE_AUDIO {
            let name = std::ffi::CStr::from_ptr(avcodec_get_name((*cp).codec_id)).to_string_lossy();
            if name.eq_ignore_ascii_case(want) {
                return Some(i as c_int);
            }
        }
    }
    None
}

/// The ffmpeg stream index of the `n`-th audio stream in file order (for native audio-track
/// selection), or None if there are fewer than n+1 audio streams. metadata.audio is filtered
/// in the same file order, so the track menu's 0-based audio index maps 1:1 here.
unsafe fn nth_audio_stream(fmt: *mut AVFormatContext, n: i32) -> Option<c_int> {
    let streams = (*fmt).streams;
    let mut count = 0i32;
    for i in 0..(*fmt).nb_streams {
        let cp = stream_codecpar(*streams.add(i as usize));
        if (*cp).codec_type == AVMEDIA_TYPE_AUDIO {
            if count == n {
                return Some(i as c_int);
            }
            count += 1;
        }
    }
    None
}

// ================================ venc =====================================
// Dev capture stream MPEG1/TS encoder (used by capture.rs when a client's hello
// asks for kind=mpegts): RGBA 960x540 -> sws_scale -> yuv420p AVFrame ->
// mpeg1video (the TV build keeps this encoder) -> mpegts muxer -> custom AVIO
// write callback -> the client socket, raw unframed TS (self-syncing 188B pkts).
//
// ABI strategy (workflow-verified 2026-07-24 against the DEVICE's own libs - the
// AVOption tables inside libavcodec.so.57.89.100 carry the true field offsets):
// every numeric encoder knob goes through av_opt_set ("b"/"g"/"bf"/"maxrate"/
// "bufsize"/"dct"/"time_base"), pix fmts come from av_get_pix_fmt() at runtime
// (AV_PIX_FMT_RGBA is 28 in THIS build - an FF_API_XVMC alias shifts the enum;
// never hardcode it), and only width/height/pix_fmt (AVCodecContext) and
// data/linesize/width/height/format/pts (AVFrame) are raw offset pokes below.
// After avcodec_open2, avcodec_parameters_from_context round-trips the values
// through the already-modeled AVCodecParameters as a runtime ABI self-check -
// a mismatch latches the whole mpeg path off instead of corrupting memory.
// mpeg1video only accepts the fixed MPEG1 frame-rate table: time_base stays
// {1,30} regardless of the actual (jittery, <=30fps) capture cadence; jsmpeg in
// streaming mode ignores timestamps and decodes as data arrives.

// AVCodecContext width/height/pix_fmt USED to be poked here at 124/128/144, under a note claiming
// they had no AVOption. That was wrong - `video_size` (an IMAGE_SIZE option writing width AND
// height) and `pixel_format` are both in libavcodec's own option table - and Venc::open sets them
// by name now, so the constants are gone with the last read of them. Which is the point: all three
// move on webOS 5 (to 92/96/112), and a by-name setter does not care. `ci/ffabi-assert.c` still
// asserts them per major, so if anyone ever needs to poke them again the proven numbers are there.
// AVFrame (avutil 55, 32-bit ARM). pts sits at +104: a 4-byte pad at +100
// 8-aligns the int64 on ARM EABI (the classic AVFrame-on-ARM quirk).
const OFF_FRAME_DATA: usize = 0; // u8*[8] - the only one that does not move
#[cfg(target_pointer_width = "32")]
const OFF_FRAME_LINESIZE: usize = 32; // c_int[8]
#[cfg(target_pointer_width = "32")]
const OFF_FRAME_WIDTH: usize = 68;
#[cfg(target_pointer_width = "32")]
const OFF_FRAME_HEIGHT: usize = 72;
#[cfg(target_pointer_width = "32")]
const OFF_FRAME_FORMAT: usize = 80;
#[cfg(target_pointer_width = "32")]
const OFF_FRAME_PTS: usize = 96;
// 64-bit: `data` is eight POINTERS rather than eight 32-bit ones, so `linesize` doubles and
// everything after it follows. `pts` needs no pad here - it is already 8-aligned - which is the
// ARM EABI quirk described above seen from the side where it does not bite.
#[cfg(target_pointer_width = "64")]
const OFF_FRAME_LINESIZE: usize = 64;
#[cfg(target_pointer_width = "64")]
const OFF_FRAME_WIDTH: usize = 104;
#[cfg(target_pointer_width = "64")]
const OFF_FRAME_HEIGHT: usize = 108;
#[cfg(target_pointer_width = "64")]
const OFF_FRAME_FORMAT: usize = 116;
#[cfg(target_pointer_width = "64")]
const OFF_FRAME_PTS: usize = 136;
const SWS_BILINEAR: c_int = 2;
const VENC_TB: AVRational = AVRational { num: 1, den: 30 };

#[inline]
unsafe fn poke_i32(base: *mut c_void, off: usize, v: i32) {
    *((base as *mut u8).add(off) as *mut i32) = v;
}
#[inline]
unsafe fn poke_i64(base: *mut c_void, off: usize, v: i64) {
    *((base as *mut u8).add(off) as *mut i64) = v;
}

/// The AVIO write side: raw TS bytes -> the mpeg client's socket. Boxed so the
/// opaque pointer stays stable; `fd` is refreshed by capture.rs before each
/// encode, `failed` reports a dead socket back without panicking inside FFmpeg.
/// `send` is the socket-write policy `capture` hands to [`Venc::open`] (`ff` sits below
/// `capture` and may not name it).
struct VencSink {
    fd: c_int,
    failed: bool,
    send: fn(c_int, &[u8]) -> bool,
}

extern "C" fn venc_write_cb(op: *mut c_void, data: *mut u8, n: c_int) -> c_int {
    unsafe {
        let s = &mut *(op as *mut VencSink);
        if s.failed || s.fd < 0 || n <= 0 {
            s.failed = true;
            return -1;
        }
        // one socket-write policy for the whole feature (partial writes, MSG_NOSIGNAL), supplied by
        // the capture tap that owns the socket
        if !(s.send)(s.fd, std::slice::from_raw_parts(data, n as usize)) {
            s.failed = true;
            return -1;
        }
        n
    }
}

pub struct Venc {
    ctx: *mut AVCodecContext,
    frame: *mut c_void,
    sws: *mut c_void,
    oc: *mut AVFormatContext,
    st: *mut AVStream,
    avio: *mut AVIOContext,
    pkt: *mut AVPacket,
    sink: Box<VencSink>,
    st_tb: AVRational,
    pts: i64,
    pub w: c_int,
    pub h: c_int,
    // RGBA -> YUV420P directly runs swscale's GENERAL scaler - scalar C on this ARM
    // build, measured ~80ms/frame at 960x540. RGBA -> NV12 has a NEON unscaled
    // converter (rgbx_to_nv12_neon), so we go RGBA -NEON-> NV12 (Y lands straight in
    // the frame's Y plane, interleaved UV in this scratch) and deinterleave UV into
    // the planar U/V planes ourselves (~260KB pass) - mpeg1video only eats yuv420p.
    nv12_uv: Vec<u8>,
    // rolling perf split, logged every ~5s: is the cost sws (colorspace) or the codec?
    t_sws_us: u64,
    t_enc_us: u64,
    t_n: u32,
    t_last_log: std::time::Instant,
}
// All FFmpeg objects are owned and touched by the single capture-encoder thread.
unsafe impl Send for Venc {}

impl Venc {
    /// Bring up encoder + muxer for one client session. Any failure logs and
    /// returns None (capture latches the mpeg path off for that client).
    /// `w`/`h` follow whatever the shared GL capture chain is producing (960x540 or
    /// 480x270) - the encoder is NOT pinned to one size. Cost scales with macroblock
    /// count and MPEG1 has no intra prediction, so a detailed screen at 960x540 can cost
    /// 50-110ms/frame on this CPU where 480x270 stays near 15-25ms; the caller rebuilds
    /// the session when the geometry changes.
    ///
    /// `send` writes every byte of a slice to a socket fd and says whether it all went (capture's
    /// `send_all`: partial writes, `MSG_NOSIGNAL`). The muxer's AVIO write callback calls it, and
    /// it is a parameter because the dev capture tap that owns the socket is above this module.
    pub fn open(
        w: c_int,
        h: c_int,
        bitrate_bps: i64,
        send: fn(c_int, &[u8]) -> bool,
    ) -> Option<Box<Venc>> {
        ensure_registered(); // the file's ONE network-init guard
        if !SWS_OK.load(Ordering::Relaxed) {
            tv_base::eventlog::log("venc: libswscale is not loaded (RELEASE build) — mpeg1 capture off");
            return None;
        }
        unsafe {
            let cname = b"mpeg1video\0".as_ptr() as *const c_char;
            let codec = avcodec_find_encoder_by_name(cname);
            if codec.is_null() {
                tv_base::eventlog::log("venc: mpeg1video encoder absent");
                return None;
            }
            let fmt_yuv = av_get_pix_fmt(b"yuv420p\0".as_ptr() as *const c_char);
            let fmt_rgba = av_get_pix_fmt(b"rgba\0".as_ptr() as *const c_char);
            let fmt_nv12 = av_get_pix_fmt(b"nv12\0".as_ptr() as *const c_char);
            if fmt_yuv < 0 || fmt_rgba < 0 || fmt_nv12 < 0 {
                tv_base::eventlog::log("venc: pix fmt lookup failed");
                return None;
            }
            let ctx = avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return None;
            }
            let mut v = Box::new(Venc {
                ctx,
                frame: std::ptr::null_mut(),
                sws: std::ptr::null_mut(),
                oc: std::ptr::null_mut(),
                st: std::ptr::null_mut(),
                avio: std::ptr::null_mut(),
                pkt: std::ptr::null_mut(),
                sink: Box::new(VencSink {
                    fd: -1,
                    failed: false,
                    send,
                }),
                st_tb: AVRational { num: 1, den: 90000 },
                pts: 0,
                w,
                h,
                nv12_uv: vec![0u8; (w * h / 2) as usize],
                t_sws_us: 0,
                t_enc_us: 0,
                t_n: 0,
                t_last_log: std::time::Instant::now(),
            });
            // {1,30} is in MPEG1's fixed frame-rate table; a non-table rate hard-fails open2.
            // maxrate+bufsize MUST be set together (one alone errors) and bufsize (in BITS)
            // must be >= bit_rate/fps; b/7 with maxrate 1.4x caps I-frame transit spikes on
            // the ~1MB/s tunnel. dct=fastint: the forward DCT is scalar C on ARM (no asm).
            let set = |name: &[u8], val: String| {
                let c = std::ffi::CString::new(val).unwrap();
                av_opt_set(
                    ctx as *mut c_void,
                    name.as_ptr() as *const c_char,
                    c.as_ptr(),
                    0,
                )
            };
            // width/height/pix_fmt BY NAME, not by poking OFF_CTX_*. `video_size` is an
            // IMAGE_SIZE option that writes width and height together, and both it and
            // `pixel_format` live in libavcodec's own option table - confirmed against the
            // device's binary AND present unchanged in 4.x, so unlike a byte offset these three
            // survive an FFmpeg major. The comment that used to justify the pokes claimed no
            // AVOption existed for them; it was simply wrong.
            set(b"video_size\0", format!("{w}x{h}"));
            set(b"pixel_format\0", "yuv420p".into());
            set(b"time_base\0", "1/30".into());
            set(b"b\0", bitrate_bps.to_string());
            set(b"maxrate\0", (bitrate_bps * 7 / 5).to_string());
            set(b"bufsize\0", (bitrate_bps / 7).to_string());
            set(b"g\0", "30".into());
            set(b"bf\0", "0".into());
            set(b"dct\0", "fastint".into());
            let r = avcodec_open2(ctx, codec, std::ptr::null_mut());
            if r < 0 {
                tv_base::eventlog::log(&format!("venc: avcodec_open2 failed ({r})"));
                return None; // Drop frees ctx
            }
            // Runtime ABI self-check: the options set above must round-trip through the
            // AVCodecParameters model, or we stop before feeding frames.
            //
            // LIBRARY-ALLOCATED, never on the stack. `avcodec_parameters_from_context` begins by
            // memset-ing sizeof(AVCodecParameters) bytes, and that size is NOT stable across
            // FFmpeg majors - 136 on the n3.3 the TVs shipped, 176 on the 6.1 this app now
            // bundles. A stack copy sized by our own model is therefore a stack smash the moment
            // the two disagree, and it would be one written by the very check meant to catch ABI
            // drift. Allocating through the library makes the size the library's problem forever;
            // our model stays a read-only PREFIX, which is all it was ever used as.
            let par = avcodec_parameters_alloc();
            if par.is_null() {
                tv_base::eventlog::log("venc: avcodec_parameters_alloc failed");
                return None;
            }
            let ok = avcodec_parameters_from_context(par, ctx) >= 0
                && (*par).width == w
                && (*par).height == h
                && (*par).format == fmt_yuv;
            if !ok {
                tv_base::eventlog::log(&format!(
                    "venc: ABI self-check FAILED (par {}x{} fmt {} vs {}x{} fmt {}) — mpeg off",
                    (*par).width,
                    (*par).height,
                    (*par).format,
                    w,
                    h,
                    fmt_yuv
                ));
                avcodec_parameters_free(&mut { par });
                return None;
            }
            // The library owns par->extradata now, so free the whole thing rather than the
            // field: avcodec_parameters_free does both, and the old hand-free of just extradata
            // would have leaked the 176-byte struct.
            avcodec_parameters_free(&mut { par });
            let frame = av_frame_alloc();
            if frame.is_null() {
                return None;
            }
            v.frame = frame;
            poke_i32(frame, OFF_FRAME_WIDTH, w);
            poke_i32(frame, OFF_FRAME_HEIGHT, h);
            poke_i32(frame, OFF_FRAME_FORMAT, fmt_yuv);
            if av_frame_get_buffer(frame, 32) < 0 {
                tv_base::eventlog::log("venc: frame buffer alloc failed");
                return None;
            }
            v.sws = sws_getContext(
                w,
                h,
                fmt_rgba,
                w,
                h,
                fmt_nv12,
                SWS_BILINEAR,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if v.sws.is_null() {
                tv_base::eventlog::log("venc: sws_getContext failed");
                return None;
            }
            // ---- muxer over custom AVIO ----
            let mut oc: *mut AVFormatContext = std::ptr::null_mut();
            let r = avformat_alloc_output_context2(
                &mut oc,
                std::ptr::null_mut(),
                b"mpegts\0".as_ptr() as *const c_char,
                std::ptr::null(),
            );
            if r < 0 || oc.is_null() {
                tv_base::eventlog::log(&format!("venc: no mpegts muxer in this build ({r})"));
                return None;
            }
            v.oc = oc;
            let iobuf = av_malloc(32768) as *mut u8;
            if iobuf.is_null() {
                return None;
            }
            let avio = avio_alloc_context(
                iobuf,
                32768,
                1,
                v.sink.as_mut() as *mut VencSink as *mut c_void,
                None,
                Some(venc_write_cb),
                None,
            );
            if avio.is_null() {
                free_ptr(iobuf as *mut c_void);
                return None;
            }
            v.avio = avio;
            (*oc).pb = avio;
            let st = avformat_new_stream(oc, std::ptr::null());
            if st.is_null() {
                return None;
            }
            v.st = st;
            let cp = stream_codecpar(st);
            if avcodec_parameters_from_context(cp, ctx) < 0 {
                return None;
            }
            // write_header emits PAT/PMT (re-emitted every 40 TS pkts + each keyframe by
            // default - do NOT set pat_period/sdt_period, that DISABLES the count-based
            // re-emit) and forces st->time_base to 1/90000; read it back for the rescale.
            // sink.fd is still -1 here: header bytes buffer in the 32KB AVIO buffer and
            // reach the socket with the first flushed frame.
            if avformat_write_header(oc, std::ptr::null_mut()) < 0 {
                tv_base::eventlog::log("venc: write_header failed");
                return None;
            }
            v.st_tb = stream_time_base(st);
            v.pkt = av_packet_alloc();
            if v.pkt.is_null() {
                return None;
            }
            tv_base::eventlog::log(&format!(
                "venc: mpeg1/ts up {}x{} @{}bps (st_tb {}/{})",
                w, h, bitrate_bps, v.st_tb.num, v.st_tb.den
            ));
            Some(v)
        }
    }

    /// Encode one top-down RGBA frame and push the muxed TS to `fd`.
    /// Returns false when the socket died (caller drops the client) - encoder
    /// errors also return false and the caller reinits on the next frame.
    /// `flip` = the capture buffer is bottom-up (the 2-pass 480x270 chain); sws_scale
    /// takes a NEGATIVE input stride for that, so there is no CPU flip pass.
    pub fn encode(&mut self, rgba: &[u8], fd: c_int, flip: bool) -> bool {
        unsafe {
            debug_assert!(rgba.len() >= (self.w * self.h * 4) as usize);
            self.sink.fd = fd;
            self.sink.failed = false;
            // the encoder keeps GOP reference frames refcounted on our frame's buffers -
            // make_writable clones them out before we overwrite the pixels (skipping this
            // corrupts inter prediction, it does not crash).
            let t0 = std::time::Instant::now();
            if av_frame_make_writable(self.frame) < 0 {
                return false;
            }
            let row = self.w * 4;
            let (src0, stride0) = if flip {
                (rgba.as_ptr().add(((self.h - 1) * row) as usize), -row)
            } else {
                (rgba.as_ptr(), row)
            };
            let src: [*const u8; 4] = [src0, std::ptr::null(), std::ptr::null(), std::ptr::null()];
            let src_stride: [c_int; 4] = [stride0, 0, 0, 0];
            // NV12 out: Y straight into the frame's Y plane, interleaved UV into scratch
            let fdata = (self.frame as *mut u8).add(OFF_FRAME_DATA) as *mut *mut u8;
            let fls = (self.frame as *mut u8).add(OFF_FRAME_LINESIZE) as *mut c_int;
            let dst: [*mut u8; 4] = [
                *fdata,
                self.nv12_uv.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ];
            let dst_stride: [c_int; 4] = [*fls, self.w, 0, 0];
            sws_scale(
                self.sws,
                src.as_ptr(),
                src_stride.as_ptr(),
                0,
                self.h,
                dst.as_ptr(),
                dst_stride.as_ptr(),
            );
            // deinterleave UVUV... -> planar U + V (chroma is w/2 x h/2)
            let (cw, chh) = ((self.w / 2) as usize, (self.h / 2) as usize);
            let (u_base, v_base) = (*fdata.add(1), *fdata.add(2));
            let (u_ls, v_ls) = (*fls.add(1) as usize, *fls.add(2) as usize);
            for row in 0..chh {
                let s = &self.nv12_uv[row * self.w as usize..row * self.w as usize + cw * 2];
                let up = u_base.add(row * u_ls);
                let vp = v_base.add(row * v_ls);
                for i in 0..cw {
                    *up.add(i) = s[i * 2];
                    *vp.add(i) = s[i * 2 + 1];
                }
            }
            let t1 = std::time::Instant::now();
            poke_i64(self.frame, OFF_FRAME_PTS, self.pts);
            self.pts += 1;
            let r = avcodec_send_frame(self.ctx, self.frame);
            if r < 0 {
                tv_base::eventlog::log(&format!("venc: send_frame failed ({r})"));
                return false;
            }
            loop {
                let r = avcodec_receive_packet(self.ctx, self.pkt);
                if r == AVERROR_EAGAIN || r == AVERROR_EOF {
                    break;
                }
                if r < 0 {
                    tv_base::eventlog::log(&format!("venc: receive_packet failed ({r})"));
                    return false;
                }
                av_packet_rescale_ts(self.pkt, VENC_TB, self.st_tb);
                (*self.pkt).stream_index = 0;
                let wr = av_write_frame(self.oc, self.pkt);
                av_packet_unref(self.pkt);
                if wr < 0 || self.sink.failed {
                    return false;
                }
            }
            // per-frame flush: the AVIO buffer otherwise batches ~100ms of TS at 2.5Mbps
            avio_flush(self.avio);
            self.t_sws_us += (t1 - t0).as_micros() as u64;
            self.t_enc_us += t1.elapsed().as_micros() as u64;
            self.t_n += 1;
            if self.t_last_log.elapsed().as_secs_f32() >= 5.0 && self.t_n > 0 {
                tv_base::eventlog::log(&format!(
                    "venc: {} frm, sws {:.1}ms enc {:.1}ms avg",
                    self.t_n,
                    self.t_sws_us as f32 / self.t_n as f32 / 1000.0,
                    self.t_enc_us as f32 / self.t_n as f32 / 1000.0
                ));
                self.t_sws_us = 0;
                self.t_enc_us = 0;
                self.t_n = 0;
                self.t_last_log = std::time::Instant::now();
            }
            !self.sink.failed
        }
    }
}

impl Drop for Venc {
    fn drop(&mut self) {
        unsafe {
            // no av_write_trailer: the peer is usually already gone, and jsmpeg
            // needs no trailer. Free order: packet, frame, sws, muxer, avio, codec.
            if !self.pkt.is_null() {
                av_packet_free(&mut self.pkt);
            }
            if !self.frame.is_null() {
                av_frame_free(&mut self.frame);
            }
            if !self.sws.is_null() {
                sws_freeContext(self.sws);
            }
            if !self.oc.is_null() {
                avformat_free_context(self.oc); // does not free the caller-set pb
            }
            if !self.avio.is_null() {
                free_avio(self.avio);
            }
            if !self.ctx.is_null() {
                avcodec_free_context(&mut self.ctx);
            }
        }
    }
}

static REGISTER: Once = Once::new();
/// One-time `avformat_network_init`. Was also the home of `av_register_all`, which FFmpeg 4.0
/// made a no-op and 5.0 deleted - registration is automatic now, and the bundled build is 9.0.
fn ensure_registered() {
    REGISTER.call_once(|| unsafe {
        avformat_network_init();
    });
}

fn ver(v: c_uint) -> String {
    format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff)
}

/// Resolve the four FFmpeg libraries by SONAME candidate list. `false` means this device has no
/// FFmpeg we can use, and NOTHING in this module may be called - not even `avformat_version`.
///
/// Kept separate from the version check that follows it because the two failures are different
/// facts about the television and deserve different log lines: "there is no libavformat here at
/// all" versus "there is one and it is the wrong major".
fn load_libraries() -> bool {
    // The bundled FFmpeg lives beside the binary, which is on no library search path - so it is
    // opened by ABSOLUTE PATH. That is not a convenience: webOS 11.2.0 ships FFmpeg 6 itself, and
    // a bare SONAME there could open the television's copy instead of ours.
    let dir = Some(tv_base::paths::app_dir());

    // DEPENDENCY ORDER, and it is load-bearing. libavformat NEEDs libavcodec NEEDs libavutil by
    // SONAME, and these libraries carry no rpath (FFmpeg's configure evals its flags, so
    // -Wl,-rpath,$ORIGIN does not survive - see ci/build-ffmpeg.sh). Loading a dependency FIRST
    // with RTLD_GLOBAL puts it in the global scope under its SONAME, so when the next library
    // names it the loader finds the one we just opened rather than searching the system paths and
    // finding the TV's. Open libavformat first and that guarantee is gone.
    let mut ok = true;
    for (what, verdict) in [
        ("avutil", avutil::load(dir)),
        ("swresample", swresample::load(dir)),
        ("avcodec", avcodec::load(dir)),
        ("avformat", avformat::load(dir)),
    ] {
        match verdict {
            tv_base::dynlib::Loaded::Ok(soname) => {
                tv_base::eventlog::log(&format!("ff: bound {what} -> {soname}"))
            }
            tv_base::dynlib::Loaded::NoLibrary => {
                ok = false;
                tv_base::eventlog::log(&format!(
                    "ff: {what} is MISSING from the app directory — the bundled FFmpeg did not \
                     deploy. Playback will refuse; reinstall the package."
                ));
            }
            tv_base::dynlib::Loaded::Incomplete(soname, n) => {
                ok = false;
                tv_base::eventlog::log(&format!(
                    "ff: {soname} is missing {n} symbol(s) we need — named above"
                ));
            }
        }
    }
    // swscale is NOT required, and treating it as required broke the RELEASE build entirely:
    // `RELEASE=1` drops it from the package (only the dev capture stream's RGBA->YUV conversion
    // uses it), so an all-or-nothing loop over four libraries would have found three, reported
    // failure, and refused to play anything - in the configuration users actually receive, and in
    // no configuration ever tested here.
    match swscale::load(dir) {
        tv_base::dynlib::Loaded::Ok(soname) => {
            SWS_OK.store(true, Ordering::Relaxed);
            tv_base::eventlog::log(&format!("ff: bound swscale -> {soname}"));
        }
        _ => {
            tv_base::eventlog::log("ff: no swscale (expected in a RELEASE build) — dev capture JPEG/MPEG1 off")
        }
    }
    ok
}

/// Did the bundled FFmpeg load and match the ABI table? The diagnostics read-out's first question
/// about the demuxer: false means every playback will refuse before it opens a socket, which from
/// the outside looks identical to a stall.
fn abi_ok() -> bool {
    ABI_OK.load(std::sync::atomic::Ordering::Relaxed)
}

/// Did the CURRENT demux session hand over a single video AU? Distinguishes "the demuxer produced
/// nothing" from "it produced AUs that never reached the decoder" - two stalls that present
/// identically as a spinner. See [`PUSHED_ANY`].
pub fn pushed_any() -> bool {
    PUSHED_ANY.load(std::sync::atomic::Ordering::Relaxed)
}

/// The three FFmpeg majors actually bound, for the read-out's build line. Zeroes before [`boot`]
/// has run or when the libraries did not load.
pub fn majors() -> (u32, u32, u32) {
    if !abi_ok() {
        return (0, 0, 0);
    }
    unsafe {
        (
            avformat_version() >> 16,
            avcodec_version() >> 16,
            avutil_version() >> 16,
        )
    }
}

/// Did [`boot`] bind an FFmpeg with the ABI this build was made for (the demuxer and the probe
/// can run)?
pub fn ready() -> bool {
    ABI_OK.load(Ordering::Acquire)
}

/// Boot smoke test + optional ABI probe. Called once at startup.
pub fn boot() {
    // The https media transport's own table, resolved here so the `dlopen` and its log line land
    // on the MAIN thread at start-up rather than inside a demux worker - and after `app.rs` has
    // run `net::global_init`, which is what makes `curl_global_init` main-thread-only as its doc
    // requires. Independent of FFmpeg: a set with no libcurl multi still demuxes local samples,
    // and a set with no FFmpeg still signs in.
    crate::curlio::boot();
    if !load_libraries() {
        ABI_OK.store(false, std::sync::atomic::Ordering::Relaxed);
        tv_base::eventlog::log("ff: FFmpeg unavailable — the app runs, playback will refuse");
        return;
    }
    unsafe {
        let (fmt, cod, utl) = (avformat_version(), avcodec_version(), avutil_version());
        crate::player::log(&format!(
            "ff: avformat={} avcodec={} avutil={}",
            ver(fmt),
            ver(cod),
            ver(utl)
        ));
        // THE MAJORS ARE THE ABI. Everything below reads FFmpeg structs at offsets that are fixed
        // per major: FFmpeg guarantees layout only within one, and majors reorder.
        //
        // One version, checked against the one we built for. This used to SELECT between two
        // offset tables because the app read the television's FFmpeg and every webOS release
        // shipped a different one; bundling made that a single equality. The refusal still
        // matters: a wrong offset does not fail, it succeeds with garbage. Reading `codecpar` at
        // the n3.3 offset on a modern AVStream lands hundreds of bytes past the struct and
        // dereferences whatever it finds, on a device with no debugger.
        // The version we BUILT against, not a version we hope to find. These libraries ship
        // inside the package under a -wi SONAME, so a mismatch means the deployed payload is
        // stale or someone put a different file there - not a firmware difference.
        let bad = (fmt >> 16, cod >> 16, utl >> 16) != (63, 63, 61);
        ABI_OK.store(!bad, Ordering::Relaxed);
        if bad {
            tv_base::eventlog::log(
                "ff: BUNDLED FFmpeg is not the one this build expects (want avformat 63 / \
                 avcodec 63 / avutil 61) — the app directory holds a stale or foreign \
                 libav*-wi; refusing to demux",
            );
        }
    }
    // Whether the software decode -> PCM path bound. Latched before any playback decides its
    // audio feed.
    adec::detect();
    // Whether the subtitle decoder bound; without it no subtitle track is listed.
    subdec::detect();
    // Phase A dev trigger: /tmp/webosiptv-ffprobe holds a media URL to open + dump streams,
    // confirming the FFmpeg-3.3 struct offsets against known media before we build on them.
    if let Some(u) = tv_base::devtrig::read("ffprobe") {
        if !u.is_empty() {
            probe(&u);
        }
    }
}

/// Open `url` via libavformat, find streams, and log codec_id/dims/time_base for each -
/// the on-device confirmation that the FFI struct layout is correct. Uses libavformat's
/// own HTTP for the probe (the demuxer proper will use a custom AVIO over stream.rs), so that one
/// request names libavformat (`Lavf/...`), not the installed `net::user_agent`: it is reachable only with
/// the `devtriggers` feature, which the shipping set does not have, and it is the one transport
/// `ci/check-user-agent.py` cannot see.
fn probe(url: &str) {
    ensure_registered();
    unsafe {
        let cu = match std::ffi::CString::new(url) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut fmt: *mut AVFormatContext = std::ptr::null_mut();
        let r = avformat_open_input(
            &mut fmt,
            cu.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if r < 0 || fmt.is_null() {
            crate::player::log(&format!("ffprobe: avformat_open_input failed r={r}"));
            return;
        }
        let r2 = avformat_find_stream_info(fmt, std::ptr::null_mut());
        if r2 < 0 {
            crate::player::log(&format!("ffprobe: find_stream_info failed r={r2}"));
            avformat_close_input(&mut fmt);
            return;
        }
        let ns = (*fmt).nb_streams;
        crate::player::log(&format!(
            "ffprobe: nb_streams={ns} duration_us={} (expect HEVC codec_id=174 3840x1920)",
            fmt_duration(fmt)
        ));
        let streams = (*fmt).streams;
        for i in 0..ns {
            let st = *streams.add(i as usize);
            let cp = stream_codecpar(st);
            let tb = stream_time_base(st);
            crate::player::log(&format!(
                "ffprobe: stream[{}] idx={} type={} codec_id={} {}x{} sr={} tb={}/{}",
                i,
                stream_index(st),
                (*cp).codec_type,
                (*cp).codec_id,
                (*cp).width,
                (*cp).height,
                (*cp).sample_rate,
                tb.num,
                tb.den
            ));
        }
        // BSF presence check (Phase A): both mp4toannexb filters must be compiled in.
        let hn = std::ffi::CString::new("hevc_mp4toannexb").unwrap();
        let an = std::ffi::CString::new("h264_mp4toannexb").unwrap();
        crate::player::log(&format!(
            "ffprobe: bsf hevc={} h264={}",
            !av_bsf_get_by_name(hn.as_ptr()).is_null(),
            !av_bsf_get_by_name(an.as_ptr()).is_null()
        ));
        avformat_close_input(&mut fmt);
    }
}

// ==== the demuxer (Phases B - D) ===================================================

// whence values for the AVIO seek callback
const SEEK_SET: c_int = 0;
const SEEK_CUR: c_int = 1;
const SEEK_END: c_int = 2;
const AVSEEK_SIZE: c_int = 0x10000;

/// **Which transport is under the AVIO.** They are not interchangeable and the choice is made
/// once, at open, from the stream itself: an `.m3u8` URL, or a curl open answered with an HLS
/// media type, is [`Src::Hls`]; an https URL, or ANY stream that carries request headers
/// ([`crate::headers::RequestHeaders`]), is pulled through curl; a header-less plaintext URL reads
/// through the socket, unless its open is redirected to https (`stream::redirect`), in which case
/// the socket open hands over to curl. Never guessed per call.
///
/// * [`Src::Socket`] is `stream.rs`'s raw TCP socket wrapping the ENGINE-owned `HttpStream` -
///   cleartext, seeking by closing and re-opening with a byte `Range`.
/// * [`Src::Curl`] is [`crate::curlio`], which the demux thread owns outright. `ff.rs` never
///   learns curl-multi mechanics: all it can ask is `read`/`seek`/`size`/`status`/`abort`,
///   deliberately the same five questions the socket answers.
enum Src {
    Socket {
        /// The Engine's stream - NOT owned here. `player::engine` allocates it, publishes it in
        /// `SHARED.hs_ptr`, `http_shutdown`s it at teardown and closes it after the join.
        hs: *mut HttpStream,
        host: CString,
        port: c_int,
        path: CString,
    },
    /// Owned by this state, and so by the demux thread - which is why teardown reaches it through
    /// `curlio`'s registry instead of a pointer the engine holds. `curlio`'s module doc says why
    /// that is not the accident it looks like.
    Curl(Box<crate::curlio::CurlSource>),
    /// An HLS playlist ([`crate::hls`]): the segments of the variant this set decodes, decrypted and
    /// concatenated (init section first for fMP4) into one byte stream. It has no byte addresses,
    /// so its AVIO is UNSEEKABLE (no `seek_cb`); a VOD seek reopens it at a segment instead
    /// (`demux`'s read loop) and a live one at its edge.
    Hls(Box<crate::hls::CurlHls>),
}

/// The pre-Load stream probe: open with the bundled demuxer, read the streams, close.
#[path = "ff_probe.rs"]
pub mod probe;
/// Audio the pipeline cannot decode (MP2; MP3 where the set has no MPEG audio row; every track on
/// a PCM-only pipeline) decoded on the CPU over the bundled libavcodec and fed as PCM.
#[path = "ff_adec.rs"]
pub mod adec;
/// DVB subtitles: the streams listed and their packets decoded into `crate::subtitles` cues.
#[path = "ff_subdec.rs"]
pub mod subdec;

/// AVIO backing state: wraps the demux transport so libavformat reads through it and can seek by
/// byte offset. Boxed so its address is stable for the C callbacks.
struct AvioState {
    src: Src,
    aq: *mut AuQueue,
    off: i64,
    size: i64,
    /// A callback saw a transport failure during the current enclosing libavformat operation.
    /// FFmpeg may heal it through `seek_cb`; only an operation that still returns failure may
    /// publish it to the player.
    io_failed: bool,
    /// Bounded socket/curl overflow while the demuxer is parked in `aq_push` (full AU lanes), so
    /// the server's TCP window stays open.
    bounce: Vec<u8>,
    bounce_pos: usize,
    /// `seek_cb` answered a seek to (or past) the known size without fetching, so the transport's
    /// stream is somewhere else and `read_cb` must answer the clean end itself. Any other read at
    /// the end still asks the transport, which is how a failure there stays an error.
    parked_at_end: bool,
}

impl AvioState {
    /// **Fold the AU lane's abort into the transport's own.**
    ///
    /// There are two abort signals in the media path and they arrive by different routes. The AU
    /// lane's flag (`aq_is_aborted`) is what these callbacks have always checked on entry, and it
    /// is set by every stopper - teardown, and `start_bufferfeed`'s early return when the media
    /// thread will not spawn. `curlio`'s wake pipe is what reaches a thread already BLOCKED inside
    /// `curl_multi_wait`, which the AU flag cannot do; teardown fires it separately.
    ///
    /// This is the join between them, in the one place both are visible: once the lane is aborted,
    /// the curl source is latched too, so it refuses on its own terms afterwards rather than
    /// depending on every future caller re-checking the lane. The socket source needs no
    /// equivalent - closing it IS the latch, and the engine owns that.
    fn latch_abort(&self) {
        if let Src::Curl(cs) = &self.src {
            cs.abort();
        }
    }

    fn clear_bounce(&mut self) {
        self.bounce.clear();
        self.bounce_pos = 0;
    }

    fn take_bounce(&mut self, dst: &mut [u8]) -> usize {
        let avail = self.bounce.len().saturating_sub(self.bounce_pos);
        if avail == 0 || dst.is_empty() {
            return 0;
        }
        let n = avail.min(dst.len());
        dst[..n].copy_from_slice(&self.bounce[self.bounce_pos..self.bounce_pos + n]);
        self.bounce_pos += n;
        if self.bounce_pos == self.bounce.len() {
            self.clear_bounce();
        }
        n
    }

    fn bounce_remaining(&self) -> bool {
        self.bounce_pos < self.bounce.len()
    }

    fn transfer_finished(&self) -> bool {
        match &self.src {
            Src::Socket { hs, .. } => tv_net::stream::http_body_done(*hs),
            Src::Curl(cs) => cs.body_complete(),
            // Nothing to drain between two segment reads: a parked HLS stream waits on its queue.
            Src::Hls(_) => true,
        }
    }

    /// Drain the live media socket into [`Self::bounce`] without blocking. Called from
    /// [`crate::aq::aq_push_with_drain`] so the stream's TCP window stays open while the AU
    /// queues are full.
    ///
    /// Returns whether the park should poll again soon: `true` only while bounce has room and
    /// more body is still expected. Cap, abort, I/O fail or a finished body `cond_wait`.
    /// A long pause at CAP can let the server keep-alive timer FIN the socket; reuse already
    /// redials on `Transport`. Do not raise CAP.
    fn drain_wire(&mut self) -> bool {
        const CAP: usize = 384 * 1024;
        if unsafe { crate::aq::aq_is_aborted(self.aq) } {
            self.clear_bounce();
            return false;
        }
        if self.bounce_pos > 0 {
            self.bounce.drain(..self.bounce_pos);
            self.bounce_pos = 0;
        }
        if self.bounce.len() >= CAP {
            return false;
        }
        loop {
            if unsafe { crate::aq::aq_is_aborted(self.aq) } {
                self.clear_bounce();
                return false;
            }
            if self.bounce.len() >= CAP {
                return false;
            }
            let start = self.bounce.len();
            self.bounce.resize(start + (CAP - start), 0);
            let n = match &mut self.src {
                Src::Socket { hs, .. } => {
                    tv_net::stream::http_drain_available(*hs, &mut self.bounce[start..])
                }
                Src::Curl(cs) => cs.drain_available(&mut self.bounce[start..]),
                Src::Hls(_) => 0,
            };
            if n > 0 {
                self.bounce.truncate(start + n as usize);
                SHARED.dg_net_rx.fetch_add(n as i64, Ordering::Relaxed);
                continue;
            }
            self.bounce.truncate(start);
            if n < 0 {
                self.io_failed = true;
                return false;
            }
            return drain_keep_polling(self.bounce.len() < CAP, self.transfer_finished());
        }
    }
}

fn drain_keep_polling(bounce_under_cap: bool, body_finished: bool) -> bool {
    bounce_under_cap && !body_finished
}

fn park_read_fails_closed(bounce_remaining: bool, io_failed: bool) -> bool {
    io_failed && !bounce_remaining
}

/// AVIOContext leading fields, so we can free avio->buffer manually (FFmpeg 3.3 has no
/// avio_context_free). `buffer` sits at +4, right after `av_class`.
#[repr(C)]
struct AVIOCtxHead {
    av_class: *const c_void,
    buffer: *mut u8,
}

/// Reopens [`read_cb`] tries, within one read, after the transfer is cut mid-body.
const BODY_HEAL_ATTEMPTS: u8 = 3;

extern "C" fn read_cb(op: *mut c_void, dst: *mut u8, n: c_int) -> c_int {
    unsafe {
        let s = &mut *(op as *mut AvioState);
        let mut heals: u8 = 0;
        loop {
            // interrupt: bail out of a blocked read on teardown (aborted) only. A seek does NOT
            // interrupt the read - the demux thread services it itself between two av_read_frame
            // calls (see the read loop), so there is nothing to unblock and nothing to race.
            if crate::aq::aq_is_aborted(s.aq) {
                s.latch_abort();
                s.clear_bounce();
                return AVERROR_EOF;
            }
            let bounced = s.take_bounce(std::slice::from_raw_parts_mut(dst, n.max(0) as usize));
            if bounced > 0 {
                s.off += bounced as i64;
                return bounced as c_int;
            }
            if park_read_fails_closed(s.bounce_remaining(), s.io_failed) {
                return AVERROR_IO;
            }
            // Both sources use the same three-way return - >0 bytes, 0 clean end, <0 error - so
            // the EOF decision below stays one branch rather than one per transport.
            let r = match &mut s.src {
                // Parked at the end by `seek_cb`: the clean end, without asking a transport whose
                // stream sits elsewhere.
                _ if s.parked_at_end => 0,
                Src::Socket { hs, .. } => tv_net::stream::http_read_until(
                    *hs,
                    dst as *mut c_uchar,
                    n,
                    None,
                    &mut tv_base::checkpoint::NoCheckpoint,
                ),
                // The null/length guard `stream::http_read` does for itself.
                Src::Curl(_) if dst.is_null() || n <= 0 => 0,
                Src::Curl(cs) => cs.read(std::slice::from_raw_parts_mut(dst, n as usize)),
                Src::Hls(_) if dst.is_null() || n <= 0 => 0,
                // Retries, skips and the live wait are the HLS source's own; what reaches here is
                // bytes, the end of a VOD stream, or a failure it has already named in the log.
                Src::Hls(h) => match h.read(std::slice::from_raw_parts_mut(dst, n as usize)) {
                    Ok(got) => got as c_int,
                    Err(e) => {
                        hls_failed(&e);
                        -1
                    }
                },
            };
            // Teardown may have arrived while the transport was blocked. Its wake commonly
            // surfaces as socket EOF or curl failure; classify from the owning AU abort flag
            // before either transport result can become an active-stream error.
            if crate::aq::aq_is_aborted(s.aq) {
                s.latch_abort();
                return AVERROR_EOF;
            }
            if r < 0 {
                // A server that drops the connection mid-body (one that reaps the socket while
                // the viewer is paused, a reset from the LAN) is answered by reopening at the byte
                // we stopped at. libavformat only heals through `seek_cb` when the demuxer itself
                // seeks; a plain read failure reaches `av_read_frame` as EIO and ends the session.
                // Stall timeouts (rc=28) stay failures; only cut transfers heal here.
                if let Src::Curl(cs) = &mut s.src {
                    let cut = cs
                        .body_failure()
                        .is_some_and(|f| matches!(f.rc, 18 | 55 | 56 | 92));
                    let inside = s.off >= 0 && (s.size < 0 || s.off < s.size);
                    if cut && inside && heals < BODY_HEAL_ATTEMPTS {
                        heals += 1;
                        if cs.seek(s.off) {
                            continue;
                        }
                    }
                }
                // Teardown was handled above through the AU flag and remains EOF. A negative
                // result here is therefore a real transport/range failure.
                s.io_failed = true;
                return AVERROR_IO;
            }
            if r == 0 {
                return AVERROR_EOF;
            }
            s.off += r as i64;
            SHARED.dg_net_rx.fetch_add(r as i64, Ordering::Relaxed);
            return r;
        }
    }
}

extern "C" fn seek_cb(op: *mut c_void, offset: i64, whence: c_int) -> i64 {
    unsafe {
        let s = &mut *(op as *mut AvioState);
        if whence == AVSEEK_SIZE {
            return s.size;
        }
        // Teardown may have raced us here - the same race the reopen site in `demux` guards, and
        // this is the third way in. Teardown aborts the lanes, fires ONE `http_shutdown` at this
        // socket, then JOINS this thread from the main thread. Our AVIO is SEEKABLE, so
        // libavformat treats the broken read as recoverable and heals it by calling US (the
        // 5938b5f mechanism, spelled out in the read loop's note below). Without this check it
        // gets a BRAND NEW connection the already-fired shutdown cannot touch - and because
        // `read_cb` then reports EOF again, that recovery repeats: measured on the host at ONE
        // full `http_open` per hop, so the main thread's join is held for a whole reopen every
        // time round, not just once. `read_cb` has bailed on this flag since it was written; a
        // seek was the way around it.
        //
        // Placed AFTER the AVSEEK_SIZE branch on purpose: a size query is a field read, not I/O,
        // and answering it during teardown costs nothing.
        if crate::aq::aq_is_aborted(s.aq) {
            s.latch_abort();
            s.clear_bounce();
            return -1;
        }
        s.clear_bounce();
        let target = match whence {
            SEEK_SET => offset,
            SEEK_CUR => s.off + offset,
            SEEK_END => s.size + offset,
            _ => return -1,
        };
        if target < 0 {
            return -1;
        }
        // **A seek to the end is a position, not a request.** libavformat restores an
        // `avio_tell` taken at EOF this way - `mov_read_chapters` does it after reading a chapter
        // track, and a moov-at-end MP4 leaves the AVIO at exactly the size - and the only answer
        // a server can give `Range: bytes=<size>-` is 416. Reopening for it closed the live
        // stream and failed, while libavformat's buffer still held the chapter samples: the first
        // video sample just past them read a dead transport ("partial file"), and the player sat
        // at t=0. Nothing here is fetched, so nothing is closed; `read_cb` answers EOF while
        // `parked_at_end`, and the next seek into the file reopens as it always has
        // (`ff_eof_seek_tests.rs`). An unsized source has no end to compare with and still asks.
        if s.size >= 0 && target >= s.size {
            s.off = target;
            s.io_failed = false;
            s.parked_at_end = true;
            return target;
        }
        s.parked_at_end = false;
        let aq = s.aq;
        let mut hopped_to_tls = None;
        let reopen_began = std::time::Instant::now();
        let ok = match &mut s.src {
            // Redirects are followed here too (`stream::redirect`), and the effective target is
            // written back so the NEXT seek starts from it rather than replaying the chain. A hop
            // to https cannot stay on this socket; it becomes the curl source below.
            Src::Socket {
                hs,
                host,
                port,
                path,
            } => {
                tv_net::stream::http_close(*hs);
                let origin = tv_net::net::origin::Origin::http(&host.to_string_lossy(), *port);
                let from = path.to_string_lossy().into_owned();
                let req = tv_net::stream::redirect::Request {
                    origin: &origin,
                    path: &from,
                    credentials: None,
                    range_from: Some(target),
                    deadline: None,
                    same_origin_only: false,
                    credential_gate: plaintext_media_gate,
                };
                match tv_net::stream::redirect::open_following(
                    *hs,
                    &req,
                    &mut tv_base::checkpoint::NoCheckpoint,
                ) {
                    Ok(tv_net::stream::redirect::Opened::Socket(t)) => {
                        if let (Ok(h), Ok(p)) =
                            (CString::new(t.origin.host()), CString::new(t.path))
                        {
                            *host = h;
                            *port = t.origin.port() as c_int;
                            *path = p;
                        }
                        true
                    }
                    Ok(tv_net::stream::redirect::Opened::Tls(t)) => {
                        // The socket only ever carries a header-less stream (see `Src`), so its
                        // https hop has no stream headers to take along either.
                        match open_curl_hop(&t.url(), target, &crate::headers::RequestHeaders::none(), aq) {
                            Ok(cs) => {
                                hopped_to_tls = Some(cs);
                                true
                            }
                            Err(e) => {
                                crate::player::log(&format!("ff: seek https hop FAILED: {e:?}"));
                                false
                            }
                        }
                    }
                    Err(_) => false,
                }
            }
            // `curlio` REFUSES a Range the server answered with a 200, where `stream.rs` accepts
            // any 2xx. That is the one behavioural difference between these two arms, and it is
            // deliberate: bytes from the head of the file, delivered as though they were the bytes
            // at `target`, are corruption that looks like success.
            Src::Curl(cs) => cs.seek(target),
            // Never reached: an HLS AVIO is allocated without a seek callback.
            Src::Hls(_) => false,
        };
        if let Some(cs) = hopped_to_tls {
            s.src = Src::Curl(cs);
        }
        // One line per reopen (a seek into the file, or libavformat healing a failed read).
        let reopen_ms = reopen_began.elapsed().as_millis();
        crate::player::log(&format!("ff: seek_cb reopen off={target} ok={ok} ms={reopen_ms}"));
        if ok && PUBLISHES_REOPENS.with(|p| p.get()) {
            note_reopen_ms(u32::try_from(reopen_ms).unwrap_or(u32::MAX));
        }
        if !ok {
            return -1;
        }
        s.off = target;
        // This may be libavformat healing a failed read. A source validated at the requested byte
        // has recovered; a later callback failure will arm the bit again.
        s.io_failed = false;
        target
    }
}

thread_local! {
    /// Set by [`demux`] on its own thread: only the playback's reopens feed
    /// [`SHARED.reopen_ms_est`](crate::player::shared::Shared::reopen_ms_est). The probe runs the
    /// same `seek_cb` on another thread and must leave `SHARED` alone (`ff_probe.rs`).
    static PUBLISHES_REOPENS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Fold one range reopen's latency into the running estimate: an EWMA (1/4 weight to the new
/// sample) so one slow reopen raises the watchdog without a single fast one erasing it.
fn note_reopen_ms(ms: u32) {
    let est = SHARED.reopen_ms_est.load(Ordering::Relaxed);
    let next = if est == 0 { ms } else { ((u64::from(est) * 3 + u64::from(ms)) / 4) as u32 };
    SHARED.reopen_ms_est.store(next.max(1), Ordering::Relaxed);
}

/// Settle one `av_read_frame` result against errors observed by its AVIO callbacks.
fn frame_read_failed(state: &mut AvioState, result: c_int) -> bool {
    if result >= 0 {
        return false;
    }
    if state.io_failed {
        // Issue #504: `r=-5` alone could not tell a proxy's cut from a reset from a stall. The
        // curl source remembers why its transfer died (until a reopen recovers), so the line
        // names it, and the same facts are published for the handled playback error.
        let failure = match &state.src {
            Src::Curl(cs) => cs.body_failure(),
            _ => None,
        };
        let cause = failure.map_or_else(String::new, |f| {
            format!(
                " curl_rc={}{}",
                f.rc,
                if f.reopen_failed { " reopen=failed" } else { "" }
            )
        });
        crate::player::log(&format!(
            "ff: media transport failed during av_read_frame r={result}{cause}"
        ));
        if let Some(f) = failure {
            SHARED.dg_transport_rc.store(f.rc, Ordering::Release);
            SHARED
                .dg_transport_age_ms
                .store(f.age_ms.min(i64::MAX as u64) as i64, Ordering::Release);
            SHARED
                .dg_transport_reopen_failed
                .store(f.reopen_failed, Ordering::Release);
        }
        SHARED.demux_io_failed.store(true, Ordering::Release);
    }
    true
}

#[inline]
unsafe fn pts_ns(pkt: *const AVPacket, st: *mut AVStream) -> i64 {
    let t = if (*pkt).pts != AV_NOPTS_VALUE {
        (*pkt).pts
    } else {
        (*pkt).dts
    };
    if t == AV_NOPTS_VALUE {
        return 0;
    }
    av_rescale_q(t, stream_time_base(st), NS_TB)
}

/// AAC sampling-frequency index → Hz (ISO 14496-3 table 1.18).
const AAC_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// The length of one ADTS frame (1024 samples at the header's rate), or None when `data` does
/// not start with an ADTS header.
fn adts_frame_ns(data: &[u8]) -> Option<i64> {
    if data.len() < 3 || data[0] != 0xFF || data[1] & 0xF6 != 0xF0 {
        return None;
    }
    aac_frame_ns((data[2] >> 2) & 0x0F)
}

fn aac_frame_ns(freq_idx: u8) -> Option<i64> {
    AAC_RATES.get(freq_idx as usize).map(|&hz| 1024 * 1_000_000_000 / hz as i64)
}

/// Times the audio AUs the demuxer hands over untimed. MPEG-TS packs several AAC frames into one
/// PES and only the first carries a PTS; each follow-on frame starts where the one before ended.
#[derive(Debug, Default)]
struct AudioClock {
    last: Option<(i64, i64)>,
}

impl AudioClock {
    /// The pts for an AU with time `pts` (None: untimed) lasting `frame_ns`.
    fn stamp(&mut self, pts: Option<i64>, frame_ns: i64) -> i64 {
        let t = match (pts, self.last) {
            (Some(p), _) => p,
            (None, Some((prev, prev_len))) => prev + prev_len,
            (None, None) => 0,
        };
        self.last = Some((t, frame_ns));
        t
    }
}

#[inline]
unsafe fn pts_ns_opt(pkt: *const AVPacket, st: *mut AVStream) -> Option<i64> {
    let t = if (*pkt).pts != AV_NOPTS_VALUE {
        (*pkt).pts
    } else {
        (*pkt).dts
    };
    (t != AV_NOPTS_VALUE).then(|| av_rescale_q(t, stream_time_base(st), NS_TB))
}

unsafe fn free_ptr(p: *mut c_void) {
    let mut q = p;
    av_freep(&mut q as *mut *mut c_void as *mut c_void);
}

/// Free a custom AVIOContext (buffer + context): FFmpeg 3.3 has no avio_context_free and
/// avformat_close_input does not free a caller-set pb.
unsafe fn free_avio(avio: *mut AVIOContext) {
    if avio.is_null() {
        return;
    }
    let head = avio as *mut AVIOCtxHead;
    av_freep(std::ptr::addr_of_mut!((*head).buffer) as *mut c_void); // frees + NULLs avio->buffer
    let mut p = avio;
    av_freep(&mut p as *mut *mut AVIOContext as *mut c_void); // frees + NULLs the context
}

// -- AV1 framing ------------------------------------------------------------------------
//
// The TV pipeline is handed no codec_data for AV1 (libpf's AV1 caps carry none, and the pipeline
// has no av1parse element), so the configuration has to travel in-band. These helpers turn the
// demuxed packet into a Section-5 temporal unit: a temporal delimiter first, the av1C config OBUs
// in front of each keyframe, and every OBU carrying a size field. Everything here is pure, so
// the framing is tested on the host (ff_packet_framing_tests.rs).

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_METADATA: u8 = 5;
/// A temporal delimiter OBU with an empty payload: header 0x12 (type 2, size field), size 0.
const AV1_TD: [u8; 2] = [0x12, 0x00];

/// Read one leb128 value from the front of `b` (AV1 section 4.10.5), returning the value and the
/// number of bytes it took. A run with no terminator inside eight bytes, a truncated run, and a
/// value wider than 32 bits are all rejected: the spec bounds every leb128 field to 32 bits.
fn read_leb128(b: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &byte) in b.iter().take(8).enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return (value <= u64::from(u32::MAX)).then_some((value, i + 1));
        }
    }
    None
}

/// Append `v` as a minimal leb128 encoding.
fn write_leb128(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// One OBU inside a byte run: where its header starts, how long the header is (one byte, plus
/// one when the extension flag is set), where its payload starts, and where it ends (exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ObuSpan {
    start: usize,
    header_len: usize,
    payload_start: usize,
    end: usize,
    obu_type: u8,
    has_size: bool,
}

/// Split an OBU run into its OBUs. An OBU without a size field runs to the end of the run and
/// is therefore the last one. Returns None for a set forbidden bit, a truncated header or size,
/// or a payload that overruns the run.
fn walk_obus(b: &[u8]) -> Option<Vec<ObuSpan>> {
    let mut spans = Vec::new();
    let mut p = 0usize;
    while p < b.len() {
        let h = b[p];
        if h & 0x80 != 0 {
            return None;
        }
        let obu_type = (h >> 3) & 0x0f;
        let header_len = 1 + usize::from((h >> 2) & 1);
        let has_size = (h >> 1) & 1 == 1;
        if p + header_len > b.len() {
            return None;
        }
        let body = p + header_len;
        let (payload_start, end) = if has_size {
            let (size, n) = read_leb128(&b[body..])?;
            let payload_start = body + n;
            // u64 arithmetic: on the 32-bit TV `payload_start + size` could wrap a usize.
            if payload_start as u64 + size > b.len() as u64 {
                return None;
            }
            (payload_start, payload_start + size as usize)
        } else {
            (body, b.len())
        };
        spans.push(ObuSpan {
            start: p,
            header_len,
            payload_start,
            end,
            obu_type,
            has_size,
        });
        p = end;
    }
    Some(spans)
}

/// Append one OBU with a size field. An OBU that already has one is copied verbatim; a sizeless
/// one (only ever the last in its run) gets the minimal leb128 of its payload length.
fn push_obu_sized(b: &[u8], s: &ObuSpan, out: &mut Vec<u8>) {
    if s.has_size {
        out.extend_from_slice(&b[s.start..s.end]);
        return;
    }
    out.push(b[s.start] | 0x02);
    if s.header_len == 2 {
        out.push(b[s.start + 1]);
    }
    write_leb128((s.end - s.payload_start) as u64, out);
    out.extend_from_slice(&b[s.payload_start..s.end]);
}

/// The av1C configOBUs to insert in front of a keyframe: every sequence header, then every
/// metadata OBU (HDR10 MDCV/CLL may live only in the record). Empty when the record carries no
/// sequence header, because metadata alone is never inserted. Accepts both the av1C record
/// (marker and version byte 0x81, three bytes of fixed fields, then the OBUs) and a bare OBU run.
fn parse_av1c(ed: &[u8]) -> Vec<u8> {
    let obus = if ed.len() >= 4 && ed[0] == 0x81 {
        &ed[4..]
    } else if !ed.is_empty() && ed[0] & 0x80 == 0 {
        ed
    } else {
        return Vec::new();
    };
    let Some(spans) = walk_obus(obus) else {
        return Vec::new();
    };
    if !spans.iter().any(|s| s.obu_type == OBU_SEQUENCE_HEADER) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for s in spans.iter().filter(|s| s.obu_type == OBU_SEQUENCE_HEADER) {
        push_obu_sized(obus, s, &mut out);
    }
    for s in spans.iter().filter(|s| s.obu_type == OBU_METADATA) {
        push_obu_sized(obus, s, &mut out);
    }
    out
}

/// Which pieces of the AV1 temporal-unit framing are emitted. DEFAULT is the conformant form.
/// The other settings exist so the reporter can A/B the Realtek decoder's expectations on a
/// device; they come from the `av1obu` dev trigger and are never set in a release build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Av1Framing {
    /// Prepend a temporal delimiter when the packet has none.
    td: bool,
    /// Prepend the av1C config block on keyframes that carry no sequence header.
    seq: bool,
    /// Pass the packet bytes through exactly as demuxed.
    raw: bool,
}

impl Av1Framing {
    const DEFAULT: Av1Framing = Av1Framing {
        td: true,
        seq: true,
        raw: false,
    };

    /// Parse the `av1obu` trigger. Tokens are separated by commas or whitespace; each one sets
    /// one flag independently, and unknown tokens are ignored.
    fn from_trigger(s: Option<&str>) -> Av1Framing {
        let mut f = Self::DEFAULT;
        for tok in s
            .unwrap_or("")
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|t| !t.is_empty())
        {
            match tok {
                "notd" => f.td = false,
                "noseq" => f.seq = false,
                "raw" => f.raw = true,
                _ => {}
            }
        }
        f
    }
}

/// Frame one demuxed AV1 packet as a temporal unit in `out`, returning `key` unchanged.
/// A packet that does not parse as OBUs is copied through untouched: it is never dropped.
fn av1_packet_to_tu(pkt: &[u8], key: bool, cfg: &[u8], f: Av1Framing, out: &mut Vec<u8>) -> bool {
    out.clear();
    let spans = match walk_obus(pkt) {
        Some(spans) if !f.raw => spans,
        _ => {
            out.extend_from_slice(pkt);
            return key;
        }
    };
    let has_td = spans
        .first()
        .is_some_and(|s| s.obu_type == OBU_TEMPORAL_DELIMITER);
    let has_seq = spans.iter().any(|s| s.obu_type == OBU_SEQUENCE_HEADER);
    let mut rest = &spans[..];
    if has_td {
        push_obu_sized(pkt, &spans[0], out);
        rest = &spans[1..];
    } else if f.td {
        out.extend_from_slice(&AV1_TD);
    }
    if key && f.seq && !has_seq && !cfg.is_empty() {
        out.extend_from_slice(cfg);
    }
    if let Some((last, body)) = rest.split_last() {
        for s in body {
            out.extend_from_slice(&pkt[s.start..s.end]);
        }
        push_obu_sized(pkt, last, out);
    }
    key
}

/// How a demuxed video packet is turned into the access unit the television's pipeline is fed:
/// the one place a codec id is mapped to a framing, so a codec with no arm is REFUSED instead of
/// falling into the Annex-B path (which on a bitstream that is not H.264/HEVC produces garbage the
/// decoder rejects with nothing in the log naming the cause).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoFraming {
    /// Length-prefixed NAL units rewritten to Annex-B with the parameter sets prepended at each
    /// keyframe (`packet_to_annexb`).
    AnnexB { hevc: bool },
    /// Section-5 temporal units with the av1C configuration in-band (`av1_packet_to_tu`).
    Av1,
    /// The packet unchanged (`vp9_packet_to_frame`).
    Vp9,
    /// The packet unchanged, like VP9: a VP8 frame carries its own headers and libpf's
    /// `video/x-vp8` caps have no `codec_data`.
    Vp8,
}

impl VideoFraming {
    pub(crate) fn of_codec_id(id: c_int) -> Option<Self> {
        match id {
            AV_CODEC_ID_H264 => Some(Self::AnnexB { hevc: false }),
            AV_CODEC_ID_HEVC => Some(Self::AnnexB { hevc: true }),
            AV_CODEC_ID_AV1 => Some(Self::Av1),
            AV_CODEC_ID_VP9 => Some(Self::Vp9),
            AV_CODEC_ID_VP8 => Some(Self::Vp8),
            _ => None,
        }
    }

    /// The Load declaration this framing belongs with. The Load is sent before the demuxer opens the
    /// file, so a disagreement can only abort and say so afterwards; that still beats the black
    /// screen it would otherwise be.
    pub(crate) fn load_video(self) -> crate::player::engine::LoadVideo {
        use crate::player::engine::LoadVideo;
        match self {
            Self::AnnexB { hevc: false } => LoadVideo::H264,
            Self::AnnexB { hevc: true } => LoadVideo::H265,
            Self::Av1 => LoadVideo::Av1,
            Self::Vp9 => LoadVideo::Vp9,
            Self::Vp8 => LoadVideo::Vp8,
        }
    }
}

/// Hand one demuxed VP9 packet to the decoder as it came, returning `key` unchanged.
///
/// A VP9 frame is self-describing: every keyframe carries its profile, size and colour config in
/// its own uncompressed header, and libpf's `video/x-vp9` appsrc caps have no `codec_data` to
/// fill, so there is nothing to prepend (compare `av1_packet_to_tu`, where the sequence header
/// has to be put back in-band). A Matroska/MP4 packet may be a SUPERFRAME, a shown frame and its
/// hidden reference frames under one trailing index; it is passed whole, the way a WebM-aware
/// hardware decoder is fed.
fn vp9_packet_to_frame(pkt: &[u8], key: bool, out: &mut Vec<u8>) -> bool {
    out.clear();
    out.extend_from_slice(pkt);
    key
}

/// Parse an avcC (H264) / hvcC (HEVC) extradata record into a ready-to-prepend Annex-B
/// parameter-set blob (VPS/SPS/PPS with 4-byte start codes) + the NAL length-prefix size.
/// (Ported from the retired mkv.rs demuxer) - the format the Starfish decoder wants.
unsafe fn parse_extradata(ed: *const u8, len: usize, is_hevc: bool) -> (Vec<u8>, usize) {
    let mut blob = Vec::new();
    // No avcC/hvcC record (MPEG-TS, raw H.264/HEVC): the packets carry start codes and their
    // parameter sets in-band. `0` says "Annex-B in".
    if ed.is_null() || len < 4 {
        return (blob, 0);
    }
    let e = std::slice::from_raw_parts(ed, len);
    // An extradata that is itself Annex-B (ASF, some AVI) is kept whole: there the parameter sets
    // live ONLY in it, and `packet_to_annexb` puts it before a keyframe that lacks its own.
    if e.starts_with(&[0, 0, 1]) || e.starts_with(&[0, 0, 0, 1]) {
        return (e.to_vec(), 0);
    }
    if len < 7 {
        return (blob, 4);
    }
    let sc = [0u8, 0, 0, 1];
    if is_hevc {
        // HEVCDecoderConfigurationRecord: ver@0==1, nal_len@21, numArrays@22, arrays@23.
        if len < 23 || e[0] != 1 {
            return (blob, 4);
        }
        let nls = (e[21] & 3) as usize + 1;
        let narr = e[22] as usize;
        let mut p = 23usize;
        for _ in 0..narr {
            if p + 3 > len {
                break;
            }
            // array header: [completeness|reserved|NAL_type(&0x3f)] u16 count.
            // ONLY VPS(32)/SPS(33)/PPS(34) belong in the prepend blob; skip SEI(39/40) and
            // anything else - a stray SEI here corrupts the parameter-set sequence.
            let keep = matches!(e[p] & 0x3f, 32 | 33 | 34);
            let cnt = ((e[p + 1] as usize) << 8) | e[p + 2] as usize;
            p += 3;
            for _ in 0..cnt {
                if p + 2 > len {
                    break;
                }
                let nl = ((e[p] as usize) << 8) | e[p + 1] as usize;
                p += 2;
                if nl == 0 || p + nl > len {
                    break;
                }
                if keep {
                    blob.extend_from_slice(&sc);
                    blob.extend_from_slice(&e[p..p + nl]);
                }
                p += nl;
            }
        }
        (blob, nls)
    } else {
        // AVCDecoderConfigurationRecord: ver@0==1, nal_len@4, SPS list @5, then PPS list.
        if e[0] != 1 {
            return (blob, 4);
        }
        let nls = (e[4] & 3) as usize + 1;
        let nsps = (e[5] & 0x1f) as usize;
        let mut p = 6usize;
        for _ in 0..nsps {
            if p + 2 > len {
                break;
            }
            let nl = ((e[p] as usize) << 8) | e[p + 1] as usize;
            p += 2;
            if nl == 0 || p + nl > len {
                break;
            }
            blob.extend_from_slice(&sc);
            blob.extend_from_slice(&e[p..p + nl]);
            p += nl;
        }
        if p < len {
            let npps = e[p] as usize;
            p += 1;
            for _ in 0..npps {
                if p + 2 > len {
                    break;
                }
                let nl = ((e[p] as usize) << 8) | e[p + 1] as usize;
                p += 2;
                if nl == 0 || p + nl > len {
                    break;
                }
                blob.extend_from_slice(&sc);
                blob.extend_from_slice(&e[p..p + nl]);
                p += nl;
            }
        }
        (blob, nls)
    }
}

static SEI_STRIPPED: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// NAL units the base-layer strip (`DvPresentation::BaseLayer`) has dropped since boot, for the log.
static DV_STRIPPED: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// **Dolby Vision Profile 7 as HDR10** (`stream::DvPresentation::BaseLayer`): drops every HEVC
/// NAL unit of type 62 (the Dolby Vision RPU) and 63 (the enhancement layer, UNSPEC63) from Annex-B
/// access units, leaving every other unit - its start code included - byte for byte as it was.
/// Owns its scratch, so the per-frame work allocates nothing once warm.
#[derive(Default)]
struct DvStrip {
    out: Vec<u8>,
    /// (start of the start code, NAL type) of each unit in the AU being stripped.
    units: Vec<(usize, u8)>,
}

impl DvStrip {
    /// Strip one access unit in place and return the number of units removed. An AU with neither
    /// type is not touched; one that held nothing else comes back empty.
    fn strip(&mut self, au: &mut Vec<u8>) -> usize {
        self.units.clear();
        let n = au.len();
        let mut i = 2;
        while i + 1 < n {
            // a start code is `00 00 01`; a byte > 1 cannot be any of its three
            let b = au[i];
            if b > 1 {
                i += 3;
            } else if b == 1 && au[i - 1] == 0 && au[i - 2] == 0 {
                let sc = if i >= 3 && au[i - 3] == 0 { i - 3 } else { i - 2 };
                self.units.push((sc, (au[i + 1] >> 1) & 0x3f));
                i += 3;
            } else {
                i += 1;
            }
        }
        if !self.units.iter().any(|u| u.1 == 62 || u.1 == 63) {
            return 0;
        }
        self.out.clear();
        self.out.extend_from_slice(&au[..self.units[0].0]);
        let mut dropped = 0;
        for (k, &(sc, t)) in self.units.iter().enumerate() {
            let end = self.units.get(k + 1).map_or(n, |u| u.0);
            if t == 62 || t == 63 {
                dropped += 1;
            } else {
                self.out.extend_from_slice(&au[sc..end]);
            }
        }
        std::mem::swap(au, &mut self.out);
        dropped
    }
}

/// [`DvStrip::strip`] with fresh scratch: the one-shot form the framing tests pin.
#[cfg(test)]
fn strip_dv_nals(au: &mut Vec<u8>) -> usize {
    DvStrip::default().strip(au)
}

/// True if this HEVC NAL is an SEI (prefix=39 / suffix=40) carrying a user-data-registered
/// ITU-T T.35 HDR10+ dynamic-metadata message (payloadType 4, country_code 0xB5, terminal
/// provider 0x003C). webOS's HEVC decoder does NOT support per-frame HDR10+ metadata and
/// stalls on it - Kodi strips it unconditionally ("webOS doesn't support HDR10+ and it can
/// cause issues"). `nal` is the NAL payload WITHOUT the start code (starts at the 2-byte
/// NAL header). Static HDR10 mastering-display / CLL SEI (payloadType 137/144) is kept.
fn is_hdr10plus_sei(nal: &[u8]) -> bool {
    if nal.len() < 2 {
        return false;
    }
    let nal_type = (nal[0] >> 1) & 0x3f;
    if nal_type != 39 && nal_type != 40 {
        return false;
    }
    let mut p = 2usize; // SEI RBSP starts after the 2-byte NAL header
    let mut ptype = 0usize; // payloadType (0xFF-run + terminator)
    while p < nal.len() && nal[p] == 0xff {
        ptype += 255;
        p += 1;
    }
    if p >= nal.len() {
        return false;
    }
    ptype += nal[p] as usize;
    p += 1;
    while p < nal.len() && nal[p] == 0xff {
        p += 1; // skip payloadSize (0xFF-run + terminator); value unused
    }
    p += 1;
    // user_data_registered_itu_t_t35 == 4; T.35 header: country 0xB5, provider 0x003C
    ptype == 4 && p + 3 <= nal.len() && nal[p] == 0xb5 && nal[p + 1] == 0x00 && nal[p + 2] == 0x3c
}

/// End offset of a NAL of length `nl` starting at `i`, or None if it is empty or would run
/// past `size`. **The width is load-bearing**: `usize` is 32 bits on the TV, `nls` is up to 4,
/// so a hostile/corrupt length up to `0xFFFF_FFFF` makes the natural `i + nl > size` guard WRAP
/// to a small number, pass its own bounds check, and panic the demux thread inside the slice -
/// killing the producer before it can EOF the queues, which hangs playback with no error path.
/// Computing in `u64` makes the bound hold on every target. Never inline this back to `i + nl`.
fn nal_end(i: usize, nl: usize, size: usize) -> Option<usize> {
    if nl == 0 {
        return None;
    }
    let end = i as u64 + nl as u64;
    if end > size as u64 {
        return None;
    }
    Some(end as usize)
}

/// Convert one length-prefixed video packet to Annex-B (4-byte start codes) into `out`,
/// prepending `param` (VPS/SPS/PPS) when the AU is a keyframe (H264 IDR type 5 / HEVC IRAP
/// types 16-23). Returns true if it is a keyframe. Mirrors mkv_handle_block.
unsafe fn packet_to_annexb(
    data: *const u8,
    size: usize,
    nls: usize,
    is_hevc: bool,
    param: &[u8],
    out: &mut Vec<u8>,
) -> bool {
    out.clear();
    if data.is_null() || size < nls + 1 {
        return false;
    }
    let d = std::slice::from_raw_parts(data, size);
    if nls == 0 {
        // Annex-B in: fed whole; a keyframe is an AU holding an IDR (H.264 5) / IRAP (HEVC 16-23),
        // and gets an Annex-B extradata's parameter sets first when it carries none of its own.
        let mut key = false;
        let mut i = 0usize;
        while i + 3 < size {
            if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
                let b0 = d[i + 3];
                if if is_hevc { (16..=23).contains(&((b0 >> 1) & 0x3f)) } else { (b0 & 0x1f) == 5 } {
                    key = true;
                    break;
                }
                i += 3;
            } else {
                i += 1;
            }
        }
        if key && !param.is_empty() && !annexb_has_parameter_sets(d, is_hevc) {
            out.extend_from_slice(param);
        }
        out.extend_from_slice(d);
        return key;
    }
    // pass 1: is this AU a keyframe?
    let mut is_key = false;
    let mut i = 0usize;
    while i + nls <= size {
        let mut nl = 0usize;
        for k in 0..nls {
            nl = (nl << 8) | d[i + k] as usize;
        }
        i += nls;
        if nal_end(i, nl, size).is_none() {
            break;
        }
        let b0 = d[i];
        let key = if is_hevc {
            (16..=23).contains(&((b0 >> 1) & 0x3f))
        } else {
            (b0 & 0x1f) == 5
        };
        if key {
            is_key = true;
            break;
        }
        i += nl;
    }
    if is_key {
        out.extend_from_slice(param);
    }
    // pass 2: emit each NAL as 00 00 00 01 + bytes, dropping per-frame HDR10+ SEI (HEVC)
    let sc = [0u8, 0, 0, 1];
    let mut i = 0usize;
    while i + nls <= size {
        let mut nl = 0usize;
        for k in 0..nls {
            nl = (nl << 8) | d[i + k] as usize;
        }
        i += nls;
        let Some(end) = nal_end(i, nl, size) else {
            break;
        };
        let nal = &d[i..end];
        if is_hevc && is_hdr10plus_sei(nal) {
            let n = SEI_STRIPPED.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 3 || n % 500 == 0 {
                crate::player::log(&format!("ff: stripped HDR10+ SEI #{n} ({nl} bytes)"));
            }
            i += nl;
            continue;
        }
        out.extend_from_slice(&sc);
        out.extend_from_slice(nal);
        i += nl;
    }
    is_key
}

static DIAG_FIRST: AtomicBool = AtomicBool::new(true);

/// Does this Annex-B access unit carry its own decoder setup - an H.264 SPS (NAL 7) or an HEVC
/// VPS/SPS (NAL 32/33)? Live H.264 often opens each GOP with an I-slice behind its SPS/PPS and
/// never sends an IDR picture, so such an AU is where a decoder can start.
fn annexb_has_parameter_sets(au: &[u8], is_hevc: bool) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let header = au[i + 3];
            let found = if is_hevc { matches!((header >> 1) & 0x3F, 32 | 33) } else { header & 0x1F == 7 };
            if found {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// The video feed starts at the first keyframe that carries a time. Before it, a decoder has
/// nothing to decode from, and an untimed packet would be stamped 0 s against a clock that may sit
/// anywhere (a live channel joined mid-stream reads ~1795 s), which keeps the TV's clock from
/// ever starting. Files and seeks land on keyframes, so this drops nothing there.
#[derive(Default)]
struct VideoStart {
    started: bool,
    first_fed: bool,
    dropped: u32,
}

impl VideoStart {
    /// Whether to feed this video AU.
    fn feed(&mut self, is_key: bool, has_pts: bool) -> bool {
        if !self.started {
            if !(is_key && has_pts) {
                self.dropped += 1;
                return false;
            }
            self.started = true;
            self.first_fed = true;
        }
        true
    }

    /// True once, for the AU that started the feed.
    fn just_started(&mut self) -> bool {
        std::mem::take(&mut self.first_fed)
    }
}

#[cfg(test)]
mod discard_tests {
    use super::{discard_for, AVDISCARD_ALL, AVDISCARD_DEFAULT};

    #[test]
    fn only_the_played_tracks_are_read() {
        let (vi, ai) = (0, 1);
        assert_eq!(discard_for(0, vi, ai), AVDISCARD_DEFAULT, "the video");
        assert_eq!(discard_for(1, vi, ai), AVDISCARD_DEFAULT, "the played audio");
        assert_eq!(discard_for(2, vi, ai), AVDISCARD_ALL, "another audio track");
        assert_eq!(discard_for(3, vi, ai), AVDISCARD_ALL, "a subtitle track");
        assert_eq!(discard_for(4, vi, -1), AVDISCARD_ALL, "no audio played: data and attachments too");
    }
}

#[cfg(test)]
mod seek_flag_tests {
    /// A scrub lands on the keyframe before its target; a skip on the first at or after it.
    #[test]
    fn a_skip_seeks_forward_and_a_scrub_backward() {
        assert_eq!(super::seek_flags(false), super::AVSEEK_FLAG_BACKWARD);
        assert_eq!(super::seek_flags(true) & super::AVSEEK_FLAG_BACKWARD, 0);
    }
}

#[cfg(test)]
mod audio_clock_tests {
    use super::AudioClock;

    /// MPEG-TS packs several AAC frames into one PES; the parser splits them and only the first
    /// carries a time. Untimed frames used to be stamped 0 and dropped as stale after a live
    /// rebase (`fp=-4501400ms`), two of every three frames: the beta's stuttering live audio.
    #[test]
    fn an_untimed_audio_frame_follows_the_one_before_it() {
        let mut c = AudioClock::default();
        let frame = 21_333_333; // 1024 samples at 48 kHz
        assert_eq!(c.stamp(Some(4_501_400_000_000), frame), 4_501_400_000_000);
        assert_eq!(c.stamp(None, frame), 4_501_400_000_000 + frame);
        assert_eq!(c.stamp(None, frame), 4_501_400_000_000 + 2 * frame);
        assert_eq!(c.stamp(Some(4_501_464_000_000), frame), 4_501_464_000_000, "a real time always wins");
        assert_eq!(AudioClock::default().stamp(None, frame), 0, "nothing to follow: as before");
        // ADTS: 1024 samples at the header's rate
        assert_eq!(super::adts_frame_ns(&[0xFF, 0xF1, 0x4C, 0x80, 0x2E, 0x7F, 0xFC]), Some(21_333_333));
        assert_eq!(super::adts_frame_ns(&[0x00, 0x01]), None);
    }
}

#[cfg(test)]
mod annexb_input_tests {
    /// MPEG-TS (live HLS) carries H.264 as Annex-B with no avcC: read as 4-byte length-prefixed
    /// NALs, the start code `00 00 00 01` became a length of 1 and the whole AU collapsed to its
    /// 5-byte AUD (the beta's live TV: `AU#0 size=5 head=[00 00 00 01 09]`, nothing decoded).
    #[test]
    fn an_annexb_packet_passes_through_whole_and_its_idr_is_a_keyframe() {
        let au: Vec<u8> = [
            &[0u8, 0, 0, 1, 0x09, 0xF0][..],
            &[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xAC][..],
            &[0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80][..],
            &[0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33][..],
        ]
        .concat();
        let (blob, nls) = unsafe { super::parse_extradata(std::ptr::null(), 0, false) };
        assert!(blob.is_empty());
        assert_eq!(nls, 0, "no avcC means the packets are Annex-B already");
        let mut out = Vec::new();
        let key = unsafe { super::packet_to_annexb(au.as_ptr(), au.len(), nls, false, &blob, &mut out) };
        assert_eq!(out, au, "the access unit is fed whole");
        assert!(key, "its IDR slice makes it a keyframe");
        let p_slice = [0u8, 0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x41, 0x9A];
        assert!(!unsafe { super::packet_to_annexb(p_slice.as_ptr(), p_slice.len(), 0, false, &[], &mut out) });
        assert_eq!(out, p_slice);
        // Annex-B extradata (some muxers put the SPS/PPS there with start codes) is Annex-B too
        let ed = [0u8, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0, 0, 0, 1, 0x68, 0xEE];
        assert_eq!(unsafe { super::parse_extradata(ed.as_ptr(), ed.len(), false) }.1, 0);
    }
}

#[cfg(test)]
mod video_start_tests {
    use super::VideoStart;

    /// A live channel joined mid-stream: the first video packets are not a keyframe, and the very
    /// first carried no timestamp, which used to go out stamped 0 s against audio at ~1795 s, so
    /// the TV's clock never started (the beta's live TV "buffering forever").
    #[test]
    fn nothing_is_fed_before_the_first_keyframe_and_nothing_without_a_time() {
        let mut v = VideoStart::default();
        assert!(!v.feed(false, false), "a lone marker with no time");
        assert!(!v.feed(false, true), "a timed packet before any keyframe");
        assert!(!v.feed(true, false), "a keyframe without a time cannot anchor the clock");
        assert_eq!(v.dropped, 3);
        assert!(v.feed(true, true), "the first timed keyframe starts the feed");
        assert!(v.feed(false, true), "and everything after it flows");
        assert!(v.feed(false, false), "a later untimed packet rides the decoder's own order");
    }

    /// Live and IPTV H.264 often never sends an IDR picture: each GOP opens with an I-slice
    /// (recovery point) behind its own SPS/PPS. Such an AU is where a decoder can start.
    #[test]
    fn an_au_carrying_its_own_parameter_sets_is_a_start() {
        let aud = [0u8, 0, 0, 1, 0x09, 0xF0];
        let sps_pps_slice = [0u8, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 0, 1, 0x68, 0xEE, 0, 0, 1, 0x41, 0x9A];
        assert!(!super::annexb_has_parameter_sets(&aud, false));
        assert!(super::annexb_has_parameter_sets(&sps_pps_slice, false));
        // HEVC: VPS (32) / SPS (33) NAL headers are two bytes, type in bits 1..6 of the first
        let hevc_vps = [0u8, 0, 0, 1, 0x40, 0x01, 0x0C];
        assert!(super::annexb_has_parameter_sets(&hevc_vps, true));
        assert!(!super::annexb_has_parameter_sets(&[0, 0, 1, 0x26, 0x01], true), "an IDR slice alone is not a parameter set");
    }
}

/// ADTS sampling_frequency_index (4 bits) for a sample rate; None if not a standard AAC rate.
fn adts_freq_index(rate: c_int) -> Option<u8> {
    Some(match rate {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        12000 => 9,
        11025 => 10,
        8000 => 11,
        7350 => 12,
        _ => return None,
    })
}

/// (freq_idx, chan_cfg) for the ADTS header, read from the stream's AudioSpecificConfig when
/// present. HE-AAC is why this parses the ASC instead of trusting codecpar: the ASC's FIRST
/// samplingFrequencyIndex is the AAC **core** rate (e.g. 24 kHz) while `codecpar.sample_rate`
/// reports the SBR **output** rate (48 kHz). ADTS must carry the core rate (+ LC profile, SBR
/// stays implicit) so the decoder up-samples through the SBR extension - a 48 kHz header makes
/// it decode each frame as 1024 plain-LC samples into a 2048-sample slot, an audible gap/crackle
/// every frame (Maxton Hall, HE-AAC 5.1). Falls back to codecpar for ASC-less streams.
unsafe fn adts_params(acp: *const AVCodecParameters) -> Option<(u8, u8)> {
    let chan_fallback = {
        let ch = (*acp).ch_layout.nb_channels;
        if (1..=7).contains(&ch) { ch as u8 } else { 2 }
    };
    // AudioSpecificConfig: aot(5) freqIdx(4) [+24-bit rate if idx==15] chanCfg(4) ...
    let (ed, n) = ((*acp).extradata, (*acp).extradata_size);
    if !ed.is_null() && n >= 2 {
        let (b0, b1) = (*ed as u32, *ed.add(1) as u32);
        let aot = (b0 >> 3) & 0x1F;
        let freq_idx = (((b0 & 0x07) << 1) | (b1 >> 7)) as u8;
        if aot != 31 && freq_idx <= 12 {
            let chan = ((b1 >> 3) & 0x0F) as u8;
            let ch = if (1..=7).contains(&chan) {
                chan
            } else {
                chan_fallback
            };
            return Some((freq_idx, ch));
        }
        // escape-coded AOT / explicit 24-bit rate - exotic; fall through to codecpar
    }
    adts_freq_index((*acp).sample_rate).map(|fi| (fi, chan_fallback))
}

/// 7-byte ADTS header for a raw AAC frame of `payload_len` bytes. LG's Starfish decodes
/// ADTS-framed AAC (ss4s: aacInfo format="adts"); mp4/matroska carry RAW AAC, so we reframe.
/// Emits AAC-LC (ADTS profile 1 = object type 2 - 1); buffer fullness = VBR (0x7FF).
fn adts_header(freq_idx: u8, chan_cfg: u8, payload_len: usize) -> [u8; 7] {
    let frame_len = (payload_len + 7) as u32; // total incl. header, 13 bits
    [
        0xFF,
        0xF1, // sync(1111) | MPEG-4(0) | layer(00) | protection_absent(1)
        (1 << 6) | (freq_idx << 2) | (chan_cfg >> 2), // profile(01=LC) | freq_idx | chan hi bit
        ((chan_cfg & 3) << 6) | ((frame_len >> 11) as u8 & 0x03),
        ((frame_len >> 3) & 0xFF) as u8,
        (((frame_len & 0x07) as u8) << 5) | 0x1F, // frame_len lo 3 | fullness hi 5 (11111)
        0xFC,                                     // fullness lo 6 (111111) | num_blocks(00)
    ]
}

/// Why a progressive media open produced no source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaOpenFail {
    /// Teardown won; not a playback failure.
    Aborted,
    Failed,
}

/// Open `url` on the curl source at byte `at`, with the stream's `headers` on every request. The
/// reservation publishes teardown's wake target BEFORE the abort check - the order `demux`'s own
/// curl open keeps.
fn open_curl_hop(
    url: &str,
    at: i64,
    headers: &crate::headers::RequestHeaders,
    aq: *mut AuQueue,
) -> Result<Box<crate::curlio::CurlSource>, crate::curlio::OpenErr> {
    let reservation = crate::curlio::CurlSource::reserve_open()?;
    if unsafe { crate::aq::aq_is_aborted(aq) } {
        return Err(crate::curlio::OpenErr::Aborted);
    }
    crate::curlio::CurlSource::open_reserved(url, at, headers, reservation)
}

/// Record an HLS failure where the read-out and the diagnostics find it: the HTTP status behind it
/// (`dg_http_status`) and the failure itself (`hls::last_failure`). Aborts are teardown's, not the
/// stream's, and leave both alone.
fn hls_failed(e: &crate::hls::HlsError) {
    if *e == crate::hls::HlsError::Aborted {
        return;
    }
    if let Some(st) = e.http_status() {
        SHARED.dg_http_status.store(c_int::from(st), Ordering::Relaxed);
    }
    crate::hls::note_failure(Some(e.clone()));
}

/// Open the HLS source for `url` ([`crate::hls`]): the playlist resolved, a variant chosen against
/// this set's codec table, and the stream positioned. A VOD stream starts at the segment holding
/// the armed seek target (`SHARED.seek_to_ns`, consumed here: the unseekable AVIO could not serve
/// `av_seek_frame`), and so does a growing EVENT playlist (an archive of the programme on air);
/// a live one opens three target durations behind its edge whatever was armed. Positions of the
/// first two are playlist time (`Shared::hls_vod_base_ns`).
/// The probe's HLS source, passed to the demuxer that follows it (see [`open_hls`]).
pub(crate) mod hls_handoff {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Older than this, a live source's position is too far behind its edge to be worth keeping.
    const FRESH: Duration = Duration::from_secs(30);

    struct Handoff {
        url: String,
        source: Box<crate::hls::CurlHls>,
        bytes: Vec<u8>,
        at: Instant,
    }
    // SAFETY: the source moves whole from the probe thread to the demux thread and is never used
    // by two threads at once; its one non-Send part, the cancel closure, is replaced on arrival
    // (`set_cancel`) before any read.
    unsafe impl Send for Handoff {}

    static SLOT: Mutex<Option<Handoff>> = Mutex::new(None);

    pub(crate) fn put(url: &str, source: Box<crate::hls::CurlHls>, bytes: Vec<u8>) {
        *SLOT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Handoff { url: url.to_owned(), source, bytes, at: Instant::now() });
    }

    /// Whether a source for `url` is waiting: the demuxer then goes straight to HLS instead of
    /// asking the URL what it is.
    pub(crate) fn waiting(url: &str) -> bool {
        SLOT.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|h| h.url == url && h.at.elapsed() < FRESH)
    }

    pub(crate) fn take(url: &str) -> Option<(Box<crate::hls::CurlHls>, Vec<u8>)> {
        let h = SLOT.lock().unwrap_or_else(|e| e.into_inner()).take()?;
        (h.url == url && h.at.elapsed() < FRESH).then(|| (h.source, h.bytes))
    }

    /// Drop a waiting source.
    pub(crate) fn clear() {
        SLOT.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

/// Publishes the same diagnostics the progressive opens do, and a VOD stream's duration from its
/// playlist (libavformat cannot know it without a file size).
///
/// A stream the probe just read from its start is not opened again: the probe's source and the
/// bytes it delivered come over ([`hls_handoff`]) and `replay` gets those bytes for the AVIO to
/// serve first. A live channel's open was ~2.6 s of refetching the segment the probe had read.
fn open_hls(
    url: &str,
    headers: &crate::headers::RequestHeaders,
    aq_p: *mut AuQueue,
    replay: &mut Vec<u8>,
) -> Result<(Src, i64), MediaOpenFail> {
    // The lane's abort flag is the teardown signal every other transport checks too; the source
    // lives and dies on this (the demux) thread, as the lane outlives it.
    let cancel = Box::new(move || unsafe { crate::aq::aq_is_aborted(aq_p) });
    let armed = SHARED.seek_to_ns.swap(-1, Ordering::AcqRel);
    let at_us = (armed >= 0).then(|| (armed / 1000) as u64);
    crate::hls::note_failure(None);
    let waiting = hls_handoff::waiting(url);
    let handed = if armed < 0 {
        hls_handoff::take(url)
    } else {
        hls_handoff::clear();
        None
    };
    if handed.is_none() {
        crate::player::log(&format!("ff: hls opens afresh (handoff waiting={waiting} armed={armed})"));
    }
    let opened = match handed {
        Some((mut h, bytes)) => {
            h.transport_mut().set_cancel(cancel);
            crate::player::log(&format!("ff: hls continues the probe's source ({} bytes replayed)", bytes.len()));
            // The probe's bytes reach the demuxer from `bounce`, which `read_cb` serves uncounted
            // (the drained bytes it holds were counted as they arrived); these never were.
            SHARED.dg_net_rx.fetch_add(bytes.len() as i64, Ordering::Relaxed);
            *replay = bytes;
            Ok(*h)
        }
        None => {
            let transport = crate::hls::CurlTransport::new(headers.clone(), cancel);
            crate::hls::CurlHls::open(transport, crate::hls::SystemClock::new(), url, at_us, tv_platform::devcaps::caps())
        }
    };
    match opened {
        Ok(h) => {
            SHARED.file_size.store(-1, Ordering::Release);
            SHARED.dg_http_status.store(200, Ordering::Relaxed);
            let base_ns = if h.in_playlist_time() { i64::try_from(h.start_us()).unwrap_or(0).saturating_mul(1000) } else { -1 };
            SHARED.hls_vod_base_ns.store(base_ns, Ordering::Release);
            if !h.is_live() {
                let dur_ns = i64::try_from(h.duration_us()).unwrap_or(i64::MAX / 1000).saturating_mul(1000);
                SHARED.duration_ns.store(dur_ns, Ordering::Relaxed);
            }
            crate::player::log(&format!(
                "ff: open hls {}{}",
                if h.is_live() { "live" } else { "vod" },
                if armed >= 0 && !h.is_live() {
                    format!(" at segment start {}ms (asked {}ms)", h.start_us() / 1000, armed / 1_000_000)
                } else {
                    String::new()
                }
            ));
            Ok((Src::Hls(Box::new(h)), -1))
        }
        Err(crate::hls::HlsError::Aborted) => Err(MediaOpenFail::Aborted),
        Err(e) => {
            crate::player::log(&format!("ff: hls open FAILED: {e}"));
            hls_failed(&e);
            Err(MediaOpenFail::Failed)
        }
    }
}

/// The socket path's credential authority (`stream::redirect::CredentialGate`): it carries no
/// credential, ever. A stream with request headers is pulled through curl from its first byte
/// (`Src`), so a plaintext hop with a header block on it is refused rather than sent.
fn plaintext_media_gate(_origin: &tv_net::net::origin::Origin, _path: &str, headers: &[&str]) -> bool {
    headers.is_empty()
}

/// The progressive open of a header-less PLAINTEXT stream URL, redirects followed. A media URL
/// commonly answers with a `302` to a presigned CDN URL - usually https, which lands on the curl
/// source. Publishes the same two diagnostics the curl arm of `demux` does.
fn open_plain_progressive(
    hs_p: *mut HttpStream,
    origin: &tv_net::net::origin::Origin,
    path: &str,
    aq_p: *mut AuQueue,
) -> Result<(Src, i64), MediaOpenFail> {
    let mut hop = |url: &str| open_curl_hop(url, 0, &crate::headers::RequestHeaders::none(), aq_p);
    match open_plain_following(hs_p, origin, path, aq_p, &mut hop) {
        Ok((src, size, st)) => {
            SHARED.file_size.store(size, Ordering::Release);
            SHARED.dg_http_status.store(st, Ordering::Relaxed);
            Ok((src, size))
        }
        Err((fail, status)) => {
            if let Some(st) = status {
                SHARED.dg_http_status.store(st, Ordering::Relaxed);
            }
            Err(fail)
        }
    }
}

/// [`open_plain_progressive`] without the diagnostics: the open itself, for the demuxer and for
/// the probe (`ff_probe.rs`), which must not write a playback's read-out before that playback
/// exists. `hop` opens the curl source an https redirect lands on - the demuxer's registers as
/// teardown's wake target, the probe's stays private. `Ok` carries the source, its size and the
/// HTTP status; `Err` the failure and the status worth publishing, if any.
fn open_plain_following(
    hs_p: *mut HttpStream,
    origin: &tv_net::net::origin::Origin,
    path: &str,
    aq_p: *mut AuQueue,
    hop: &mut dyn FnMut(&str) -> Result<Box<crate::curlio::CurlSource>, crate::curlio::OpenErr>,
) -> Result<(Src, i64, c_int), (MediaOpenFail, Option<c_int>)> {
    use tv_net::stream::redirect::Opened;
    tv_net::stream::http_close(hs_p);
    let req = tv_net::stream::redirect::Request {
        origin,
        path,
        credentials: None,
        range_from: None,
        deadline: None,
        same_origin_only: false,
        credential_gate: plaintext_media_gate,
    };
    match tv_net::stream::redirect::open_following(hs_p, &req, &mut tv_base::checkpoint::NoCheckpoint)
    {
        Ok(Opened::Socket(t)) => {
            let size = tv_net::stream::hs_content_length(hs_p);
            let st = tv_net::stream::hs_status(hs_p);
            crate::player::log(&format!("ff: open status={st} clen={size}"));
            let (Ok(host), Ok(path)) = (CString::new(t.origin.host()), CString::new(t.path)) else {
                return Err((MediaOpenFail::Failed, Some(st)));
            };
            Ok((
                Src::Socket {
                    hs: hs_p,
                    host,
                    port: t.origin.port() as c_int,
                    path,
                },
                size,
                st,
            ))
        }
        Ok(Opened::Tls(t)) => {
            match hop(&t.url()) {
                Ok(cs) => {
                    let (st, size) = (cs.status(), cs.size());
                    crate::player::log(&format!("ff: open https status={st} clen={size}"));
                    Ok((Src::Curl(cs), size, st))
                }
                Err(crate::curlio::OpenErr::Aborted) => Err((MediaOpenFail::Aborted, None)),
                Err(e) => {
                    crate::player::log(&format!("ff: https open FAILED: {e:?}"));
                    let st = if let crate::curlio::OpenErr::Status(st) = e { Some(st) } else { None };
                    Err((MediaOpenFail::Failed, st))
                }
            }
        }
        Err(e) => {
            if unsafe { crate::aq::aq_is_aborted(aq_p) } {
                return Err((MediaOpenFail::Aborted, None));
            }
            let st = tv_net::stream::hs_status(hs_p);
            crate::player::log(&format!("ff: http_open FAILED status={st} ({e:?})"));
            Err((MediaOpenFail::Failed, Some(st)))
        }
    }
}

/// Whether a demux exit that published no access unit is a playback failure - the ONE rule every
/// demux exit reads. A demuxer that produced nothing and ended on its own
/// failed; one whose lane teardown aborted was told to stop (a reload, a seek, BACK), whatever
/// early exit that took, and must neither log a failure nor raise `demux_failed`.
fn unproductive_exit_failed(pushed_any: bool, lane_aborted: bool) -> bool {
    !pushed_any && !lane_aborted
}

/// `AVERROR_INVALIDDATA` (`FFERRTAG('I','N','D','A')`).
pub const AVERROR_INVALIDDATA: c_int = -1_094_995_529;

/// **An open that failed on the bytes it was given, not on the stream**: `avformat_open_input`
/// found no container (`INVALIDDATA`: a debrid host's first hop answered with an HTML error page)
/// or ran out of bytes inside the header (`EOF`/`EIO`: a truncated first transfer). The same URL
/// opened fine a moment later on the television, so the open is retried ONCE from the original
/// URL - a fresh redirect chain, not the effective URL of the failed one - before it is refused.
fn open_failure_is_transient(r: c_int) -> bool {
    matches!(r, AVERROR_INVALIDDATA | AVERROR_EOF | AVERROR_IO)
}

/// The log line for an `avformat_open_input` that returned no context. Teardown aborts the lanes
/// and `read_cb` answers the abort with EOF, so an open cut short that way returns whatever the
/// truncated probe made of it (`AVERROR_INVALIDDATA` on a Matroska header) - a stop that must not
/// read as the source failing to open. Issue #266's normalize run was misread exactly so.
fn open_input_failure_note(r: c_int, lane_aborted: bool) -> String {
    if lane_aborted {
        format!("ff: aborted during open_input r={r}")
    } else {
        format!("ff: open_input failed r={r}")
    }
}

/// **The most an HLS stream's stream-info search reads**, bytes (FFmpeg's default is 5 MB). A live
/// channel's MPEG-TS carries AAC, which the bundled FFmpeg has no decoder for, so the search never
/// completes on its own and reads to the cap: 5 MB is 20-30 s of a 1-2 Mbit/s channel, read (and
/// waited for, at the live edge) before the first picture. 2 MB still spans several keyframes of
/// any channel; the AAC parameters come from the ADTS headers (`probe`'s first packets).
pub(crate) const HLS_PROBE_BYTES: i64 = 2_000_000;

/// Cap an HLS stream's stream-info search at [`HLS_PROBE_BYTES`], on a context not yet opened.
unsafe fn cap_hls_probe(fmt: *mut AVFormatContext) {
    if av_opt_set_int(fmt.cast(), c"probesize".as_ptr(), HLS_PROBE_BYTES, 0) < 0 {
        crate::player::log("ff: probesize could not be capped for HLS");
    }
}

/// **How much further the VIDEO lane may run ahead while the AUDIO lane has nothing yet.**
///
/// The two lanes are filled from ONE read of the file, in file order, and each has its own byte cap
/// (`player::engine`'s `AQ_VIDEO_BYTES`, 48 MiB). A Matroska file whose audio track carries a
/// container `Delay` has no audio block before that many seconds of video, so the video lane can
/// fill before the first audio packet is even read. Measured on the device, the video lane held
/// 10,171,164 bytes when the first audio AU was read for the reported film (an E-AC-3 `Delay` of
/// 11.008 s at ~9.5 Mbit/s, after the pump had popped some) and 13,628,369 bytes for the
/// `pipe_late_audio_eac3` fixture (12 Mbit/s, the same delay). The demuxer parked on the full video
/// lane, the pipeline (which will not start its clock without audio) took no more video, and the
/// first audio block was never read: a start that buffered forever, while a resume past the delay
/// played at once. FFmpeg's own probe said as much on stderr: no E-AC-3 parameters within its 5 MB
/// `probesize`.
///
/// So from the first video push that WOULD PARK at the base cap while no audio AU has been pushed in
/// this open or seek, the video lane's cap is its base cap PLUS this much: **48 MiB + 24 MiB =
/// 72 MiB (~75.5 MB) in total.** The grant is lazy on purpose: an ordinary file (video a few
/// packets ahead of the first audio, B-frames, a muxer that leads with video) never fills the base
/// cap before its first audio packet, so it never sees the grant and keeps the memory behaviour it
/// always had; on a 60-100 Mbit/s UHD stream an eager grant would have cost 18-30 MiB of extra
/// queue on a RAM-tight 32-bit set for files that never needed it. That is a BOUND, not a
/// measurement of what a file needs: at an 11 s delay it holds video up to ~55 Mbit/s,
/// and a longer delay or a higher rate outruns it (50 s at 12 Mbit/s, 24 s at 25 Mbit/s). It is
/// large enough for the delays seen in the field and small enough for a 32-bit set that is
/// RAM-tight, and a file whose audio never comes cannot grow the lane without limit.
///
/// **Exhausting it fails the open** ([`PreAudioHeadroom::before_video`], [`VideoStep::Fail`]): the
/// lane is full, no audio has been read, and the consumer has popped nothing since the grant, so
/// parking would be a spinner nobody can leave. The demuxer says so in the log and raises the same
/// `demux_io_failed` the pump turns into the failure read-out. A lane the consumer IS
/// draining (a seek into an audioless stretch: the pump pops video while the demuxer reads at LAN
/// speed, and the engine's video-only prime escape plays it) is not that hang: the grant is
/// released and the lane parks like any full lane, as it did before the grant existed.
///
/// **The grant never outlives its epoch.** It is released when the audio lead-in has been pushed
/// (below), when the consumer is seen draining a full lane (above), at a seek (the next epoch
/// starts from the base cap) and when the read loop ends.
///
/// **The grant is not withdrawn when the first audio AU lands.** The engine feeds a late track's
/// first AU AND the next `AUDIO_LEAD_IN_NS` of audio ahead of a clock that cannot start without
/// them (`player::engine`), so the demuxer must READ that much audio with the video lane still full
/// and undrained. Withdrawing the headroom at the first audio push dropped the video cap under what
/// the lane held, the next video push parked, and the audio lane got two packets and no more (device
/// log: `feed a#1`, `feed a#2`, then nothing). The grant is therefore released once the audio lane
/// has been pushed [`AUDIO_LEAD_IN_NS`](crate::player::engine::AUDIO_LEAD_IN_NS) plus
/// [`LEAD_IN_MARGIN_NS`] of audio, and the release is `aq_release_headroom`: the cap follows the
/// consumer's drain down to the base rather than dropping under the lane's occupancy.
const PRE_AUDIO_VIDEO_HEADROOM: c_long = 24 * 1024 * 1024;

/// Audio pushed past the engine's lead-in before the video lane's headroom is released: the lead-in
/// is measured from the AU the engine ACCEPTS, in its own timeline, and the lane is pushed in whole
/// AUs, so the demuxer reads a little beyond it rather than exactly to it.
const LEAD_IN_MARGIN_NS: i64 = 500_000_000;

/// The per-open (and per-seek) state of the video lane's pre-audio headroom. The decisions
/// ([`before_video`](Self::before_video), [`after_audio`](Self::after_audio)) are PURE; the two
/// methods that take a lane ([`withdraw`](Self::withdraw), [`restart`](Self::restart)) only call
/// `aq_release_headroom`.
///
/// `granted` latches at the first video push that WOULD PARK at the base cap while an audio lane is
/// being fed and no audio has been read yet; a file whose audio comes first, or whose video never
/// fills the base cap before its audio, never gets a grant. `pops_at_grant` is the lane's pop
/// counter then, the evidence for "the consumer is draining". `first_audio_ns` is the pts of the
/// first audio AU pushed since the open or seek. `released` latches once the grant has been given
/// back, whichever way. Every field restarts on a seek ([`restart`](Self::restart)).
#[derive(Default, Debug)]
struct PreAudioHeadroom {
    granted: bool,
    released: bool,
    first_audio_ns: Option<i64>,
    pops_at_grant: u64,
}

/// What the video push about to be made means for the lane's headroom.
#[derive(Debug, PartialEq, Eq)]
enum VideoStep {
    /// Push as usual.
    Proceed,
    /// Raise the lane's cap by this much, then push: the base cap is full and no audio has come.
    Grant(c_long),
    /// The consumer is draining a lane that is full under the grant: give the grant back
    /// (`aq_release_headroom`) and push, parking like any full lane.
    Release,
    /// Full, no audio, and nothing popped since the grant: parking is a spinner nobody can leave.
    Fail,
}

impl PreAudioHeadroom {
    /// A video AU is about to be pushed. `audio_lane` is whether an audio lane is being fed at all,
    /// `would_park` whether the push would park at the cap now in force (`aq_would_park`), `pops`
    /// the lane's pop counter (`aq_pops`).
    fn before_video(&mut self, audio_lane: bool, would_park: bool, pops: u64) -> VideoStep {
        if !audio_lane || self.released || !would_park {
            return VideoStep::Proceed;
        }
        if !self.granted {
            if self.first_audio_ns.is_some() {
                // Audio came first: the ordinary backpressure applies.
                return VideoStep::Proceed;
            }
            self.granted = true;
            self.pops_at_grant = pops;
            return VideoStep::Grant(PRE_AUDIO_VIDEO_HEADROOM);
        }
        // The grant stands and the push would park: the cap it gave is spent.
        if pops != self.pops_at_grant {
            self.released = true;
            VideoStep::Release
        } else if self.first_audio_ns.is_none() {
            VideoStep::Fail
        } else {
            // Audio is in and the lead-in logic owns the release; nothing has been popped yet.
            VideoStep::Proceed
        }
    }

    /// An audio AU at `pts_ns` was pushed since the open or seek.
    fn after_audio(&mut self, pts_ns: i64) -> AudioStep {
        let first = self.first_audio_ns.is_none();
        let start = *self.first_audio_ns.get_or_insert(pts_ns);
        let release = self.granted
            && !self.released
            && pts_ns - start >= crate::player::engine::AUDIO_LEAD_IN_NS + LEAD_IN_MARGIN_NS;
        self.released |= release;
        AudioStep { first, release }
    }

    /// Give the lane's grant back if one stands: an exit from the granted state that no audio push
    /// or draining consumer reached (a seek, the end of the file, a stop, audio whose timestamps
    /// never span the lead-in). The cap follows the consumer down to the base.
    fn withdraw(&mut self, video_lane: *mut AuQueue) {
        if self.granted && !self.released {
            crate::aq::aq_release_headroom(video_lane);
            self.released = true;
        }
    }

    /// A seek starts a new epoch: whatever grant the last one left standing goes back, and every
    /// field starts over (the next video push that would park grants afresh).
    fn restart(&mut self, video_lane: *mut AuQueue) {
        self.withdraw(video_lane);
        *self = PreAudioHeadroom::default();
    }
}

/// What an audio push means for the video lane's headroom.
struct AudioStep {
    /// The first audio AU since the open or seek: the caller logs how far video had to run.
    first: bool,
    /// The audio lead-in is in: release the headroom now.
    release: bool,
}

#[cfg(test)]
mod pre_audio_headroom_tests {
    use super::*;

    const LEAD: i64 = crate::player::engine::AUDIO_LEAD_IN_NS + LEAD_IN_MARGIN_NS;

    const GRANT: VideoStep = VideoStep::Grant(PRE_AUDIO_VIDEO_HEADROOM);

    /// The grant is LAZY and made once: only at a video push that would park at the base cap with an
    /// audio lane being fed and no audio read. A push that fits (an ordinary file: video a few
    /// packets ahead of its audio), a file with no audio lane, or one whose audio came first never
    /// gets one.
    #[test]
    fn the_grant_is_made_once_and_only_when_video_would_park_ahead_of_the_audio() {
        let mut h = PreAudioHeadroom::default();
        assert_eq!(h.before_video(true, false, 0), VideoStep::Proceed, "room left: no grant");
        assert!(!h.granted);
        assert_eq!(h.before_video(true, true, 0), GRANT);
        assert_eq!(h.before_video(true, false, 0), VideoStep::Proceed, "the cap has room again");

        assert_eq!(
            PreAudioHeadroom::default().before_video(false, true, 0),
            VideoStep::Proceed,
            "no audio lane: nothing to wait for"
        );

        let mut audio_first = PreAudioHeadroom::default();
        assert!(audio_first.after_audio(0).first);
        assert_eq!(audio_first.before_video(true, true, 0), VideoStep::Proceed, "audio came first: no grant");
        assert!(!audio_first.after_audio(LEAD).release, "and so nothing to release");
    }

    /// The first audio AU does NOT release the grant: the engine needs `AUDIO_LEAD_IN_NS` of audio
    /// more, and the demuxer can only read it while the video lane still has room. The release
    /// comes exactly when that much audio, plus the margin, has been pushed.
    #[test]
    fn the_grant_is_held_until_the_audio_lead_in_has_been_pushed() {
        let mut h = PreAudioHeadroom::default();
        assert_eq!(h.before_video(true, true, 0), GRANT);
        let first = h.after_audio(11_008_000_000);
        assert!(first.first && !first.release, "the first AU alone releases nothing");
        assert!(!h.after_audio(11_008_000_000 + crate::player::engine::AUDIO_LEAD_IN_NS).release,
            "the engine's lead-in without the margin is not yet enough");
        let done = h.after_audio(11_008_000_000 + LEAD);
        assert!(!done.first && done.release, "lead-in plus margin pushed: release");
        assert!(!h.after_audio(11_008_000_000 + LEAD + 32_000_000).release, "released once");
    }

    /// A full lane under the grant with no audio and nothing popped since the grant is the hang
    /// the bound exists for; once audio is in, a full lane is ordinary backpressure (the lead-in
    /// logic owns what follows).
    #[test]
    fn a_full_lane_with_no_audio_and_no_pops_fails_the_open() {
        let mut h = PreAudioHeadroom::default();
        assert_eq!(h.before_video(true, true, 7), GRANT);
        assert_eq!(h.before_video(true, false, 7), VideoStep::Proceed, "room left");
        assert_eq!(h.before_video(true, true, 7), VideoStep::Fail, "full, no audio, nothing popped");
        h.after_audio(0);
        assert_eq!(h.before_video(true, true, 7), VideoStep::Proceed, "audio arrived: not this case");
    }

    /// **F1.** A lane whose consumer IS draining is not the hang: after a seek into an audioless
    /// stretch the pump pops video while the demuxer reads at LAN speed, and main parks that lane
    /// (and plays through the engine's video-only prime escape) instead of failing the open. The
    /// pop counter is read from a real lane.
    #[test]
    fn a_lane_that_is_draining_is_released_not_failed() {
        let mut q = crate::aq::aq_new(100);
        let qp = &mut *q as *mut AuQueue;
        let au = [0u8; 100];
        let push = || assert_eq!(crate::aq::aq_push(qp, au.as_ptr(), au.len() as c_int, 0, 1, 1), 0);
        let mut h = PreAudioHeadroom::default();
        for _ in 0..2 {
            push(); // 200 bytes held over the base cap of 100: a push would park
        }
        assert!(crate::aq::aq_would_park(qp));
        let VideoStep::Grant(extra) = h.before_video(true, crate::aq::aq_would_park(qp), crate::aq::aq_pops(qp)) else {
            panic!("a push that would park with no audio grants");
        };
        crate::aq::aq_set_headroom(qp, extra);
        // Fill past the standing cap: the next push would park again.
        while !crate::aq::aq_would_park(qp) {
            push();
        }
        assert_eq!(
            h.before_video(true, true, crate::aq::aq_pops(qp)),
            VideoStep::Fail,
            "full, no audio and no pops: the hang"
        );
        // The consumer pops, then the lane fills again.
        for _ in 0..3 {
            let n = crate::aq::aq_pop(qp, std::ptr::null_mut());
            assert!(!n.is_null());
            unsafe { libc::free(n as *mut c_void) };
        }
        while !crate::aq::aq_would_park(qp) {
            push();
        }
        assert_eq!(
            h.before_video(true, crate::aq::aq_would_park(qp), crate::aq::aq_pops(qp)),
            VideoStep::Release,
            "pops happened since the grant: the consumer is draining, so this is not the hang"
        );
        assert!(h.released);
        assert_eq!(h.before_video(true, true, crate::aq::aq_pops(qp)), VideoStep::Proceed, "released once");
    }

    /// **F3.** The seek reset must take the lane's grant away. Restarting the struct alone left a
    /// seek that landed while the grant was unreleased with the video lane at base + 24 MiB for the
    /// rest of the session (the next epoch may be audio-first, or carry audio whose pts never span
    /// the lead-in), with the exhaustion check off.
    #[test]
    fn a_seek_with_the_grant_outstanding_returns_the_lane_to_its_base_cap() {
        let mut q = crate::aq::aq_new(100);
        let qp = &mut *q as *mut AuQueue;
        let mut h = PreAudioHeadroom::default();
        let VideoStep::Grant(extra) = h.before_video(true, true, 0) else { panic!("grant") };
        crate::aq::aq_set_headroom(qp, extra);
        assert_eq!(crate::aq::aq_cap(qp), 100 + extra as i64);
        h.restart(qp);
        assert_eq!(crate::aq::aq_cap(qp), 100, "the lane is back at its base cap after the seek reset");
        assert_eq!(h.before_video(false, true, 0), VideoStep::Proceed, "and the state started over");
        assert!(!h.granted && !h.released && h.first_audio_ns.is_none());
        // The next epoch grants afresh.
        assert_eq!(h.before_video(true, true, 0), GRANT);
    }

    /// With the lane holding more than its base cap, the seek reset still never drops the cap under
    /// what the lane holds (`aq_release_headroom`), and a grant that was already released (or never
    /// made) is left alone.
    #[test]
    fn restart_releases_through_the_drain_and_leaves_settled_states_alone() {
        let mut q = crate::aq::aq_new(100);
        let qp = &mut *q as *mut AuQueue;
        let au = [0u8; 100];
        crate::aq::aq_set_headroom(qp, 1_000);
        for _ in 0..5 {
            assert_eq!(crate::aq::aq_push(qp, au.as_ptr(), au.len() as c_int, 0, 1, 1), 0);
        }
        let mut h = PreAudioHeadroom { granted: true, ..Default::default() };
        h.restart(qp);
        assert_eq!(crate::aq::aq_cap(qp), 500, "never under the lane's occupancy");
        // Never granted: no touch.
        crate::aq::aq_set_headroom(qp, 1_000);
        PreAudioHeadroom::default().restart(qp);
        assert_eq!(crate::aq::aq_cap(qp), 1_100, "nothing granted by this state, nothing withdrawn");
    }

    /// The geometry the constants claim, pinned: the cap is the base plus the headroom, the
    /// reported film's 11 s delay now fits the base alone, the late-audio fixture (20 s at 24 Mbit/s)
    /// outruns the base yet fits with the engine's lead-in, and a delay at ~60 Mbit/s does not (the
    /// bound is real).
    #[test]
    fn the_bound_holds_the_measured_cases_and_not_everything() {
        let base = crate::player::aq_caps().0;
        let cap = base + PRE_AUDIO_VIDEO_HEADROOM as i64;
        assert_eq!(cap, 72 * 1024 * 1024);
        let bytes = |mbit: f64, secs: f64| (mbit * 1e6 / 8.0 * secs) as i64;
        // the reported film: 11.008 s of ~9.5 Mbit/s video
        assert!(bytes(9.495_770, 11.008) < base, "the plain cap alone holds it");
        // the fixture: 24 Mbit/s, 20 s, and then the lead-in's worth of video as well
        let lead_s = (crate::player::engine::AUDIO_LEAD_IN_NS + LEAD_IN_MARGIN_NS) as f64 / 1e9;
        assert!(bytes(24.0, 20.0) > base, "the fixture needs the grant");
        assert!(bytes(24.0, 20.0 + lead_s) < cap, "headroom spans the pre-audio stretch AND the lead-in");
        assert!(bytes(60.0, 11.008) > cap, "a bound, not a promise");
    }
}

/// The bookkeeping after one audio AU (stamped `pts`) reached the audio lane - the same for a
/// demuxed packet and a decoded PCM batch: the pre-audio headroom's progress (and its release
/// once the engine's lead-in is in), and the audio tail the seek logic reads.
fn audio_pushed(headroom: &mut PreAudioHeadroom, aq_p: *mut AuQueue, pts: i64) {
    let step = headroom.after_audio(pts);
    if step.first && headroom.granted {
        // The late audio arrived: say how far ahead video had to run.
        crate::player::log(&format!(
            "ff: first audio at {}ms after {} bytes of video queued ahead of it",
            pts / 1_000_000,
            crate::aq::aq_bytes(aq_p)
        ));
    }
    if step.release {
        // The engine's lead-in is on the audio lane: from here the video lane
        // falls back to its base cap as the consumer drains it, never under what
        // it holds (see `PRE_AUDIO_VIDEO_HEADROOM`).
        crate::aq::aq_release_headroom(aq_p);
        crate::player::log(&format!(
            "ff: audio lead-in pushed ({}ms past the first); video headroom released with {} bytes queued",
            (pts - headroom.first_audio_ns.unwrap_or(pts)) / 1_000_000,
            crate::aq::aq_bytes(aq_p)
        ));
    }
    SHARED.audio_tail_ns.store(pts, Ordering::Release);
}

/// The demux thread body (spawned by `engine::start_bufferfeed`).
///
/// Takes the stream's URL and its request headers ([`crate::headers::RequestHeaders`]) because
/// **the stream decides the transport**: an `https` URL, or any stream carrying headers, is pulled
/// through [`crate::curlio`], which applies those headers to every request it makes; a header-less
/// `http` URL reads through the Engine's `stream.rs` socket. An origin is parsed from a URL and
/// never rebuilt from an address, which is what keeps the hostname TLS validates against intact
/// all the way down here (`net/origin.rs`). `hs` is passed on both paths - it is the Engine's, and
/// it stays unused (fd = -1, published as `SHARED.hs_ptr`) when the stream goes through curl.
///
/// `acodec` is the audio codec the Load declared: the default audio track is the first one of that
/// codec, so the elementary stream fed matches what the pipeline was told.
#[allow(clippy::too_many_arguments)]
pub fn demux(
    _off: &tv_base::task::OffFrame,
    url: String,
    headers: crate::headers::RequestHeaders,
    acodec: String,
    aq: SendPtr<AuQueue>,
    aqa: SendPtr<AuQueue>,
    hs: SendPtr<HttpStream>,
) {
    PUSHED_ANY.store(false, Ordering::Relaxed);
    PUBLISHES_REOPENS.with(|p| p.set(true));
    // Refuse rather than read a struct whose shape we do not know (see `boot`). EOF must still be
    // set on both lanes or `pump` waits forever on a queue nothing will ever fill - the same
    // contract the panic barrier below exists to keep.
    if !ABI_OK.load(Ordering::Relaxed) {
        crate::player::log("demux: refusing — FFmpeg ABI on this device is not the one built for");
        crate::aq::aq_set_eof(aq.0);
        crate::aq::aq_set_eof(aqa.0);
        return;
    }
    DIAG_FIRST.store(true, Ordering::Relaxed);
    ensure_registered();
    let aq_p = aq.0; // VIDEO lane (also the AVIO abort ptr + EOF marker)
    let aqa_p = aqa.0; // AUDIO lane (es=2) - always a distinct queue on the ff (two-lane) path
    let hs_p = hs.0;
    let (origin, path) = tv_net::net::origin::split(&url);
    let path = path.to_owned();
    // ONE transport decision, here: curl for TLS and for any stream whose requests must carry the
    // stream's own headers (the socket path cannot carry them; see `Src`).
    // An `.m3u8` is HLS ([`crate::hls`]), which fetches through curl too, one request per playlist,
    // key and segment, each reserving its own abort handle. A playlist served from a URL that does
    // not say so is recognised by its Content-Type after the curl open below.
    let hls_url = crate::hls::is_playlist_url(&url) || hls_handoff::waiting(&url);
    let mut replay: Vec<u8> = Vec::new();
    let via_curl = origin.is_tls() || !headers.is_empty() || hls_url;

    // PANIC BARRIER around the whole producer body. Not about the unwind itself - this thread is
    // started by `task::spawn`, so std already catches a panic at the thread boundary and turns it
    // into the `Err` that `task::join` logs. The problem is what the unwind SKIPS: the two
    // `aq_set_eof` calls in the tail below. The consumer is a one-way FIFO with no other liveness
    // signal - `aq_pop` reports EOF only from the `eof` flag the producer sets - so a producer that
    // dies without setting it leaves `pump` feeding a queue that will never fill and never end:
    // no EOS is ever pushed (`engine.rs`'s EOS is keyed on the video lane's true EOF), no error is
    // ever surfaced, and the app sits on a frozen picture until BACK. That is strictly worse than
    // the crash it came from - a rare panic became a common-looking hang with no log line pointing
    // at it. So: catch it, name it in the event log, and then run the SAME teardown the normal exit
    // runs, so the pump terminates the way it does at end-of-stream.
    //
    // `AssertUnwindSafe` is honest here rather than a rubber stamp: the state the closure mutates
    // across the boundary is the two AU queues (their own pthread mutexes, and this is their only
    // producer, which is now dead) and `SHARED`'s atomics. What a panic DOES leak is the FFmpeg
    // side - `AVFormatContext`/`AVIOContext`/`AVPacket` are freed at the end of the block, not by a
    // `Drop`, so an unwind past them leaks them plus the socket buffer. That is deliberate: the
    // alternative is calling `avformat_close_input` on a context whose invariants a panicking
    // libavformat may have left broken, and a one-off leak on a path that is meant never to run is
    // cheaper than a segfault. The session ends here either way.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // A breakable block that runs once, or twice at most: every `break` below is an early exit
        // to the EOF/cleanup tail after it. The only `continue`s are an HLS seek's reopen and the
        // single retry of an open that failed transiently (`open_retried`). It used to be a real loop that reopened the whole
        // AVFormatContext on a fresh URL, which is how a direct-play seek was supposed to reach its
        // target - but the reopen could never be triggered (see the seek note in the read loop), and
        // the remaining callers replace the engine outright instead: a reload-mode seek and an
        // audio-track switch each `reload_at`, which spawns a new demux thread. So the reopen had no
        // live writer and no live caller.
        // `avformat_open_input` failed on bytes that were not a whole header, and the open was
        // retried once (`open_failure_is_transient`).
        let mut open_retried = false;
        loop {
            // An HLS seek reopens the stream at a segment (see the read loop's seek): set there,
            // read after this pass's cleanup.
            let mut hls_reopen = false;
            unsafe {
                // Publish curl's wake target BEFORE the final AU-abort check. These two operations
                // close each other's race: teardown either sees and signals the reservation, or it
                // aborts the lane first and this check refuses to start I/O. Creating/registering the
                // source only after the check leaves a window where teardown's one wake sees nothing
                // and the demuxer then opens a fresh connection under the main thread's join.
                let mut curl_open = if via_curl && !hls_url {
                    match crate::curlio::CurlSource::reserve_open() {
                        Ok(r) => Some(r),
                        Err(e) => {
                            crate::player::log(&format!("ff: https reservation FAILED: {e:?}"));
                            SHARED.demux_failed.store(true, Ordering::Release);
                            break;
                        }
                    }
                } else {
                    None
                };
                // Teardown may have raced us here (it aborts the lanes, then signals both transports,
                // then JOINS this thread on the main thread). The reservation above makes the HTTPS
                // side as race-free as the socket's already-published `hs_ptr`.
                if crate::aq::aq_is_aborted(aq_p) {
                    crate::player::log("ff: aborted before reopen");
                    break;
                }
                // ONE decision, here, from the scheme; the only later switch is a plaintext open
                // redirected to https, which `open_plain_progressive` hands to curl. Both arms publish the same two diagnostics (`dg_http_status`, `file_size`)
                // before anything else can fail, because the read-out panel is the first thing anybody
                // looks at when a part will not play and it must mean the same thing either way.
                let open_began = std::time::Instant::now();
                let (src, size) = if hls_url {
                    match open_hls(&url, &headers, aq_p, &mut replay) {
                        Ok(opened) => opened,
                        Err(MediaOpenFail::Aborted) => {
                            crate::player::log("ff: aborted during open");
                            break;
                        }
                        Err(MediaOpenFail::Failed) => {
                            SHARED.demux_failed.store(true, Ordering::Release);
                            break;
                        }
                    }
                } else if via_curl {
                    let reservation = curl_open
                        .take()
                        .expect("the curl path reserved its abort handle above");
                    match crate::curlio::CurlSource::open_reserved(&url, 0, &headers, reservation) {
                        Ok(cs) if cs.content_type().is_some_and(crate::hls::is_playlist_content_type) => {
                            // A playlist the URL did not name: hand the stream to the HLS client,
                            // which requests it again on its own transfer.
                            crate::player::log("ff: the stream answered with an HLS playlist (Content-Type)");
                            drop(cs);
                            match open_hls(&url, &headers, aq_p, &mut replay) {
                                Ok(opened) => opened,
                                Err(MediaOpenFail::Aborted) => {
                                    crate::player::log("ff: aborted during open");
                                    break;
                                }
                                Err(MediaOpenFail::Failed) => {
                                    SHARED.demux_failed.store(true, Ordering::Release);
                                    break;
                                }
                            }
                        }
                        Ok(cs) => {
                            let (st, size) = (cs.status(), cs.size());
                            SHARED.file_size.store(size, Ordering::Release);
                            SHARED.dg_http_status.store(st, Ordering::Relaxed);
                            crate::player::log(&format!(
                                "ff: open {} status={st} clen={size}",
                                if origin.is_tls() { "https" } else { "http (curl, stream headers)" }
                            ));
                            (Src::Curl(cs), size)
                        }
                        Err(e) => {
                            // A status we could not stream is worth publishing; a transport failure has
                            // no status, and 0 is how the panel already spells "never answered".
                            if let crate::curlio::OpenErr::Status(st) = e {
                                SHARED.dg_http_status.store(st, Ordering::Relaxed);
                            }
                            crate::player::log(&format!("ff: curl open FAILED: {e:?}"));
                            SHARED.demux_failed.store(true, Ordering::Release);
                            break;
                        }
                    }
                } else {
                    // Redirects followed; an https hop comes back as the curl source.
                    match open_plain_progressive(hs_p, &origin, &path, aq_p) {
                        Ok(opened) => opened,
                        Err(MediaOpenFail::Aborted) => {
                            crate::player::log("ff: aborted during open");
                            break;
                        }
                        Err(MediaOpenFail::Failed) => {
                            SHARED.demux_failed.store(true, Ordering::Release);
                            break;
                        }
                    }
                };

                crate::player::log(&format!("ff: open ms={}", open_began.elapsed().as_millis()));

                let mut state = Box::new(AvioState {
                    src,
                    aq: aq_p,
                    off: 0,
                    size,
                    io_failed: false,
                    bounce: std::mem::take(&mut replay),
                    bounce_pos: 0,
                    parked_at_end: false,
                });
                let buf = av_malloc(65536) as *mut u8;
                if buf.is_null() {
                    crate::player::log("ff: av_malloc failed");
                    break;
                }
                // No seek callback for HLS: an unseekable AVIO makes libavformat skip forward by
                // reading (mov's sample positions inside a fragment) and never ask for a byte
                // address the concatenated stream does not have.
                let is_hls = matches!(state.src, Src::Hls(_));
                let avio = avio_alloc_context(
                    buf,
                    65536,
                    0,
                    &mut *state as *mut AvioState as *mut c_void,
                    Some(read_cb),
                    None,
                    if is_hls { None } else { Some(seek_cb) },
                );
                if avio.is_null() {
                    crate::player::log("ff: avio_alloc_context failed");
                    free_ptr(buf as *mut c_void);
                    break;
                }
                let mut fmt = avformat_alloc_context();
                if fmt.is_null() {
                    crate::player::log("ff: avformat_alloc_context failed");
                    free_avio(avio);
                    break;
                }
                (*fmt).pb = avio;
                if is_hls {
                    cap_hls_probe(fmt);
                }
                let r = avformat_open_input(
                    &mut fmt,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                if r < 0 || fmt.is_null() {
                    let aborted = crate::aq::aq_is_aborted(aq_p);
                    crate::player::log(&open_input_failure_note(r, aborted));
                    free_avio(avio);
                    if !aborted && !is_hls && !open_retried && open_failure_is_transient(r) {
                        // Once, from the ORIGINAL url: the redirect is resolved afresh.
                        open_retried = true;
                        crate::player::log("ff: open_input retry from the original URL");
                        continue;
                    }
                    break;
                }
                if avformat_find_stream_info(fmt, std::ptr::null_mut()) < 0 {
                    crate::player::log("ff: find_stream_info failed");
                    avformat_close_input(&mut fmt);
                    free_avio(avio);
                    break;
                }
                // The container's own track names, published before anything else can fail: the track
                // menu is the only reader and it wants them whether or not this part turns out to have
                // a video stream. See `stream_name` for why they are read at all - they are the ONLY
                // place a track's identity exists.
                {
                    let names = track_names(fmt);
                    let named = names.audio.iter().filter(|n| !n.is_empty()).count();
                    if named > 0 {
                        crate::player::log(&format!("ff: container names {named}/{} audio tracks", names.audio.len()));
                    }
                    *SHARED.track_names.lock().unwrap() = names;
                }
                {
                    // Count only: the titles are file text and are never logged.
                    let chapters = read_chapters(fmt);
                    if !chapters.is_empty() {
                        crate::player::log(&format!("ff: {} chapters", chapters.len()));
                    }
                    *SHARED.chapters.lock().unwrap_or_else(|e| e.into_inner()) = chapters;
                }
                let vi =
                    av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, std::ptr::null_mut(), 0);
                // native audio-track selection: feed the chosen Nth audio stream (SHARED.desired_
                // audio_idx, set by the track menu), else the pipeline's best/default audio.
                let want_aidx = SHARED.desired_audio_idx.load(Ordering::Relaxed);
                let ai = if want_aidx >= 0 {
                    nth_audio_stream(fmt, want_aidx).unwrap_or_else(|| {
                        av_find_best_stream(
                            fmt,
                            AVMEDIA_TYPE_AUDIO,
                            -1,
                            -1,
                            std::ptr::null_mut(),
                            0,
                        )
                    })
                } else {
                    // default: feed the track matching the Load payload's codec (Media[0].audioCodec),
                    // NOT av_find_best_stream - a codec mismatch stalls the audio ES and wedges video.
                    audio_stream_matching(fmt, &acodec).unwrap_or_else(|| {
                        av_find_best_stream(
                            fmt,
                            AVMEDIA_TYPE_AUDIO,
                            -1,
                            -1,
                            std::ptr::null_mut(),
                            0,
                        )
                    })
                };
                if vi < 0 {
                    // "No video stream" alone reads as a demuxer fault. When audio IS present the
                    // demuxer worked fine - the stream itself arrived without video (an audio-only
                    // stream, or a source that dropped the track). Record which shape this is so the
                    // error the user sees can say so; the main thread words it.
                    if ai >= 0 {
                        SHARED.demux_no_video.store(true, Ordering::Release);
                        crate::player::log("ff: no video stream — the stream carries audio only");
                    } else {
                        crate::player::log("ff: no video stream");
                    }
                    avformat_close_input(&mut fmt);
                    free_avio(avio);
                    break;
                }
                let streams = (*fmt).streams;
                let vst = *streams.add(vi as usize);
                let vcp = stream_codecpar(vst);
                // AAC needs ADTS framing for LG's decoder (mp4/mkv carry raw AAC). Precompute the
                // per-frame ADTS fields (freq index + channel config) for the selected audio stream;
                // None => not AAC (or a non-standard rate) => fed verbatim.
                let mut audio_clock = AudioClock::default();
                let aac_adts: Option<(u8, u8)> = if ai >= 0 {
                    let acp = stream_codecpar(*streams.add(ai as usize));
                    if (*acp).codec_id == AV_CODEC_ID_AAC {
                        adts_params(acp)
                    } else {
                        None
                    }
                } else {
                    None
                };
                // The session decided this track is decoded here and fed as PCM (`AudioFeed::SoftPcm`):
                // open the decoder now, before any packet is read, so a build or a stream it cannot
                // serve fails the open with a reason rather than a silent lane.
                let pcm_ch = SOFT_PCM_CHANNELS.load(Ordering::Relaxed);
                let mut pcm = if ai >= 0 && pcm_ch > 0 && FEED_AUDIO.load(Ordering::Relaxed) {
                    let ast = *streams.add(ai as usize);
                    let acp = stream_codecpar(ast);
                    match adec::PcmFeed::open(acp, stream_time_base(ast), pcm_ch) {
                        Ok(p) => {
                            crate::player::log(&format!(
                                "ff: software audio decode on ({} -> PCM {}ch {}Hz)",
                                std::ffi::CStr::from_ptr(avcodec_get_name((*acp).codec_id)).to_string_lossy(),
                                p.channels,
                                adec::PCM_RATE
                            ));
                            Some(p)
                        }
                        Err(e) => {
                            crate::player::log(&format!("ff: software audio decode unavailable ({e}); refusing the stream"));
                            SHARED.demux_failed.store(true, Ordering::Release);
                            avformat_close_input(&mut fmt);
                            free_avio(avio);
                            break;
                        }
                    }
                } else {
                    None
                };
                if let Some((fi, ch)) = aac_adts {
                    crate::player::log(&format!(
                        "ff: AAC: raw packets get an ADTS header (freq_idx={fi} ch={ch}), ADTS ones pass as they are"
                    ));
                }
                let dur = fmt_duration(fmt);
                if dur > 0 {
                    SHARED
                        .duration_ns
                        .store(dur.saturating_mul(1000), Ordering::Relaxed);
                }
                // The NAME as well as the id, and the name is what anything downstream should grade.
                // A raw AV_CODEC_ID is an FFmpeg enum, and that enum RENUMBERS between majors - H264
                // is 28 on the n3.3 the televisions ship, 27 on FFmpeg 6, and HEVC is 174 / 173 / 172
                // across n3.3 / 6 / 9. The on-device suite asserted the bare number, so bundling our
                // own FFmpeg failed all 21 cases at once on a codec the app had identified perfectly
                // well. `avcodec_get_name` is stable, means what the assertion meant, and is free.
                // Publish the coded size: the webOS 5+ exported window needs the frame it is being
                // fed, and this is the only place that knows it for certain - a stream's declared
                // dimensions and its actual coded size need not agree.
                SHARED.publish_video_raster((*vcp).width, (*vcp).height);
                let cname =
                    std::ffi::CStr::from_ptr(avcodec_get_name((*vcp).codec_id)).to_string_lossy();
                // Refuse a stream the pipeline cannot be fed, BEFORE anything else is opened for it:
                // the exit then has the same shape as the "no video stream" one above, with nothing
                // to release but the input and its AVIO.
                let Some(framing) = VideoFraming::of_codec_id((*vcp).codec_id) else {
                    crate::player::log(&format!(
                        "ff: no framing for video codec {cname} (codec_id={}); refusing the stream",
                        (*vcp).codec_id
                    ));
                    SHARED.demux_failed.store(true, Ordering::Release);
                    avformat_close_input(&mut fmt);
                    free_avio(avio);
                    break;
                };
                // What the Load declared (`dg_load_v`, 0 when this start sent a static payload) must
                // be what the file is: a Load that says one codec over another codec's bitstream is a
                // black screen, so stop and name both.
                let declared = SHARED.dg_load_v.load(Ordering::Relaxed);
                if declared != 0 && declared != framing.load_video().diag_code() {
                    crate::player::log(&format!(
                        "ff: the Load declared {} but the file is {cname}; refusing the stream",
                        crate::player::load_v_name(declared)
                    ));
                    SHARED.demux_failed.store(true, Ordering::Release);
                    avformat_close_input(&mut fmt);
                    free_avio(avio);
                    break;
                }
                // WHAT THE STREAM ACTUALLY IS, not just what it is called. `codec=hevc` is the same
                // four letters for an SDR file, an HDR10 file, a Dolby Vision Profile 8.1 file whose
                // base layer is that same HDR10, and a Profile 5 file that will display in visibly
                // wrong colours - `avcodec_get_name` cannot tell them apart and neither could this log
                // line or any assertion built on it. These fields can. `trc`/`pri`/`spc` are the raw
                // AVCOL_* enum values, logged as NUMBERS because naming them would mean binding three
                // more FFmpeg symbols for a diagnostic: trc 16 = smpte2084 (PQ/HDR10), 18 = arib-std-b67
                // (HLG), spc 9 = bt2020nc, pri 9 = bt2020, and **2 = UNSPECIFIED on all three** (every
                // one of those six numbers checked against the vendored `libavutil/pixfmt.h`, not
                // recalled). A Profile 5 file is expected to read 2/2/2, since IPT-PQ signals no
                // ordinary transfer at all - inferred rather than seen from here; this line is where
                // a television settles it.
                // All three were declared in the offsets table above and read NOWHERE until now.
                let dv = match dovi_conf(vcp) {
                    // `bl_compat` is the field that decides whether the base layer means anything on
                    // its own, so it is logged beside the profile rather than left to be inferred
                    // from it - 0 is Profile 5's "none", 1 the HDR10 of a Profile 8.1.
                    Some(d) => format!(
                        " dovi=P{} level={} bl_compat={} rpu={} el={} bl={}",
                        d.dv_profile,
                        d.dv_level,
                        d.dv_bl_signal_compatibility_id,
                        d.rpu_present_flag,
                        d.el_present_flag,
                        d.bl_present_flag
                    ),
                    None => String::new(),
                };
                crate::player::log(&format!(
                    "ff: v=#{vi} codec={cname} codec_id={} {}x{} trc={} pri={} spc={}{dv} a=#{ai} dur_ns={}",
                    (*vcp).codec_id,
                    (*vcp).width,
                    (*vcp).height,
                    (*vcp).color_trc,
                    (*vcp).color_primaries,
                    (*vcp).color_space,
                    SHARED.duration_ns.load(Ordering::Relaxed)
                ));

                for i in 0..(*fmt).nb_streams as c_int {
                    set_stream_discard(*streams.add(i as usize), discard_for(i, vi, ai));
                }
                // The subtitle tracks, published, read and decoded (their discard lifted).
                let mut subs = subdec::Reader::open(fmt);

                // Hand-roll length-prefix -> Annex-B + prepend VPS/SPS/PPS at every keyframe (the
                // format Starfish decodes). FFmpeg's hevc_mp4toannexb does NOT reliably prepend the
                // parameter sets on this 3.3 build (it leaves the keyframe starting with SEI), so we
                // build the AU from the codecpar extradata; libavformat still owns demux + seeking.
                // AV1 is framed by av1_packet_to_tu as Section-5 temporal units with the av1C config
                // in-band on keyframes, because the TV pipeline is given no codec_data for AV1.
                let is_hevc = framing == VideoFraming::AnnexB { hevc: true };
                let is_av1 = framing == VideoFraming::Av1;
                // VP8 rides the VP9 arm: the packet as it came.
                let is_vp9 = matches!(framing, VideoFraming::Vp9 | VideoFraming::Vp8);
                let (param_blob, nal_len_size) = if is_av1 || is_vp9 {
                    (Vec::new(), 0)
                } else {
                    parse_extradata(
                        (*vcp).extradata,
                        (*vcp).extradata_size.max(0) as usize,
                        is_hevc,
                    )
                };
                let av1_ed: &[u8] = if (*vcp).extradata.is_null() || (*vcp).extradata_size <= 0 {
                    &[]
                } else {
                    std::slice::from_raw_parts((*vcp).extradata, (*vcp).extradata_size as usize)
                };
                let av1_cfg = if is_av1 { parse_av1c(av1_ed) } else { Vec::new() };
                let av1_framing = if is_av1 {
                    Av1Framing::from_trigger(tv_base::devtrig::read("av1obu").as_deref())
                } else {
                    Av1Framing::DEFAULT
                };
                #[cfg(feature = "devtriggers")]
                {
                    if is_av1 && av1_framing != Av1Framing::DEFAULT {
                        crate::player::log(&format!(
                            "ff: av1obu trigger: td={} seq={} raw={}",
                            av1_framing.td, av1_framing.seq, av1_framing.raw
                        ));
                    }
                }
                // A VP9 stream's PQ is in the container only; hand it to the pump, which writes it into
                // the sourceInfo envelope ACB is sent. Always replaced, so a previous stream's colour
                // cannot outlive it.
                *SHARED.container_hdr.lock().unwrap() = if framing == VideoFraming::Vp9 { Some(container_hdr(vcp)) } else { None };
                crate::player::log(&format!(
                    "ff: param_sets={} bytes nal_len={} is_hevc={} is_av1={} is_vp9={} av1_cfg={}B framing={framing:?}",
                    param_blob.len(),
                    nal_len_size,
                    is_hevc,
                    is_av1,
                    is_vp9,
                    av1_cfg.len()
                ));
                let mut aubuf: Vec<u8> = Vec::with_capacity(4 * 1024 * 1024);
                // Dolby Vision Profile 7's base-layer strip; its scratch lives as long as this open.
                let mut dv_strip = DvStrip::default();

                let pkt = av_packet_alloc();
                if pkt.is_null() {
                    crate::player::log("ff: packet_alloc failed");
                    avformat_close_input(&mut fmt);
                    free_avio(avio);
                    break;
                }

                let mut video_start = VideoStart::default();
                // The video lane's pre-audio headroom (`PreAudioHeadroom`): granted at the first
                // video push that WOULD PARK at the base cap with no audio yet, held until the
                // engine's audio lead-in has been pushed, then released as the consumer drains.
                // Restarted (and the lane's grant withdrawn) by every seek and at loop exit.
                let mut headroom = PreAudioHeadroom::default();
                SHARED.first_audio_pts.store(i64::MIN, Ordering::Release);
                // The read loop ended on the file's own end (not a failure, a stop or a full lane):
                // the PCM decoder's held-back samples are still owed to the audio lane.
                let mut reached_end = false;
                // INNER read loop
                loop {
                    // Direct-play seek (and the armed resume, which is just a seek published before
                    // the first read): the pump leaves a target ns in SHARED.seek_to_ns and THIS
                    // thread - the only one that ever touches `fmt` - av_seek_frame's between two
                    // av_read_frame calls.
                    //
                    // It used to be an INTERRUPT instead: the pump shutdown(2)'d the socket to break
                    // the read, and the outer loop reopened the URL and seeked the fresh context.
                    // That could not work, and the test suite hid it behind inherited resume offsets
                    // until 2026-07-28. Our AVIO is SEEKABLE (`seek_cb` reopens with a byte Range),
                    // so libavformat treats a read error as recoverable, calls seek_cb, and gets a
                    // brand-new connection at the same offset - av_read_frame never returned an
                    // error, the inner loop never broke, and no reopen ever happened. Every
                    // direct-play seek ran out the stuck-watchdog on PRE-seek packets
                    // (`rebase: dropping stale kf` at the old position) and escalated to a full
                    // reload. Seeking here needs no interrupt at all, so nothing can race it.
                    // `demux_seeking` is raised BEFORE the mailbox is emptied and lowered after the
                    // seek returns, so a pump that reads the mailbox empty and then the flag down
                    // knows this seek has landed (`pump`'s retarget).
                    let seek_ns = if !is_hls && SHARED.seek_to_ns.load(Ordering::Acquire) >= 0 {
                        SHARED.demux_seeking.store(true, Ordering::SeqCst);
                        let ns = SHARED.seek_to_ns.swap(-1, Ordering::SeqCst);
                        if ns < 0 {
                            SHARED.demux_seeking.store(false, Ordering::SeqCst);
                        }
                        ns
                    } else {
                        SHARED.seek_to_ns.swap(-1, Ordering::Acquire)
                    };
                    if seek_ns >= 0 && is_hls {
                        // HLS has no byte addresses to seek in: re-arm the target and reopen the
                        // stream at the segment holding it (live: at its edge), after this pass's
                        // cleanup. The pump has already flushed the lanes for the seek.
                        SHARED.video_tail_ns.store(-1, Ordering::Release);
                        SHARED.audio_tail_ns.store(-1, Ordering::Release);
                        crate::player::log(&format!("ff: hls seek {}s: reopening at its segment", seek_ns / 1_000_000_000));
                        SHARED.seek_to_ns.store(seek_ns, Ordering::Release);
                        hls_reopen = true;
                        break;
                    }
                    if seek_ns >= 0 {
                        // These are content from the old seek epoch until the first new packets land.
                        SHARED.video_tail_ns.store(-1, Ordering::Release);
                        SHARED.audio_tail_ns.store(-1, Ordering::Release);
                        let ts = av_rescale_q(seek_ns, NS_TB, stream_time_base(vst));
                        crate::player::log(&format!("ff: seek begin {}s", seek_ns / 1_000_000_000));
                        let seek_began = std::time::Instant::now();
                        let forward = SEEK_FORWARD.swap(false, Ordering::AcqRel);
                        let mut sr = av_seek_frame(fmt, vi, ts, seek_flags(forward));
                        if forward && sr < 0 {
                            // no keyframe after the target (its end): the one before, as ever
                            sr = av_seek_frame(fmt, vi, ts, seek_flags(false));
                        }
                        SHARED.demux_seeking.store(false, Ordering::SeqCst);
                        crate::player::log(&format!(
                            "ff: seek {}s{} rv={sr} ms={}",
                            seek_ns / 1_000_000_000,
                            if forward { " (forward)" } else { "" },
                            seek_began.elapsed().as_millis()
                        ));
                        headroom.restart(aq_p);
                        SHARED.first_audio_pts.store(i64::MIN, Ordering::Release);
                        if let Some(p) = pcm.as_mut() {
                            p.reset();
                        }
                        subs.seeked();
                    }
                    let r = av_read_frame(fmt, pkt);
                    if frame_read_failed(&mut state, r) {
                        reached_end = r == AVERROR_EOF && !state.io_failed;
                        // Genuine end of stream, teardown, or an unrecovered I/O error published by
                        // `frame_read_failed`. NOT an app seek - that is serviced at the top of this
                        // loop and never surfaces as a read error.
                        break;
                    }
                    let si = (*pkt).stream_index;
                    if si == vi {
                        let is_key = if is_av1 {
                            let pkt_bytes: &[u8] = if (*pkt).data.is_null() {
                                &[]
                            } else {
                                std::slice::from_raw_parts((*pkt).data, (*pkt).size.max(0) as usize)
                            };
                            av1_packet_to_tu(
                                pkt_bytes,
                                (*pkt).flags & AV_PKT_FLAG_KEY != 0,
                                &av1_cfg,
                                av1_framing,
                                &mut aubuf,
                            )
                        } else if is_vp9 {
                            let pkt_bytes: &[u8] = if (*pkt).data.is_null() {
                                &[]
                            } else {
                                std::slice::from_raw_parts((*pkt).data, (*pkt).size.max(0) as usize)
                            };
                            vp9_packet_to_frame(pkt_bytes, (*pkt).flags & AV_PKT_FLAG_KEY != 0, &mut aubuf)
                        } else {
                            packet_to_annexb(
                                (*pkt).data,
                                (*pkt).size.max(0) as usize,
                                nal_len_size,
                                is_hevc,
                                &param_blob,
                                &mut aubuf,
                            )
                        };
                        // Profile 7 played as its base layer: drop the RPU and the enhancement
                        // layer (`stream::DvPresentation::BaseLayer`). An AU left with nothing is
                        // not fed.
                        if is_hevc && SHARED.dv_strip_layers.load(Ordering::Relaxed) {
                            let n = dv_strip.strip(&mut aubuf) as i64;
                            if n > 0 {
                                let total = DV_STRIPPED.fetch_add(n, Ordering::Relaxed) + n;
                                if total == n || total / 5000 != (total - n) / 5000 {
                                    crate::player::log(&format!("ff: dv base layer only — {total} RPU/EL NAL units dropped"));
                                }
                            }
                            if aubuf.is_empty() {
                                av_packet_unref(pkt);
                                continue;
                            }
                        }
                        // A start point: an IRAP/IDR by our own scan, FFmpeg's keyframe flag (the
                        // parser's recovery-point reading), or an AU carrying its parameter sets.
                        let decodable = is_key
                            || (*pkt).flags & AV_PKT_FLAG_KEY != 0
                            || (!is_av1 && !is_vp9 && annexb_has_parameter_sets(&aubuf, is_hevc));
                        if !video_start.feed(decodable, pts_ns_opt(pkt, vst).is_some()) {
                            if video_start.dropped == 250 {
                                crate::player::log(&format!(
                                    "ff: no video start point in the first 250 AU (last: key={is_key} flag={} param_sets={} pts={})",
                                    (*pkt).flags & AV_PKT_FLAG_KEY != 0,
                                    annexb_has_parameter_sets(&aubuf, is_hevc),
                                    pts_ns_opt(pkt, vst).is_some()
                                ));
                            }
                            av_packet_unref(pkt);
                            continue;
                        }
                        if video_start.dropped > 0 && video_start.just_started() {
                            crate::player::log(&format!(
                                "ff: video starts at the first timed keyframe ({} AU before it dropped)",
                                video_start.dropped
                            ));
                        }
                        let pts = pts_ns(pkt, vst);
                        if DIAG_FIRST.swap(false, Ordering::Relaxed) {
                            let n = aubuf.len().min(40);
                            let head: String = aubuf[..n]
                                .iter()
                                .map(|b| format!("{:02x}", b))
                                .collect::<Vec<_>>()
                                .join(" ");
                            crate::player::log(&format!(
                                "ff: AU#0 size={} key={} head=[{}]",
                                aubuf.len(),
                                is_key,
                                head
                            ));
                        }
                        match headroom.before_video(
                            ai >= 0 && FEED_AUDIO.load(Ordering::Relaxed),
                            crate::aq::aq_would_park(aq_p),
                            crate::aq::aq_pops(aq_p),
                        ) {
                            VideoStep::Proceed => {}
                            VideoStep::Grant(extra) => crate::aq::aq_set_headroom(aq_p, extra),
                            VideoStep::Release => {
                                // The lane is full under the grant but the consumer is draining it
                                // (a seek into an audioless stretch): not the hang the grant is
                                // bounded for. Park like any full lane, as before the grant.
                                crate::aq::aq_release_headroom(aq_p);
                                crate::player::log(&format!(
                                    "ff: video headroom released with {} bytes queued: the lane is draining and no audio has come",
                                    crate::aq::aq_bytes(aq_p)
                                ));
                            }
                            VideoStep::Fail => {
                                // The grant is spent, no audio AU has been read, and nothing has
                                // been popped since it was given: the consumer cannot drain video
                                // before its clock starts (which needs audio), so parking here is
                                // a spinner nobody can leave. Fail the open instead, the way an
                                // unfeedable audio track does, so the failure read-out shows.
                                crate::player::log(&format!(
                                    "ff: no audio AU after {} bytes of video queued (cap {} = base + the {} byte pre-audio headroom); failing the open",
                                    crate::aq::aq_bytes(aq_p),
                                    crate::aq::aq_cap(aq_p),
                                    PRE_AUDIO_VIDEO_HEADROOM
                                ));
                                SHARED.demux_failed.store(true, Ordering::Release);
                                SHARED.demux_io_failed.store(true, Ordering::Release);
                                av_packet_unref(pkt);
                                break;
                            }
                        }
                        let pushed = crate::aq::aq_push_with_drain(
                            aq_p,
                            aubuf.as_ptr(),
                            aubuf.len() as c_int,
                            pts,
                            if decodable { 1 } else { 0 },
                            1,
                            || state.drain_wire(),
                        );
                        av_packet_unref(pkt);
                        if pushed != 0 {
                            break;
                        }
                        PUSHED_ANY.store(true, Ordering::Relaxed);
                        SHARED.video_tail_ns.store(pts, Ordering::Release);
                    } else if si == ai && FEED_AUDIO.load(Ordering::Relaxed) && pcm.is_some() {
                        // Anything in, PCM out: the packet may complete zero or more AUs, each with
                        // its own (sample-counted) pts. The lane bookkeeping runs per AU pushed,
                        // exactly as for a demuxed one.
                        let ast = *streams.add(ai as usize);
                        let pts_in = pts_ns(pkt, ast);
                        let out = pcm.as_mut().expect("checked").push(pkt, pts_in);
                        av_packet_unref(pkt);
                        let frames = match out {
                            Ok(f) => f,
                            Err(e) => {
                                crate::player::log(&format!("ff: software audio (PCM decode) failed ({e}); failing the source"));
                                SHARED.demux_failed.store(true, Ordering::Release);
                                SHARED.demux_io_failed.store(true, Ordering::Release);
                                break;
                            }
                        };
                        let mut lane_closed = false;
                        for a in frames {
                            if headroom.first_audio_ns.is_none() {
                                SHARED.first_audio_pts.store(a.pts_ns, Ordering::Release);
                            }
                            let pushed = crate::aq::aq_push_with_drain(
                                aqa_p,
                                a.data.as_ptr(),
                                a.data.len() as c_int,
                                a.pts_ns,
                                1,
                                2,
                                || state.drain_wire(),
                            );
                            if pushed != 0 {
                                lane_closed = true;
                                break;
                            }
                            audio_pushed(&mut headroom, aq_p, a.pts_ns);
                        }
                        if lane_closed {
                            break;
                        }
                    } else if si == ai && FEED_AUDIO.load(Ordering::Relaxed) {
                        let ast = *streams.add(ai as usize);
                        let raw_audio: &[u8] = if (*pkt).data.is_null() { &[] } else { std::slice::from_raw_parts((*pkt).data, (*pkt).size.max(0) as usize) };
                        let frame_ns = adts_frame_ns(raw_audio).or_else(|| aac_adts.and_then(|(fi, _)| aac_frame_ns(fi))).unwrap_or(0);
                        let pts = audio_clock.stamp(pts_ns_opt(pkt, ast), frame_ns);
                        // MPEG-TS AAC is ADTS already: never wrap it in a second header.
                        let already_adts = raw_audio.len() >= 2 && raw_audio[0] == 0xFF && raw_audio[1] & 0xF6 == 0xF0;
                        if headroom.first_audio_ns.is_none() {
                            // Before the push, so the feed thread that pops this AU sees it: the
                            // audio lane feeds a late-starting track's first AU on this mark.
                            SHARED.first_audio_pts.store(pts, Ordering::Release);
                            crate::player::log(&format!(
                                "ff: first audio AU pts={}ms size={} adts={already_adts} reframe={}",
                                pts / 1_000_000, raw_audio.len(), aac_adts.is_some() && !already_adts
                            ));
                        }
                        let pushed = if let Some((freq_idx, chan_cfg)) = aac_adts.filter(|_| !already_adts) {
                            // prepend a 7-byte ADTS header so LG's decoder can frame the raw AAC
                            let plen = (*pkt).size as usize;
                            let mut framed = Vec::with_capacity(7 + plen);
                            framed.extend_from_slice(&adts_header(freq_idx, chan_cfg, plen));
                            framed.extend_from_slice(std::slice::from_raw_parts((*pkt).data, plen));
                            crate::aq::aq_push_with_drain(
                                aqa_p,
                                framed.as_ptr(),
                                framed.len() as c_int,
                                pts,
                                1,
                                2,
                                || state.drain_wire(),
                            )
                        } else {
                            crate::aq::aq_push_with_drain(
                                aqa_p,
                                (*pkt).data,
                                (*pkt).size,
                                pts,
                                1,
                                2,
                                || state.drain_wire(),
                            )
                            // AUDIO lane
                        };
                        av_packet_unref(pkt);
                        if pushed != 0 {
                            break;
                        }
                        audio_pushed(&mut headroom, aq_p, pts);
                    } else if let Some(track) = subs.track_of(si) {
                        subs.decode(track, pkt);
                        av_packet_unref(pkt);
                    } else {
                        av_packet_unref(pkt);
                    }
                    if crate::aq::aq_is_aborted(aq_p) {
                        break;
                    }
                }

                // The file ended: the PCM decoder's held-back samples and its last short batch go to
                // the audio lane, so the final few ms are not lost. A failed push here is a stop
                // arriving at the same moment, and changes nothing.
                if let Some(p) = pcm.as_mut().filter(|_| reached_end && !crate::aq::aq_is_aborted(aq_p)) {
                    for a in p.finish() {
                        if crate::aq::aq_push_with_drain(aqa_p, a.data.as_ptr(), a.data.len() as c_int, a.pts_ns, 1, 2, || state.drain_wire()) != 0 {
                            break;
                        }
                        SHARED.audio_tail_ns.store(a.pts_ns, Ordering::Release);
                    }
                }
                drop(pcm);

                // Whatever way the read loop ended, the video lane does not keep a standing grant.
                headroom.withdraw(aq_p);

                // cleanup this stream (we own pb, so close_input won't free the AVIO)
                drop(subs);
                let mut pkt_m = pkt;
                av_packet_free(&mut pkt_m);
                avformat_close_input(&mut fmt);
                free_avio(avio);
                let _ = &state; // keep the AvioState alive until after free_avio
            }

            if unsafe { crate::aq::aq_is_aborted(aq_p) } {
                break;
            }
            if hls_reopen {
                continue;
            }
            break;
        }
    })); // end panic barrier. The body above is deliberately NOT re-indented into the closure: a
    // ~340-line whitespace diff would bury every real change this file ever gets again.

    // The panic path joins the normal one HERE, so both leave the consumer in the same state.
    if let Err(e) = &outcome {
        // The payload is almost always a `&str`/`String` (a slice index, an `unwrap`), and the
        // message is worth far more than the bare fact of a panic: this thread walks packet buffers
        // whose sizes come off the wire, so knowing WHICH one gave way is most of the triage. The
        // C tracer in `main.c` cannot help here - nothing faults, so there is no PC to symbolize.
        let what = e
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| e.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string payload>".to_string());
        crate::player::log(&format!("ff: demux PANICKED — {what}"));
        // The producer is gone before it could publish a duration or a single frame, which is the
        // one case EOF alone cannot resolve: `engine.rs` pushes EOS only once the pipeline has
        // reached `Stage::Streaming`, so a panic during open/find_stream_info would otherwise leave
        // the pump waiting on a stage it can never reach. This is the same flag the `http_open`
        // failure above raises, and `pump` turns it into `PlaybackState::Error` (gated on
        // `frames == 0`, so a panic MID-playback is left to the EOF/EOS path below instead).
        SHARED.demux_failed.store(true, Ordering::Release);
    }
    // A demux that ends having produced NOTHING is a failure, whatever route it took out.
    // Only two exits used to say so - the http_open failure and the panic handler - while every
    // other early `break` in the open sequence (the ABI refusal, av_malloc, avio_alloc_context,
    // avformat_alloc_context, open_input, find_stream_info, no video stream) simply set EOF and
    // returned. EOF with zero AUs is indistinguishable from a zero-length file: the pump keeps
    // waiting, `frames` stays 0, and the HUD says "Buffering..." forever with nothing in the log to
    // say why. That is precisely the report that came back from webOS 6 and 10, and it is why the
    // report could not name a cause. Failing loudly here does not fix any of those bugs; it makes
    // the next one diagnosable. An exit teardown asked for is not one of them: see
    // `unproductive_exit_failed`.
    if unproductive_exit_failed(PUSHED_ANY.load(Ordering::Relaxed), unsafe {
        crate::aq::aq_is_aborted(aq_p)
    }) {
        crate::player::log("ff: demux produced no access units — treating as a failure");
        SHARED.demux_failed.store(true, Ordering::Release);
    }
    crate::aq::aq_set_eof(aq_p);
    crate::aq::aq_set_eof(aqa_p); // EOF on the audio lane too
    crate::player::log("ff: demux ended");
}

// ---------------------------------------------------------------------------------------
#[cfg(test)]
#[path = "ff_test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "ff_packet_framing_tests.rs"]
mod packet_framing_tests;

#[cfg(test)]
#[path = "ff_source_teardown_tests.rs"]
mod source_teardown_tests;

#[cfg(test)]
#[path = "ff_redirect_tests.rs"]
mod redirect_tests;

#[cfg(test)]
#[path = "ff_late_audio_tests.rs"]
mod late_audio_tests;

#[cfg(test)]
#[path = "ff_eof_seek_tests.rs"]
mod eof_seek_tests;

#[cfg(test)]
#[path = "ff_open_seek_tests.rs"]
mod open_seek_tests;

#[cfg(test)]
#[path = "ff_hls_tests.rs"]
mod hls_tests;

#[cfg(test)]
#[path = "ff_adec_tests.rs"]
mod adec_tests;

#[cfg(test)]
#[path = "ff_subdec_tests.rs"]
mod subdec_tests;
