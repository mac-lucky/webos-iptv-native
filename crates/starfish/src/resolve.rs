// Copyright (c) 2026 Gleb Linnik
// Copyright (C) 2026 the webos-iptv-native authors
// SPDX-License-Identifier: GPL-3.0-or-later

//! The decisions behind the seam's runtime symbol resolution, kept apart from `dlsym` so the host
//! tests can hold them to the C's order. The lookups themselves are in `tv`.

use std::ffi::{c_int, CStr};

/// Neither binding: video cannot be displayed, but the app still runs.
pub(crate) const VP_NONE: c_int = 0;
/// webOS 2.2.3 .. 4.10.0: `libAcbAPI.so.1`.
pub(crate) const VP_ACB: c_int = 1;
/// webOS 5.3.1 .. 11.2.0: `SDL_webOS*ExportedWindow*` (device-verified on webOS 6.5.2).
pub(crate) const VP_EXPORTED: c_int = 2;

/// What probing `libAcbAPI.so.1` found.
pub(crate) enum AcbProbe<A> {
    /// `dlopen` failed: webOS 5 and later deleted the library.
    Absent,
    /// Opened, but one of the required entry points is missing.
    Incomplete,
    /// Every required entry point resolved (the optional `setMediaAudioData` may still be absent).
    Complete(A),
}

/// The video-plane binding a television gets.
pub(crate) enum Plane<A, S> {
    None,
    Acb(A),
    Exported(S),
}

impl<A, S> Plane<A, S> {
    pub(crate) fn mode(&self) -> c_int {
        match self {
            Plane::None => VP_NONE,
            Plane::Acb(_) => VP_ACB,
            Plane::Exported(_) => VP_EXPORTED,
        }
    }
}

/// ACB first: on a 4.x set it is the only path, and probing SDL there costs failed lookups. SDL's
/// exported window is probed only when ACB is not complete, and is usable only with both
/// `SDL_webOSCreateExportedWindow` and `SDL_webOSSetExportedWindow` (`probe_sdl` answers `None`
/// otherwise). The two mechanisms are complementary across every known firmware, never both.
pub(crate) fn select_plane<A, S>(
    acb: AcbProbe<A>,
    probe_sdl: impl FnOnce() -> Option<S>,
) -> Plane<A, S> {
    if let AcbProbe::Complete(api) = acb {
        return Plane::Acb(api);
    }
    match probe_sdl() {
        Some(sdl) => Plane::Exported(sdl),
        None => Plane::None,
    }
}

/// `std::string StarfishMediaAPIs::Feed(const char *)` under the C++11 string ABI (`B5cxx11`), on
/// the 4.x inventories and later (4.4.2 is the oldest graded with it).
pub(crate) const FEED_CXX11: &CStr = c"_ZN17StarfishMediaAPIs4FeedB5cxx11EPKc";
/// The same method under the pre-C++11 copy-on-write string, on webOS 3.9.2 and earlier.
pub(crate) const FEED_COW: &CStr = c"_ZN17StarfishMediaAPIs4FeedEPKc";
/// `std::basic_string<char>::~basic_string()` of the firmware's own libstdc++: a COW reply's buffer
/// is a refcounted heap rep and must go back through it.
pub(crate) const COW_STRING_DTOR: &CStr = c"_ZNSsD1Ev";
/// `std::__cxx11::basic_string<char>::~basic_string()` of the same libstdc++ (GLIBCXX_3.4.21): a
/// cxx11 reply longer than the 15-byte inline buffer owns a heap buffer. webOS 25 answers every
/// Feed `{"returnValue":"Ok"}` (20 bytes), so without it each Feed leaked one.
pub(crate) const CXX11_STRING_DTOR: &CStr =
    c"_ZNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEED1Ev";

/// The Feed ABI this firmware has.
pub(crate) struct FeedAbi<F, D> {
    pub(crate) feed: Option<F>,
    /// Set when the COW name is the one that resolved. Kept apart from `reply_dtor` because that
    /// can be missing on a COW library, and the ABI is then still COW.
    pub(crate) cow: bool,
    /// The destructor of the resolved Feed's string ABI, which every reply goes through.
    pub(crate) reply_dtor: Option<D>,
}

impl<F, D> FeedAbi<F, D> {
    /// The log's name for it; with no destructor every Feed reply that does not fit the string's
    /// inline buffer (all of them under COW) leaks.
    pub(crate) fn label(&self) -> &'static str {
        match (&self.feed, self.cow) {
            (None, _) => "NONE",
            (Some(_), true) => "cow",
            (Some(_), false) => "cxx11",
        }
    }
}

/// The cxx11 name first, the COW name only when it is missing, and the destructor of whichever
/// string ABI resolved. Both names are looked up at run time: a strong reference to the cxx11 name
/// is a lazily bound PLT slot that kills the process at the first Feed on webOS 3.4.0.
pub(crate) fn resolve_feed<F, D>(
    lookup_feed: impl Fn(&CStr) -> Option<F>,
    lookup_dtor: impl FnOnce(&CStr) -> Option<D>,
) -> FeedAbi<F, D> {
    if let Some(feed) = lookup_feed(FEED_CXX11) {
        return FeedAbi {
            feed: Some(feed),
            cow: false,
            reply_dtor: lookup_dtor(CXX11_STRING_DTOR),
        };
    }
    match lookup_feed(FEED_COW) {
        Some(feed) => FeedAbi {
            feed: Some(feed),
            cow: true,
            reply_dtor: lookup_dtor(COW_STRING_DTOR),
        },
        None => FeedAbi {
            feed: None,
            cow: false,
            reply_dtor: None,
        },
    }
}

/// `dladdr`'s `dli_fname` names LG's player library. A substring test, as the C's `strstr`: the
/// path differs between firmware releases, the file name does not.
pub(crate) fn owned_by_player_apis(fname: Option<&CStr>) -> bool {
    fname.is_some_and(|f| f.to_bytes().windows(16).any(|w| w == b"libplayerAPIs.so"))
}

/// What the callback-hook resolution observed (`dlsym` + `dlerror` + `dladdr`, twice).
pub(crate) struct HookEvidence<'a> {
    /// `dlerror()` was clear after `dlsym(RTLD_NEXT, callbackFunctionHook)`.
    pub(crate) hook_lookup_ok: bool,
    pub(crate) hook_found: bool,
    /// The lookup answered our own interposer (the export did not interpose anything).
    pub(crate) hook_is_ours: bool,
    pub(crate) hook_owner: Option<&'a CStr>,
    /// `dlerror()` was clear after `dlsym(RTLD_DEFAULT, Load-with-context)`.
    pub(crate) load_lookup_ok: bool,
    pub(crate) load_found: bool,
    pub(crate) load_owner: Option<&'a CStr>,
}

/// The gate is valid only when the real hook and the Load-with-context overload were both found,
/// both in libplayerAPIs, and the hook is not our own interposer. Anything less blocks the lifecycle:
/// a release lacking the overload must refuse playback at run time, not stop the app reaching main.
pub(crate) fn hook_proven(e: &HookEvidence<'_>) -> bool {
    let owner_ok = e.hook_found && !e.hook_is_ours && owned_by_player_apis(e.hook_owner);
    let load_owner_ok = e.load_found && owned_by_player_apis(e.load_owner);
    e.hook_lookup_ok && owner_ok && e.load_lookup_ok && load_owner_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn complete_acb_wins_and_sdl_is_never_probed() {
        let plane: Plane<u8, u8> = select_plane(AcbProbe::Complete(1), || panic!("probed SDL"));
        assert_eq!(plane.mode(), VP_ACB);
    }

    #[test]
    fn incomplete_or_absent_acb_falls_through_to_sdl() {
        let p: Plane<u8, u8> = select_plane(AcbProbe::Incomplete, || Some(2));
        assert_eq!(p.mode(), VP_EXPORTED);
        let p: Plane<u8, u8> = select_plane(AcbProbe::Absent, || Some(2));
        assert_eq!(p.mode(), VP_EXPORTED);
    }

    #[test]
    fn no_binding_at_all_is_vp_none() {
        let p: Plane<u8, u8> = select_plane(AcbProbe::Absent, || None);
        assert_eq!(p.mode(), VP_NONE);
        let p: Plane<u8, u8> = select_plane(AcbProbe::Incomplete, || None);
        assert_eq!(p.mode(), VP_NONE);
    }

    #[test]
    fn mode_numbers_match_starfish_h() {
        assert_eq!((VP_NONE, VP_ACB, VP_EXPORTED), (0, 1, 2));
    }

    /// A lookup that answers the names in `present` and records every name asked.
    fn lookup<'a>(
        present: &'a [&CStr],
        asked: &'a RefCell<Vec<String>>,
    ) -> impl Fn(&CStr) -> Option<u8> + 'a {
        move |name| {
            asked.borrow_mut().push(name.to_string_lossy().into_owned());
            present.contains(&name).then_some(1)
        }
    }

    /// The webOS 25 reply, `{"returnValue":"Ok"}`, is past the cxx11 string's inline buffer: a
    /// cxx11 Feed without its destructor leaked a heap buffer per access unit.
    #[test]
    fn cxx11_feed_is_preferred_and_carries_its_destructor() {
        let asked = RefCell::new(Vec::new());
        let abi = resolve_feed(lookup(&[FEED_CXX11, FEED_COW], &asked), |name| {
            assert_eq!(
                name, CXX11_STRING_DTOR,
                "the cxx11 string's destructor, never the COW one"
            );
            Some(7u8)
        });
        assert!(abi.feed.is_some() && !abi.cow);
        assert_eq!(abi.reply_dtor, Some(7));
        assert_eq!(abi.label(), "cxx11");
        assert_eq!(*asked.borrow(), [FEED_CXX11.to_str().unwrap()]);
    }

    #[test]
    fn cxx11_without_a_destructor_is_still_cxx11() {
        let asked = RefCell::new(Vec::new());
        let abi = resolve_feed(lookup(&[FEED_CXX11], &asked), |_| None::<u8>);
        assert!(abi.feed.is_some() && !abi.cow && abi.reply_dtor.is_none());
        assert_eq!(abi.label(), "cxx11");
    }

    #[test]
    fn cow_feed_is_the_fallback_and_carries_its_destructor() {
        let asked = RefCell::new(Vec::new());
        let abi = resolve_feed(lookup(&[FEED_COW], &asked), |name| {
            assert_eq!(name, COW_STRING_DTOR);
            Some(9u8)
        });
        assert!(abi.feed.is_some() && abi.cow);
        assert_eq!(abi.reply_dtor, Some(9));
        assert_eq!(abi.label(), "cow");
        assert_eq!(asked.borrow().len(), 2);
    }

    #[test]
    fn cow_without_a_destructor_is_still_cow() {
        let asked = RefCell::new(Vec::new());
        let abi = resolve_feed(lookup(&[FEED_COW], &asked), |_| None::<u8>);
        assert!(abi.feed.is_some() && abi.cow && abi.reply_dtor.is_none());
    }

    #[test]
    fn no_feed_at_all() {
        let asked = RefCell::new(Vec::new());
        let abi = resolve_feed(lookup(&[], &asked), |_| -> Option<u8> {
            panic!("destructor without COW")
        });
        assert!(abi.feed.is_none() && !abi.cow);
        assert_eq!(abi.label(), "NONE");
    }

    #[test]
    fn owner_check_is_a_substring_of_the_loaded_path() {
        assert!(owned_by_player_apis(Some(c"/usr/lib/libplayerAPIs.so.1")));
        assert!(owned_by_player_apis(Some(c"libplayerAPIs.so")));
        assert!(!owned_by_player_apis(Some(c"/usr/lib/libpf-1.0.so.1")));
        assert!(!owned_by_player_apis(Some(
            c"/media/developer/apps/usr/palm/applications/x/app"
        )));
        assert!(!owned_by_player_apis(None));
    }

    fn proven() -> HookEvidence<'static> {
        HookEvidence {
            hook_lookup_ok: true,
            hook_found: true,
            hook_is_ours: false,
            hook_owner: Some(c"/usr/lib/libplayerAPIs.so.1"),
            load_lookup_ok: true,
            load_found: true,
            load_owner: Some(c"/usr/lib/libplayerAPIs.so.1"),
        }
    }

    #[test]
    fn the_hook_is_proven_only_with_every_piece_of_evidence() {
        assert!(hook_proven(&proven()));
        let broken: [fn(&mut HookEvidence<'static>); 8] = [
            |e| e.hook_lookup_ok = false,
            |e| e.hook_found = false,
            |e| e.hook_is_ours = true,
            |e| e.hook_owner = Some(c"/media/developer/apps/x/app"),
            |e| e.load_lookup_ok = false,
            |e| e.load_found = false,
            |e| e.load_owner = None,
            |e| e.load_owner = Some(c"/usr/lib/libpf-1.0.so.1"),
        ];
        for (i, f) in broken.iter().enumerate() {
            let mut e = proven();
            f(&mut e);
            assert!(!hook_proven(&e), "case {i} must block the lifecycle");
        }
    }
}
