//! The platform port: what `tv_platform::tv` asks of this app at boot. Only the parts an IPTV
//! player uses are wired: the video sink (the television's Starfish seam, or the simulator's
//! host sink), the set's own probe (firmware, decoder table, jail), its locale and sound output,
//! and the bus pump that services the GLib main context the media pipeline replies on.
//! The rest of the table (secure storage, the home screen, system toasts, frame probes) stays
//! inert, and so do its surface entries: the app's own `sdl::Video` owns the Wayland surface.

use tv_platform::tv::toast::{Outcome, Sent};
use tv_platform::tv::{sandbox, sink::VideoSink, Port};

// The set's own answers: the television's, or the simulator's stand-ins. A plain host build
// (tests, tools) has no platform at all.
#[cfg(any(target_arch = "arm", feature = "hostsim"))]
use tv_platform::webos::{probe, sound_settings, system_locale};
#[cfg(not(any(target_arch = "arm", feature = "hostsim")))]
fn probe() {}
#[cfg(not(any(target_arch = "arm", feature = "hostsim")))]
fn sound_settings() -> Option<String> {
    None
}
#[cfg(not(any(target_arch = "arm", feature = "hostsim")))]
fn system_locale() -> tv_platform::tv::LocaleReply {
    tv_platform::tv::LocaleReply::NoPlatform
}

// The bus the set's locale and sound settings are asked on (`tv_platform::webos::ls2`).
#[cfg(all(target_os = "linux", target_arch = "arm"))]
#[link(name = "luna-service2")]
#[link(name = "glib-2.0")]
extern "C" {
    fn g_main_context_pending(ctx: *mut std::ffi::c_void) -> std::ffi::c_int;
    fn g_main_context_iteration(
        ctx: *mut std::ffi::c_void,
        may_block: std::ffi::c_int,
    ) -> std::ffi::c_int;
}

/// Service the default GLib main context, at most eight dispatches a call, never blocking. LG's
/// media pipeline delivers its bus replies there; the frame loop calls this every iteration.
#[cfg(all(target_os = "linux", target_arch = "arm"))]
fn pump_bus() {
    for _ in 0..8 {
        // SAFETY: the default context (null) from the main thread; neither call blocks.
        unsafe {
            if g_main_context_pending(std::ptr::null_mut()) == 0 {
                break;
            }
            g_main_context_iteration(std::ptr::null_mut(), 0);
        }
    }
}
#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
use nothing as pump_bus;

#[cfg(feature = "hostsim")]
const SINK: &dyn VideoSink = &tv_media::player::ffi_host::HostSink;
#[cfg(all(target_arch = "arm", not(feature = "hostsim")))]
const SINK: &dyn VideoSink = &tv_media::player::ffi::StarfishSink;
/// A host build that is not the simulator has no video at all.
#[cfg(not(any(target_arch = "arm", feature = "hostsim")))]
const SINK: &dyn VideoSink = &tv_platform::tv::sink::NoSink;

fn nothing() {}

static PORT: Port = Port {
    probe_device: probe,
    start_capability_probe: nothing,
    repair_sandbox: || Err(sandbox::Failure::Unsupported),
    seal: |_| None,
    open: |_| None,
    remove: |_, _| {},
    system_locale,
    sound_settings,
    go_home: nothing,
    poll_home: nothing,
    // The shell draws its own toasts; nothing asks the system for one.
    deliver_toast: |_, _| Sent {
        reply: None,
        outcome: Outcome::NoBus,
    },
    bind_window: |_| {},
    grab_surface: |_| {},
    release_surface: || false,
    arm_opaque_region: nothing,
    opaque_route: |_| {},
    clear_opaque_region: nothing,
    pump_bus,
    frame_probe_request: nothing,
    frame_probe_prepare_begin: nothing,
    frame_probe_prepare_end: nothing,
    frame_probe_waiting: nothing,
    frame_probe_acquired: nothing,
    frame_probe_fields: String::new,
    sink: SINK,
};

/// Install the port and probe the set (firmware, what it decodes), before anything plays.
pub fn install() {
    if tv_platform::tv::install(&PORT).is_ok() {
        tv_platform::tv::probe_device();
    }
}
