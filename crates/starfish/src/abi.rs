// Copyright (C) 2026 the webos-iptv-native authors
// SPDX-License-Identifier: GPL-3.0-or-later

//! The C ABI between this crate and `tv_media`, written once: the exports and the two callbacks
//! it imports. On the television each item in `tv` is type-checked against its table at compile
//! time; on the host the tests compare the tables with `tv_media`'s text: the extern block of
//! `player/ffi.rs` (the declaration this crate implements) and the `#[no_mangle]` callbacks in
//! `player/mod.rs`. The names are unmangled, so a signature changed on one side only would still
//! link; this fails the build or a test instead of corrupting registers.

macro_rules! seam_abi {
    ($table:ident: $(fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;)*) => {
        #[cfg(test)]
        pub(crate) const $table: &[(&str, &str)] =
            &[$((stringify!($name), stringify!(($($arg: $ty),*) $(-> $ret)?))),*];

        #[cfg(target_arch = "arm")]
        const _: () = {
            use std::ffi::{c_char, c_int, c_long, c_uint};
            $(let _: unsafe extern "C" fn($($ty),*) $(-> $ret)? = crate::tv::$name;)*
        };
    };
}

seam_abi! {
    EXPORTS:
    fn sf_load(payload: *const c_char, epoch: c_uint) -> c_int;
    fn sf_ready() -> c_int;
    fn sf_is_load_completed() -> c_int;
    fn sf_play() -> c_int;
    fn sf_pause() -> c_int;
    fn sf_set_volume(volume: c_int) -> c_int;
    fn sf_flush() -> c_int;
    fn sf_push_eos() -> c_int;
    fn sf_set_time_to_decode(position_ns: i64) -> c_int;
    fn sf_set_content_info(position_ns: i64) -> c_int;
    fn sf_send_segment() -> c_int;
    fn sf_seek_ms(ms: i64) -> c_int;
    fn sf_feed(p: *const u8, size: c_uint, pts: i64, es_data: c_int) -> c_char;
    fn sf_feed_abi_is_cow() -> c_int;
    fn sf_unload();
    fn sf_callback_gate_retire() -> c_int;
    fn sf_callback_intercepts() -> c_uint;
    fn sf_destroy() -> c_int;
    fn sf_quarantine();

    fn vp_mode() -> c_int;
    fn vp_create_window() -> *const c_char;
    fn vp_window_id() -> *const c_char;
    fn vp_place(
        src_x: c_int,
        src_y: c_int,
        src_w: c_int,
        src_h: c_int,
        dst_x: c_int,
        dst_y: c_int,
        dst_w: c_int,
        dst_h: c_int,
    ) -> c_int;
    fn vp_destroy_window();

    fn acb_create(app_id: *const c_char, player_type: c_int) -> c_long;
    fn acb_bind(media_id: *const c_char);
    fn acb_send_video_data(source_info: *const c_char) -> c_int;
    fn acb_send_atmos(media_id: *const c_char) -> c_int;
    fn acb_start(x: c_long, y: c_long, w: c_long, h: c_long);
    fn acb_unload();
    fn acb_pause();
    fn acb_resume();
}

seam_abi! {
    IMPORTS:
    fn sf_on_event(epoch: c_uint, ty: c_int, num: i64, s: *const c_char);
    fn acb_on_event(ev: c_long, reply: *const c_char);
}

#[cfg(test)]
mod tests {
    use super::{EXPORTS, IMPORTS};

    /// The declarations `tv_media` links against. Read as text: the media crate keeps them in a
    /// private module on purpose, and its host build compiles `ffi_host` instead.
    const FFI_RS: &str = include_str!("../../media/src/player/ffi.rs");
    /// Where the engine defines the two callbacks this crate calls.
    const PLAYER_MOD_RS: &str = include_str!("../../media/src/player/mod.rs");

    fn normalize(s: &str) -> String {
        let flat: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        flat.replace(",)", ")")
    }

    /// `(name, signature)` for every `fn` in ffi.rs's `extern "C"` block.
    fn declared() -> Vec<(String, String)> {
        let start = FFI_RS
            .find("extern \"C\" {")
            .expect("ffi.rs has an extern \"C\" block");
        let body = &FFI_RS[start..];
        let body = &body[..body.find("\n    }\n").expect("the extern block closes")];
        body.split(';')
            .filter_map(|decl| {
                let decl = &decl[decl.find("fn ")? + 3..];
                let open = decl.find('(')?;
                Some((decl[..open].trim().to_string(), normalize(&decl[open..])))
            })
            .collect()
    }

    #[test]
    fn every_ffi_rs_declaration_is_exported_with_the_same_signature() {
        let declared = declared();
        assert!(
            declared.len() >= 30,
            "parsed too few declarations: {declared:?}"
        );
        for (name, signature) in &declared {
            let ours = EXPORTS.iter().find(|(n, _)| n == name);
            let Some((_, ours)) = ours else {
                panic!("ffi.rs declares {name}, this crate does not export it")
            };
            assert_eq!(&normalize(ours), signature, "signature of {name}");
        }
        for (name, _) in EXPORTS {
            assert!(
                declared.iter().any(|(n, _)| n == name),
                "{name} is exported but ffi.rs no longer declares it"
            );
        }
        assert_eq!(declared.len(), EXPORTS.len());
    }

    #[test]
    fn the_imported_callbacks_match_their_definitions_in_the_engine() {
        for (name, ours) in IMPORTS {
            let head = format!("pub extern \"C\" fn {name}(");
            let at = PLAYER_MOD_RS
                .find(&head)
                .unwrap_or_else(|| panic!("player/mod.rs no longer defines {name}"));
            let sig = &PLAYER_MOD_RS[at + head.len() - 1..];
            let sig = &sig[..sig.find('{').expect("the definition has a body")];
            assert_eq!(normalize(ours), normalize(sig), "signature of {name}");
        }
    }
}
