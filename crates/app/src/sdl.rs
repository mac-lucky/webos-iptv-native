//! The SDL and GL edge: every `extern "C"` call the app crate makes itself lives here.
//!
//! The television gets a fullscreen 1920x1080 window with an OpenGL ES 2.0 context, RGBA8 so the
//! video plane can show through, and no depth or stencil buffer. The video plane sits under the
//! window's Wayland surface, which the compositor treats as opaque until its opaque region is
//! cleared: [`Video`] borrows SDL's `wl_surface` for that. The simulator gets a
//! desktop window and a 4.1 core context, the only modern profile macOS offers; `tv_gfx` adapts
//! its GLSL ES shaders to it.
//!
//! Main thread only: SDL's video subsystem is not thread-safe and macOS refuses windows anywhere
//! else. [`Video`] holds raw pointers, so it is neither `Send` nor `Sync` and cannot leave it.

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};

pub const SDL_QUIT: u32 = 0x100;
pub const SDL_APP_TERMINATING: u32 = 0x101;
pub const SDL_APP_WILLENTERBACKGROUND: u32 = 0x103;
pub const SDL_APP_DIDENTERBACKGROUND: u32 = 0x104;
pub const SDL_APP_DIDENTERFOREGROUND: u32 = 0x106;
pub const SDL_WINDOWEVENT: u32 = 0x200;
pub const SDL_KEYDOWN: u32 = 0x300;
pub const SDL_KEYUP: u32 = 0x301;
pub const SDL_TEXTINPUT: u32 = 0x303;
pub const SDL_MOUSEMOTION: u32 = 0x400;
pub const SDL_MOUSEBUTTONDOWN: u32 = 0x401;
pub const SDL_MOUSEBUTTONUP: u32 = 0x402;
pub const SDL_MOUSEWHEEL: u32 = 0x403;
const SDL_USEREVENT: u32 = 0x8000;

const SDL_INIT_VIDEO: u32 = 0x20;
const SDL_WINDOW_OPENGL: u32 = 0x2;
const SDL_WINDOW_INPUT_FOCUS: u32 = 0x200;

/// The window asked for: (x and y, divisor of the canvas size, flags). The television owns the
/// whole panel at the authored canvas size.
#[cfg(not(feature = "hostsim"))]
const WINDOW: (c_int, c_int, u32) = (0, 1, SDL_WINDOW_OPENGL | 0x1 /* FULLSCREEN */);
/// The simulator: centred, half the canvas in points with ALLOW_HIGHDPI, which a Retina display
/// backs with a 1920x1080 drawable, the 1:1 surface the television has. Not resizable:
/// `surface::probe` reads the drawable once.
#[cfg(feature = "hostsim")]
const WINDOW: (c_int, c_int, u32) = (
    0x2FFF_0000, /* CENTERED */
    2,
    SDL_WINDOW_OPENGL | 0x2000, /* ALLOW_HIGHDPI */
);

/// The context asked for: (profile, major, minor).
#[cfg(not(feature = "hostsim"))]
const CONTEXT: (c_int, c_int, c_int) = (0x4 /* ES */, 2, 0);
#[cfg(feature = "hostsim")]
const CONTEXT: (c_int, c_int, c_int) = (0x1 /* CORE */, 4, 1);

// SDL_GLattr
const GL_RED_SIZE: c_int = 0;
const GL_GREEN_SIZE: c_int = 1;
const GL_BLUE_SIZE: c_int = 2;
const GL_ALPHA_SIZE: c_int = 3;
const GL_BUFFER_SIZE: c_int = 4;
const GL_DEPTH_SIZE: c_int = 6;
const GL_STENCIL_SIZE: c_int = 7;
const GL_CONTEXT_MAJOR_VERSION: c_int = 17;
const GL_CONTEXT_MINOR_VERSION: c_int = 18;
const GL_CONTEXT_PROFILE_MASK: c_int = 21;

const GL_RENDERER: u32 = 0x1F01;
const GL_VERSION: u32 = 0x1F02;
const GL_ALPHA_BITS: u32 = 0x0D55;

// The television links the system's own libraries from the NDK sysroot. A desktop build gets its
// link lines from `tv_gfx`'s build script (`build_support/host_link.rs`).
#[cfg(all(target_os = "linux", target_arch = "arm"))]
#[link(name = "SDL2")]
#[link(name = "SDL2_ttf")]
#[link(name = "GLESv2")]
#[link(name = "EGL")]
#[link(name = "wayland-client")]
extern "C" {}

/// `SDL_SYSWM_WAYLAND` in SDL2's `SDL_SYSWM_TYPE`.
#[cfg(any(
    test,
    all(target_os = "linux", target_arch = "arm", not(feature = "hostsim"))
))]
const SDL_SYSWM_WAYLAND: u32 = 6;
/// `wl_surface.set_opaque_region`.
#[cfg(all(target_os = "linux", target_arch = "arm"))]
const WL_SURFACE_SET_OPAQUE_REGION: u32 = 4;

#[cfg(all(target_os = "linux", target_arch = "arm", not(feature = "hostsim")))]
extern "C" {
    /// Fills `SDL_version`: three `Uint8`, major, minor, patch.
    fn SDL_GetVersion(ver: *mut u8);
    fn SDL_GetWindowWMInfo(win: *mut c_void, info: *mut c_void) -> c_int;
}

#[cfg(all(target_os = "linux", target_arch = "arm"))]
extern "C" {
    fn wl_proxy_marshal(proxy: *mut c_void, opcode: u32, ...);
}

extern "C" {
    fn SDL_SetMainReady();
    fn SDL_SetHint(name: *const c_char, value: *const c_char) -> c_int;
    fn SDL_Init(flags: u32) -> c_int;
    fn SDL_Quit();
    fn SDL_GetError() -> *const c_char;
    fn SDL_GL_SetAttribute(attr: c_int, value: c_int) -> c_int;
    fn SDL_CreateWindow(
        title: *const c_char,
        x: c_int,
        y: c_int,
        w: c_int,
        h: c_int,
        flags: u32,
    ) -> *mut c_void;
    fn SDL_DestroyWindow(win: *mut c_void);
    fn SDL_GL_CreateContext(win: *mut c_void) -> *mut c_void;
    fn SDL_GL_DeleteContext(ctx: *mut c_void);
    fn SDL_GL_SetSwapInterval(interval: c_int) -> c_int;
    fn SDL_GL_SwapWindow(win: *mut c_void);
    fn SDL_PollEvent(event: *mut c_void) -> c_int;
    fn SDL_WaitEventTimeout(event: *mut c_void, timeout: c_int) -> c_int;
    fn SDL_PushEvent(event: *mut c_void) -> c_int;
    fn SDL_GetWindowFlags(win: *mut c_void) -> u32;
    fn SDL_StartTextInput();
    fn SDL_StopTextInput();
    fn glGetString(name: u32) -> *const c_char;
    fn glGetIntegerv(name: u32, data: *mut c_int);
    fn glViewport(x: c_int, y: c_int, w: c_int, h: c_int);
}

#[cfg(feature = "hostsim")]
extern "C" {
    fn SDL_GL_GetDrawableSize(win: *mut c_void, w: *mut c_int, h: *mut c_int);
    fn SDL_GetWindowSize(win: *mut c_void, w: *mut c_int, h: *mut c_int);
    fn SDL_SetWindowSize(win: *mut c_void, w: c_int, h: c_int);
    fn glReadPixels(
        x: c_int,
        y: c_int,
        w: c_int,
        h: c_int,
        format: u32,
        ty: u32,
        pixels: *mut c_void,
    );
}

/// One `SDL_Event`, as bytes. SDL2's union is 56 bytes and holds pointers, hence the size and the
/// alignment; the television's fork writes no more than that.
#[repr(C, align(8))]
pub struct Event(pub [u8; 64]);

impl Event {
    pub fn new() -> Self {
        Self([0; 64])
    }

    /// The `type` field every event starts with.
    pub fn kind(&self) -> u32 {
        self.u32_at(0)
    }

    pub fn u32_at(&self, off: usize) -> u32 {
        u32::from_ne_bytes(self.0[off..off + 4].try_into().expect("four bytes"))
    }

    pub fn i32_at(&self, off: usize) -> i32 {
        self.u32_at(off) as i32
    }

    pub fn f32_at(&self, off: usize) -> f32 {
        f32::from_bits(self.u32_at(off))
    }
}

/// Wake the loop from any thread: an `SDL_USEREVENT` ends its wait like any other event.
pub fn wake() {
    let mut ev = Event::new();
    ev.0[..4].copy_from_slice(&SDL_USEREVENT.to_ne_bytes());
    // SAFETY: SDL_PushEvent may be called from any thread and copies the event out of `ev`.
    unsafe { SDL_PushEvent(ev.0.as_mut_ptr().cast()) };
}

/// The window and its GL context, current on this thread until dropped.
pub struct Video {
    win: *mut c_void,
    ctx: *mut c_void,
    /// SDL's `wl_surface` for the window, borrowed while the app is in the foreground. Null on the
    /// simulator, when SDL did not answer with a Wayland window, and between a background edge
    /// and the next foreground (SDL may drop it meanwhile).
    surface: Cell<*mut c_void>,
    /// Drawable pixels per window unit: pointer events come in window units, which a Retina
    /// display doubles on the simulator; 1 on the television.
    pointer_scale: f32,
}

impl Video {
    /// Bring up SDL, the window and the GL context. `Err` carries SDL's own message.
    pub fn open(title: &str) -> Result<Self, String> {
        let title = CString::new(title).unwrap_or_default();
        // webOS withholds BACK from a native app unless its access policy asks for it. Set in the
        // environment, as the C boot this replaces did; no other thread exists yet.
        #[cfg(not(feature = "hostsim"))]
        std::env::set_var("SDL_WEBOS_ACCESS_POLICY_KEYS_BACK", "true");
        let (logical_w, logical_h) = (
            tv_base::surface::LOGICAL_W as c_int,
            tv_base::surface::LOGICAL_H as c_int,
        );
        // SAFETY: called once, from `main` on the main thread, before any other SDL call. Every
        // pointer passed is a NUL-terminated literal or out-parameter that outlives its call, and
        // each handle is checked before use and released on the error paths.
        unsafe {
            SDL_SetMainReady();
            SDL_SetHint(c"SDL_VIDEO_ALLOW_SCREENSAVER".as_ptr(), c"0".as_ptr());
            if SDL_Init(SDL_INIT_VIDEO) != 0 {
                return Err(format!("SDL_Init: {}", owned(SDL_GetError())));
            }
            let (profile, major, minor) = CONTEXT;
            for (attr, value) in [
                (GL_CONTEXT_PROFILE_MASK, profile),
                (GL_CONTEXT_MAJOR_VERSION, major),
                (GL_CONTEXT_MINOR_VERSION, minor),
                (GL_RED_SIZE, 8),
                (GL_GREEN_SIZE, 8),
                (GL_BLUE_SIZE, 8),
                (GL_ALPHA_SIZE, 8),
                (GL_BUFFER_SIZE, 32),
                (GL_DEPTH_SIZE, 0),
                (GL_STENCIL_SIZE, 0),
            ] {
                SDL_GL_SetAttribute(attr, value);
            }
            let (pos, div, flags) = WINDOW;
            let win = SDL_CreateWindow(
                title.as_ptr(),
                pos,
                pos,
                logical_w / div,
                logical_h / div,
                flags,
            );
            if win.is_null() {
                let e = format!("SDL_CreateWindow: {}", owned(SDL_GetError()));
                SDL_Quit();
                return Err(e);
            }
            let ctx = SDL_GL_CreateContext(win);
            if ctx.is_null() {
                let e = format!("SDL_GL_CreateContext: {}", owned(SDL_GetError()));
                SDL_DestroyWindow(win);
                SDL_Quit();
                return Err(e);
            }
            // vsync: a presented frame waits for the panel instead of spinning.
            SDL_GL_SetSwapInterval(1);
            // A 1x display gives the half-size window a half-size drawable: ask for the full canvas
            // in points, so a screenshot is 1920x1080 wherever the display allows it.
            #[cfg(feature = "hostsim")]
            {
                let (mut w, mut h) = (0, 0);
                SDL_GL_GetDrawableSize(win, &mut w, &mut h);
                if w < logical_w {
                    SDL_SetWindowSize(win, logical_w, logical_h);
                }
            }
            tv_base::surface::probe(win);
            // Desktop SDL turns text input on at init when there is no screen keyboard; the
            // television's starts with it off. Match it, so text arrives only while a field asks.
            #[cfg(feature = "hostsim")]
            SDL_StopTextInput();
            #[cfg(feature = "hostsim")]
            let pointer_scale = {
                let ((mut ww, mut wh), (mut dw, mut dh)) = ((0, 0), (0, 0));
                SDL_GetWindowSize(win, &mut ww, &mut wh);
                SDL_GL_GetDrawableSize(win, &mut dw, &mut dh);
                if ww > 0 && dw > 0 {
                    dw as f32 / ww as f32
                } else {
                    1.0
                }
            };
            #[cfg(not(feature = "hostsim"))]
            let pointer_scale = 1.0;
            let video = Self {
                win,
                ctx,
                surface: Cell::new(std::ptr::null_mut()),
                pointer_scale,
            };
            video.grab_surface();
            Ok(video)
        }
    }

    /// Borrow SDL's `wl_surface` for the window and clear its opaque region, so the video plane
    /// under it shows. At boot and on every foreground edge.
    ///
    /// The television's SDL fork writes a larger `SDL_SysWMinfo` than the headers declare, so the
    /// query gets an oversized buffer, and it declares the library's own version: from 2.0.6 on
    /// SDL refuses an older caller without filling the struct (webOS 6 ships 2.0.10, webOS 25
    /// 2.0.14).
    pub fn grab_surface(&self) {
        #[cfg(all(target_os = "linux", target_arch = "arm", not(feature = "hostsim")))]
        {
            let mut info = [0u8; 512];
            // SAFETY: SDL_GetVersion writes three bytes at the head of the buffer; the window is
            // live and the buffer is writable and larger than any SDL_SysWMinfo this SDL writes.
            let ok = unsafe {
                SDL_GetVersion(info.as_mut_ptr());
                SDL_GetWindowWMInfo(self.win, info.as_mut_ptr().cast())
            };
            let surface = wayland_surface(ok, &info);
            self.surface.set(surface);
            crate::logs::log(&format!(
                "wm: sdl={}.{}.{} wayland surface {}",
                info[0],
                info[1],
                info[2],
                if surface.is_null() {
                    "missing: the video plane stays hidden under the UI"
                } else {
                    "borrowed"
                }
            ));
            self.clear_opaque_region();
        }
    }

    /// Let go of the borrowed surface on a background edge; [`Video::grab_surface`] takes it again
    /// on the next foreground.
    pub fn release_surface(&self) {
        self.surface.set(std::ptr::null_mut());
    }

    /// `wl_surface.set_opaque_region(NULL)`: the compositor may blend the whole UI surface, so the
    /// video plane shows wherever the UI draws nothing. The commit is the next swap's (a bare
    /// commit would present a surface without a buffer).
    fn clear_opaque_region(&self) {
        #[cfg(all(target_os = "linux", target_arch = "arm"))]
        {
            let surface = self.surface.get();
            if !surface.is_null() {
                // SAFETY: `surface` is SDL's live wl_surface for this window (released on a
                // background edge, before SDL may drop it); opcode 4 takes one nullable
                // `wl_region`.
                unsafe {
                    wl_proxy_marshal(
                        surface,
                        WL_SURFACE_SET_OPAQUE_REGION,
                        std::ptr::null_mut::<c_void>(),
                    )
                }
            }
        }
    }

    /// The driver's renderer and version strings, for the boot log.
    /// The alpha bits are the framebuffer's: without them the surface cannot be see-through and
    /// the video plane stays hidden whatever the UI draws.
    pub fn gl_strings(&self) -> String {
        let mut alpha: c_int = -1;
        // SAFETY: the context is current; glGetString returns a static string or null and
        // glGetIntegerv writes one integer for GL_ALPHA_BITS.
        unsafe {
            glGetIntegerv(GL_ALPHA_BITS, &mut alpha);
            format!(
                "{} / {} alpha_bits={alpha}",
                owned(glGetString(GL_RENDERER)),
                owned(glGetString(GL_VERSION))
            )
        }
    }

    /// The next queued event, without waiting.
    pub fn poll(&self, ev: &mut Event) -> bool {
        // SAFETY: `ev` is a writable, aligned buffer larger than any SDL_Event.
        unsafe { SDL_PollEvent(ev.0.as_mut_ptr().cast()) != 0 }
    }

    /// The next queued event, waiting up to `ms` for one (0: do not wait). SDL 2.0.5, the
    /// television's, implements the wait as a 10 ms poll; newer ones block.
    pub fn wait(&self, ev: &mut Event, ms: u32) -> bool {
        let ms = c_int::try_from(ms).unwrap_or(c_int::MAX);
        // SAFETY: as `poll`.
        unsafe { SDL_WaitEventTimeout(ev.0.as_mut_ptr().cast(), ms) != 0 }
    }

    /// Drawable pixels per window unit, for pointer positions ([`crate::input::decode_pointer`]).
    pub fn pointer_scale(&self) -> f32 {
        self.pointer_scale
    }

    /// Raise (`true`) or dismiss the television's keyboard; typed text then arrives as
    /// `SDL_TEXTINPUT`. LG's SDL raises the panel only while the window has input focus and says
    /// nothing when it has not, so the flag goes in the log line.
    pub fn text_input(&self, on: bool) {
        // SAFETY: `win` is the live window; both calls only toggle SDL's text input state.
        unsafe {
            if on {
                let focus = SDL_GetWindowFlags(self.win) & SDL_WINDOW_INPUT_FOCUS != 0;
                crate::logs::log(&format!("keyboard: show focus={}", u8::from(focus)));
                SDL_StartTextInput();
            } else {
                crate::logs::log("keyboard: hide");
                SDL_StopTextInput();
            }
        }
    }

    /// Point GL at the canvas rect `tv_base::surface` derived from the drawable.
    pub fn begin_frame(&self) {
        let (x, y, w, h) = tv_base::surface::viewport();
        // SAFETY: the context is current on this thread.
        unsafe { glViewport(x, y, w, h) }
    }

    /// Present the frame (blocks on vsync). The opaque region is cleared again first, as
    /// PlxNative's frame loop does on every frame, and the swap commits it.
    pub fn swap(&self) {
        self.clear_opaque_region();
        // SAFETY: `win` is the live window the context draws into.
        unsafe { SDL_GL_SwapWindow(self.win) }
    }

    /// The drawn canvas as `(width, height, RGB rows top first)`.
    #[cfg(feature = "hostsim")]
    pub fn read_rgb(&self) -> (usize, usize, Vec<u8>) {
        const GL_RGBA: u32 = 0x1908;
        const GL_UNSIGNED_BYTE: u32 = 0x1401;
        let (x, y, w, h) = tv_base::surface::viewport();
        let (w, h) = (w.max(0) as usize, h.max(0) as usize);
        let mut rgba = vec![0u8; w * h * 4];
        if rgba.is_empty() {
            return (0, 0, rgba);
        }
        // SAFETY: the buffer holds exactly w*h RGBA pixels, GL's default pack alignment (4) divides
        // an RGBA row, and the context is current on this thread.
        unsafe {
            glReadPixels(
                x,
                y,
                w as c_int,
                h as c_int,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                rgba.as_mut_ptr().cast(),
            )
        };
        let mut rgb = Vec::with_capacity(w * h * 3);
        // GL's first row is the bottom one.
        for row in rgba.chunks_exact(w * 4).rev() {
            for [r, g, b, _] in row.as_chunks::<4>().0 {
                rgb.extend_from_slice(&[*r, *g, *b]);
            }
        }
        (w, h, rgb)
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        // SAFETY: both handles came from `open` on this thread and are released once, context first.
        unsafe {
            SDL_GL_DeleteContext(self.ctx);
            SDL_DestroyWindow(self.win);
            SDL_Quit();
        }
    }
}

/// A C string SDL or GL handed back, copied.
///
/// # Safety
/// `p` is null or points to a NUL-terminated string that stays valid for the call.
unsafe fn owned(p: *const c_char) -> String {
    if p.is_null() {
        return "?".into();
    }
    CStr::from_ptr(p).to_string_lossy().into_owned()
}

/// The `wl_surface` in a filled `SDL_SysWMinfo`: null unless the query succeeded (`ok`) and
/// answered for a Wayland window. The version takes the first three bytes and the subsystem the
/// next four; the union after it starts with the display and surface pointers, which never moved
/// between SDL versions. The buffer has no pointer alignment, so both reads are unaligned.
#[cfg(any(
    test,
    all(target_os = "linux", target_arch = "arm", not(feature = "hostsim"))
))]
fn wayland_surface(ok: c_int, info: &[u8; 512]) -> *mut c_void {
    let subsystem = u32::from_ne_bytes([info[4], info[5], info[6], info[7]]);
    if ok == 0 || subsystem != SDL_SYSWM_WAYLAND {
        return std::ptr::null_mut();
    }
    // SAFETY: offset 8 plus two pointers lies well inside the 512-byte buffer.
    let (display, surface) = unsafe {
        let pointers = info.as_ptr().add(8).cast::<*mut c_void>();
        (pointers.read_unaligned(), pointers.add(1).read_unaligned())
    };
    if display.is_null() {
        std::ptr::null_mut()
    } else {
        surface
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(ok_subsystem: u32, display: usize, surface: usize) -> [u8; 512] {
        let mut info = [0u8; 512];
        info[..3].copy_from_slice(&[2, 0, 14]);
        info[4..8].copy_from_slice(&ok_subsystem.to_ne_bytes());
        let width = size_of::<usize>();
        info[8..8 + width].copy_from_slice(&display.to_ne_bytes());
        info[8 + width..8 + 2 * width].copy_from_slice(&surface.to_ne_bytes());
        info
    }

    #[test]
    fn the_surface_comes_only_from_a_successful_wayland_answer() {
        let wayland = info(SDL_SYSWM_WAYLAND, 0x1000, 0x2000);
        assert_eq!(wayland_surface(1, &wayland) as usize, 0x2000);
        assert!(wayland_surface(0, &wayland).is_null(), "a failed query");
        assert!(
            wayland_surface(1, &info(2, 0x1000, 0x2000)).is_null(),
            "an X11 window"
        );
        assert!(
            wayland_surface(1, &info(SDL_SYSWM_WAYLAND, 0, 0x2000)).is_null(),
            "no display"
        );
        assert!(wayland_surface(1, &info(SDL_SYSWM_WAYLAND, 0x1000, 0)).is_null());
    }
}
