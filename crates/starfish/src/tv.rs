// Copyright (c) 2026 Gleb Linnik
// Copyright (C) 2026 the webos-iptv-native authors
// SPDX-License-Identifier: GPL-3.0-or-later
// Rust port of PlxNative's src/starfish.c, by way of StremioNative's native/starfish.c.

//! The television half of the seam: every LG symbol, and every exported verb.
//!
//! How each LG symbol is resolved follows the C exactly, because the choice is about which
//! firmware the binary can start on:
//!
//! - libplayerAPIs and libpf-1.0 keep the same SONAME on every release from 2.2.3 to 11.2.0, so
//!   their methods are linked normally (`#[link_name]` with the mangled name, which is what the C's
//!   `__asm__` labels did), with real link-time symbol checking.
//! - Feed (two string ABIs), the COW string destructor, the Load-with-context overload and the real
//!   `callbackFunctionHook` are `dlsym`'d: a strong reference to a name a firmware lacks is a loader
//!   failure before main, or a lazily bound PLT slot that kills the process mid-playback.
//! - `libAcbAPI.so.1` is `dlopen`'d (webOS 5 deleted it, and a DT_NEEDED entry for it kills the
//!   process at exec(), before the event log is open), and the SDL exported-window functions are
//!   `dlsym`'d out of the already-loaded libSDL2 (naming them at link time would demand symbols
//!   webOS 4.5 does not have).
//!
//! # The contract of every export
//!
//! These are called only through `tv_media::player::ffi`, whose `MainThread` token carries the
//! contract: every verb runs on the main thread, except `sf_load` (the media worker, by design:
//! Load blocks for the pipeline construction) and `sf_unload` (the `sf-unload` worker, on an
//! `UnloadTicket`). Pointer arguments are NULL or NUL-terminated C strings, except `sf_feed`'s `p`,
//! which addresses `size` readable bytes until the call returns. The C kept plain statics under
//! the same contract; [`MainThreadCell`] is where this file relies on it. The contract is stated
//! once here rather than as a `# Safety` section on each of the 32 exports.
#![allow(clippy::missing_safety_doc)]

use std::cell::UnsafeCell;
use std::ffi::{c_char, c_int, c_long, c_uint, c_void, CStr};
use std::mem::{size_of, MaybeUninit};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::OnceLock;

use crate::elog;
use crate::lifecycle::{Lifecycle, Native};
use crate::resolve::{self, AcbProbe, FeedAbi, HookEvidence, Plane};
use crate::text::{self, Ptr};

const _: () = assert!(
    size_of::<*const c_void>() == 4,
    "the Starfish seam requires the 32-bit ARM ABI"
);

// ---- libplayerAPIs: `this` is the over-allocated in-place object (see `lifecycle`), in r0. ----
#[link(name = "playerAPIs")]
extern "C" {
    /// The ARM C++ ABI's constructor returns `this`; nothing reads it.
    #[link_name = "_ZN17StarfishMediaAPIsC1EPKc"]
    fn smp_ctor(this: *mut c_void, uid: *const c_char);
    #[link_name = "_ZN17StarfishMediaAPIsD1Ev"]
    fn smp_dtor(this: *mut c_void);
    #[link_name = "_ZN17StarfishMediaAPIs4PlayEv"]
    fn smp_play(this: *mut c_void) -> c_int;
    #[link_name = "_ZN17StarfishMediaAPIs6UnloadEv"]
    fn smp_unload(this: *mut c_void) -> c_int;
    #[link_name = "_ZN17StarfishMediaAPIs16notifyForegroundEv"]
    fn smp_notify_foreground(this: *mut c_void);
    #[link_name = "_ZN17StarfishMediaAPIs15isLoadCompletedEv"]
    fn smp_is_load_completed(this: *mut c_void) -> c_int;
    #[link_name = "_ZN17StarfishMediaAPIs5PauseEv"]
    fn smp_pause(this: *mut c_void) -> c_int;
    #[link_name = "_ZN17StarfishMediaAPIs5flushEv"]
    fn smp_flush(this: *mut c_void) -> c_int;
    /// LG's PUBLIC seek: `bool Seek(const char *millis)`, decimal milliseconds; a negative value is
    /// refused. Present on every firmware 1.2-11.2. Decompiled on 4.5: it parses the argument, takes
    /// the Play() mutex and runs a FLUSH|KEY_UNIT gst_element_seek whose segment starts at the
    /// millis passed: no private offset, no decoder re-init.
    #[link_name = "_ZN17StarfishMediaAPIs4SeekEPKc"]
    fn smp_seek(this: *mut c_void, millis: *const c_char) -> c_int;
    /// Per-pipeline output volume (the paused-preroll mute): `bool setVolume(const char *json)`.
    /// Present on every firmware 4.4.2-11.2.0 and declared in the NDK header. Decompiled on 4.x: it
    /// parses `{"volume":<int>}` and, without an "ease" block, applies it at once; its bool proves
    /// nothing (it returns 1 for a null element). Not setAudioMute (absent on 4.4.2 / 4.10.0).
    ///
    /// getVolume is DELIBERATELY NOT BOUND. The NDK header says `const char *getVolume()`, but the
    /// 4.5 libplayerAPIs tail-calls the pipeline's vtable slot 0xc8, and libpf's implementations
    /// return a plain int (the g_object_get "volume" value, 100 when absent, -1 from the base
    /// class). Reading that int as a char* crashed the TV (SIGSEGV, r0=0x64 into strstr). The
    /// mangled name carries no return type and only one firmware was decompiled, so no readback is
    /// declared: the preroll restores the pipeline default (100).
    #[link_name = "_ZN17StarfishMediaAPIs9setVolumeEPKc"]
    fn smp_set_volume(this: *mut c_void, json: *const c_char) -> c_int;
    /// Kodi parity: signal true end-of-stream so the pipeline drains the last frames instead of
    /// hanging on them. Verified present on webOS 4.5.
    #[link_name = "_ZN17StarfishMediaAPIs7pushEOSEv"]
    fn smp_push_eos(this: *mut c_void) -> c_int;
    /// Kodi in-place seek: `setTimeToDecode({"position":<ns>})`, then the CustomPipeline's
    /// sendSegmentEvent().
    #[link_name = "_ZN17StarfishMediaAPIs15setTimeToDecodeEPKc"]
    fn smp_set_time_to_decode(this: *mut c_void, json: *const c_char) -> c_int;
}

// ---- libpf-1.0: mediapipeline::CustomPipeline methods, `this` = the pipeline (see `pipeline`). ----
#[link(name = "pf-1.0")]
extern "C" {
    #[link_name = "_ZN13mediapipeline14CustomPipeline16sendSegmentEventEv"]
    fn cp_send_segment_event(pipeline: *mut c_void);
    /// The webOS<11 in-place-seek fallback (the path the official app and Kodi use): fills the whole
    /// 312-byte MEDIA_CUSTOM_CONTENT_INFO from current state (it memsets 0x138 and repopulates via
    /// config_contentinfo). Verified by decompile of libpf-1.0.so.1: struct size 0x138,
    /// ptsToDecode at 0x28, SRC_TYPE_ES = 7.
    #[link_name = "_ZN13mediapipeline14CustomPipeline15loadSpi_getInfoEP25MEDIA_CUSTOM_CONTENT_INFO"]
    fn cp_load_spi_get_info(pipeline: *mut c_void, ci: *mut c_void) -> c_int;
    /// memcpy's the struct back into the running pipeline.
    #[link_name = "_ZN13mediapipeline14CustomPipeline14setContentInfoE23MEDIA_CUSTOM_SRC_TYPE_TP25MEDIA_CUSTOM_CONTENT_INFO"]
    fn cp_set_content_info(pipeline: *mut c_void, src_type: c_int, ci: *mut c_void);
}

// ---- the player engine's library-thread handlers (tv_media::player, #[no_mangle]) ----
extern "C" {
    pub(crate) fn sf_on_event(epoch: c_uint, ty: c_int, num: i64, s: *const c_char);
    pub(crate) fn acb_on_event(ev: c_long, reply: *const c_char);
}

/// The Load callback: `void (*)(int type, long long num, const char *str, void *ctx)`.
type SfCallback = unsafe extern "C" fn(c_int, i64, *const c_char, *mut c_void);
/// `bool StarfishMediaAPIs::Load(const char *, void (*)(int, long long, const char *, void *), void *)`.
type LoadWithContextFn =
    unsafe extern "C" fn(*mut c_void, *const c_char, SfCallback, *mut c_void) -> c_int;
/// `void StarfishMediaAPIs::callbackFunctionHook(int, long long, const char *)`.
type CallbackHookFn = unsafe extern "C" fn(*mut c_void, c_int, i64, *const c_char);
/// `std::string StarfishMediaAPIs::Feed(const char *)`: on ARM the hidden sret slot comes first
/// (r0), then `this` (r1), then the argument.
type FeedFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char);
/// `std::basic_string<char>::~basic_string()` of the library's string ABI (COW or cxx11).
type StringDtorFn = unsafe extern "C" fn(*mut c_void);

const HOOK_SYMBOL: &CStr = c"_ZN17StarfishMediaAPIs20callbackFunctionHookEixPKc";
/// Callback-context overload, device-proven on webOS 4.10.2: it stores cb at object+0x9c and ctx
/// at +0xa0, then tail-calls the ordinary Load, which preserves Load's integer result in r0.
/// `sendCallbackEvent` dispatches the callback with that exact context, which is what lets Rust
/// reject a late event from a retired Load without guessing from PTS or payload. `dlsym`'d, not a
/// strong reference: only this firmware artifact proves the overload's behaviour, so a release
/// lacking it must refuse playback at run time, not prevent the app from reaching main.
const LOAD_WITH_CONTEXT_SYMBOL: &CStr = c"_ZN17StarfishMediaAPIs4LoadEPKcPFvixS1_PvES2_";

/// State the C kept in plain file-scope statics, touched only under the main-thread contract
/// (module doc), so no two accesses ever overlap.
struct MainThreadCell<T>(UnsafeCell<T>);

// SAFETY: every access happens on the main thread (the export contract in the module doc).
unsafe impl<T: Send> Sync for MainThreadCell<T> {}

impl<T> MainThreadCell<T> {
    const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    fn get(&self) -> *mut T {
        self.0.get()
    }
}

/// `dlsym` typed as the function pointer `F`; `None` when the symbol is absent.
///
/// # Safety
/// `handle` is `RTLD_DEFAULT`, `RTLD_NEXT` or a live `dlopen` handle, and `F` is the
/// `unsafe extern "C" fn` type of the symbol's real ABI.
unsafe fn dlsym_fn<F: Copy>(handle: *mut c_void, name: &CStr) -> Option<F> {
    const { assert!(size_of::<F>() == size_of::<*mut c_void>()) };
    // SAFETY: a NUL-terminated name and a valid handle (this fn's contract).
    let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
    // SAFETY: a non-NULL code address, read as an `F` of the same size; the caller vouches for its
    // type.
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) })
}

/// `dlsym` with `dlerror` cleared before and checked after: the address, and whether the lookup
/// reported no error.
///
/// # Safety
/// As [`dlsym_fn`]'s handle.
unsafe fn dlsym_checked(handle: *mut c_void, name: &CStr) -> (*mut c_void, bool) {
    // SAFETY: dlerror takes no arguments; dlsym gets a valid handle and a NUL-terminated name.
    unsafe {
        libc::dlerror();
        let p = libc::dlsym(handle, name.as_ptr());
        (p, libc::dlerror().is_null())
    }
}

/// The file `dladdr` says defines `p`.
fn owner_of(p: *mut c_void) -> Option<&'static CStr> {
    if p.is_null() {
        return None;
    }
    let mut info = MaybeUninit::<libc::Dl_info>::zeroed();
    // SAFETY: `info` is writable storage for one Dl_info; an all-zero Dl_info is valid (four
    // pointers), so it is initialized whether or not dladdr writes it.
    let (found, info) = unsafe { (libc::dladdr(p, info.as_mut_ptr()) != 0, info.assume_init()) };
    // SAFETY: dli_fname points at the loader's NUL-terminated path of a library that is never
    // unloaded (libplayerAPIs is DT_NEEDED), so it lives for the process.
    (found && !info.dli_fname.is_null()).then(|| unsafe { CStr::from_ptr(info.dli_fname) })
}

fn path_or_none(p: Option<&CStr>) -> std::borrow::Cow<'_, str> {
    p.map_or("(none)".into(), CStr::to_string_lossy)
}

// ---- the lifecycle's LG half ----

/// The real hook and the Load-with-context overload, once [`Lg::resolve`] proved them.
struct HookFns {
    real: CallbackHookFn,
    load: LoadWithContextFn,
}

struct Lg {
    hook: OnceLock<HookFns>,
}

impl Native for Lg {
    fn resolve(&self) -> bool {
        // SAFETY: RTLD_NEXT from this executable finds the next definition, libplayerAPIs' own.
        let (real, hook_lookup_ok) = unsafe { dlsym_checked(libc::RTLD_NEXT, HOOK_SYMBOL) };
        let ours = sf_callback_hook_interposer as CallbackHookFn as *mut c_void;
        let hook_is_ours = real == ours;
        let hook_owner = if hook_is_ours { None } else { owner_of(real) };
        // SAFETY: RTLD_DEFAULT is a valid handle.
        let (load, load_lookup_ok) =
            unsafe { dlsym_checked(libc::RTLD_DEFAULT, LOAD_WITH_CONTEXT_SYMBOL) };
        let load_owner = owner_of(load);
        let valid = resolve::hook_proven(&HookEvidence {
            hook_lookup_ok,
            hook_found: !real.is_null(),
            hook_is_ours,
            hook_owner,
            load_lookup_ok,
            load_found: !load.is_null(),
            load_owner,
        });
        if !valid {
            elog!(
                "native callback gate: BLOCKED symbol={} real={} owner={} hookLookup={} \
                 loadSymbol={} load={} loadOwner={} loadLookup={}",
                HOOK_SYMBOL.to_string_lossy(),
                Ptr::of(real),
                path_or_none(hook_owner),
                hook_lookup_ok as c_int,
                LOAD_WITH_CONTEXT_SYMBOL.to_string_lossy(),
                Ptr::of(load),
                path_or_none(load_owner),
                load_lookup_ok as c_int,
            );
            return false;
        }
        // SAFETY: both are non-NULL definitions in libplayerAPIs of the mangled names above, whose
        // parameter lists the two type aliases spell out.
        let fns = unsafe {
            HookFns {
                real: std::mem::transmute::<*mut c_void, CallbackHookFn>(real),
                load: std::mem::transmute::<*mut c_void, LoadWithContextFn>(load),
            }
        };
        let _ = self.hook.set(fns);
        elog!(
            "native callback gate: hook={} owner={} loadCtx={} loadOwner={}",
            Ptr::of(real),
            path_or_none(hook_owner),
            Ptr::of(load),
            path_or_none(load_owner),
        );
        true
    }

    unsafe fn construct(&self, object: *mut c_void) {
        // SAFETY: a fresh zeroed 64 KB slot (the trait's contract). uid NULL: the object registers
        // on the pre-authorized uMS namespace, and this process never LSRegisters as a
        // com.webos.media client of its own.
        unsafe { smp_ctor(object, ptr::null()) }
    }

    unsafe fn notify_foreground(&self, object: *mut c_void) {
        // SAFETY: a constructed object (the trait's contract).
        unsafe { smp_notify_foreground(object) }
    }

    unsafe fn load(&self, object: *mut c_void, payload: *const c_char, epoch: c_uint) -> c_int {
        let Some(fns) = self.hook.get() else { return 0 };
        // SAFETY: a constructed object and a NUL-terminated payload (the trait's contract). The
        // epoch travels as the callback context, a plain integer the firmware hands back.
        unsafe {
            (fns.load)(
                object,
                payload,
                sf_cb,
                ptr::without_provenance_mut(epoch as usize),
            )
        }
    }

    unsafe fn forward(&self, object: *mut c_void, ty: c_int, num: i64, s: *const c_char) {
        if let Some(fns) = self.hook.get() {
            // SAFETY: the firmware's own arguments to the hook it would have called itself.
            unsafe { (fns.real)(object, ty, num, s) }
        }
    }

    unsafe fn destruct(&self, object: *mut c_void) {
        // SAFETY: constructed, unloaded and unreachable by callbacks (the trait's contract).
        unsafe { smp_dtor(object) }
    }
}

static SEAM: Lifecycle<Lg> = Lifecycle::new(Lg {
    hook: OnceLock::new(),
});

/// The executable's definition of `StarfishMediaAPIs::callbackFunctionHook`, which libplayerAPIs'
/// libpf thunk binds to through its preemptible JUMP_SLOT once the final link exports exactly this
/// symbol (`--export-dynamic-symbol`). The firmware's own definition is reached with `RTLD_NEXT`.
#[export_name = "_ZN17StarfishMediaAPIs20callbackFunctionHookEixPKc"]
pub unsafe extern "C" fn sf_callback_hook_interposer(
    this: *mut c_void,
    ty: c_int,
    num: i64,
    s: *const c_char,
) {
    // SAFETY: the firmware's own call, passed through unchanged.
    unsafe { SEAM.intercept(this, ty, num, s) }
}

/// Trampoline: forward Load's library-thread events to the engine.
unsafe extern "C" fn sf_cb(ty: c_int, num: i64, s: *const c_char, ctx: *mut c_void) {
    let epoch = ctx.addr() as c_uint;
    SEAM.observe_event(epoch, ty);
    // SAFETY: the engine's handler takes the firmware's arguments as they came.
    unsafe { sf_on_event(epoch, ty, num, s) }
}

/// Trampoline: `void (*)(long acbId, long taskId, long eventType, long appState, long playState,
/// const char *reply)` to the engine's handler.
unsafe extern "C" fn acb_cb(
    _acb: c_long,
    _task: c_long,
    ev: c_long,
    _app: c_long,
    _play: c_long,
    reply: *const c_char,
) {
    // SAFETY: the engine's handler takes the firmware's reply as it came.
    unsafe { acb_on_event(ev, reply) }
}

// ---- StarfishMediaAPIs verbs ----

/// Run `verb` on the dispatchable object (constructed AND its Load returned), else answer `idle`.
fn with_object<R>(idle: R, verb: impl FnOnce(*mut c_void) -> R) -> R {
    SEAM.ready_object().map_or(idle, verb)
}

#[no_mangle]
pub unsafe extern "C" fn sf_load(payload: *const c_char, epoch: c_uint) -> c_int {
    // SAFETY: payload is NULL or NUL-terminated (the export contract).
    unsafe { SEAM.load(payload, epoch) }
}

#[no_mangle]
pub unsafe extern "C" fn sf_ready() -> c_int {
    SEAM.ready() as c_int
}

#[no_mangle]
pub unsafe extern "C" fn sf_is_load_completed() -> c_int {
    // SAFETY: a dispatchable object.
    with_object(0, |o| unsafe { smp_is_load_completed(o) })
}

#[no_mangle]
pub unsafe extern "C" fn sf_play() -> c_int {
    // SAFETY: a dispatchable object.
    with_object(0, |o| unsafe { smp_play(o) })
}

/// This pipeline's volume (0..100). The library's bool; 0 also when nothing is loaded.
#[no_mangle]
pub unsafe extern "C" fn sf_set_volume(volume: c_int) -> c_int {
    with_object(0, |o| {
        let json = text::volume_json(volume);
        // SAFETY: a dispatchable object; `json` is NUL-terminated and outlives the call. bool in
        // r0: only the low byte is defined.
        unsafe { smp_set_volume(o, json.as_ptr()) & 0xff }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sf_pause() -> c_int {
    // SAFETY: a dispatchable object.
    with_object(0, |o| unsafe { smp_pause(o) })
}

#[no_mangle]
pub unsafe extern "C" fn sf_flush() -> c_int {
    // SAFETY: a dispatchable object.
    with_object(0, |o| unsafe { smp_flush(o) })
}

#[no_mangle]
pub unsafe extern "C" fn sf_push_eos() -> c_int {
    // SAFETY: a dispatchable object.
    with_object(0, |o| unsafe { smp_push_eos(o) })
}

#[no_mangle]
pub unsafe extern "C" fn sf_unload() {
    // SAFETY: a dispatchable object; Unload's result is not used, as in the C.
    with_object((), |o| unsafe {
        smp_unload(o);
    })
}

/// Close native callback admission and wait, without a timeout, for every admitted hook call. 1
/// proves this object's interposer was actually crossed.
#[no_mangle]
pub unsafe extern "C" fn sf_callback_gate_retire() -> c_int {
    SEAM.gate_retire() as c_int
}

#[no_mangle]
pub unsafe extern "C" fn sf_callback_intercepts() -> c_uint {
    SEAM.intercepts()
}

#[no_mangle]
pub unsafe extern "C" fn sf_quarantine() {
    SEAM.quarantine()
}

/// D1 only after gate retirement + an intercepted callback + the synchronous type 23; otherwise
/// quarantine instead of destructing.
#[no_mangle]
pub unsafe extern "C" fn sf_destroy() -> c_int {
    // SAFETY: main thread, and the engine joins the `sf-unload` worker (`settle_release`) before it
    // destroys, so no other verb runs on the object meanwhile.
    unsafe { SEAM.destroy() as c_int }
}

/// `StarfishMediaAPIs::player` is a shared_ptr whose `_M_ptr` sits at object+0x4c.
const PLAYER_OFFSET: usize = 0x4c;
/// `AbstractPlayer::pipeline`'s `_M_ptr` at player+0x4. Pipeline is CustomPipeline's primary
/// base, so the pointer is usable as `this`.
const PIPELINE_OFFSET: usize = 0x04;

/// The CustomPipeline* reached from our object (verified by decompile). player@0x4c is populated
/// on our uid=NULL object: sf_play and sf_flush already dispatch through it.
fn pipeline() -> *mut c_void {
    let Some(object) = SEAM.ready_object() else {
        return ptr::null_mut();
    };
    // SAFETY: `object` is our live 64 KB, 16-aligned storage, so +0x4c is an in-bounds, aligned
    // pointer slot that the constructor and Load filled.
    let player = unsafe {
        object
            .cast::<u8>()
            .add(PLAYER_OFFSET)
            .cast::<*mut c_void>()
            .read()
    };
    if player.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `player` is the library's live AbstractPlayer, held by that shared_ptr.
    unsafe {
        player
            .cast::<u8>()
            .add(PIPELINE_OFFSET)
            .cast::<*mut c_void>()
            .read()
    }
}

/// Kodi in-place seek, on the first video AU after flush(): tell the pipeline the new decode
/// timestamp, then inject a fresh GStreamer SEGMENT (the step a bare flush() omits; its absence is
/// the stale-segment stall the reload path works around). `position_ns` = the fed (0-based rebased)
/// PTS of that first frame. Returns 0 on webOS<11.
#[no_mangle]
pub unsafe extern "C" fn sf_set_time_to_decode(position_ns: i64) -> c_int {
    with_object(0, |o| {
        let json = text::position_json(position_ns);
        // SAFETY: a dispatchable object; `json` is NUL-terminated and outlives the call.
        unsafe { smp_set_time_to_decode(o, json.as_ptr()) }
    })
}

/// Public in-place seek: the pipeline flushes itself and re-anchors its segment at `ms`, so the
/// caller must feed PTS on that same timeline afterwards and must NOT call sf_flush() (a flush is
/// itself a seek to 0). Not safe against a concurrent sf_feed: the caller quiesces the feeder.
/// Returns the library's bool: 0 = refused (negative ms, not loaded, or failed inside the pipeline,
/// which then rejects Feed until a seek/flush succeeds).
#[no_mangle]
pub unsafe extern "C" fn sf_seek_ms(ms: i64) -> c_int {
    with_object(0, |o| {
        let arg = text::seek_arg(ms);
        // SAFETY: a dispatchable object; `arg` is NUL-terminated and outlives the call. bool in
        // r0: only the low byte is defined.
        let rv = unsafe { smp_seek(o, arg.as_ptr()) & 0xff };
        elog!("Seek({ms}) -> {rv}");
        rv
    })
}

#[no_mangle]
pub unsafe extern "C" fn sf_send_segment() -> c_int {
    let p = pipeline();
    elog!("sendSegment: pipeline={}", Ptr::of(p));
    if p.is_null() {
        return 0;
    }
    // SAFETY: the live CustomPipeline reached from the dispatchable object.
    unsafe { cp_send_segment_event(p) };
    1
}

#[repr(C, align(16))]
struct ContentInfo([u8; CI_BUFFER]);

/// Over-allocated MEDIA_CUSTOM_CONTENT_INFO_T (real size 0x138); libpf owns the layout.
const CI_BUFFER: usize = 1024;
/// int64 ptsToDecode (verified three ways).
const CI_PTS_OFFSET: usize = 0x28;
/// MEDIA_CUSTOM_SRC_TYPE_T::ES (verified).
const MEDIA_CUSTOM_SRC_TYPE_ES: c_int = 7;

static CONTENT_INFO: MainThreadCell<ContentInfo> = MainThreadCell::new(ContentInfo([0; CI_BUFFER]));

/// webOS<11 in-place seek: the fallback for when setTimeToDecode returns 0 (it dispatches to
/// CustomPlayer::setTimeToDecodeSpi, which only succeeds while the pipeline is in PausedState).
/// Fill the current content info, override ptsToDecode, write it back. Call on the first post-flush
/// keyframe, BEFORE sf_send_segment(). Returns 0 only if the pipeline is not reachable.
#[no_mangle]
pub unsafe extern "C" fn sf_set_content_info(position_ns: i64) -> c_int {
    let p = pipeline();
    if p.is_null() {
        return 0;
    }
    let ci = CONTENT_INFO.get().cast::<u8>();
    // SAFETY: main thread only, so nothing else touches the buffer. It is 1024 bytes for libpf's
    // 312-byte struct and 16-aligned, so +0x28 is an aligned, in-bounds i64. `p` is the live
    // pipeline.
    unsafe {
        ci.write_bytes(0, CI_BUFFER);
        cp_load_spi_get_info(p, ci.cast());
        ci.add(CI_PTS_OFFSET).cast::<i64>().write(position_ns);
        cp_set_content_info(p, MEDIA_CUSTOM_SRC_TYPE_ES, ci.cast());
    }
    elog!("setContentInfo: pts={position_ns} ES={MEDIA_CUSTOM_SRC_TYPE_ES}");
    1
}

static FEED: OnceLock<FeedAbi<FeedFn, StringDtorFn>> = OnceLock::new();

/// Feed returns std::string, so its mangled name carries the library's string ABI (`resolve`).
/// Either way the hidden sret slot starts with the char pointer, so the reply reads the same, and
/// either way the string goes back through the firmware's own destructor for that ABI.
fn feed_abi() -> &'static FeedAbi<FeedFn, StringDtorFn> {
    FEED.get_or_init(|| {
        let abi = resolve::resolve_feed(
            // SAFETY: RTLD_DEFAULT; both Feed names have FeedFn's ABI (sret, this, const char *).
            |name| unsafe { dlsym_fn::<FeedFn>(libc::RTLD_DEFAULT, name) },
            // SAFETY: RTLD_DEFAULT; a destructor takes only `this`.
            |name| unsafe { dlsym_fn::<StringDtorFn>(libc::RTLD_DEFAULT, name) },
        );
        elog!(
            "feed abi: {} fn={} dtor={}",
            abi.label(),
            Ptr(abi.feed.map_or(0, |f| f as usize)),
            Ptr(abi.reply_dtor.map_or(0, |d| d as usize)),
        );
        abi
    })
}

/// Room for Feed's std::string result: 24 bytes under cxx11 (pointer, length, 16-byte SSO buffer),
/// 4 under COW.
#[repr(C, align(8))]
struct SretString([u8; 32]);

static FEED_REPLIES_LOGGED: AtomicU32 = AtomicU32::new(0);

/// Feed one access unit; hides the sret std::string. 'O' (Ok), 'B' (BufferFull) or 'e'.
#[no_mangle]
pub unsafe extern "C" fn sf_feed(p: *const u8, size: c_uint, pts: i64, es_data: c_int) -> c_char {
    // The only verb that once dispatched with NO readiness check, relying on the pump returning
    // early. Guarded here too, so the object can never be fed mid-constructor.
    let Some(object) = SEAM.ready_object() else {
        return b'e' as c_char;
    };
    let abi = feed_abi();
    let Some(feed) = abi.feed else {
        return b'e' as c_char;
    };
    let json = text::feed_json(Ptr::of(p), size, pts, es_data);
    let mut sret = SretString([0; 32]);
    let slot = sret.0.as_mut_ptr();
    // SAFETY: Feed's real ABI is (sret, this, payload); `slot` is 32 zeroed, 8-aligned bytes,
    // larger than either string object, and stays in place until the destructor below. The object
    // is dispatchable and `json` is NUL-terminated. LG reads `p` (named inside the JSON) during
    // the call, which is the caller's contract.
    unsafe { feed(slot.cast(), object, json.as_ptr()) };
    // SAFETY: the string's `_M_p` is at offset 0 under both ABIs; `slot` is aligned for it.
    let s = unsafe { slot.cast::<*const c_char>().read() };
    // SAFETY: a non-NULL `_M_p` points at the string's NUL-terminated buffer (SSO inside `sret`, a
    // cxx11 heap buffer or the COW heap rep), alive until the destructor below.
    let reply = (!s.is_null()).then(|| unsafe { CStr::from_ptr(s) });
    if crate::EVENT_LOG.get().is_some() && FEED_REPLIES_LOGGED.load(Relaxed) < 3 {
        FEED_REPLIES_LOGGED.fetch_add(1, Relaxed);
        elog!(
            "feed reply=\"{}\"",
            reply.map_or("(null)".into(), CStr::to_string_lossy)
        );
    }
    let verdict = text::feed_verdict(reply);
    // Every reply is destroyed, cxx11 ones too (`resolve::CXX11_STRING_DTOR`: webOS 25's reply
    // outgrows the SSO buffer). The COW destructor reads the rep header just below `_M_p`, so a
    // NULL `_M_p` (Feed returned without constructing its result) is skipped.
    if let (false, Some(dtor)) = (s.is_null(), abi.reply_dtor) {
        // SAFETY: a std::string of the ABI `dtor` belongs to (`resolve_feed` pairs them), which
        // the firmware constructed in `slot`; destroyed exactly once. An SSO reply's `_M_p` points
        // into `slot` itself, which the destructor recognises and does not free.
        unsafe { dtor(slot.cast()) };
    }
    verdict as c_char
}

/// 1 when the library's Feed is the pre-C++11 copy-on-write build: direct evidence that its
/// StarfishMediaAPIs layout is not the 4.5 one the in-place seek offsets were decompiled from.
#[no_mangle]
pub unsafe extern "C" fn sf_feed_abi_is_cow() -> c_int {
    feed_abi().cow as c_int
}

// ---- the video-plane binding: decoded sink -> display plane ----
//
// Decoded video does not reach the panel by itself: something has to tell the TV to punch a hole in
// the compositor and route the pipeline's sink into it. LG changed how exactly once, at webOS 5.0,
// and the two mechanisms are complementary across every firmware, never both:
//
//   webOS 2.2.3 .. 4.10.0   libAcbAPI.so.1 present, SDL exports no exported-window entry points
//   webOS 5.3.1 .. 11.2.0   libAcbAPI.so.1 DELETED, SDL exports all of them
//
// (Verified upstream against 14 real firmware inventories.) So both are resolved at run time and
// one is picked (`resolve::select_plane`). The exported-window call shapes come from the two
// implementations that ship (Kodi and mariotaku's ss4s); upstream device-verified them on webOS
// 6.5.2.

/// libAcbAPI's C entry points. Audio is owned by the pipeline: ACB is never handed an audio sink or
/// elementary stream.
struct AcbApi {
    create: unsafe extern "C" fn() -> c_long,
    initialize: unsafe extern "C" fn(c_long, c_int, *const c_char, AcbCallback) -> c_int,
    set_sink_type: unsafe extern "C" fn(c_long, c_int) -> c_int,
    set_media_id: unsafe extern "C" fn(c_long, *const c_char) -> c_int,
    /// 3-arg ABI CONFIRMED: createTask(TaskType, long*) writes the task id through arg3; the 2-arg
    /// form leaves garbage in r2 and segfaults or corrupts memory. The same holds for every taskId
    /// out-param below.
    set_media_video_data: unsafe extern "C" fn(c_long, *const c_char, *mut c_long) -> c_int,
    set_state: unsafe extern "C" fn(c_long, c_int, c_int, *mut c_long) -> c_int,
    set_display_window:
        unsafe extern "C" fn(c_long, c_long, c_long, c_long, c_long, c_int, *mut c_long) -> c_int,
    /// OPTIONAL, deliberately not required: exported on webOS 3.9.2 / 4.4.2 / 4.10.0 but NOT on 2.2.3
    /// or 3.4.0. Requiring it would refuse ACB outright on those two releases and take ALL VIDEO
    /// with it; a Dolby Atmos read-out is not worth a black screen. See `acb_send_atmos`.
    set_media_audio_data: Option<unsafe extern "C" fn(c_long, *const c_char, *mut c_long) -> c_int>,
}

type AcbCallback = unsafe extern "C" fn(c_long, c_long, c_long, c_long, c_long, *const c_char);

/// LG's SDL fork's exported-window entry points.
struct SdlApi {
    create: unsafe extern "C" fn(c_int) -> *const c_char,
    set: unsafe extern "C" fn(*const c_char, *mut VpRect, *mut VpRect) -> c_int,
    destroy: Option<unsafe extern "C" fn(*const c_char)>,
}

/// Same layout as SDL_Rect, four ints since SDL2 existed. Declared here: the NDK's SDL headers
/// predate SDL_webOS.h.
#[repr(C)]
struct VpRect {
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
}

/// The webOS 5 exported-window type. LG's header spells the constant EXPORED (sic); the literal
/// avoids inheriting that.
const VP_EXPORTED_TYPE_VIDEO: c_int = 0;

static PLANE: OnceLock<Plane<AcbApi, SdlApi>> = OnceLock::new();

fn plane() -> &'static Plane<AcbApi, SdlApi> {
    PLANE.get_or_init(|| {
        let plane = resolve::select_plane(probe_acb(), probe_sdl);
        match plane {
            Plane::Acb(_) => elog!("vplane: ACB (webOS 4.x)"),
            Plane::Exported(_) => elog!("vplane: SDL exported window (webOS 5+)"),
            Plane::None => {
                elog!(
                    "vplane: NONE - no libAcbAPI and no SDL exported window; video cannot be shown"
                )
            }
        }
        plane
    })
}

fn probe_acb() -> AcbProbe<AcbApi> {
    // RTLD_NOW, so a half-usable library is rejected here rather than mid-playback. Never closed.
    // SAFETY: a NUL-terminated library name.
    let h = unsafe {
        libc::dlopen(
            c"libAcbAPI.so.1".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_GLOBAL,
        )
    };
    if h.is_null() {
        return AcbProbe::Absent;
    }
    // SAFETY: a live dlopen handle.
    match unsafe { acb_table(h) } {
        Some(api) => AcbProbe::Complete(api),
        None => {
            elog!("vplane: libAcbAPI.so.1 opened but is missing entry points");
            AcbProbe::Incomplete
        }
    }
}

/// Every pointer resolved here is required, except setMediaAudioData. Add one only when something
/// calls it: upstream once checked seven of eight and dropped `destroy` silently, which is why
/// `destroy` and the unused SDL crop/property pointers are not resolved at all.
///
/// # Safety
/// `h` is a live `dlopen` handle of libAcbAPI.
unsafe fn acb_table(h: *mut c_void) -> Option<AcbApi> {
    // SAFETY: a live handle (this fn's contract); each field's type is the AcbAPI C signature of
    // that name.
    unsafe {
        Some(AcbApi {
            create: dlsym_fn(h, c"AcbAPI_create")?,
            initialize: dlsym_fn(h, c"AcbAPI_initialize")?,
            set_sink_type: dlsym_fn(h, c"AcbAPI_setSinkType")?,
            set_media_id: dlsym_fn(h, c"AcbAPI_setMediaId")?,
            set_media_video_data: dlsym_fn(h, c"AcbAPI_setMediaVideoData")?,
            set_state: dlsym_fn(h, c"AcbAPI_setState")?,
            set_display_window: dlsym_fn(h, c"AcbAPI_setDisplayWindow")?,
            set_media_audio_data: dlsym_fn(h, c"AcbAPI_setMediaAudioData"),
        })
    }
}

/// RTLD_DEFAULT: libSDL2 is a normal DT_NEEDED dependency, so its symbols are already in the global
/// scope. This asks "did the SDL that actually loaded bring these", which is exactly the question:
/// the NDK's sysroot copy carries stub bodies for them, so a link-time check would say yes on every
/// device and mean nothing.
fn probe_sdl() -> Option<SdlApi> {
    // SAFETY: RTLD_DEFAULT; each field's type is the LG SDL signature of that name.
    unsafe {
        Some(SdlApi {
            create: dlsym_fn(libc::RTLD_DEFAULT, c"SDL_webOSCreateExportedWindow")?,
            set: dlsym_fn(libc::RTLD_DEFAULT, c"SDL_webOSSetExportedWindow")?,
            destroy: dlsym_fn(libc::RTLD_DEFAULT, c"SDL_webOSDestroyExportedWindow"),
        })
    }
}

fn exported() -> Option<&'static SdlApi> {
    match plane() {
        Plane::Exported(sdl) => Some(sdl),
        _ => None,
    }
}

/// Our copy of SDL's window id; zero-initialised, so "" (never NULL) is the "no window" answer.
static WINDOW_ID: MainThreadCell<[u8; 64]> = MainThreadCell::new([0; 64]);

#[no_mangle]
pub unsafe extern "C" fn vp_mode() -> c_int {
    plane().mode()
}

/// VP_EXPORTED only: create the exported window and return its id, or NULL.
///
/// Must be called BEFORE Load, because the id has to travel inside the Load payload as
/// option.windowId: that string is the whole of the binding on webOS 5+. The compositor assigns it
/// ("_Window_Id_<n>") and SDL hands back a pointer whose ownership and lifetime LG does not
/// document, so it is copied at once; ss4s does the same.
#[no_mangle]
pub unsafe extern "C" fn vp_create_window() -> *const c_char {
    let Some(sdl) = exported() else {
        return ptr::null();
    };
    // SAFETY: SDL_webOSCreateExportedWindow(int type).
    let id = unsafe { (sdl.create)(VP_EXPORTED_TYPE_VIDEO) };
    if id.is_null() {
        elog!("vplane: SDL_webOSCreateExportedWindow returned NULL");
        return ptr::null();
    }
    // SAFETY: SDL returned a NUL-terminated id; main thread only, so WINDOW_ID has no other
    // accessor during the copy.
    let window_id = unsafe {
        let window_id = &mut *WINDOW_ID.get();
        text::copy_c_string(window_id, CStr::from_ptr(id));
        CStr::from_bytes_until_nul(window_id).unwrap_or_default()
    };
    elog!("vplane: exported windowId={}", window_id.to_string_lossy());
    window_id.as_ptr()
}

/// The exported window id we hold, or "" when there is none: for the diagnostics read-out, which
/// needs to say whether the window was ever created. This module's own copy, so the pointer is
/// permanently valid and never owned by the caller.
#[no_mangle]
pub unsafe extern "C" fn vp_window_id() -> *const c_char {
    WINDOW_ID.get().cast_const().cast()
}

/// Place the video: a part of the source frame -> on-screen rect. The VP_EXPORTED counterpart of
/// acb_start's setDisplayWindow; the pair also expresses scaling, and a source part smaller than the
/// frame is a crop (the aspect zooms), which keeps the window on the screen.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn vp_place(
    src_x: c_int,
    src_y: c_int,
    src_w: c_int,
    src_h: c_int,
    dst_x: c_int,
    dst_y: c_int,
    dst_w: c_int,
    dst_h: c_int,
) -> c_int {
    let Some(sdl) = exported() else { return 0 };
    let window_id = WINDOW_ID.get();
    // SAFETY: main thread only.
    if unsafe { (*window_id)[0] } == 0 {
        return 0;
    }
    let mut src = VpRect {
        x: src_x,
        y: src_y,
        w: src_w,
        h: src_h,
    };
    let mut dst = VpRect {
        x: dst_x,
        y: dst_y,
        w: dst_w,
        h: dst_h,
    };
    // SAFETY: SDL_webOSSetExportedWindow(const char *windowId, SDL_Rect *src, SDL_Rect *dst) with our
    // NUL-terminated id copy and two rects that live across the call.
    let rv = unsafe { (sdl.set)(window_id.cast(), &mut src, &mut dst) };
    elog!(
        "vplane: place {src_x},{src_y} {src_w}x{src_h} -> {dst_x},{dst_y} {dst_w}x{dst_h} rv={rv}"
    );
    rv
}

#[no_mangle]
pub unsafe extern "C" fn vp_destroy_window() {
    // SAFETY: main thread only.
    let window_id = unsafe { &mut *WINDOW_ID.get() };
    // Not `plane()`: a window id exists only once the plane resolved as exported.
    let destroy = match PLANE.get() {
        Some(Plane::Exported(sdl)) => sdl.destroy,
        _ => None,
    };
    if let Some(destroy) = destroy.filter(|_| window_id[0] != 0) {
        // SAFETY: the id SDL assigned, as our NUL-terminated copy.
        unsafe { destroy(window_id.as_ptr().cast()) };
    }
    window_id[0] = 0;
}

// ---- ACB verbs: the 3-arg taskId ABI is hidden ----
//
// Every one is a no-op when this device has no ACB, so the caller does not branch: on webOS 5 the
// whole setSinkType / setMediaId / setMediaVideoData / setState sequence has no replacement, it is
// simply deleted, which is what both reference implementations do (ss4s stubs them to `return
// true`, Kodi guards each with `if (acb)`).
//
// Issue #74 D.1: these dispatch on the ACB handle alone and never consult the lifecycle's
// in-flight-Load gate, unlike the sf_* verbs above. They are bounded only by the pump's stage
// machine: every ACB call that binds or updates media requires the engine to be playing, which it
// reaches only after observing Load return. Extending the gate itself into ACB needs the same device
// verification as any Starfish/ACB bind-order change, so "the gate covers every Starfish/ACB verb"
// is not true of this block until that is done.

const SINK_TYPE_MAIN: c_int = 0;
const APPSTATE_FOREGROUND: c_int = 1;
const PLAYSTATE_UNLOADED: c_int = 0;
const PLAYSTATE_LOADED: c_int = 1;
const PLAYSTATE_PLAYING: c_int = 2;
const PLAYSTATE_PAUSED: c_int = 3;

/// The initialized ACB handle (0 = none) and the task id every call writes back.
static ACB: MainThreadCell<c_long> = MainThreadCell::new(0);
static TASK_ID: MainThreadCell<c_long> = MainThreadCell::new(0);

/// The ACB table and the live handle, when there is one.
fn acb() -> Option<(&'static AcbApi, c_long)> {
    // SAFETY: main thread only.
    let handle = unsafe { *ACB.get() };
    match PLANE.get() {
        Some(Plane::Acb(api)) if handle != 0 => Some((api, handle)),
        _ => None,
    }
}

fn set_state(api: &AcbApi, handle: c_long, play_state: c_int) {
    // SAFETY: a live handle; the task id is written through a main-thread-only cell.
    unsafe { (api.set_state)(handle, APPSTATE_FOREGROUND, play_state, TASK_ID.get()) };
}

/// create + initialize; 0 = failed.
///
/// NO FALLBACK ID. Substituting a shipped app's id for a NULL `app_id` would bind the video plane
/// for the WRONG application when a developer build sits beside it on one television: audio with a
/// black plane and no error line anywhere. The engine passes the id read from the install
/// directory, which cannot be absent; refusing here rather than guessing keeps the pair readable.
#[no_mangle]
pub unsafe extern "C" fn acb_create(app_id: *const c_char, player_type: c_int) -> c_long {
    let Plane::Acb(api) = plane() else { return 0 };
    // SAFETY: AcbAPI_create(void).
    let mut handle = unsafe { (api.create)() };
    if handle != 0 {
        if app_id.is_null() {
            // AND HAND BACK NOTHING: returning the live handle would let acb_bind and friends run
            // against an object the library was never told about. The handle leaks, because no
            // AcbAPI_destroy is resolved; one leaked handle on an unreachable path is the right trade
            // against calling into uninitialized memory.
            elog!("acb: no appId - NOT initialized and NOT returned; video will not bind");
            handle = 0;
        } else {
            // SAFETY: a fresh handle, a NUL-terminated app id (the export contract), and acb_cb has
            // the callback's ABI.
            unsafe { (api.initialize)(handle, player_type, app_id, acb_cb) };
        }
    }
    // SAFETY: main thread only.
    unsafe { *ACB.get() = handle };
    handle
}

/// setSinkType(MAIN) + setMediaId + setState(LOADED).
#[no_mangle]
pub unsafe extern "C" fn acb_bind(media_id: *const c_char) {
    let Some((api, handle)) = acb() else { return };
    // SAFETY: a live handle and a NUL-terminated media id (the export contract).
    unsafe {
        (api.set_sink_type)(handle, SINK_TYPE_MAIN);
        (api.set_media_id)(handle, media_id);
    }
    set_state(api, handle, PLAYSTATE_LOADED);
}

/// Tell ACB the stream is Dolby Atmos: the AUDIO counterpart of acb_send_video_data(), and a
/// METADATA descriptor rather than anything resembling an audio sink.
///
/// Recovered upstream from a television's own binaries: libcbe's
/// `media::MediaAPIsWrapper::SetDolbyAtmosInfoToACB` (the only caller of the ACB audio entry point
/// in the harvested libraries, and the path LG's own web apps take) builds exactly this two-key
/// object and hands it to `Acb::setMediaAudioData` with the 3-arg taskId ABI.
/// `AcbAPI_setMediaAudioData` is instruction for instruction the same shape as
/// `AcbAPI_setMediaVideoData`; below it ACB parses the JSON, dedups against its cached copy and
/// posts luna://com.webos.service.acb/setAudioInfo. It touches no sink, no elementary stream, no
/// codec descriptor and no second bind.
///
/// `context` is the SAME string acb_bind() passed to setMediaId; carrying it is why LG synthesises
/// this object instead of forwarding the pipeline's own AUDIO_INFO callback, which has no context.
///
/// This does not break the old "never feed audio to ACB" rule: what that rule is right about is the
/// audio ES, which the pipeline owns, and ACB is never handed a sink. Its stated consequence
/// (`SOUND_ERROR_019`) appears in no library on the device. The ACB daemon's side of the Luna post is
/// unread, hence the engine arms this behind a trigger rather than assuming it.
///
/// Returns 1 accepted, -1 our JSON was rejected client-side (nothing left the process), -2 ACB not
/// initialized, 0 no ACB / no symbol / no id. Call after acb_bind(), which is where libcbe fires it
/// (on LOADCOMPLETED): no decoded frame is needed and no state is read.
#[no_mangle]
pub unsafe extern "C" fn acb_send_atmos(media_id: *const c_char) -> c_int {
    let Some((api, handle)) = acb() else { return 0 };
    let Some(set_audio) = api.set_media_audio_data else {
        return 0;
    };
    if media_id.is_null() {
        return 0;
    }
    // SAFETY: a non-NULL, NUL-terminated media id (the export contract).
    let json = text::atmos_json(unsafe { CStr::from_ptr(media_id) });
    // SAFETY: a live handle, a NUL-terminated payload that outlives the call, the main-thread task id.
    let rv = unsafe { set_audio(handle, json.as_ptr(), TASK_ID.get()) };
    let payload = json
        .as_bytes()
        .strip_suffix(b"\n")
        .unwrap_or(json.as_bytes());
    elog!(
        "acb setMediaAudioData rv={rv} payload={}",
        String::from_utf8_lossy(payload)
    );
    rv
}

/// setMediaVideoData with the pipeline's own envelope (vp9_hdr may have added HDR keys); -1 =
/// rejected.
#[no_mangle]
pub unsafe extern "C" fn acb_send_video_data(source_info: *const c_char) -> c_int {
    let Some((api, handle)) = acb() else { return 0 };
    // SAFETY: a live handle, a NUL-terminated envelope (the export contract), the main-thread task id.
    unsafe { (api.set_media_video_data)(handle, source_info, TASK_ID.get()) }
}

/// setDisplayWindow + setState(PLAYING).
#[no_mangle]
pub unsafe extern "C" fn acb_start(x: c_long, y: c_long, w: c_long, h: c_long) {
    let Some((api, handle)) = acb() else { return };
    // SAFETY: a live handle and the main-thread task id.
    unsafe { (api.set_display_window)(handle, x, y, w, h, 1, TASK_ID.get()) };
    set_state(api, handle, PLAYSTATE_PLAYING);
}

/// setState(UNLOADED).
#[no_mangle]
pub unsafe extern "C" fn acb_unload() {
    if let Some((api, handle)) = acb() {
        set_state(api, handle, PLAYSTATE_UNLOADED);
    }
}

/// Kodi parity: mirror the ACB PLAYSTATE on transport pause/resume (the app owns the sink; the
/// pipeline's Pause/Play alone leave the ACB state stale). Only meaningful once the plane is bound.
#[no_mangle]
pub unsafe extern "C" fn acb_pause() {
    if let Some((api, handle)) = acb() {
        set_state(api, handle, PLAYSTATE_PAUSED);
    }
}

#[no_mangle]
pub unsafe extern "C" fn acb_resume() {
    if let Some((api, handle)) = acb() {
        set_state(api, handle, PLAYSTATE_PLAYING);
    }
}
