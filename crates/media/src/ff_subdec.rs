//! **Subtitle decode** for the demuxer: the subtitle streams the app can render, each one's
//! packets decoded into [`crate::subtitles::Cue`]s of its track.
//!
//! The bitmap subtitles, by evidence: DVB, what a DVB restream carries in the MPEG-TS this
//! demuxer already reads, and PGS (Blu-ray's presentation graphics), what a Matroska film carries.
//! Both run on the video's own PTS clock, FFmpeg's `dvbsub` and `pgssub` decoders are native and
//! LGPL, and as bitmaps they need no font. Teletext needs libzvbi, which the bundled build does
//! not have; WebVTT arrives as a separate HLS rendition the HLS client does not fetch; the text
//! tracks of a film (SRT, ASS) would need a font renderer. None is listed, so the menu offers
//! only what shows. A build that lacks one of the two decoders still lists the other's tracks.
//!
//! Every listed track is read and decoded, picked or not (see `crate::subtitles` for why), so a
//! pick or turning subtitles off never reloads the playback and never waits for the demuxer's
//! read-ahead. Subtitle streams the app cannot draw stay discarded like every other track the
//! player does not play (`discard_for`).
//!
//! **PGS timing.** A PGS display set has no end time: it shows until the next one, which may be
//! empty (a clear), so its cue ends with the track's next cue. It shows from its presentation
//! segment's time, which the cue keeps even when the segments came in packets of their own.
//!
//! Every packet, rect and palette is network data. A packet over [`MAX_PACKET`] is not decoded,
//! what each decoder would keep between packets is counted before it is given one
//! ([`RegionBudget`], [`ObjectBudget`]), and a picture is measured from its rects' own fields
//! before anything is allocated ([`picture`]), against the caps in [`crate::subtitles`].

use super::*;
use crate::subtitles::{self, Cue, Rect, Track, MAX_CUE_PIXELS, MAX_RECTS, MAX_SIDE, MAX_TRACKS};
use std::ffi::CStr;

// The entry points the demuxer's tables (ff.rs) do not bind, from the same library by the same
// candidate names (see `adec`).
tv_base::dynlib! {
    subdec_avcodec: ["libavcodec-wi.so.63", "libavcodec-wi.63.dylib"] {
        fn avcodec_descriptor_get_by_name(name: *const c_char) -> *const Descriptor;
        fn avcodec_find_decoder(id: c_int) -> *const AVCodec;
        fn avcodec_parameters_to_context(ctx: *mut AVCodecContext, par: *const AVCodecParameters) -> c_int;
        fn avcodec_decode_subtitle2(ctx: *mut AVCodecContext, sub: *mut AVSubtitle, got: *mut c_int, pkt: *const AVPacket) -> c_int;
        fn avsubtitle_free(sub: *mut AVSubtitle);
    }
}

/// The subtitle codecs drawn, by FFmpeg name, in [`Kind`] order; a stream of any other is not
/// listed.
pub const RENDERED: &[&str] = &["dvb_subtitle", "hdmv_pgs_subtitle"];
/// The largest packet decoded. A DVB subtitle PES is at most 64 KiB, and a PGS display set of
/// subtitle lines a few tens; anything far past that is not a subtitle.
pub const MAX_PACKET: usize = 256 << 10;
/// The canvas when the decoder names none: ETSI EN 300 743's default display.
const DVB_CANVAS: (u32, u32) = (720, 576);
/// The canvas when a PGS decoder names none: a Blu-ray's video.
const PGS_CANVAS: (u32, u32) = (1920, 1080);

const AVMEDIA_TYPE_SUBTITLE: c_int = 3;
/// `enum AVSubtitleType`: `SUBTITLE_BITMAP`.
const SUBTITLE_BITMAP: c_int = 1;
const AV_DISPOSITION_FORCED: c_int = 1 << 6;
const AV_DISPOSITION_HEARING_IMPAIRED: c_int = 1 << 7;

/// Bit `i` set: the bundled libavcodec decodes [`RENDERED`]`[i]`. Zero lists no subtitles.
pub(super) static PRESENT: AtomicU32 = AtomicU32::new(0);

/// Which of the [`RENDERED`] codecs a stream is: what its decoder keeps, its default canvas, when
/// a display set shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dvb,
    Pgs,
}

impl Kind {
    const ALL: [Kind; 2] = [Kind::Dvb, Kind::Pgs];

    /// The kind of the codec FFmpeg names `codec`, if it is drawn at all.
    pub fn of(codec: &str) -> Option<Kind> {
        RENDERED.iter().position(|c| *c == codec).map(|i| Kind::ALL[i])
    }

    /// Its bit in [`PRESENT`].
    pub fn bit(self) -> u32 {
        1 << self as u32
    }

    /// The canvas a display set is placed on when the decoder names none.
    fn default_canvas(self) -> (u32, u32) {
        match self {
            Kind::Dvb => DVB_CANVAS,
            Kind::Pgs => PGS_CANVAS,
        }
    }

    /// Whether `canvas` has square pixels and stands for the whole frame the video was cut from.
    /// A Blu-ray's HD graphics do (1280x720 and up; its SD ones are anamorphic, like every DVB
    /// display).
    pub fn square(self, canvas: (u32, u32)) -> bool {
        self == Kind::Pgs && canvas.0 >= 1280
    }
}

/// `AVCodecDescriptor`: only `id`, its first field, is read. Library-owned, never constructed.
#[repr(C)]
pub struct Descriptor {
    id: c_int,
}

/// `AVSubtitle` (libavcodec/avcodec.h, FFmpeg 9.0): one decoded display set. The decoder writes
/// into this crate's copy, so its size is pinned as well as its fields: 32 bytes on both widths.
#[repr(C)]
pub struct AVSubtitle {
    pub format: u16,
    pub start_display_time: u32,
    pub end_display_time: u32,
    pub num_rects: c_uint,
    pub rects: *mut *mut AVSubtitleRect,
    pub pts: i64,
}

/// `AVSubtitleRect` (FFmpeg 9.0). Library-allocated and only read. `flags` comes before `type`,
/// and nothing past `type` is read.
#[repr(C)]
pub struct AVSubtitleRect {
    pub x: c_int,
    pub y: c_int,
    pub w: c_int,
    pub h: c_int,
    pub nb_colors: c_int,
    /// `data[0]`: PAL8 indices; `data[1]`: the palette, 0xAARRGGBB words.
    pub data: [*mut u8; 4],
    pub linesize: [c_int; 4],
    pub flags: c_int,
    pub type_: c_int,
    pub text: *mut c_char,
    pub ass: *mut c_char,
}

// Checked with `offsetof` against the 9.0.2 headers for armv7 and the 64-bit host (the same
// numbers StremioNative's `ci/ffabi-assert.c` pins).
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<AVSubtitle>() == 32);
    assert!(offset_of!(AVSubtitle, end_display_time) == 8);
    assert!(offset_of!(AVSubtitle, num_rects) == 12);
    assert!(offset_of!(AVSubtitle, rects) == 16);
    assert!(offset_of!(AVSubtitle, pts) == 24);
    assert!(offset_of!(AVSubtitleRect, nb_colors) == 16);
};
#[cfg(target_pointer_width = "32")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<AVSubtitleRect>() == 68);
    assert!(offset_of!(AVSubtitleRect, data) == 20);
    assert!(offset_of!(AVSubtitleRect, linesize) == 36);
    assert!(offset_of!(AVSubtitleRect, type_) == 56);
};
#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<AVSubtitleRect>() == 96);
    assert!(offset_of!(AVSubtitleRect, data) == 24);
    assert!(offset_of!(AVSubtitleRect, linesize) == 56);
    assert!(offset_of!(AVSubtitleRect, type_) == 76);
};

const _: () = assert!(Kind::ALL.len() == RENDERED.len());

/// The decoders `avformat_find_stream_info` may open: every one the app bundles (xtask's
/// `--enable-decoder`) but `pgssub`.
pub const STREAM_INFO_DECODERS: &CStr = c"mp2,mp3,aac,aac_latm,ac3,eac3,dca,flac,opus,vorbis,dvbsub";

/// Keep `avformat_find_stream_info` from decoding PGS on `fmt`, a context not yet opened.
///
/// It decodes a PGS stream's packets until one names the video size (`has_codec_parameters`
/// wants it), with a decoder of its own that [`ObjectBudget`] never sees: a film whose first
/// display set has 64 objects each declaring a 16 MiB RLE (a 2 KB block) has it allocate and zero
/// 1.1 GB, measured on the host build. Kept off it, the size stays unknown, as with a build
/// without `pgssub`, and nothing else changes: the app's own decoder has a context of its own,
/// out of the list's reach, and reads the stream under the budget.
pub(super) unsafe fn guard_stream_info(fmt: *mut AVFormatContext) {
    if av_opt_set(fmt.cast(), c"codec_whitelist".as_ptr(), STREAM_INFO_DECODERS.as_ptr(), 0) < 0 {
        crate::player::log("ff: subtitles: the stream-info decoders could not be restricted");
    }
}

/// Bind the table from the app directory once the bundled FFmpeg passed its major check, and ask
/// for the decoders: a build without `dvbsub` or `pgssub` lists none of that codec's tracks rather
/// than failing an open, and still lists the other's.
pub(super) fn detect() -> u32 {
    let present = if ABI_OK.load(Ordering::Acquire) && load_from(tv_base::paths::app_dir()) { decoders_present() } else { 0 };
    PRESENT.store(present, Ordering::Release);
    let on = |kind: Kind| if present & kind.bit() != 0 { "available" } else { "UNAVAILABLE" };
    crate::player::log(&format!("ff: DVB subtitles {}, PGS subtitles {}", on(Kind::Dvb), on(Kind::Pgs)));
    present
}

/// Bind the table from `dir`, where the demuxer's own tables already loaded the same library.
pub(super) fn load_from(dir: &std::path::Path) -> bool {
    match subdec_avcodec::load(Some(dir)) {
        tv_base::dynlib::Loaded::Ok(_) => true,
        tv_base::dynlib::Loaded::NoLibrary => {
            crate::player::log("ff: subtitles: no avcodec in the app directory");
            false
        }
        tv_base::dynlib::Loaded::Incomplete(soname, n) => {
            crate::player::log(&format!("ff: subtitles: {soname} is missing {n} symbol(s)"));
            false
        }
    }
}

/// Which [`RENDERED`] codecs the loaded libavcodec decodes, as [`PRESENT`]'s bits. Only once
/// [`load_from`] bound it.
pub(super) fn decoders_present() -> u32 {
    Kind::ALL.iter().zip(RENDERED).fold(0, |bits, (kind, name)| {
        // SAFETY: the table is bound; `c` is NUL-terminated for the call, and a non-null
        // descriptor is a static FFmpeg table entry.
        let present = CString::new(*name).is_ok_and(|c| unsafe {
            let d = avcodec_descriptor_get_by_name(c.as_ptr());
            !d.is_null() && !avcodec_find_decoder((*d).id).is_null()
        });
        if !present {
            crate::player::log(&format!("ff: subtitles: the bundled libavcodec has no {name} decoder"));
        }
        if present {
            bits | kind.bit()
        } else {
            bits
        }
    })
}

/// The canvas the decoder placed its rects on, once it has decoded: `avcodec_parameters_from_context`
/// copies its width and height into the parameters this crate models, so no codec-context offset
/// is read. `default` when it names none, or a size no display has. (The PGS decoder takes it from
/// each presentation segment: the video size the graphics were authored on.)
unsafe fn canvas(ctx: *mut AVCodecContext, default: (u32, u32)) -> (u32, u32) {
    let par = avcodec_parameters_alloc();
    if par.is_null() {
        return default;
    }
    let wh = if avcodec_parameters_from_context(par, ctx) >= 0 { ((*par).width, (*par).height) } else { (0, 0) };
    let mut p = par;
    avcodec_parameters_free(&mut p);
    let side = |v: c_int| u32::try_from(v).ok().filter(|v| (160..=MAX_SIDE).contains(v));
    match (side(wh.0), side(wh.1)) {
        (Some(w), Some(h)) => (w, h),
        _ => default,
    }
}

/// The rects of a decoded display set as the store keeps them, or `None` for a picture this app
/// refuses: more than [`MAX_RECTS`] rects, a side past [`MAX_SIDE`], more than [`MAX_CUE_PIXELS`]
/// together, or fields that cannot describe a bitmap. Every rect is measured before the first one
/// is copied, so a refused picture allocates nothing; `usize` is 32 bits on the television, and no
/// product here can overflow it.
///
/// # Safety
/// `sub` is a display set the decoder filled and nobody has freed yet.
pub unsafe fn picture(sub: &AVSubtitle) -> Option<Vec<Rect>> {
    let n = sub.num_rects as usize;
    if n == 0 {
        return Some(Vec::new());
    }
    if n > MAX_RECTS || sub.rects.is_null() {
        return None;
    }
    let rects: Vec<&AVSubtitleRect> = (0..n).map(|i| (*sub.rects.add(i)).as_ref()).collect::<Option<_>>()?;
    let mut total = 0usize;
    for r in &rects {
        let side = |v: c_int| u32::try_from(v).ok().filter(|&v| v <= MAX_SIDE);
        let (w, h) = (side(r.w)?, side(r.h)?);
        side(r.x)?;
        side(r.y)?;
        if w == 0 || h == 0 {
            continue;
        }
        if r.type_ != SUBTITLE_BITMAP || r.data[0].is_null() || r.data[1].is_null() || r.linesize[0] < r.w {
            return None;
        }
        total += w as usize * h as usize;
        if total > MAX_CUE_PIXELS {
            return None;
        }
    }
    let mut out = Vec::with_capacity(n);
    for r in rects {
        let (w, h) = (r.w as usize, r.h as usize);
        if w == 0 || h == 0 {
            continue;
        }
        let mut index = Vec::with_capacity(w * h);
        for row in 0..h {
            index.extend_from_slice(std::slice::from_raw_parts(r.data[0].add(row * r.linesize[0] as usize), w));
        }
        // The decoder allocates a whole AVPALETTE_SIZE but fills only `nb_colors`: the rest reads
        // transparent rather than whatever is there.
        let colors = usize::try_from(r.nb_colors).unwrap_or(0).min(256);
        let pal = r.data[1] as *const u32;
        let mut palette = Box::new([[0u8; 4]; 256]);
        for (i, entry) in palette.iter_mut().enumerate().take(colors) {
            let p = pal.add(i).read_unaligned();
            *entry = [(p >> 16) as u8, (p >> 8) as u8, p as u8, (p >> 24) as u8];
        }
        out.push(Rect { x: r.x, y: r.y, w: w as u32, h: h as u32, index, palette });
    }
    Some(out)
}

/// The cue's span in demux time from the packet's `pts_ns` and the display set's own times (ms
/// after it). No end, one at or before the start, or `u32::MAX` (the PGS decoder's "none": the
/// next display set ends it) shows until the next cue.
pub fn span(pts_ns: i64, start_ms: u32, end_ms: u32) -> (i64, i64) {
    let start = pts_ns.saturating_add(i64::from(start_ms) * 1_000_000);
    let end = if end_ms > start_ms && end_ms != u32::MAX {
        pts_ns.saturating_add(i64::from(end_ms) * 1_000_000)
    } else {
        i64::MAX
    };
    (start, end)
}

/// The most bytes a decoder may hold at once in DVB regions or PGS objects: two full HD screens
/// of pixels.
pub const REGION_BUDGET: usize = 2 * MAX_CUE_PIXELS;
/// Bytes one object placement in a region composition costs the decoder: its display entry
/// (32 bytes on a 64-bit host) with the allocator's header and alignment.
pub const DISPLAY_COST: usize = 64;
/// A region composition's fixed part; each object placement after it takes at least 6 bytes.
const REGION_HEADER: usize = 10;

/// What the DVB decoder keeps between packets, measured before it is asked to keep it.
///
/// `dvbsubdec` holds one `width * height` byte buffer and one list of object placements per
/// region id (up to 256 ids). FFmpeg bounds one region by `max_pixels` and nothing bounds how many
/// there are, so 256 large regions sent over a few packets would hold hundreds of megabytes. Each
/// packet's segments are read here first (sync byte, type, page id, length, payload; EN 300 743
/// 7.2), and a packet that could take the total past [`REGION_BUDGET`] is refused, after which
/// the caller starts the decoder over and counts the packet alone.
///
/// The count is an upper bound, never a model of the decoder: a page that starts over (a mode
/// change or an acquisition point) does not empty it, because the decoder skips a page that
/// repeats its version or names another page id, and stops reading a packet at the first segment
/// it refuses, keeping every region. A region keeps the largest size any segment declared for it,
/// as a refused one (0 x 0, past FFmpeg's own cap) leaves the old buffer in place. Only a fresh
/// decoder, which holds nothing, starts the count over.
#[derive(Default)]
pub struct RegionBudget {
    sizes: Vec<(u8, usize)>,
    total: usize,
}

impl RegionBudget {
    /// Count the most `packet` could leave the decoder holding; `false` (nothing counted) when
    /// that is past the budget.
    pub fn admit(&mut self, packet: &[u8]) -> bool {
        let mut next = (self.sizes.clone(), self.total);
        let mut p = packet;
        while p.len() >= 6 && p[0] == 0x0f {
            let len = usize::from(u16::from_be_bytes([p[4], p[5]]));
            let Some(body) = p.get(6..6 + len) else { break };
            // Region composition: id, flags, width, height, ..., then the object placements.
            if p[1] == 0x11 && body.len() >= 6 {
                let id = body[0];
                let w = usize::from(u16::from_be_bytes([body[2], body[3]]));
                let h = usize::from(u16::from_be_bytes([body[4], body[5]]));
                let placements = body.len().saturating_sub(REGION_HEADER) / 6;
                // 65535 x 65535 still fits a 32-bit usize; a sum past it saturates.
                let size = (w * h).saturating_add(placements.saturating_mul(DISPLAY_COST));
                match next.0.iter_mut().find(|(i, _)| *i == id) {
                    Some(slot) if slot.1 >= size => {}
                    Some(slot) => {
                        next.1 = (next.1 - slot.1).saturating_add(size);
                        slot.1 = size;
                    }
                    None => {
                        next.0.push((id, size));
                        next.1 = next.1.saturating_add(size);
                    }
                }
                if next.1 > REGION_BUDGET {
                    return false;
                }
            }
            p = &p[6 + len..];
        }
        (self.sizes, self.total) = next;
        true
    }
}

/// PGS segment types (`pgssubdec.c`).
const PGS_OBJECT: u8 = 0x15;
const PGS_PRESENTATION: u8 = 0x16;
const PGS_END: u8 = 0x80;

/// The segments of a PGS packet as the decoder steps through them: type, 16-bit length, payload,
/// until the packet ends or a segment other than the end of the display set runs past it (the
/// decoder reads a header cut short from the zeroed padding after the packet). Each is
/// `(type, payload)`; an end segment's payload may be cut short.
pub fn pgs_segments(packet: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut p = packet;
    std::iter::from_fn(move || {
        let &kind = p.first()?;
        let at = |i: usize| p.get(i).copied().unwrap_or(0);
        let len = usize::from(u16::from_be_bytes([at(1), at(2)]));
        let rest = p.get(3..).unwrap_or_default();
        if p.len() < 3 || len > rest.len() {
            p = &[];
            return (kind == PGS_END).then_some((kind, rest));
        }
        p = &rest[len..];
        Some((kind, &rest[..len]))
    })
}

/// What the PGS decoder keeps between packets, measured before it is asked to keep it.
///
/// `pgssubdec` keeps the RLE data of up to 64 objects per epoch, each in a zeroed buffer of the
/// length its first object segment declares (24 bits: up to 16 MiB) whatever the bitmap's size,
/// and a buffer only grows. Each packet's segments are read here first ([`pgs_segments`]), and a
/// packet that could take the total past [`REGION_BUDGET`] is refused, after which the caller
/// starts the decoder over and counts the packet alone.
///
/// The count is an upper bound: each object id keeps the largest buffer declared for it. It
/// starts over only where the decoder is sure to have freed every object: at a packet that opens
/// with a presentation segment starting an epoch or an acquisition point, of a video size the
/// decoder takes (`ff_set_dimensions` against `max_pixels`, which comes before the flush). A
/// Blu-ray stream usually starts an epoch with each new subtitle, so a film does not run into the
/// budget by its length alone; one that does only loses the objects of the epoch under way.
#[derive(Default)]
pub struct ObjectBudget {
    sizes: Vec<(u16, usize)>,
    total: usize,
}

impl ObjectBudget {
    /// Count the most `packet` could leave the decoder holding; `false` (nothing counted) when
    /// that is past the budget.
    pub fn admit(&mut self, packet: &[u8]) -> bool {
        let mut next = (self.sizes.clone(), self.total);
        for (i, (kind, body)) in pgs_segments(packet).enumerate() {
            match kind {
                // Video width and height, frame rate, composition number, then the state.
                PGS_PRESENTATION if i == 0 && body.len() >= 8 && body[7] >> 6 != 0 => {
                    let w = usize::from(u16::from_be_bytes([body[0], body[1]]));
                    let h = usize::from(u16::from_be_bytes([body[2], body[3]]));
                    // `av_image_check_size2`'s stride bound always holds within MAX_CUE_PIXELS.
                    if w > 0 && h > 0 && w * h <= MAX_CUE_PIXELS {
                        next = (Vec::new(), 0);
                    }
                }
                // Object id, version, sequence flags (0x80: the first of the object's segments),
                // then the declared length, which counts the 4 bytes of width and height.
                PGS_OBJECT if body.len() >= 12 && body[3] & 0x80 != 0 => {
                    let id = u16::from_be_bytes([body[0], body[1]]);
                    let rle = u32::from_be_bytes([0, body[4], body[5], body[6]]).wrapping_sub(4) as usize;
                    // `av_fast_mallocz` adds the padding (64), a sixteenth and 32 bytes; a length
                    // that wrapped is past the budget, as the decoder's allocation would fail.
                    let size = rle.saturating_add(rle / 16).saturating_add(100);
                    match next.0.iter_mut().find(|(i, _)| *i == id) {
                        Some(slot) if slot.1 >= size => {}
                        Some(slot) => {
                            next.1 = (next.1 - slot.1).saturating_add(size);
                            slot.1 = size;
                        }
                        None => {
                            next.0.push((id, size));
                            next.1 = next.1.saturating_add(size);
                        }
                    }
                    if next.1 > REGION_BUDGET {
                        return false;
                    }
                }
                _ => {}
            }
        }
        (self.sizes, self.total) = next;
        true
    }
}

/// What one decoder keeps between packets, counted by its codec's own rules.
enum Budget {
    Regions(RegionBudget),
    Objects(ObjectBudget),
}

impl Budget {
    fn new(kind: Kind) -> Budget {
        match kind {
            Kind::Dvb => Budget::Regions(RegionBudget::default()),
            Kind::Pgs => Budget::Objects(ObjectBudget::default()),
        }
    }

    fn admit(&mut self, packet: &[u8]) -> bool {
        match self {
            Budget::Regions(b) => b.admit(packet),
            Budget::Objects(b) => b.admit(packet),
        }
    }
}

/// One open subtitle decoder, for one stream of the open input.
struct Decoder {
    ctx: *mut AVCodecContext,
    st: *mut AVStream,
    index: c_int,
    kind: Kind,
    budget: Budget,
    /// PGS: the demux time of the last presentation segment given to the decoder, which the
    /// display set it opens shows from (`pgssubdec` keeps it the same way, in `sub->pts`, but in
    /// a time base the context is not told). Taken by the end of the display set.
    presented_ns: Option<i64>,
    /// Each note logged once per decoder: the first picture, a packet or picture refused.
    first_logged: bool,
    refused_logged: bool,
}

impl Decoder {
    /// Open the `kind` decoder for stream `st` (index `index`); `None` when the build has none or
    /// it refuses the stream's parameters.
    unsafe fn open(st: *mut AVStream, index: c_int, kind: Kind) -> Option<Decoder> {
        let cp = stream_codecpar(st);
        let codec = avcodec_find_decoder((*cp).codec_id);
        if codec.is_null() {
            return None;
        }
        let mut ctx = avcodec_alloc_context3(codec);
        if ctx.is_null() {
            return None;
        }
        // The parameters carry the extradata: the DVB composition and ancillary page ids. One DVB
        // region, or a PGS video size and so each of its objects, is bounded by `max_pixels`
        // (FFmpeg's default is INT_MAX); how many the decoder keeps, by `budget`.
        if avcodec_parameters_to_context(ctx, cp) < 0
            || av_opt_set_int(ctx as *mut c_void, c"max_pixels".as_ptr(), MAX_CUE_PIXELS as i64, 0) < 0
            || avcodec_open2(ctx, codec, std::ptr::null_mut()) < 0
        {
            avcodec_free_context(&mut ctx);
            crate::player::log(&format!("ff: subtitles: the decoder refused stream #{index}"));
            return None;
        }
        Some(Decoder {
            ctx,
            st,
            index,
            kind,
            budget: Budget::new(kind),
            presented_ns: None,
            first_logged: false,
            refused_logged: false,
        })
    }

    /// Log `what` the first time anything on this stream is refused.
    fn refused(&mut self, what: &str) {
        if !std::mem::replace(&mut self.refused_logged, true) {
            crate::player::log(&format!("ff: subtitles: {what} on #{} (further ones not logged)", self.index));
        }
    }

    /// Decode one packet of the stream into a cue, if it completes a display set.
    unsafe fn decode(&mut self, pkt: *const AVPacket) -> Option<Cue> {
        let size = usize::try_from((*pkt).size).unwrap_or(0);
        if size == 0 || (*pkt).data.is_null() {
            return None;
        }
        if size > MAX_PACKET {
            self.refused(&format!("a {size} byte packet is not decoded"));
            return None;
        }
        // A display set with no time cannot be placed against the picture.
        let pts = pts_ns_opt(pkt, self.st)?;
        let packet = std::slice::from_raw_parts((*pkt).data, size);
        if !self.budget.admit(packet) {
            // Start over: a fresh decoder holds no regions or objects. A packet past the budget
            // even then is not decoded.
            self.refused("what the decoder would keep is past the budget; it starts over");
            let fresh = Decoder::open(self.st, self.index, self.kind)?;
            let logged = (self.first_logged, true);
            *self = fresh;
            (self.first_logged, self.refused_logged) = logged;
            if !self.budget.admit(packet) {
                return None;
            }
        }
        // A PGS display set shows from its presentation segment, which may have come alone.
        let mut ends = false;
        if self.kind == Kind::Pgs {
            for (kind, _) in pgs_segments(packet) {
                match kind {
                    PGS_PRESENTATION => self.presented_ns = Some(pts),
                    PGS_END => ends = true,
                    _ => {}
                }
            }
        }
        let shown_from = if ends { self.presented_ns.take() } else { None }.unwrap_or(pts);
        let mut sub: AVSubtitle = std::mem::zeroed();
        let mut got: c_int = 0;
        // A failed decode frees what it filled; a packet that completes nothing is freed here.
        if avcodec_decode_subtitle2(self.ctx, &mut sub, &mut got, pkt) < 0 {
            return None;
        }
        if got == 0 {
            avsubtitle_free(&mut sub);
            return None;
        }
        let rects = picture(&sub);
        let (start_ns, end_ns) = span(shown_from, sub.start_display_time, sub.end_display_time);
        avsubtitle_free(&mut sub);
        let Some(rects) = rects else {
            self.refused("a picture past the caps was dropped");
            return None;
        };
        let canvas = canvas(self.ctx, self.kind.default_canvas());
        if !rects.is_empty() && !std::mem::replace(&mut self.first_logged, true) {
            crate::player::log(&format!(
                "ff: subtitles: first picture on #{} ({} rects, canvas {}x{})",
                self.index,
                rects.len(),
                canvas.0,
                canvas.1
            ));
        }
        let square = self.kind.square(canvas);
        Some(Cue { track: 0, start_ns, end_ns, canvas, square, rects, serial: 0 })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: allocated by `open`, freed once here.
        unsafe { avcodec_free_context(&mut self.ctx) }
    }
}

/// The subtitle side of one open input: a decoder for each listed track, in track order. Lives on
/// the demux thread for as long as the input is open.
pub(super) struct Reader {
    decoders: Vec<Decoder>,
}

impl Reader {
    /// List the input's renderable subtitle streams (at most [`MAX_TRACKS`]), publish them as the
    /// playback's tracks (which drops every cue), lift their discard and open their decoders. Call
    /// after the discard flags are set.
    ///
    /// # Safety
    /// `fmt` is an open input whose streams were read (`avformat_find_stream_info`).
    pub(super) unsafe fn open(fmt: *mut AVFormatContext) -> Reader {
        let mut decoders = Vec::new();
        let mut tracks = Vec::new();
        let present = PRESENT.load(Ordering::Acquire);
        if present != 0 {
            for i in 0..(*fmt).nb_streams as c_int {
                if tracks.len() == MAX_TRACKS {
                    break;
                }
                let st = *(*fmt).streams.add(i as usize);
                let cp = stream_codecpar(st);
                if (*cp).codec_type != AVMEDIA_TYPE_SUBTITLE {
                    continue;
                }
                let codec = std::ffi::CStr::from_ptr(avcodec_get_name((*cp).codec_id)).to_string_lossy().into_owned();
                let Some(kind) = Kind::of(&codec).filter(|k| present & k.bit() != 0) else { continue };
                let Some(decoder) = Decoder::open(st, i, kind) else { continue };
                set_stream_discard(st, AVDISCARD_DEFAULT);
                let disposition = *((st as *const u8).add(OFF_STREAM_DISPOSITION) as *const c_int);
                decoders.push(decoder);
                tracks.push(Track {
                    language: stream_language(st).unwrap_or_default(),
                    codec,
                    hearing_impaired: disposition & AV_DISPOSITION_HEARING_IMPAIRED != 0,
                    forced: disposition & AV_DISPOSITION_FORCED != 0,
                });
            }
        }
        if !tracks.is_empty() {
            crate::player::log(&format!("ff: {} subtitle tracks", tracks.len()));
        }
        subtitles::opened(tracks);
        Reader { decoders }
    }

    /// The listed track stream `index` is, if it is one.
    pub(super) fn track_of(&self, index: c_int) -> Option<usize> {
        self.decoders.iter().position(|d| d.index == index)
    }

    /// Decode a packet of listed track `track`; a finished display set goes to the store.
    ///
    /// # Safety
    /// `pkt` is a packet of that track's stream ([`Reader::track_of`]), not yet unreferenced.
    pub(super) unsafe fn decode(&mut self, track: usize, pkt: *const AVPacket) {
        if let Some(mut cue) = self.decoders.get_mut(track).and_then(|d| d.decode(pkt)) {
            cue.track = track;
            subtitles::push(cue);
        }
    }

    /// A seek: start every decoder over (their regions and objects belong to the old position)
    /// and drop every cue. A decoder that does not reopen keeps its old state.
    ///
    /// # Safety
    /// The input [`Reader::open`] was given is still open.
    pub(super) unsafe fn seeked(&mut self) {
        subtitles::flush();
        for d in &mut self.decoders {
            if let Some(fresh) = Decoder::open(d.st, d.index, d.kind) {
                *d = fresh;
            }
        }
    }
}
