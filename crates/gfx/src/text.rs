//! SDL2_ttf text rendering (was src/text.c): font cache + glyph-texture LRU +
//! draw_text. Main-thread only (all GL), so the caches are plain statics (no
//! locking). Uses gfx's link_program/use_prog (crate path). Mostly GL/TTF FFI -
//! the retui text backend.
//!
//! The glyph cache is an LRU **with a hot window** since phase 11 (spec §8.1): an entry drawn
//! within the last eight frames is not evicted while any colder slot exists, and the case where
//! every slot is hot gives way rather than refusing - counted, not silent. See [`evict_slot`],
//! [`begin_frame`] and [`take_evicted_hot`].
//!
//! # One face, and what it cannot draw
//!
//! Every string renders in Inter (`appfont.ttf` / `appfont-bold.ttf`), whose 2853 codepoints
//! cover Latin, Cyrillic and Greek: the languages of the channel names and programme guides this
//! player is built for. SDL2_ttf 2.0.x has no fallback-font API (one `TTF_Font` is one face), so a
//! character Inter lacks draws as its .notdef box, the honest tofu. There is deliberately no
//! fallback chain: a CJK face would add 13 MB to the package for scripts no playlist here carries.
//!
//! **Emoji are respelled, then dropped.** Playlist text sometimes carries them (a group named
//! "U+26BD Sport"). The face has no shaper and no pictographs, so before a string is measured or
//! rendered ([`simplified_emoji`], `fontcov::simplify_emoji`) a flag becomes its region code, a
//! ZWJ sequence its first pictograph, a keycap its digit; VS15/VS16, skin tones and tags go, and a
//! pictograph Inter lacks is dropped with the space around it rather than boxed. Measuring and
//! drawing both start there, so widths, elision, the marquee and both caches agree.
//!
//! **RTL is out of scope.** Inter has no Hebrew or Arabic, and that is deliberate twice over:
//! coverage is not support. Arabic needs joining and contextual shaping, Hebrew needs bidi
//! reordering, and neither exists anywhere in this module: `TTF_RenderUTF8_Blended` in SDL2_ttf
//! 2.0.x is a left-to-right advance loop with no shaper behind it. Adding a face without a shaper
//! would turn tofu into fluent-looking WRONG text, which is harder to notice and worse to ship.
//! `fontcov`'s `rtl_is_out_of_scope_and_stays_that_way` asserts the absence so this stays a
//! decision rather than an oversight.
use std::collections::hash_map::DefaultHasher;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::ffi::{CStr, CString};
use std::hash::{Hash, Hasher};
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr::{addr_of, addr_of_mut};
use std::rc::Rc;

use tv_base::surface::{LOGICAL_H as SCR_H, LOGICAL_W as SCR_W};
use crate::overdraw::{gate, Class};

/// Last resort only. Reaching this is a DEFECT, not a graceful degradation - see `font_at`.
const DROIDSANS: &CStr = c"/usr/share/fonts/DroidSans.ttf";

/// The shipped fonts, addressed relative to wherever the ipk actually got installed
/// (`tv_base::paths` explains why this cannot be a literal). Built once; `TTF_OpenFont` wants a
/// `*const c_char` that outlives the call, so these are leaked `CString`s rather than temporaries.
fn app_font(bold: bool) -> &'static CStr {
    static REG: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    static BOLD: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    let slot = if bold { &BOLD } else { &REG };
    slot.get_or_init(|| {
        let name = if bold {
            "appfont-bold.ttf"
        } else {
            "appfont.ttf"
        };
        CString::new(
            tv_base::paths::in_app_dir(name)
                .into_os_string()
                .into_encoded_bytes(),
        )
        .unwrap_or_else(|_| CString::new("/nonexistent").expect("literal has no NUL"))
    })
}

const VS_TEXT: &CStr = crate::gfx::glsl!("shaders/vs_text.vert");
const FS_TEXT: &CStr = crate::gfx::glsl!("shaders/fs_text.frag");
const FS_TEXT_FADE: &CStr = crate::gfx::glsl!("shaders/fs_text_fade.frag");
// GL enums
const GL_TEXTURE_2D: c_uint = 0x0DE1;
const GL_TRIANGLE_STRIP: c_uint = 0x0005;
const TTF_STYLE_BOLD: c_int = 0x01;
const TTF_HINTING_LIGHT: c_int = 1;

enum TtfFont {}

#[repr(C)]
struct SdlSurface {
    flags: u32,
    format: *mut c_void,
    w: c_int,
    h: c_int,
    pitch: c_int,
    pixels: *mut c_void,
    // remaining fields (userdata, clip_rect, ...) unused
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SdlColor {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

// SDL2_ttf is declared here rather than routed through `dynlib!` because the SONAME holds still,
// which is that module's actual criterion: `libSDL2_ttf-2.0.so.0` on all 14 firmware inventories
// (`tools/fwcompat.py --inventory libSDL2_ttf`), unlike libcurl's `.so.5` -> `.so.4`.
//
// **READ THIS BEFORE ADDING ANOTHER `TTF_*` SYMBOL.** For this one library, "it compiles and
// links" is evidence of NOTHING. The repo pins SDL2's *core* headers in `include/` but not
// SDL_ttf's, so a TTF call compiles against the NDK's header and links against the NDK's
// `libSDL2_ttf-2.0.so.0.18.0` (2.0.18), while every firmware ships 2.0.10 or 2.0.14. The NDK
// exports 75 `TTF_*` functions; the televisions export 47 or 48, and 27 of them link cleanly here
// and exist on no supported release (`TTF_SetFontSize`, `TTF_MeasureUTF8`, `TTF_SetDirection`,
// every `*32` and `*_Wrapped`, ...). There is no local signal at all: it builds, `make check`
// passes, and the process dies at the first call on every set. Grade a new symbol with
// `tools/fwcompat.py --lib libSDL2_ttf-2.0.so.0 --grep '^NAME$'` before writing the call.
//
// That is exactly the trap the coverage read dodged. The query one would reach for,
// `TTF_GlyphIsProvided`, takes a `Uint16`, so it cannot be asked about anything above U+FFFF; and
// `TTF_GlyphIsProvided32` is in the NDK header, exported by the NDK `.so`, and present on none of
// the 14 (`--grep '32$'` returns zero matches on every release). So `tv_base::fontcov` reads the
// cmap from the file instead, which also makes coverage a host question rather than a device one.
extern "C" {
    fn TTF_Init() -> c_int;
    fn TTF_OpenFont(file: *const c_char, ptsize: c_int) -> *mut TtfFont;
    fn TTF_RenderUTF8_Blended(
        font: *mut TtfFont,
        text: *const c_char,
        fg: SdlColor,
    ) -> *mut SdlSurface;
    /// Metrics-only sizing - the same numbers `TTF_RenderUTF8_Blended` would produce, with no
    /// rasterization and no surface. Present in the TV's own `libSDL2_ttf-2.0.so.0.14.0` (verified
    /// on device 2026-07-29 by dumping its dynamic symbols), and in the NDK sysroot copy we link
    /// against, so this resolves for real at link time like every other TTF symbol here.
    fn TTF_SizeUTF8(font: *mut TtfFont, text: *const c_char, w: *mut c_int, h: *mut c_int)
        -> c_int;
    fn TTF_SetFontStyle(font: *mut TtfFont, style: c_int);
    fn TTF_SetFontHinting(font: *mut TtfFont, hinting: c_int);
    fn SDL_FreeSurface(surf: *mut SdlSurface);
    // GLES2
    fn glGetUniformLocation(program: c_uint, name: *const c_char) -> c_int;
    fn glBindTexture(target: c_uint, texture: c_uint);
    fn glDeleteTextures(n: c_int, textures: *const c_uint);
    fn glUniform2f(loc: c_int, x: f32, y: f32);
    fn glUniform4fv(loc: c_int, count: c_int, value: *const f32);
    fn glUniform1i(loc: c_int, x: c_int);
    fn glUniform4f(loc: c_int, x: f32, y: f32, z: f32, w: f32);
    fn glDrawArrays(mode: c_uint, first: c_int, count: c_int);
}

static mut TPROG: c_uint = 0;
static mut TL_RECT: c_int = 0;
static mut TL_SCREEN: c_int = 0;
static mut TL_COL: c_int = 0;
static mut TL_TEX: c_int = 0;
// the fade program (draw_text_fade only) + its own uniform locations
static mut TPROGF: c_uint = 0;
static mut TLF_RECT: c_int = 0;
static mut TLF_SCREEN: c_int = 0;
static mut TLF_COL: c_int = 0;
static mut TLF_TEX: c_int = 0;
static mut TLF_FADE: c_int = 0;
/// **Vertical edge-fade bands, in logical screen y** (the same absolute space `u_trect`'s `y`
/// lives in) - the counterpart of [`TLF_FADE`]'s horizontal one. `(0.0, 0.0)` (the default, since
/// `y1 > y0` is then false) means "off": the shader's gate skips the multiply rather than relying
/// on a sentinel width like the horizontal band's, because a vertical band has no natural "past
/// the string" edge to sentinel against. See [`draw_text_fade`] and `ui::widgets::edge_feather`'s
/// replacement note in `screens::person_bio` for why this exists - a scrolling viewport's edge used to
/// be an OPAQUE panel-coloured gradient painted over the glass, which read as a grey band rather
/// than the text dissolving.
static mut TLF_VTOP: c_int = 0;
static mut TLF_VBOT: c_int = 0;
/// One opened face per pixel size, per array. 80 covers the whole `theme::size` ladder; the
/// simulator's supersampled rendering (`surface::render_scale`, up to 4x) rasterises at up to 4x it.
const FONT_SLOTS: usize = if cfg!(feature = "hostsim") { 320 } else { 80 };
static mut FONTS: [*mut TtfFont; FONT_SLOTS] = [std::ptr::null_mut(); FONT_SLOTS];
static mut FONTS_B: [*mut TtfFont; FONT_SLOTS] = [std::ptr::null_mut(); FONT_SLOTS];
static mut TEXT_OK: c_int = 0;

#[derive(Clone, Copy)]
struct TCacheEntry {
    s: [u8; 96], // key PREFIX (set_entry_key truncates to 95 bytes + NUL); klen holds the full length
    hash: u64,   // key hash (FULL string+sz+bold) - a cheap pre-compare before the byte memcmp
    // Full (untruncated) key length. Load-bearing for ≥96-byte strings: the stored key is a
    // truncated prefix, so a `prefix == full_probe` equality could NEVER match - every long
    // string (a Cyrillic synopsis line crosses 95 bytes at ~48 chars) re-rendered, re-uploaded
    // and evicted a slot EVERY FRAME. The predicate is hash + klen + prefix; the 64-bit SipHash
    // over the full string is the discriminator, the prefix/len compares are the sanity check.
    klen: u32,
    sz: c_int,
    bold: c_int,
    tex: c_uint,
    w: c_int,
    h: c_int,
    // vertical *ink* bounds within the h-tall texture: the first/last rows that actually contain
    // visible glyph pixels. Lets callers centre by what's drawn, not the font's line box.
    ink_t: c_int,
    ink_b: c_int,
    use_: c_uint,
    /// The [`TFRAME`] this entry was last drawn on - 0 for "never, this session".
    ///
    /// `use_` is a use ORDER and answers "least recently used"; this answers "is it ON SCREEN
    /// NOW", which is a different question and the only one the occupancy rule (§8.1) can be
    /// written against. Both are kept because both are asked: the hot window picks the CANDIDATES,
    /// `use_` picks between them.
    last_frame: c_uint,
}
impl TCacheEntry {
    const ZERO: TCacheEntry = TCacheEntry {
        s: [0; 96],
        hash: 0,
        klen: 0,
        sz: 0,
        bold: 0,
        tex: 0,
        w: 0,
        h: 0,
        ink_t: 0,
        ink_b: 0,
        use_: 0,
        last_frame: 0,
    };
}

/// the cache key hash: string bytes + size + bold. The per-frame lookup is a linear scan of all
/// 160 slots (~80-100 text ops/frame on the detail page); comparing one u64 first makes the
/// non-matching 159 probes a single integer compare instead of a size/bold/byte-key memcmp each.
fn key_hash(s: &[u8], sz: c_int, bold: c_int) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    sz.hash(&mut h);
    bold.hash(&mut h);
    h.finish()
}
// Big enough to hold every distinct string visible in one frame WITHOUT eviction. The detail About
// panel (~30 runs) + Cast row (~14) + episodes/related/hero titles push well past ~48; at that point
// an LRU thrashes - each frame re-renders dozens of lines via TTF (+ a full-surface ink scan) and
// re-uploads them, which showed up as ~22ms About / ~12ms Cast (sub-30fps scrolling into them). With
// headroom past the ~80 simultaneous worst case, stable text is a pure cache hit after first paint.
// The simulator's supersampled rendering keeps a second, `n`x texture per visible string beside
// the logical one layout measures, so it gets the room for both.
const TCACHE: usize = if cfg!(feature = "hostsim") { 480 } else { 160 };
static mut TCACHE_A: [TCacheEntry; TCACHE] = [TCacheEntry::ZERO; TCACHE];
static mut TCLOCK: c_uint = 0;

/// **The glyph cache's OCCUPANCY instruments** (spec §8.1), main-render-thread only like the cache
/// they describe.
///
/// [`TFRAME`] is a frame serial advanced by [`begin_frame`], [`TLIVE`] is how many distinct entries
/// have been touched since that call, and [`TEVICTED_HOT`] counts the evictions the hot rule could
/// not honour. They exist because the LRU alone cannot see the failure that matters here: a screen
/// with more distinct strings than the cache has slots evicts exactly the entries it is about to
/// ask for again, one `TTF_RenderUTF8_Blended` plus a full-surface ink scan plus an upload apiece,
/// every frame, for as long as it is on screen. That is invisible in `use_` and invisible in the
/// frame rate until it is already bad.
static mut TFRAME: c_uint = 0;
/// Has [`begin_frame`] ever been called? Until it has, [`TFRAME`] is 0, every entry's `last_frame`
/// reads as "this frame", and the hot rule would refuse every eviction while the counter counted
/// all of them - so the rule stays OFF and the cache behaves exactly as it did before phase 11.
/// **The one call site in the loop is wired at integration**; see [`begin_frame`].
static mut TFRAME_ARMED: bool = false;
static mut TLIVE: u32 = 0;
static mut TEVICTED_HOT: u32 = 0;

/// Leave this many slots outside a page prewarm. They are the moving window for chrome, a modal,
/// and strings which arrive after the recording pass (spec §8.1's occupancy bound).
const PREWARM_HEADROOM: usize = 32;

/// One cache key copied out of a recording painter. The C string passed to a painter is usually a
/// frame-local `CString`; owning the bytes here is what makes a request safe to drain on a later
/// dip frame.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WarmKey {
    bytes: Vec<u8>,
    sz: c_int,
    bold: c_int,
}

thread_local! {
    /// Main-render-thread only, like the glyph cache itself. A queue instead of eager work is the
    /// boundary that lets `PageDip` spend a fixed slice of each outgoing frame.
    static PREWARM: RefCell<VecDeque<WarmKey>> = const { RefCell::new(VecDeque::new()) };
    /// Strings of a page nobody is looking at yet ([`park_prewarm_as_background`]). Kept apart
    /// from [`PREWARM`] because that queue belongs to the live page: the draw empties it every
    /// frame it has no page warm (`ui::dispatch`), a held surface waits on it
    /// ([`prewarm_pending`]), and a new live walk replaces it. None of that may touch work
    /// nothing waits on, so it waits here and is drained in the time the live queue leaves each frame.
    static BACKGROUND: RefCell<VecDeque<WarmKey>> = const { RefCell::new(VecDeque::new()) };
    /// Which park the [`BACKGROUND`] queue belongs to: bumped by every
    /// [`park_prewarm_as_background`], compared by [`clear_background_prewarm_owned`].
    static BACKGROUND_OWNER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// This iteration's answer to "is recorded text still warming?", latched once before dispatch
    /// by [`latch_surface_text_pending`]. `None` outside the product loop (host tests that drive a
    /// dispatcher directly), where [`surface_text_pending`] reads the queue itself.
    static SURFACE_TEXT_PENDING: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    #[cfg(any(test, feature = "test-support"))]
    static PREWARMED_FOR_TEST: RefCell<Vec<WarmKey>> = const { RefCell::new(Vec::new()) };
    #[cfg(any(test, feature = "test-support"))]
    static CAPTURED_FOR_TEST: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// Every text run a recording painter (`ui::Painter::recording`) is handed while `f` runs on this
/// thread - whether or not the glyph cache or the prewarm queue would take it.
#[cfg(any(test, feature = "test-support"))]
pub fn capture_text_runs_for_test(f: impl FnOnce()) -> Vec<String> {
    CAPTURED_FOR_TEST.with(|c| *c.borrow_mut() = Some(Vec::new()));
    f();
    CAPTURED_FOR_TEST.with(|c| c.borrow_mut().take()).unwrap_or_default()
}

/// Does the resident cache already hold this exact key? Unlike `text_tex`, this does not touch the
/// LRU or the frame occupancy: merely observing a destination must not make it hot.
fn cache_has(s: &[u8], sz: c_int, bold: c_int) -> bool {
    let hash = key_hash(s, sz, bold);
    unsafe {
        (&*addr_of!(TCACHE_A)).iter().any(|e| {
            e.hash == hash
                && e.tex != 0
                && e.sz == sz
                && e.bold == bold
                && e.klen as usize == s.len()
                && s.starts_with(entry_key(e))
        })
    }
}

/// Record a text-cache miss from an off-screen painter. Duplicate draws of the same label collapse
/// to one job, and the queue is refused before it can consume the cache's 32-slot safety margin.
pub fn queue_prewarm(s: *const c_char, sz: c_int, bold: c_int) {
    if s.is_null() {
        return;
    }
    let bytes = unsafe { CStr::from_ptr(s).to_bytes() };
    #[cfg(any(test, feature = "test-support"))]
    CAPTURED_FOR_TEST.with(|c| {
        if let (Some(runs), false) = (c.borrow_mut().as_mut(), bytes.is_empty()) {
            runs.push(String::from_utf8_lossy(bytes).into_owned());
        }
    });
    if bytes.is_empty() || cache_has(bytes, sz, bold) {
        return;
    }
    PREWARM.with(|q| {
        let mut q = q.borrow_mut();
        if q.iter().any(|k| k.sz == sz && k.bold == bold && k.bytes == bytes) {
            return;
        }
        let ceiling = TCACHE.saturating_sub(PREWARM_HEADROOM);
        if live_this_frame() as usize + q.len() >= ceiling {
            return;
        }
        q.push_back(WarmKey { bytes: bytes.to_vec(), sz, bold });
    });
}

/// Drain jobs until the deadline reached by `now`. The clock is read before starting each next
/// item, so an indivisible TTF render may finish just beyond the deadline but no further render is
/// begun there. On the measured pages one item is ~1 ms; the caller's budget keeps
/// that bounded while the first item guarantees forward progress.
fn drain_budgeted<T>(
    jobs: &mut VecDeque<T>,
    budget_us: u64,
    mut now: impl FnMut() -> u64,
    mut run: impl FnMut(&T),
) -> usize {
    let start = now();
    let mut done = 0;
    while !jobs.is_empty() && (done == 0 || now().saturating_sub(start) < budget_us) {
        let job = jobs.pop_front().expect("checked non-empty");
        run(&job);
        done += 1;
    }
    done
}

/// Rasterise and upload recorded misses for one dip frame. Host tests deliberately substitute a
/// residency ledger: there is no GL context there, while the queue and deadline remain identical.
pub fn drain_prewarm(budget_us: u64, now: impl FnMut() -> u64) -> usize {
    // dump mode: drain to empty, whatever the CPU's speed (`crate::dump`)
    let budget_us = if crate::dump::armed() { u64::MAX } else { budget_us };
    PREWARM.with(|slot| {
        let mut jobs = std::mem::take(&mut *slot.borrow_mut());
        let done = drain_budgeted(&mut jobs, budget_us, now, rasterise_warm);
        *slot.borrow_mut() = jobs;
        done
    })
}

/// Rasterise and upload one queued key (host tests: record it as resident).
fn rasterise_warm(job: &WarmKey) {
    #[cfg(not(any(test, feature = "test-support")))]
    unsafe {
        if *addr_of!(TEXT_OK) != 0 {
            if let Ok(s) = CString::new(job.bytes.clone()) {
                let _ = text_tex(&job.bytes, s.as_ptr(), job.sz, job.bold);
            }
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    PREWARMED_FOR_TEST.with(|w| {
        let mut w = w.borrow_mut();
        if !w.contains(job) {
            w.push(job.clone());
        }
    });
}

/// Move everything a recording walk just queued into the background queue, replacing what was
/// there: the caller's walk is the page it now wants warm. Returns the queue's owner token; only
/// [`clear_background_prewarm_owned`] with that token clears it, so a menu that is still fading
/// out cannot wipe the queue of the one opened over it. A later park takes the queue AND the
/// ownership: the earlier owner's token goes stale.
pub fn park_prewarm_as_background() -> u64 {
    let jobs = PREWARM.with(|q| std::mem::take(&mut *q.borrow_mut()));
    BACKGROUND.with(|b| *b.borrow_mut() = jobs);
    BACKGROUND_OWNER.with(|o| {
        o.set(o.get() + 1);
        o.get()
    })
}

/// Rasterise background strings, oldest first, until the deadline `budget_us` after the first
/// read of `now` (the clock is read before each next string, so one string may finish past it, as
/// in [`drain_prewarm`]; the first string is always taken, so a caller that has budget left makes
/// progress). A string something else cached in the meantime is skipped without costing time.
/// Stops once this frame's occupancy ([`live_this_frame`]) plus the string about to be rasterised
/// would reach the same ceiling a recording walk is held to (`TCACHE - PREWARM_HEADROOM`): the
/// strings stay queued for a calmer frame instead of evicting what the frame is standing on.
pub fn drain_background_prewarm(budget_us: u64, now: impl FnMut() -> u64) -> usize {
    drain_background_prewarm_at(budget_us, now, || live_this_frame() as usize)
}

fn drain_background_prewarm_at(budget_us: u64, mut now: impl FnMut() -> u64, live: impl Fn() -> usize) -> usize {
    // dump mode: no time budget (the occupancy ceiling below is logical and still applies)
    let budget_us = if crate::dump::armed() { u64::MAX } else { budget_us };
    BACKGROUND.with(|b| {
        let mut done = 0;
        let ceiling = TCACHE.saturating_sub(PREWARM_HEADROOM);
        let start = now();
        loop {
            if live() + 1 >= ceiling || (done > 0 && now().saturating_sub(start) >= budget_us) {
                break;
            }
            let Some(job) = b.borrow_mut().pop_front() else { break };
            if !cache_has(&job.bytes, job.sz, job.bold) {
                rasterise_warm(&job);
                done += 1;
            }
        }
        done
    })
}

/// Background strings not yet rasterised.
pub fn background_prewarm_pending() -> bool {
    BACKGROUND.with(|b| !b.borrow().is_empty())
}

/// Drop the background queue: the page it was warming can no longer be shown.
pub fn clear_background_prewarm() {
    BACKGROUND.with(|b| b.borrow_mut().clear());
}

/// Drop the background queue only if `owner` (a [`park_prewarm_as_background`] token) still owns
/// it. A stale token - a newer park took the queue over - leaves it alone.
pub fn clear_background_prewarm_owned(owner: u64) {
    if BACKGROUND_OWNER.with(std::cell::Cell::get) == owner {
        clear_background_prewarm();
    }
}

/// Recorded text a prewarm pass has not yet rasterised. A held page image is not replaced by a
/// live capture while this is true (`ui::dispatch`'s quiescence predicate), so the capture frame
/// never pays for a page's newly landed strings all at once.
pub fn prewarm_pending() -> bool {
    PREWARM.with(|q| !q.borrow().is_empty())
}

/// Latch this iteration's text readiness for held surfaces (`PopoverMotion::tick`).
///
/// How many frames the queue takes to drain is wall-clock work: [`drain_prewarm`] spends a
/// microsecond budget, so a slower CPU rasterises fewer strings per frame and holds a surface
/// shut for more frames. That hold decides the frame a modal turns `Open`, which is logical state,
/// so the answer is an environmental observation like the page capture's GPU readiness: the
/// product loop samples the queue once before dispatch, lets the recorder record it or supply
/// the recorded one (`app::recorder::Recplay::capture_readiness`), and latches the result here.
/// The queue changes only inside an iteration's own phases - a panel's recording walk in
/// `update`, the budgeted drain on the presenting side before the draw, the draw's warm pass - and
/// a frame that does not present drains nothing, so the latch is the queue as that iteration's
/// springs saw it. In dump mode the latch is always `false` (see `crate::dump`).
pub fn latch_surface_text_pending(pending: bool) {
    // dump mode: forced false on the premise that the dump driver drains to empty before the
    // sample, so the sample is never an input (`crate::dump`); the product loop's order differs
    let pending = pending && !crate::dump::armed();
    SURFACE_TEXT_PENDING.with(|latch| latch.set(Some(pending)));
}

/// The text readiness a held surface waits on: this iteration's latched observation, or the live
/// queue when no product loop latched one.
pub fn surface_text_pending() -> bool {
    SURFACE_TEXT_PENDING.with(|latch| latch.get()).unwrap_or_else(prewarm_pending)
}

/// A transition ended or was replaced. Never carry its destination's work into an unrelated dip.
pub fn clear_prewarm() {
    PREWARM.with(|q| q.borrow_mut().clear());
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_prewarm_for_test() {
    clear_prewarm();
    clear_background_prewarm();
    SURFACE_TEXT_PENDING.with(|latch| latch.set(None));
    PREWARMED_FOR_TEST.with(|w| w.borrow_mut().clear());
}

/// How many distinct strings the host's stand-in rasteriser has made resident since the last reset:
/// the deterministic clock the drain-budget tests run on (one string is one unit of time).
#[cfg(any(test, feature = "test-support"))]
pub fn rasterised_for_test() -> u64 {
    PREWARMED_FOR_TEST.with(|w| w.borrow().len() as u64)
}

#[cfg(any(test, feature = "test-support"))]
pub fn prewarm_resident_any_size_for_test(bytes: &[u8]) -> bool {
    PREWARMED_FOR_TEST.with(|w| w.borrow().iter().any(|k| k.bytes == bytes))
}

#[cfg(any(test, feature = "test-support"))]
pub fn prewarm_resident_for_test(bytes: &[u8], sz: c_int, bold: c_int) -> bool {
    PREWARMED_FOR_TEST.with(|w| {
        w.borrow().iter().any(|k| k.bytes == bytes && k.sz == sz && k.bold == bold)
    })
}

/// Every rung of `theme::size` - the faces a page's layout is measured in.
const WARM_SIZES: [c_int; 9] = {
    use crate::gfx::tokens::size::*;
    [HERO, DISPLAY, TITLE, HEADLINE, BODY, LABEL, CAPTION, MICRO, DIAGNOSTIC]
};
/// Printable ASCII: the glyphs nearly every string the product measures is made of.
const WARM_GLYPHS: &[u8] =
    b" !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~";
/// Glyphs per warming job, so one job stays well inside an idle slice.
const WARM_CHUNK: usize = 19;
const WARM_CHUNKS: usize = WARM_GLYPHS.len().div_ceil(WARM_CHUNK);
const WARM_JOBS: usize = WARM_SIZES.len() * 2 * WARM_CHUNKS;

thread_local! {
    /// The next [`warm_job`] to run. Main-render-thread only, like the faces it warms.
    static WARM_NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Job `j`: a face (size, weight) and the glyph run to load its metrics for. The first chunk of a
/// face also opens it.
fn warm_job(j: usize) -> (c_int, c_int, &'static [u8]) {
    let chunk = j % WARM_CHUNKS;
    let face = j / WARM_CHUNKS;
    let lo = chunk * WARM_CHUNK;
    let hi = (lo + WARM_CHUNK).min(WARM_GLYPHS.len());
    (WARM_SIZES[face / 2], (face % 2) as c_int, &WARM_GLYPHS[lo..hi])
}

/// Run warming jobs from the cursor until `budget_us` has passed on `now` - at least one, like
/// [`drain_budgeted`]. Returns how many ran; 0 once every face is warm.
fn warm_step(budget_us: u64, mut now: impl FnMut() -> u64, mut run: impl FnMut(c_int, c_int, &[u8])) -> usize {
    WARM_NEXT.with(|next| {
        let start = now();
        let mut done = 0;
        while next.get() < WARM_JOBS && (done == 0 || now().saturating_sub(start) < budget_us) {
            let (sz, bold, glyphs) = warm_job(next.get());
            run(sz, bold, glyphs);
            next.set(next.get() + 1);
            done += 1;
        }
        done
    })
}

/// **Open every theme face and load its ASCII glyph metrics, a slice at a time, while the loop is
/// idle.** SDL_ttf opens a face on its first use at a size and loads each glyph's metrics on the
/// first string that contains it, so the first layout of a page in a size the session has not
/// used yet pays for both at once. Measured on the television: the first Detail page after boot
/// spent 23 ms in 176 width measurements and four face opens in ONE frame (the page's first
/// text-prewarm walk), and later visits spent under 1 ms. The metrics are per face and live as
/// long as it does, so this is paid once per process, on frames that were going to sleep.
pub fn warm_fonts_idle(budget_us: u64, now: impl FnMut() -> u64) -> usize {
    if unsafe { *addr_of!(TEXT_OK) } == 0 {
        return 0;
    }
    warm_step(budget_us, now, |sz, bold, glyphs| unsafe {
        let f = font_at(sz, bold);
        if f.is_null() {
            return;
        }
        if let Ok(run) = CString::new(glyphs) {
            let (mut w, mut h) = (0, 0);
            TTF_SizeUTF8(f, run.as_ptr(), &mut w, &mut h);
        }
    })
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_font_warm_for_test() {
    WARM_NEXT.with(|n| n.set(0));
}

/// An entry drawn within this many frames is HOT and is not evicted while any colder slot exists.
///
/// Eight at 60 Hz is ~133 ms - comfortably longer than a route cross-fade's dip and than the
/// stretch either side of a modal capture, which are the moments two screens' text is alive at
/// once, and short enough that a string genuinely left behind by a navigation is evictable within
/// a fifth of a second.
const TCACHE_HOT_FRAMES: c_uint = 8;

/// **Start a frame for the glyph cache**: advance the frame serial and clear the live count.
///
/// It arms the hot rule the first time it is called. The ONE call site is the loop's draw entry
/// (`app/run.rs`, immediately before `draw` on a PRESENTING iteration): a frame that does not
/// present rasterises nothing, so the serial counts drawn frames and "touched within 8 frames"
/// means eight frames the cache could actually have been consulted on - not eight settled loop
/// iterations, after which everything on a still screen would read as cold.
pub fn begin_frame() {
    unsafe {
        TFRAME = TFRAME.wrapping_add(1).max(1);
        TFRAME_ARMED = true;
        TLIVE = 0;
    }
}

/// **How many distinct cache entries this frame has touched** - its occupancy.
///
/// The number the page-dip prewarm is refused against: §8.1's bound is
/// `live_this_frame + queued > TCACHE - 32`. The outgoing page touches its live text first, then the
/// recording painter admits destination misses only while that safety margin remains. A warmed
/// entry is touched by `text_tex` during the budgeted drain and therefore joins this count exactly
/// like an ordinary draw.
///
/// It counts an entry the first time it is touched in a frame, whether that touch was a HIT or a
/// fresh store: the question is how much of the cache this frame is standing on, and a string
/// rendered this frame occupies its slot exactly as one that was already there does.
pub fn live_this_frame() -> u32 {
    unsafe { *addr_of!(TLIVE) }
}

/// Take the count of entries evicted while still hot, and reset it (the heartbeat's
/// `evicted_hot`, read on every heartbeat by `app/run.rs`). Zero on a settled screen is the
/// property worth watching; a burst of new strings on a cold open is where it can move.
pub fn take_evicted_hot() -> u32 {
    unsafe {
        let n = *addr_of!(TEVICTED_HOT);
        TEVICTED_HOT = 0;
        n
    }
}

/// Mark an entry as drawn on this frame, counting it once per frame for [`live_this_frame`].
fn touch(e: &mut TCacheEntry) {
    unsafe {
        let f = *addr_of!(TFRAME);
        if e.last_frame != f {
            e.last_frame = f;
            TLIVE = TLIVE.saturating_add(1);
        }
    }
}

/// [`evict_slot`], plus the counting the pure half deliberately does not do.
unsafe fn take_slot(cache: &[TCacheEntry; TCACHE]) -> usize {
    let (slot, forced) = evict_slot(cache, *addr_of!(TFRAME), *addr_of!(TFRAME_ARMED));
    if forced {
        TEVICTED_HOT = TEVICTED_HOT.saturating_add(1);
    }
    slot
}

use tv_base::eventlog::log;

/// One line per boot, not one per size - `font_at` is called for every rung of the size ladder,
/// and 70-odd identical lines would bury the rest of the log.
fn log_font_fallback_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        log(&format!(
            "FONT FALLBACK: shipped fonts missing at {} — rendering in system DroidSans; \
             the theme size ladder and the hinting contract are INVALID in this build",
            tv_base::paths::app_dir().display()
        ));
    });
}

unsafe fn font_at(sz: c_int, bold: c_int) -> *mut TtfFont {
    let sz = sz.clamp(8, FONT_SLOTS as c_int - 1) as usize;
    let arr = if bold != 0 {
        &mut *addr_of_mut!(FONTS_B)
    } else {
        &mut *addr_of_mut!(FONTS)
    };
    if arr[sz].is_null() {
        arr[sz] = TTF_OpenFont(app_font(bold != 0).as_ptr(), sz as c_int);
        if arr[sz].is_null() {
            arr[sz] = TTF_OpenFont(app_font(false).as_ptr(), sz as c_int);
            if arr[sz].is_null() {
                // DroidSans is NOT an acceptable outcome, and it used to be an invisible one: the
                // app booted looking plausible while every rung of the theme::size ladder rendered
                // in a face the light-hinting/pixel-snapping contract was never tuned for - and
                // with no bold companion, so each bold rung became synthetic emboldening applied
                // AFTER grid-fitting. `init_text` still logged `ok=1`. Measured against the shipped
                // face: -4.67% mean advance (-45.8% on `J`), +4.2% line box, 2792 -> 873 codepoints.
                // tools/font-hint-audit.py reads HOST files, so it structurally cannot catch this.
                // Say so once, loudly, so it is a reportable bug rather than a mystery.
                log_font_fallback_once();
                arr[sz] = TTF_OpenFont(DROIDSANS.as_ptr(), sz as c_int);
            }
            if !arr[sz].is_null() && bold != 0 {
                TTF_SetFontStyle(arr[sz], TTF_STYLE_BOLD);
            }
        }
        if !arr[sz].is_null() {
            // Light hinting y-snaps strokes to the NEAREST pixel. Arial's own bytecode (default
            // NORMAL hinting) rounds horizontal bars UP - at bold 26 that draws 4px bars over
            // 3px stems, inverting the design's stem>bar weight - so every size token had to
            // dodge "unlucky" px values. Light keeps the whole theme::size scale design-true.
            TTF_SetFontHinting(arr[sz], TTF_HINTING_LIGHT);
        }
    }
    arr[sz]
}

/// Inter's coverage, read from `appfont.ttf`'s cmap on FIRST NEED and never again. `None` after
/// `BASE_COV_TRIED` means the file is absent or unreadable.
static mut BASE_COV: Option<tv_base::fontcov::Coverage> = None;
static mut BASE_COV_TRIED: bool = false;

/// Can the app face draw `cp`? Loads its coverage on first ask. Coverage is read from the REGULAR
/// face only, which is sound because `fontcov`'s `bold_face_covers_exactly_what_the_regular_one_does`
/// asserts the two are identical.
///
/// Answers `true` for everything when the cmap cannot be read: an install whose `appfont.ttf` is
/// missing has already fallen through to `font_at`'s DroidSans branch and logged loudly, and in
/// that state the useful behaviour is to render the string as spelled with whatever face opened,
/// not to drop every emoji-class character against a coverage set of nothing.
unsafe fn base_covers(cp: u32) -> bool {
    let tried = &mut *addr_of_mut!(BASE_COV_TRIED);
    if !*tried {
        *tried = true;
        let path = tv_base::paths::in_app_dir("appfont.ttf");
        match tv_base::fontcov::of_file(&path) {
            Ok(c) => {
                log(&format!("font coverage: {} codepoints={}", path.display(), c.len()));
                *addr_of_mut!(BASE_COV) = Some(c);
            }
            Err(e) => log(&format!("font coverage unavailable - {}: {e}", path.display())),
        }
    }
    (*addr_of!(BASE_COV)).as_ref().is_none_or(|c| c.contains(cp))
}

/// `s` with its emoji respelled for the app face (`fontcov::simplify_emoji`: flags to region
/// codes, ZWJ sequences to their first pictograph, marks gone, emoji the face lacks dropped), or
/// `None` when it is drawn as spelled. The ONE place both [`text_tex_miss`] (drawing) and
/// [`measure_native_bounds`] (measuring) take the string from, so a width and the texture drawn
/// in it always describe the same characters, and so does every path built on them: `elide`,
/// the fitted-line memo, the marquee, the table columns.
unsafe fn simplified_emoji(s: &[u8]) -> Option<CString> {
    simplified_emoji_with(s, |cp| base_covers(cp))
}

/// [`simplified_emoji`] with the coverage oracle passed in, so the host suite can drive it against
/// the REAL shipped cmap without a `TTF_Font`, a GL context or an install directory.
fn simplified_emoji_with(s: &[u8], covers: impl Fn(u32) -> bool) -> Option<CString> {
    let st = std::str::from_utf8(s).ok()?;
    let clean = tv_base::fontcov::simplify_emoji(st, covers)?;
    CString::new(clean).ok()
}

pub fn init_text() {
    FITTED_LINES.with(|cache| cache.borrow_mut().entries.clear());
    MEASURED_BOUNDS.with(|cache| cache.borrow_mut().entries.clear());
    unsafe {
        if TTF_Init() != 0 {
            log("TTF_Init failed");
            return;
        }
        TPROG = match crate::gfx::link_program(VS_TEXT.as_ptr(), FS_TEXT.as_ptr()) {
            Some(p) => p,
            None => {
                log("text prog link failed");
                return;
            }
        };
        TL_RECT = glGetUniformLocation(TPROG, c"u_trect".as_ptr());
        TL_SCREEN = glGetUniformLocation(TPROG, c"u_tscreen".as_ptr());
        TL_COL = glGetUniformLocation(TPROG, c"u_tcol".as_ptr());
        TL_TEX = glGetUniformLocation(TPROG, c"u_tex".as_ptr());
        // the fade program shares VS_TEXT; a link failure leaves TPROGF=0 and draw_text_fade
        // falls back to the plain path (the fade is a nicety, not load-bearing)
        TPROGF = crate::gfx::link_program(VS_TEXT.as_ptr(), FS_TEXT_FADE.as_ptr()).unwrap_or_else(
            || {
                log("text fade prog link failed");
                0
            },
        );
        if TPROGF != 0 {
            TLF_RECT = glGetUniformLocation(TPROGF, c"u_trect".as_ptr());
            TLF_SCREEN = glGetUniformLocation(TPROGF, c"u_tscreen".as_ptr());
            TLF_COL = glGetUniformLocation(TPROGF, c"u_tcol".as_ptr());
            TLF_TEX = glGetUniformLocation(TPROGF, c"u_tex".as_ptr());
            TLF_FADE = glGetUniformLocation(TPROGF, c"u_tfade".as_ptr());
            TLF_VTOP = glGetUniformLocation(TPROGF, c"u_vfadeT".as_ptr());
            TLF_VBOT = glGetUniformLocation(TPROGF, c"u_vfadeB".as_ptr());
        }
        // Constant uniforms, set once per program (per-program state): the fixed 1920x1080
        // screen and sampler unit 0. draw_text/_fade no longer re-send them per string.
        crate::gfx::use_prog(TPROG);
        glUniform2f(TL_SCREEN, SCR_W, SCR_H);
        glUniform1i(TL_TEX, 0);
        if TPROGF != 0 {
            crate::gfx::use_prog(TPROGF);
            glUniform2f(TLF_SCREEN, SCR_W, SCR_H);
            glUniform1i(TLF_TEX, 0);
        }
        if !font_at(28, 0).is_null() {
            TEXT_OK = 1;
        }
        // read through a raw pointer, not `&TEXT_OK`: `format!` takes its arguments BY REFERENCE,
        // which is a shared reference to a `static mut` (`static_mut_refs`, a future hard error)
        log(&format!("init_text ok={}", *addr_of!(TEXT_OK)));
    }
}

fn entry_key(e: &TCacheEntry) -> &[u8] {
    tv_base::cbuf::as_bytes(&e.s)
}
fn set_entry_key(e: &mut TCacheEntry, s: &[u8]) {
    tv_base::cbuf::set_bytes_raw(&mut e.s, s);
}

/// scan a freshly-rendered TTF surface for its vertical ink bounds - the first and last rows that
/// hold a visible (non-transparent) pixel. Blended text is white-on-clear, so alpha (byte 3 of each
/// ARGB8888 pixel) is the coverage. An all-blank string (e.g. a space) reports the full box.
unsafe fn surface_ink_v(surf: *const SdlSurface) -> (c_int, c_int) {
    let (w, h, pitch) = ((*surf).w, (*surf).h, (*surf).pitch as isize);
    let base = (*surf).pixels as *const u8;
    if base.is_null() || w <= 0 || h <= 0 {
        return (0, h.max(0));
    }
    let (mut top, mut bot) = (h, -1);
    for y in 0..h {
        let row = base.offset(y as isize * pitch);
        let mut ink = false;
        for x in 0..w {
            if *row.offset(x as isize * 4 + 3) > 24 {
                ink = true;
                break;
            }
        }
        if ink {
            if y < top {
                top = y;
            }
            bot = y;
        }
    }
    if bot < 0 {
        (0, h)
    } else {
        (top, bot + 1)
    }
}

/// returns (GL texture id (0 on failure), w, h, ink_top, ink_bottom)
unsafe fn text_tex(
    s_bytes: &[u8],
    s_c: *const c_char,
    sz: c_int,
    bold: c_int,
) -> (c_uint, c_int, c_int, c_int, c_int) {
    let hash = key_hash(s_bytes, sz, bold);
    {
        let cache = &mut *addr_of_mut!(TCACHE_A);
        for e in cache.iter_mut() {
            // klen + prefix, not full equality: the stored key is truncated to 95 bytes (see
            // TCacheEntry::klen) - starts_with degrades to equality for every short string.
            if e.hash == hash
                && e.tex != 0
                && e.sz == sz
                && e.bold == bold
                && e.klen as usize == s_bytes.len()
                && s_bytes.starts_with(entry_key(e))
            {
                TCLOCK = TCLOCK.wrapping_add(1);
                e.use_ = TCLOCK;
                touch(e);
                return (e.tex, e.w, e.h, e.ink_t, e.ink_b);
            }
        }
    }
    // A miss rasterises, scans ink and allocates a fresh GL texture - the first-use cost a cold
    // screen pays per string. Timed as the frame's `text` span, so a `FRAMEDROP` line says how
    // many strings a slow frame rasterised and what they cost (`diag::spans`).
    tv_base::diag::spans::span("text", || text_tex_miss(s_bytes, s_c, sz, bold, hash))
}

/// [`text_tex`]'s miss: render, upload and store one string.
unsafe fn text_tex_miss(
    key: &[u8],
    s_c: *const c_char,
    sz: c_int,
    bold: c_int,
    hash: u64,
) -> (c_uint, c_int, c_int, c_int, c_int) {
    // Rendered with its emoji respelled for the face; cached under the string as the caller spelled it.
    let clean = simplified_emoji(key);
    let (s_bytes, s_c) = match &clean {
        Some(c) => (c.to_bytes(), c.as_ptr()),
        None => (key, s_c),
    };
    if s_bytes.is_empty() {
        return (0, 0, 0, 0, 0); // nothing drawable is left (a label that was only emoji)
    }
    let f = font_at(sz, bold);
    if f.is_null() {
        return (0, 0, 0, 0, 0);
    }
    let white = SdlColor {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };
    let surf = TTF_RenderUTF8_Blended(f, s_c, white);
    if surf.is_null() {
        return (0, 0, 0, 0, 0);
    }
    let (sw, sh, pixels) = ((*surf).w, (*surf).h, (*surf).pixels);
    let (ink_t, ink_b) = surface_ink_v(surf);
    // Honour the surface's PITCH. SDL is free to pad each row, and `pitch` - not `w * 4` - is the
    // stride it actually wrote; `surface_ink_v` above has always read it, and only this upload
    // assumed the two were equal. They are on the television's SDL2_ttf, which is why this was
    // invisible for the app's whole life; they are NOT on the desktop SDL2_ttf the simulator
    // links, where every string rendered as diagonal hatching because each row was read four bytes
    // early and the image sheared.
    //
    // Repacking rather than `GL_UNPACK_ROW_LENGTH`: that pixel-store parameter does not exist in
    // GLES2, so the one-line fix would work on the simulator and silently do nothing on the TV.
    // The copy runs only when the stride is actually padded, so the device path is unchanged.
    let stride = (*surf).pitch as isize;
    let tight = sw as isize * 4;
    let tex = if stride == tight {
        crate::gfx::upload_rgba(0, sw, sh, pixels as *const u8)
    } else {
        let mut packed = vec![0u8; (tight * sh as isize) as usize];
        let base = pixels as *const u8;
        for y in 0..sh as isize {
            std::ptr::copy_nonoverlapping(
                base.offset(y * stride),
                packed.as_mut_ptr().offset(y * tight),
                tight as usize,
            );
        }
        crate::gfx::upload_rgba(0, sw, sh, packed.as_ptr())
    };
    SDL_FreeSurface(surf);
    cache_store(key, hash, sz, bold, tex, sw, sh, ink_t, ink_b)
}

/// **Which slot a new entry takes**, and the whole of the eviction policy - pure, over the array,
/// so it can be graded by a host test that renders nothing.
///
/// Today's rule, unchanged by this extraction: the first EMPTY slot, else the least recently used
/// (lowest `use_`, the [`TCLOCK`] serial). `use_` is a use ORDER and knows nothing about frames, so
/// this cannot tell an entry drawn on this very frame from one last drawn a minute ago - which is
/// what the occupancy bound the phase-11 plan asks for (§8.1) has to fix.
fn evict_slot(cache: &[TCacheEntry; TCACHE], frame: c_uint, armed: bool) -> (usize, bool) {
    let hot = |e: &TCacheEntry| {
        // `last_frame == 0` is "never touched this session", which is cold however new the cache
        // is; the serial itself starts at 1 (`begin_frame`) so the two cannot be confused.
        armed && e.last_frame != 0 && frame.wrapping_sub(e.last_frame) < TCACHE_HOT_FRAMES
    };
    let mut lru = 0usize;
    let mut lru_use = c_uint::MAX;
    let mut cold = None::<usize>;
    let mut cold_use = c_uint::MAX;
    for (i, e) in cache.iter().enumerate() {
        if e.tex == 0 {
            return (i, false);
        }
        if e.use_ < lru_use {
            lru_use = e.use_;
            lru = i;
        }
        if !hot(e) && e.use_ < cold_use {
            cold_use = e.use_;
            cold = Some(i);
        }
    }
    match cold {
        Some(i) => (i, false),
        // Every slot is on screen. Forward progress beats the rule - refusing to store would
        // re-render this string on every frame for as long as the screen is up - so the LRU goes
        // anyway and the caller counts it.
        None => (lru, true),
    }
}

/// Evict the LRU slot and install a freshly uploaded texture in it, returning what `text_tex`
/// returns.
#[allow(clippy::too_many_arguments)]
unsafe fn cache_store(
    s_bytes: &[u8],
    hash: u64,
    sz: c_int,
    bold: c_int,
    tex: c_uint,
    sw: c_int,
    sh: c_int,
    ink_t: c_int,
    ink_b: c_int,
) -> (c_uint, c_int, c_int, c_int, c_int) {
    let cache = &mut *addr_of_mut!(TCACHE_A);
    let slot = take_slot(cache);
    if cache[slot].tex != 0 {
        crate::gfx::tex_ledger::deleted(cache[slot].tex);
        glDeleteTextures(1, &cache[slot].tex);
    }
    set_entry_key(&mut cache[slot], s_bytes);
    cache[slot].hash = hash;
    cache[slot].klen = s_bytes.len() as u32;
    if s_bytes.len() >= 96 {
        // one-shot visibility: long keys are legal (klen disambiguates) but worth knowing about
        static mut LONG_KEY_LOGGED: bool = false;
        if !LONG_KEY_LOGGED {
            LONG_KEY_LOGGED = true;
            log(&format!(
                "text cache: key len {} exceeds the 95-byte prefix (fine, klen-keyed)",
                s_bytes.len()
            ));
        }
    }
    cache[slot].sz = sz;
    cache[slot].bold = bold;
    cache[slot].tex = tex;
    cache[slot].w = sw;
    cache[slot].h = sh;
    cache[slot].ink_t = ink_t;
    cache[slot].ink_b = ink_b;
    TCLOCK = TCLOCK.wrapping_add(1);
    cache[slot].use_ = TCLOCK;
    // The slot's `last_frame` is deliberately NOT reset first: a slot recycled mid-frame was
    // already counted by whoever touched it, and counting it twice would let `live_this_frame`
    // exceed the number of slots there are.
    touch(&mut cache[slot]);
    (tex, sw, sh, ink_t, ink_b)
}

/// Pixel width of `s` at `sz`/`bold` **without rasterizing anything** - pure font metrics.
///
/// This used to go through `text_tex`, i.e. it ran `TTF_RenderUTF8_Blended` (a full glyph-run
/// rasterization), scanned the resulting surface for its ink rows, uploaded a GL texture and then
/// threw all of it away to return one integer. `TTF_SizeUTF8` answers the same integer from the
/// font's glyph metrics: in SDL_ttf the blended renderer *sizes its surface with that very call*,
/// so the two widths are equal by construction, not by approximation. It also means the glyph
/// cache is no longer churned by pure measurement - the elide binary search, `TextView`'s
/// per-word wrap sweep and the HUD's template widths were each evicting real, about-to-be-drawn
/// entries out of the 160 slots to store strings nobody paints.
///
/// No caller depends on the old rasterize-as-a-side-effect: every one of them uses the number for
/// layout, and the ones that also draw the string get their texture from `draw_text` on the same
/// frame (a cache miss there, but the identical single render the measure used to do - so the
/// per-frame render count never rises, and for measure-only strings it falls to zero).
///
/// Native widths are cached by the exact bytes, size and weight, so different fitting budgets and
/// repeated toolbar draws reuse the same metrics. A pure-ASCII cache miss costs one
/// `TTF_SizeUTF8`, decided before any coverage is loaded. Failed measurements and the
/// uninitialized-font estimate are never cached.
pub fn text_width(s: *const c_char, sz: c_int, bold: c_int) -> f32 {
    text_bounds(s,sz,bold).0
}

/// CPU-only layout bounds: the width and height `TTF_SizeUTF8` reports, which is the box the
/// rendered texture has. A declaration pass needs the height too. This does not render or upload
/// a glyph and does not spend the text-prewarm budget.
pub fn text_bounds(s: *const c_char, sz: c_int, bold: c_int) -> (f32,f32) {
    unsafe {
        if s.is_null() || *s == 0 {
            return (0.0, 0.0);
        }
        if TEXT_OK == 0 {
            // NEVER a silent 0.0 (restructure spec §4.3): a layout built on zero widths reads as
            // a layout, and the fonts-fell-through-to-DroidSans failure this project has already
            // had was invisible for exactly that reason. Without a font the answer is the
            // average-advance estimate, and the fault is recorded once for the frame tail to log.
            MEASURE_FAULT.store(true, Ordering::Relaxed);
            return (unmeasured_width(CStr::from_ptr(s).to_bytes().len(), sz), sz as f32);
        }
        let text = CStr::from_ptr(s);
        MEASURED_BOUNDS.with(|cache| cache.borrow_mut().bounds(text, sz, bold,
            || measure_native_bounds(text, sz, bold)))
    }
}

/// The second result says the measurement succeeded, so it may be cached.
fn measure_native_bounds(s: &CStr, sz: c_int, bold: c_int) -> ((f32, f32), bool) {
    unsafe {
        // Measured exactly as `text_tex_miss` draws it: with its emoji respelled for the face.
        let clean = simplified_emoji(s.to_bytes());
        let s = clean.as_deref().unwrap_or(s);
        if s.is_empty() {
            return ((0.0, 0.0), true);
        }
        let f = font_at(sz, bold);
        if f.is_null() {
            return ((0.0, 0.0), false);
        }
        // Both outputs are real locals. The painter needs height as well as width, with no
        // rasterization or texture upload during declaration.
        let (mut w, mut h): (c_int, c_int) = (0, 0);
        if TTF_SizeUTF8(f, s.as_ptr(), &mut w, &mut h) != 0 {
            return ((0.0, 0.0), false); // same "unmeasurable → 0" contract the null-surface path had
        }
        ((w as f32, h as f32), true)
    }
}

/// Set when a width was asked for before any font opened (or after every font failed): the
/// restructure's `Fault::Measure`. `take_measure_fault` is read at the frame tail, which logs it once.
static MEASURE_FAULT: AtomicBool = AtomicBool::new(false);

/// The width a string gets with no font to measure it: half an em per byte - the average advance
/// of a Latin face - so the layout is roughly right and visibly so, rather than collapsed.
fn unmeasured_width(bytes: usize, sz: c_int) -> f32 {
    bytes as f32 * sz as f32 * 0.5
}

/// Whether a measurement happened without a font since the last call (and clears it).
pub fn take_measure_fault() -> bool {
    MEASURE_FAULT.swap(false, Ordering::Relaxed)
}

/// The device / simulator `Measure` (restructure spec §4.3): synchronous, over the loaded fonts,
/// and LOUD when there are none - a debug build panics, a release build answers the average
/// advance and records the fault. The other two implementations are `TableMeasure` (replay) and
/// `FixtureMeasure` (host tests).
pub struct TtfMeasure;

impl tv_machine::machine::Measure for TtfMeasure {
    fn fit_line(&self, s: &str, budget: f32, sz: i32, bold: bool) -> Rc<CStr> {
        if unsafe { addr_of!(TEXT_OK).read() } == 0 {
            return fit_line_by(self, s, budget, sz, bold);
        }
        FITTED_LINES.with(|cache| cache.borrow_mut().fit(self, s, budget, sz, bold))
    }
    fn width(&self, s: &CStr, sz: c_int, bold: bool) -> f32 {
        // `text_width` reads TEXT_OK, a main-thread static the C-era init writes once at boot.
        let ok = unsafe { std::ptr::addr_of!(TEXT_OK).read() } != 0;
        debug_assert!(ok, "TtfMeasure::width before init_text: no font is loaded");
        text_width(s.as_ptr(), sz, bold as c_int)
    }
    fn cap_h(&self, sz: c_int) -> f32 {
        cap_h(sz, 0)
    }
    fn line_h(&self, sz: c_int) -> f32 {
        text_height(sz, 0)
    }
    fn live_font(&self) -> bool {
        true
    }
}

static mut ELIDE_CACHE: Option<HashMap<u64, String>> = None;

#[derive(Default)]
pub struct MeasuredBounds {
    entries: HashMap<u64, MeasuredBound>,
}

struct MeasuredBound {
    text: Box<[u8]>,
    spec: (c_int, c_int),
    bounds: (f32, f32),
}

thread_local! {
    static MEASURED_BOUNDS: std::cell::RefCell<MeasuredBounds> = Default::default();
    static NATIVE_WIDTH_FAILURES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl MeasuredBounds {
    fn bounds(&mut self, text: &CStr, sz: c_int, bold: c_int,
        compute: impl FnOnce() -> ((f32, f32), bool)) -> (f32, f32) {
        let bytes = text.to_bytes();
        let spec = (sz, bold);
        let mut hash = DefaultHasher::new();
        (bytes, spec).hash(&mut hash);
        let key = hash.finish();
        if let Some(entry) = self.entries.get(&key) {
            if entry.spec == spec && entry.text.as_ref() == bytes { return entry.bounds; }
        }
        let (bounds, complete) = compute();
        if complete {
            if self.entries.len() >= 512 { self.entries.clear(); }
            self.entries.insert(key, MeasuredBound { text: bytes.into(), spec, bounds });
        } else {
            NATIVE_WIDTH_FAILURES.with(|generation| generation.set(generation.get().wrapping_add(1)));
        }
        bounds
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn width(&mut self, text: &CStr, sz: c_int, bold: c_int,
        compute: impl FnOnce() -> (f32, bool)) -> f32 {
        self.bounds(text, sz, bold, || {
            let (width, complete) = compute();
            ((width, 0.0), complete)
        }).0
    }
}

#[cfg(test)]
mod measured_width_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn native_bounds_are_cached_only_once_complete() {
        let mut cache = MeasuredBounds::default();
        assert_eq!(cache.bounds(c"A label", 26, 0, || ((120.0, 35.0), false)), (120.0, 35.0));
        assert!(cache.entries.is_empty(), "a failed measurement cannot become a permanent bound");
        assert_eq!(cache.bounds(c"A label", 26, 0, || ((180.0, 41.0), true)), (180.0, 41.0));
        assert_eq!(cache.bounds(c"A label", 26, 0, || panic!("cached bounds")), (180.0, 41.0));
        assert_eq!(cache.bounds(c"A label", 26, 1, || ((183.0, 43.0), true)), (183.0, 43.0));
    }

    #[test]
    fn repeated_native_widths_avoid_metric_calls() {
        let mut cache = MeasuredBounds::default();
        let calls = Cell::new(0);
        for _ in 0..60 {
            let width = cache.width(c"A show with a long title", 26, 1, || {
                calls.set(calls.get() + 1);
                (321.5, true)
            });
            assert_eq!(width, 321.5);
            assert_eq!(calls.get(), 1, "repeated fits and toolbar draws must reuse font metrics");
        }
    }

    #[test]
    fn native_width_keys_are_exact_and_storage_is_bounded() {
        let mut cache = MeasuredBounds::default();
        let calls = Cell::new(0);
        for (text, sz, bold) in [(c"Episode", 24, 0), (c"Episode", 26, 0),
            (c"Episode", 26, 1), (c"Épisode", 26, 1)] {
            let width = cache.width(text, sz, bold, || {
                calls.set(calls.get() + 1);
                (calls.get() as f32, true)
            });
            assert_eq!(cache.width(text, sz, bold, || panic!("already measured")), width);
        }
        assert_eq!(calls.get(), 4);
        for index in 0..1024 {
            let text = CString::new(index.to_string()).unwrap();
            cache.width(&text, 24, 0, || (1.0, true));
            assert!(cache.entries.len() <= 512);
        }
    }

    #[test]
    fn failed_or_partial_native_metrics_are_retried() {
        let mut cache = MeasuredBounds::default();
        let calls = Cell::new(0);
        for width in [0.0, 25.0] {
            assert_eq!(cache.width(c"episode", 24, 0, || {
                calls.set(calls.get() + 1);
                (width, false)
            }), width);
            assert!(cache.entries.is_empty());
        }
        assert_eq!(calls.get(), 2);
        assert_eq!(cache.width(c"episode", 24, 0, || (42.0, true)), 42.0);
        assert_eq!(cache.width(c"episode", 24, 0, || panic!("success is cached")), 42.0);
    }
}

// `Measure::fit_line`'s default body and the supplied-metric elision under it are the machine
// layer's (`tv_machine::machine`), which may not name this module; they stay reachable at these paths.
pub use tv_machine::machine::{elide_by, fit_line_by};

#[derive(Default)]
pub struct FittedLines {
    entries: HashMap<u64, FittedLine>,
}

struct FittedLine {
    text: Box<str>,
    spec: (u32, i32, bool),
    run: Rc<CStr>,
}

// Only the native capability owns this memo. A recording/replay capability must issue its own
// metric queries even when an identical string was already fitted against a live font.
thread_local! {
    static FITTED_LINES: std::cell::RefCell<FittedLines> = Default::default();
}

impl FittedLines {
    pub fn fit(&mut self, measure: &impl tv_machine::machine::Measure, s: &str,
        budget: f32, sz: i32, bold: bool) -> Rc<CStr> {
        let spec = (budget.to_bits(), sz, bold);
        let mut hash = DefaultHasher::new();
        (s, spec).hash(&mut hash);
        let key = hash.finish();
        if let Some(entry) = self.entries.get(&key) {
            // Retain the full input as well: a hash collision must never show another item's label.
            if entry.spec == spec && entry.text.as_ref() == s { return Rc::clone(&entry.run); }
        }
        let before = NATIVE_WIDTH_FAILURES.with(std::cell::Cell::get);
        let run = fit_line_by(measure, s, budget, sz, bold);
        // Preserve this frame's fallback, but retry a fit that relied on failed/partial metrics.
        // Cache hits return above, so a successful hot run pays no generation checks.
        if NATIVE_WIDTH_FAILURES.with(std::cell::Cell::get) != before { return run; }
        if self.entries.len() >= 512 { self.entries.clear(); }
        self.entries.insert(key, FittedLine { text: s.into(), spec, run: Rc::clone(&run) });
        run
    }
}

#[cfg(test)]
mod fitted_line_tests {
    use super::*;
    use tv_machine::machine::Measure;
    use std::cell::Cell;

    #[derive(Default)]
    struct CountMeasure(Cell<usize>);
    impl Measure for CountMeasure {
        fn width(&self, s: &CStr, sz: i32, _: bool) -> f32 {
            self.0.set(self.0.get() + 1);
            s.to_string_lossy().chars().count() as f32 * sz as f32
        }
        fn cap_h(&self, _: i32) -> f32 { 1.0 }
        fn line_h(&self, _: i32) -> f32 { 1.0 }
    }

    #[test]
    fn repeated_still_lines_reuse_measurement_and_drawable_storage() {
        let measure = CountMeasure::default();
        let mut cache = FittedLines::default();
        let show = cache.fit(&measure, "A show with a long title", 10.0, 1, true);
        let address = cache.fit(&measure, "S1 · E2", 10.0, 1, false);
        let calls = measure.0.get();
        assert!(calls > 0);
        for _ in 0..60 {
            let next_show = cache.fit(&measure, "A show with a long title", 10.0, 1, true);
            let next_address = cache.fit(&measure, "S1 · E2", 10.0, 1, false);
            assert_eq!(measure.0.get(), calls, "stable overlays must not remeasure each frame");
            assert!(Rc::ptr_eq(&show, &next_show));
            assert!(Rc::ptr_eq(&address, &next_address));
        }
    }

    #[test]
    fn a_failed_metric_does_not_cache_a_stale_fitted_line() {
        struct RetryMeasure {
            fail: Cell<bool>,
            widths: std::cell::RefCell<MeasuredBounds>,
            calls: Cell<usize>,
        }
        impl Measure for RetryMeasure {
            fn width(&self, s: &CStr, sz: i32, bold: bool) -> f32 {
                self.widths.borrow_mut().width(s, sz, bold as c_int, || {
                    self.calls.set(self.calls.get() + 1);
                    let failed = self.fail.replace(false);
                    (if failed { 0.0 } else { s.to_string_lossy().chars().count() as f32 }, !failed)
                })
            }
            fn cap_h(&self, _: i32) -> f32 { 1.0 }
            fn line_h(&self, _: i32) -> f32 { 1.0 }
        }
        let measure = RetryMeasure { fail: Cell::new(true), widths: Default::default(), calls: Cell::new(0) };
        let mut cache = FittedLines::default();
        let failed = cache.fit(&measure, "episode", 4.0, 1, false);
        assert_eq!(failed.to_bytes(), b"episode"); // the failed zero width preserves this frame's fallback
        let recovered = cache.fit(&measure, "episode", 4.0, 1, false);
        assert_eq!(recovered.to_bytes(), "epi…".as_bytes(), "retry must use the recovered font metrics");
        let calls = measure.calls.get();
        assert!(Rc::ptr_eq(&recovered, &cache.fit(&measure, "episode", 4.0, 1, false)));
        assert_eq!(measure.calls.get(), calls, "a successful retry restores the cache fast path");
    }

    #[test]
    fn fitted_lines_keep_exact_width_text_size_and_weight_in_the_key() {
        let measure = CountMeasure::default();
        let mut cache = FittedLines::default();
        let run = cache.fit(&measure, "абвг", 3.25, 1, false);
        assert_eq!(run.to_bytes(), "аб…".as_bytes());
        for (text, budget, sz, bold) in [
            ("абвг", 3.75, 1, false), ("абвг", 3.75, 2, false),
            ("абвг", 3.75, 2, true), ("different", 3.75, 2, true),
        ] {
            let calls = measure.0.get();
            cache.fit(&measure, text, budget, sz, bold);
            assert!(measure.0.get() > calls);
        }
    }

    #[test]
    fn fitted_line_memo_stays_bounded_and_old_runs_remain_drawable() {
        let measure = CountMeasure::default();
        let mut cache = FittedLines::default();
        let old = cache.fit(&measure, "episode", 20.0, 1, false);
        for width in 0..1024 {
            cache.fit(&measure, "episode", width as f32, 1, false);
            assert!(cache.entries.len() <= 512);
        }
        assert_eq!(old.to_bytes(), b"episode");
    }
}

/// Truncate `s` to fit `budget` px at `sz`/`bold`, ellipsised - and **memoised**. The per-frame
/// binary search over candidate widths (each candidate a `text_width` call) is the text-measure
/// thrash that dropped the player Info panel to ~1fps; caching the RESULT by (text, budget, sz, bold,
/// cont) makes every later frame a hash lookup, thrash-proof no matter how long the input. `cont=false`
/// returns `s` unchanged when it already fits (a plain elide - track menu / chapters / info card);
/// `cont=true` always marks the result with `...` (a continued line whose caller already knows more text
/// follows - `TextView`). The single truncation impl for the whole UI. Main-thread only, like the
/// glyph cache.
pub fn elide(s: &str, budget: f32, sz: c_int, bold: c_int, cont: bool) -> String {
    let mut hh = DefaultHasher::new();
    s.hash(&mut hh);
    (budget as i32).hash(&mut hh);
    sz.hash(&mut hh);
    bold.hash(&mut hh);
    cont.hash(&mut hh);
    let key = hh.finish();
    let cache = unsafe { (*addr_of_mut!(ELIDE_CACHE)).get_or_insert_with(HashMap::new) };
    if let Some(v) = cache.get(&key) {
        return v.clone();
    }
    let out = elide_compute(s, budget, sz, bold, cont);
    if cache.len() > 512 {
        cache.clear(); // crude cap - plenty for every label/line across a few screens
    }
    cache.insert(key, out.clone());
    out
}

fn elide_compute(s: &str, budget: f32, sz: c_int, bold: c_int, cont: bool) -> String {
    let measure = |t: &str| {
        CString::new(t)
            .ok()
            .map(|c| text_width(c.as_ptr(), sz, bold))
            .unwrap_or(0.0)
    };
    elide_by(s, budget, cont, measure)
}

/// [`elide_by`] that cuts from the MIDDLE: the start and the end of `s` survive around one `...`
/// (the longer half on the start when the kept count is odd), for text whose END matters or has
/// other text right behind it - a name followed by its sentence's period reads
/// "Maximilian...czyk." rather than the four dots of "Maximilian....". `s` unchanged when it fits (or
/// the budget is not positive).
pub fn elide_middle_by(s: &str, budget: f32, measure: impl Fn(&str) -> f32) -> String {
    if budget <= 0.0 || measure(s) <= budget {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    // `keep` characters survive: the first ceil(keep/2) and the last floor(keep/2).
    let cut = |keep: usize| {
        let head: String = chars[..keep.div_ceil(2)].iter().collect();
        let tail: String = chars[chars.len() - keep / 2..].iter().collect();
        format!("{}\u{2026}{}", head.trim_end(), tail.trim_start())
    };
    let (mut lo, mut hi) = (0usize, chars.len().saturating_sub(1));
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&cut(mid)) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    cut(lo)
}

#[cfg(test)]
mod supplied_elide_tests {
    // `elide_by`'s own test is beside it in `tv_machine::machine`; this one grades the middle cut.
    #[test]
    fn middle_elision_keeps_both_ends_within_the_budget() {
        let width = |s: &str| s.chars().count() as f32;
        assert_eq!(super::elide_middle_by("abcdefghij", 6.0, width), "abc…ij");
        assert_eq!(super::elide_middle_by("abcdefghij", 5.0, width), "ab…ij");
        assert_eq!(super::elide_middle_by("абвгдежз", 5.0, width), "аб…жз");
        assert_eq!(super::elide_middle_by("a🙂bcdefg", 4.0, width), "a🙂…g");
        assert_eq!(super::elide_middle_by("short", 8.0, width), "short");
        assert_eq!(super::elide_middle_by("short", 0.0, width), "short");
        assert_eq!(super::elide_middle_by("abcdefghij", 1.0, width), "…");
        // a space beside the cut is not kept hanging against the ellipsis
        assert_eq!(super::elide_middle_by("ab cd ef gh", 6.0, width), "ab…gh");
    }
}

/// rendered height in px of a line of text at `sz`/`bold` - the font's line height, independent of
/// the string. Used to vertically center a glyph on a text line (e.g. the transport clock).
pub fn text_height(sz: c_int, bold: c_int) -> f32 {
    unsafe {
        if TEXT_OK == 0 {
            return sz as f32;
        }
        let (_tex, _w, h, _it, _ib) = text_tex(b"0", c"0".as_ptr(), sz, bold);
        h as f32
    }
}

/// The font's **cap band** at `sz`/`bold`: (cap_top, baseline) offsets from the draw-y, measured
/// once from a reference capital ("H", which is flat-topped and sits on the baseline). This is the
/// stable, string-independent band UI toolkits centre type on - it deliberately ignores descenders
/// (g j y p q) and ascenders, so every label of a given size aligns the same way. Falls back to a
/// rough em band without TTF.
pub fn text_cap_band(sz: c_int, bold: c_int) -> (f32, f32) {
    unsafe {
        if TEXT_OK == 0 {
            return (sz as f32 * 0.15, sz as f32 * 0.9);
        }
        let (_t, _w, _h, it, ib) = text_tex(b"H", c"H".as_ptr(), sz, bold);
        (it as f32, ib as f32)
    }
}

/// The layout HEIGHT of one line of `sz`/`bold` text - its cap band, cap-top to baseline. Layout ≠
/// paint: descenders hang below this and are deliberately not measured (see `ui::label`'s module
/// docs for the rule). The one place this subtraction lives, because every screen that stacks text
/// needs it and three of them had written it out by hand.
pub fn cap_h(sz: c_int, bold: c_int) -> f32 {
    let (top, base) = text_cap_band(sz, bold);
    base - top
}

/// The draw-`y` (texture top) at which text of `sz`/`bold` centres its **cap band** on `cy`. Centres
/// on the font's cap-top→baseline band rather than the specific string's ink, so a label with
/// descenders ("From Beginning") and one without ("Go to Movie") land identically - descenders hang
/// below the optical centre instead of dragging the whole line up.
pub fn text_vcenter_y(sz: c_int, bold: c_int, cy: f32) -> f32 {
    let (ct, cb) = text_cap_band(sz, bold);
    cy - (ct + cb) * 0.5
}

/// The draw-`y` at which text of `sz`/`bold` sits on the **baseline** of a run of `on_sz`/`on_bold`
/// text drawn at `on_y` - the one-line twin of [`text_vcenter_y`], for two runs of DIFFERENT sizes
/// on the same line.
///
/// Two such runs must align by baseline, never by top: a caption beside a number, a `·` and a
/// handle beside a heading. Top-aligning them hangs the smaller run's whole cap band above the
/// larger one's baseline, so it reads as a superscript; the design mocks say `align-items:baseline`
/// for exactly this. The subtraction is the cap-band bottoms, which is a font metric and therefore
/// string-independent (see [`text_cap_band`]).
///
/// The ONE place this arithmetic lives - `widgets::rating_group` had written it out inline, and the
/// shelf heading's source annotation needs the same three lines.
///
/// The two band bottoms are subtracted from EACH OTHER before `on_y` is touched, deliberately: a
/// run measured against its own tokens then returns `on_y` bit-for-bit, so a caller that resolves
/// every run of a flow through this (`ui::cards::draw_heading`) does not nudge its reference run by
/// an ULP for the privilege.
pub fn baseline_y(sz: c_int, bold: c_int, on_sz: c_int, on_bold: c_int, on_y: f32) -> f32 {
    // The same-token case is the COMMON one - a flow resolves its own reference run through here
    // too - and it is not free: `text_cap_band` rasterizes through the glyph cache, whose lookup is
    // a linear scan of 160 entries. Twice per heading, per shelf, per frame, inside the grid draw
    // loop, to compute a difference that is zero by construction. Take it as an identity instead.
    if sz == on_sz && bold == on_bold {
        return on_y;
    }
    on_y + (text_cap_band(on_sz, on_bold).1 - text_cap_band(sz, bold).1)
}

/// The texture a string is DRAWN with and its size in logical px. The logical texture itself, except
/// under the simulator's supersampled rendering (`surface::render_scale`), where it is the same
/// string rasterised at `n`x and drawn at `1/n` - so glyphs stay 1:1 with physical pixels while
/// every width and metric the layout reads is still the logical one.
#[inline]
unsafe fn drawn_tex(
    s_bytes: &[u8],
    s: *const c_char,
    sz: c_int,
    bold: c_int,
    logical: (c_uint, c_int, c_int),
) -> (c_uint, f32, f32) {
    #[cfg(feature = "hostsim")]
    {
        let n = tv_base::surface::render_scale();
        if n > 1 {
            let (t, w, h, _, _) = text_tex(s_bytes, s, sz * n, bold);
            if t != 0 {
                return (t, w as f32 / n as f32, h as f32 / n as f32);
            }
        }
    }
    #[cfg(not(feature = "hostsim"))]
    let _ = (s_bytes, s, sz, bold);
    (logical.0, logical.1 as f32, logical.2 as f32)
}

/// A string's texture, laid out and placed for drawing: everything [`draw_text`] and
/// [`draw_text_fade`] share before they bind their own program.
struct Placed {
    tex: c_uint,
    /// The destination quad on screen, after the caller's zoom.
    quad: crate::gfx::Rect,
    /// The texture's unzoomed width - the fade program's px→uv divisor.
    dw: f32,
    /// The layout width the caller gets back (independent of zoom).
    w: f32,
}

impl Placed {
    /// Glyphs are 1:1 texel:pixel only unzoomed, so only then is the origin snapped (see
    /// `gfx::snap`); a zoomed run is meant to resample smoothly as it grows.
    fn origin(&self, zoom: crate::gfx::Zoom) -> (f32, f32) {
        if zoom.is_none() {
            (crate::gfx::snap(self.quad.x), crate::gfx::snap(self.quad.y))
        } else {
            (self.quad.x, self.quad.y)
        }
    }
}

/// Look up `s`'s glyph texture (the SAME entry at any zoom - an animation never re-rasterizes) and
/// place its quad, grown by `zoom`. `Err(w)` is the width to return when nothing is drawn: 0 for
/// an empty/unavailable string, the layout width for a run outside the blur-source crop (the
/// texture stays warm, the driver state changes are skipped).
unsafe fn place_text(
    s: *const c_char,
    x: f32,
    y: f32,
    sz: c_int,
    align: c_int,
    bold: c_int,
    zoom: crate::gfx::Zoom,
) -> Result<Placed, f32> {
    if TEXT_OK == 0 || s.is_null() {
        return Err(0.0);
    }
    let cs = CStr::from_ptr(s);
    let s_bytes = cs.to_bytes();
    if s_bytes.is_empty() {
        return Err(0.0);
    }
    let (tex, w, h, _it, _ib) = text_tex(s_bytes, s, sz, bold);
    if tex == 0 {
        return Err(0.0);
    }
    let (tex, dw, dh) = drawn_tex(s_bytes, s, sz, bold, (tex, w, h));
    let dx = match align {
        1 => x - dw * 0.5,
        2 => x - dw,
        _ => x,
    };
    let quad = zoom.map(crate::gfx::Rect::new(dx, y, dw, dh));
    if crate::gfx::culled(quad.x, quad.y, quad.w, quad.h)
        || gate(Class::Text, quad.x, quad.y, quad.w, quad.h)
    {
        return Err(w as f32);
    }
    Ok(Placed { tex, quad, dw, w: w as f32 })
}

/// align: 0 left, 1 center, 2 right (x is the anchor edge). returns text width. `zoom` (in the
/// same space as `x`/`y`) grows the drawn quad about its origin; layout width is unaffected.
pub fn draw_text(
    s: *const c_char,
    x: f32,
    y: f32,
    sz: c_int,
    col: *const f32,
    align: c_int,
    bold: c_int,
    zoom: crate::gfx::Zoom,
) -> f32 {
    unsafe {
        let p = match place_text(s, x, y, sz, align, bold, zoom) {
            Ok(p) => p,
            Err(w) => return w,
        };
        crate::gfx::use_prog(TPROG); // TL_SCREEN / TL_TEX / texture unit 0 set once at init
        glUniform4fv(TL_COL, 1, col);
        glBindTexture(GL_TEXTURE_2D, p.tex);
        let (ox, oy) = p.origin(zoom);
        glUniform4f(TL_RECT, ox, oy, p.quad.w, p.quad.h);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        p.w
    }
}

/// [`draw_text`] with up to three independent fades (its OWN GL program, so plain text pays no
/// per-fragment fade cost - see the shader's own header for why every ordinary glyph stays off
/// it): a HORIZONTAL one (`hfade`, glyph alpha 1→0 between `from`..`to` px from the string's LEFT
/// edge regardless of `align` - the About card's and the person header bio's `MORE` dissolve), and
/// two VERTICAL ones in absolute logical screen y (`vfade_top` ramps 0→1 rising through the band,
/// `vfade_bot` ramps 1→0 falling through it - a line crossing a SCROLLING viewport's clipped edge,
/// see `ui::text_view::TextView::edge_fade`). Each is `None` by default and costs the shader
/// nothing extra to skip (a uniform compare, not a texture sample). Falls back to plain
/// [`draw_text`] if the fade program failed to link - a device with no working fade shader still
/// shows every word, just without the dissolve.
///
/// `zoom` is [`draw_text`]'s: a dissolving line inside a lifted block grows with the lines above
/// it. The horizontal band is a fraction of the string's own texture, so it needs no mapping; the
/// vertical bands are screen y and go through the same zoom as the quad.
#[allow(clippy::too_many_arguments)]
pub fn draw_text_fade(
    s: *const c_char,
    x: f32,
    y: f32,
    sz: c_int,
    col: *const f32,
    align: c_int,
    bold: c_int,
    hfade: Option<(f32, f32)>,
    vfade_top: Option<(f32, f32)>,
    vfade_bot: Option<(f32, f32)>,
    zoom: crate::gfx::Zoom,
) -> f32 {
    unsafe {
        if TPROGF == 0 {
            return draw_text(s, x, y, sz, col, align, bold, zoom);
        }
        let p = match place_text(s, x, y, sz, align, bold, zoom) {
            Ok(p) => p,
            Err(w) => return w,
        };
        crate::gfx::use_prog(TPROGF); // TLF_SCREEN / TLF_TEX / texture unit 0 set once at init
        glUniform4fv(TLF_COL, 1, col);
        // px → string-texture uv (the varying spans the one-quad string). `(0.0, 0.0)` is "off" -
        // the shader gates on `to > from`, so a caller with no horizontal fade need not sentinel
        // against the string's own width.
        let (hf0, hf1) = hfade.unwrap_or((0.0, 0.0));
        glUniform2f(TLF_FADE, hf0 / p.dw, hf1 / p.dw);
        let band = |b: Option<(f32, f32)>| {
            b.map_or((0.0, 0.0), |(a, z)| (zoom.map_y(a), zoom.map_y(z)))
        };
        let (vt0, vt1) = band(vfade_top);
        glUniform2f(TLF_VTOP, vt0, vt1);
        let (vb0, vb1) = band(vfade_bot);
        glUniform2f(TLF_VBOT, vb0, vb1);
        glBindTexture(GL_TEXTURE_2D, p.tex);
        let (ox, oy) = p.origin(zoom);
        glUniform4f(TLF_RECT, ox, oy, p.quad.w, p.quad.h);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        p.w
    }
}

#[cfg(test)]
mod tests {
    //! The emoji respelling, against the real shipped cmap. Everything else in this module needs a
    //! `TTF_Font`, a GL context and a television; what a string becomes before it is drawn is a
    //! property of a file this repository ships, so it is gradable in `make check`.

    use super::*;
    use tv_base::fontcov;

    /// Inter's real coverage, parsed once for the whole test binary (see `fontcov::shipped`).
    fn covers(cp: u32) -> bool {
        fontcov::shipped("appfont.ttf").as_ref().is_ok_and(|c| c.contains(cp))
    }

    /// What the face draws for `s`: the string both `text_tex_miss` and `measure_native_bounds`
    /// take (`simplified_emoji`).
    fn drawn(s: &str) -> String {
        simplified_emoji_with(s.as_bytes(), covers).map_or_else(|| s.to_owned(), |c| c.into_string().unwrap())
    }

    /// Playlist emoji: what a shaper would ligate is respelled, a pictograph Inter lacks is dropped
    /// with its gap, and what is left is drawable end to end (no tofu).
    #[test]
    fn emoji_are_respelled_and_the_ones_the_face_lacks_dropped() {
        let cases = [
            ("\u{26BD} Sport", "Sport"),
            ("[\u{1F6F0}\u{FE0F}] SKY 1", "[] SKY 1"),
            ("\u{1F1F5}\u{1F1F1} Polsat", "PL Polsat"),
            ("Kino \u{1F3AC} HD", "Kino HD"),
            ("Kids \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} Mix", "Kids Mix"),
            ("Season 1\u{FE0F}\u{20E3}", "Season 1"),
        ];
        for (label, want) in cases {
            let got = drawn(label);
            assert_eq!(got, want, "{label:?}");
            let tofu: Vec<char> = got.chars().filter(|&c| !covers(c as u32)).collect();
            assert!(tofu.is_empty(), "{label:?} still draws {tofu:?} as boxes");
        }
    }

    /// Strings with nothing to respell are untouched, not copied; symbols Inter carries stay; a
    /// non-emoji character the face lacks keeps its honest box (Hebrew, see the module header).
    #[test]
    fn covered_symbols_stay_and_other_uncovered_text_keeps_its_box() {
        for s in ["\u{266A} It seems today \u{266B}", "\u{2605} 8.1", "\u{00A9} 2024 \u{00AE}", "Am\u{00E9}lie \u{00B7} \u{0418}\u{0440}\u{043E}\u{043D}\u{0438}\u{044F}", "Breaking Bad"] {
            assert!(simplified_emoji_with(s.as_bytes(), covers).is_none(), "{s:?} was respelled");
        }
        assert_eq!(drawn("\u{05D0}\u{05D1} \u{1F525}"), "\u{05D0}\u{05D1}");
        assert_eq!(drawn("\u{05D0}\u{05D1}"), "\u{05D0}\u{05D1}", "Hebrew is no emoji: it stays, as boxes");
    }

    /// With no coverage at all (`appfont.ttf` missing), no emoji-class character is dropped: the
    /// string renders as spelled in whatever face opened.
    #[test]
    fn an_unreadable_face_drops_nothing() {
        assert!(simplified_emoji_with("\u{2605} 8 \u{1F525}".as_bytes(), |_| true).is_none());
    }
}

#[cfg(test)]
mod cache_policy_tests {
    //! **The glyph cache's EVICTION policy, without a font, a GL context or a television.**
    //!
    //! `evict_slot` is the whole of it and is pure over the array, so the rule can be graded here;
    //! what these cannot see is the rasterization on the other side of it, which is a device
    //! question exactly as the run splitter's is.
    use super::*;

    #[test]
    fn warming_respects_the_per_frame_budget_and_never_blocks_the_dip() {
        use std::cell::Cell;
        use std::collections::VecDeque;

        let clock = Cell::new(0u64);
        let mut jobs = VecDeque::from([1u8, 2, 3, 4, 5]);
        let warmed = drain_budgeted(
            &mut jobs,
            6_000,
            || clock.get(),
            |_| clock.set(clock.get() + 2_000),
        );

        assert_eq!(warmed, 3, "only work fitting this dip frame was started");
        assert_eq!(clock.get(), 6_000, "the warmer yielded at its named deadline");
        assert_eq!(jobs.into_iter().collect::<Vec<_>>(), vec![4, 5]);
    }

    #[test]
    fn idle_font_warming_covers_every_theme_face_and_yields_at_its_budget() {
        use std::cell::{Cell, RefCell};
        reset_font_warm_for_test();
        let clock = Cell::new(0u64);
        let seen = RefCell::new(Vec::new());
        // one job per 1 ms against a 2 ms slice: two jobs, then the loop sleeps
        let ran = warm_step(2_000, || clock.get(), |sz, bold, glyphs| {
            clock.set(clock.get() + 1_000);
            seen.borrow_mut().push((sz, bold, glyphs.to_vec()));
        });
        assert_eq!(ran, 2);
        // the rest of the idle slices finish the ladder, then warming is a no-op
        while warm_step(2_000, || clock.get(), |sz, bold, glyphs| {
            clock.set(clock.get() + 1_000);
            seen.borrow_mut().push((sz, bold, glyphs.to_vec()));
        }) > 0 {}
        assert_eq!(warm_step(2_000, || clock.get(), |_, _, _| panic!("already warm")), 0);
        let seen = seen.into_inner();
        for &sz in &WARM_SIZES {
            for bold in [0, 1] {
                let glyphs: Vec<u8> = seen.iter()
                    .filter(|(s, b, _)| *s == sz && *b == bold)
                    .flat_map(|(_, _, g)| g.iter().copied())
                    .collect();
                assert_eq!(glyphs, WARM_GLYPHS, "size {sz} bold {bold}: every printable ASCII glyph, once");
            }
        }
        assert!(WARM_SIZES.contains(&crate::gfx::tokens::size::HERO));
        assert!(WARM_SIZES.contains(&crate::gfx::tokens::size::DIAGNOSTIC));
        reset_font_warm_for_test();
    }

    /// A cache of `n` occupied slots whose `use_` serials are the given ones (in slot order), the
    /// rest empty.
    fn cache_with(uses: &[c_uint]) -> [TCacheEntry; TCACHE] {
        let mut c = [TCacheEntry::ZERO; TCACHE];
        for (i, u) in uses.iter().enumerate() {
            c[i].tex = i as c_uint + 1;
            c[i].use_ = *u;
        }
        c
    }

    /// A cache with every slot occupied, `use_` ascending and every entry drawn on frame 100.
    fn full_cache() -> [TCacheEntry; TCACHE] {
        let mut c = [TCacheEntry::ZERO; TCACHE];
        for (i, e) in c.iter_mut().enumerate() {
            e.tex = i as c_uint + 1;
            e.use_ = 1_000 + i as c_uint;
            e.last_frame = 100;
        }
        c
    }

    /// An empty slot is always taken before anything is evicted, and it is the FIRST one.
    #[test]
    fn a_free_slot_is_taken_before_anything_is_evicted() {
        let mut c = cache_with(&[10, 11, 12]);
        assert_eq!(evict_slot(&c, 100, true).0, 3, "the first empty slot");
        for e in c.iter_mut() {
            e.tex = 1;
            e.use_ = 9;
            e.last_frame = 1;
        }
        c[7].use_ = 1;
        assert_eq!(evict_slot(&c, 100, true).0, 7, "full: the least recently used");
    }

    /// **An entry drawn within the last eight frames is not evictable** (§8.1's occupancy rule).
    ///
    /// `use_` is a use ORDER: it cannot tell an entry that is on screen RIGHT NOW from one last
    /// drawn a minute ago, so a screen holding more distinct strings than the cache has slots
    /// evicts exactly the entries it is about to ask for again - the thrash this cache exists to
    /// prevent, at one `TTF_RenderUTF8_Blended` plus a full-surface ink scan plus an upload apiece.
    #[test]
    fn a_hot_text_entry_is_never_evicted_within_eight_frames() {
        let mut c = full_cache();
        c[0].use_ = 1; // the LRU by use order - and on screen this frame
        c[40].last_frame = 92; // eight frames back: outside the window
        c[41].last_frame = 93; // seven: inside it
        let (slot, forced) = evict_slot(&c, 100, true);
        assert_eq!(slot, 40, "the coldest entry outside the hot window, not the LRU");
        assert!(!forced, "…and nothing hot was evicted");

        // Redraw 40 and the only remaining candidate is 41, seven frames back - INSIDE the
        // window, so still hot. Nothing is evictable and the rule gives way rather than refuse.
        c[40].last_frame = 100;
        let (slot, forced) = evict_slot(&c, 100, true);
        assert_eq!(slot, 0, "no cold slot left: the LRU goes after all");
        assert!(forced, "…and that is the forced case, counted rather than silent");
    }

    /// **If every slot is hot the LRU goes anyway, and it is COUNTED.** Forward progress beats the
    /// rule - refusing to store would re-render that string on every frame forever - but a screen
    /// that cannot fit its own text in 160 slots is a fact worth having, so it rides the heartbeat
    /// (`evicted_while_hot`) instead of passing in silence.
    #[test]
    fn evicted_while_hot_counts_each_forced_eviction() {
        let _g = tv_base::testlock::serial();
        let mut c = full_cache();
        c[13].use_ = 1;
        let (slot, forced) = evict_slot(&c, 100, true);
        assert_eq!(slot, 13, "every slot is hot: fall back to the least recently used");
        assert!(forced, "…and say so");

        take_evicted_hot(); // whatever an earlier test left behind
        begin_frame();
        unsafe {
            let f = *addr_of!(TFRAME);
            for e in c.iter_mut() {
                e.last_frame = f;
            }
            take_slot(&c);
            take_slot(&c);
        }
        assert_eq!(take_evicted_hot(), 2, "each forced eviction counts once");
        assert_eq!(take_evicted_hot(), 0, "…and the take resets it");
    }

    /// **Occupancy is per ENTRY per frame**: touching the same one twice is one occupied slot, and
    /// the count starts again with the frame. It can therefore never exceed [`TCACHE`], which is
    /// what makes `live_this_frame + prewarmed > TCACHE - 32` a sentence about the same quantity
    /// on both sides.
    #[test]
    fn the_live_count_counts_each_entry_once_per_frame() {
        let _g = tv_base::testlock::serial();
        begin_frame();
        let mut a = TCacheEntry::ZERO;
        let mut b = TCacheEntry::ZERO;
        touch(&mut a);
        touch(&mut a);
        assert_eq!(live_this_frame(), 1, "the same entry twice is one occupied slot");
        touch(&mut b);
        assert_eq!(live_this_frame(), 2);
        begin_frame();
        assert_eq!(live_this_frame(), 0, "a new frame starts the count again");
        touch(&mut a);
        assert_eq!(live_this_frame(), 1);
    }

    /// The hot window is inert until the loop starts calling [`begin_frame`]: with no frame
    /// counter every entry looks touched on frame 0, every eviction would read as forced, and the
    /// counter would say nothing.
    #[test]
    fn the_hot_window_is_inert_until_the_frame_counter_is_armed() {
        let mut c = full_cache();
        c[9].use_ = 1;
        let (slot, forced) = evict_slot(&c, 100, false);
        assert_eq!(slot, 9, "unarmed: the plain LRU, exactly as before phase 11");
        assert!(!forced, "…and nothing reported as forced");
    }

    /// **The background drain honours the occupancy bound**: with the frame already standing on
    /// the ceiling it rasterises nothing and keeps the queue; below it, it proceeds.
    #[test]
    fn the_background_drain_stops_at_the_occupancy_bound() {
        let _g = tv_base::testlock::serial();
        reset_prewarm_for_test();
        let key = |n: u8| WarmKey { bytes: vec![b'z', n], sz: 20, bold: 0 };
        BACKGROUND.with(|b| *b.borrow_mut() = (0..3).map(key).collect());
        let ceiling = TCACHE - PREWARM_HEADROOM;
        let steady = || 0;
        assert_eq!(drain_background_prewarm_at(9, steady, || ceiling - 1), 0, "live + 1 >= ceiling: refused");
        assert!(background_prewarm_pending(), "refused strings stay queued");
        assert_eq!(drain_background_prewarm_at(9, steady, || ceiling - 2), 3, "one below the bound proceeds");
        reset_prewarm_for_test();
    }

    /// **A background queue has one owner**: a stale token (the menu fading out) cannot clear what
    /// a newer park queued.
    #[test]
    fn a_stale_owner_cannot_clear_a_newer_background_queue() {
        let _g = tv_base::testlock::serial();
        reset_prewarm_for_test();
        PREWARM.with(|q| q.borrow_mut().push_back(WarmKey { bytes: b"old".to_vec(), sz: 20, bold: 0 }));
        let old = park_prewarm_as_background();
        PREWARM.with(|q| q.borrow_mut().push_back(WarmKey { bytes: b"new".to_vec(), sz: 20, bold: 0 }));
        let new = park_prewarm_as_background();
        assert_ne!(old, new);
        clear_background_prewarm_owned(old);
        assert!(background_prewarm_pending(), "the old owner's drop left the new queue alone");
        clear_background_prewarm_owned(new);
        assert!(!background_prewarm_pending(), "the current owner clears it");
    }
}
