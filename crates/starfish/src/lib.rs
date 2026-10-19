// Copyright (c) 2026 Gleb Linnik
// Copyright (C) 2026 the webos-iptv-native authors
// SPDX-License-Identifier: GPL-3.0-or-later
// Rust port of PlxNative's src/starfish.c and src/starfish.h, by way of StremioNative's
// native/starfish.c (which added the cropping vp_place). Own interoperability adapter, written
// from the public ABI contract; LG runtime binaries are not distributed with this file.

//! tv_starfish: the LG StarfishMediaAPIs (C++), ACB and exported-window seam.
//!
//! Exports the flat `sf_*`, `vp_*` and `acb_*` C ABI that `tv_media::player::ffi` declares, and
//! forwards the library-thread callbacks to `sf_on_event` / `acb_on_event`, which the player engine
//! defines. Every C++-ABI hazard of the television lives in this crate: the mangled symbols, Feed's
//! sret `std::string` (and the pre-C++11 copy-on-write build of it), the over-allocated in-place
//! object that is never `new`/`delete`d and never reused, the callback gate around
//! `callbackFunctionHook`, and the 3-arg ACB taskId out-param. Callers touch only the flat verbs.
//!
//! - `lifecycle`: Load, the callback gate, quarantine and destroy, generic over the few LG calls
//!   they make, so the host tests drive the same state machine the television runs.
//! - `abi`: the signature table the exports are checked against, and its comparison with
//!   `tv_media`'s `player/ffi.rs`.
//! - `text`: every payload the seam formats, with `snprintf`'s truncation, byte-exact.
//! - `resolve`: the pure decisions behind runtime symbol selection (video plane, Feed ABI, hook
//!   owner).
//! - `tv` (`target_arch = "arm"` only): the LG symbols and the exported verbs.
//!
//! On the host this crate exports nothing: the simulator's video sink is `tv_media::player::ffi_host`.
//!
//! What the final television link must provide (the crate cannot say it itself): the executable
//! exports `_ZN17StarfishMediaAPIs20callbackFunctionHookEixPKc` in `.dynsym`
//! (`-Wl,--export-dynamic-symbol=...`, never `-E`), and the application references this crate
//! (calling [`set_event_log`] at boot does), or the `sf_*` externs of `tv_media` stay unresolved.

#[cfg(any(target_arch = "arm", test))]
mod abi;
#[cfg(any(target_arch = "arm", test))]
mod lifecycle;
#[cfg(any(target_arch = "arm", test))]
mod resolve;
#[cfg(any(target_arch = "arm", test))]
mod text;
#[cfg(target_arch = "arm")]
mod tv;

use std::sync::OnceLock;

static EVENT_LOG: OnceLock<fn(&str)> = OnceLock::new();

/// Route the seam's diagnostic lines into the application's event log (`tv_base::eventlog::log`).
///
/// The C wrote to `extern FILE *elogf`, owned by the boot shim, and wrote nothing while it was
/// NULL. This is the same contract without naming the base layer from here: until a sink is set,
/// the seam is silent. The first call wins; the lines carry no trailing newline (the sink adds it).
pub fn set_event_log(sink: fn(&str)) {
    let _ = EVENT_LOG.set(sink);
}

/// Format and write one event-log line, only when a sink is set (the C's `if (elogf)`).
#[cfg(any(target_arch = "arm", test))]
macro_rules! elog {
    ($($arg:tt)*) => {
        if let Some(sink) = $crate::EVENT_LOG.get() {
            sink(&format!($($arg)*));
        }
    };
}
#[cfg(any(target_arch = "arm", test))]
pub(crate) use elog;
