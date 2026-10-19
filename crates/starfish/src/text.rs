// Copyright (c) 2026 Gleb Linnik
// Copyright (C) 2026 the webos-iptv-native authors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Every string the seam hands LG, formatted exactly as the C's `snprintf` calls did: the same
//! keys, the same number formatting, the same buffer sizes and the same silent truncation at
//! `size - 1` bytes. No allocation, because `sf_feed` builds one per access unit.

use std::ffi::{c_char, c_int, c_uint, CStr};
use std::fmt;

/// A fixed, always NUL-terminated buffer written like `snprintf(buf, N, ...)`: output past `N - 1`
/// bytes is dropped without an error.
pub(crate) struct CBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> CBuf<N> {
    pub(crate) const fn new() -> Self {
        const { assert!(N > 0) };
        Self {
            buf: [0; N],
            len: 0,
        }
    }

    /// Append raw bytes (the C's `%s` of a C string), truncating like `snprintf`.
    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) {
        let room = N - 1 - self.len;
        let take = bytes.len().min(room);
        self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
        self.buf[self.len] = 0;
    }

    /// The content, without the terminating NUL.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// For the FFI call: valid for as long as `self` is neither moved nor dropped.
    pub(crate) fn as_ptr(&self) -> *const c_char {
        self.buf.as_ptr().cast()
    }
}

impl<const N: usize> fmt::Write for CBuf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_bytes(s.as_bytes());
        Ok(())
    }
}

fn cformat<const N: usize>(args: fmt::Arguments<'_>) -> CBuf<N> {
    let mut out = CBuf::new();
    // CBuf::write_str never fails, so neither can this.
    let _ = fmt::write(&mut out, args);
    out
}

/// A pointer as glibc's `%p` prints it: `0x` and lowercase hex, `(nil)` for NULL. Feed's
/// `bufferAddr` is parsed back into a pointer by libplayerAPIs, so this is wire format, not
/// cosmetics.
#[derive(Clone, Copy)]
pub(crate) struct Ptr(pub(crate) usize);

impl Ptr {
    pub(crate) fn of<T>(p: *const T) -> Self {
        Ptr(p.addr())
    }
}

impl fmt::Display for Ptr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            f.write_str("(nil)")
        } else {
            write!(f, "{:#x}", self.0)
        }
    }
}

/// `Feed`'s argument for one access unit (`char j[160]`).
pub(crate) fn feed_json(addr: Ptr, size: c_uint, pts: i64, es_data: c_int) -> CBuf<160> {
    cformat(format_args!(
        "{{\"bufferAddr\":\"{addr}\",\"bufferSize\":{size},\"pts\":{pts},\"esData\":{es_data}}}"
    ))
}

/// `setVolume`'s argument (`char json[32]`); without an `ease` block the pipeline applies it at once.
pub(crate) fn volume_json(volume: c_int) -> CBuf<32> {
    cformat(format_args!("{{\"volume\":{volume}}}"))
}

/// `setTimeToDecode`'s argument (`char j[64]`).
pub(crate) fn position_json(position_ns: i64) -> CBuf<64> {
    cformat(format_args!("{{\"position\":{position_ns}}}"))
}

/// `Seek`'s argument: decimal milliseconds (`char arg[32]`).
pub(crate) fn seek_arg(ms: i64) -> CBuf<32> {
    cformat(format_args!("{ms}"))
}

/// The two-key Dolby Atmos descriptor for `AcbAPI_setMediaAudioData` (`char j[192]`). The trailing
/// newline is jsoncpp FastWriter's and is byte-for-byte what Chromium sends; json-c ignores it, it is
/// there for exactness. A media id long enough to truncate this yields JSON ACB rejects client-side
/// (-1), as the C did.
pub(crate) fn atmos_json(media_id: &CStr) -> CBuf<192> {
    let mut out = CBuf::new();
    out.push_bytes(b"{\"audio\":{\"immersive\":\"ATMOS\"},\"context\":\"");
    out.push_bytes(media_id.to_bytes());
    out.push_bytes(b"\"}\n");
    out
}

/// `snprintf(dst, sizeof dst, "%s", src)`: copy at most `dst.len() - 1` bytes and terminate.
pub(crate) fn copy_c_string(dst: &mut [u8], src: &CStr) {
    let take = src.to_bytes().len().min(dst.len().saturating_sub(1));
    dst[..take].copy_from_slice(&src.to_bytes()[..take]);
    if let Some(end) = dst.get_mut(take) {
        *end = 0;
    }
}

/// Feed's reply as the engine reads it: `B` for BufferFull, `O` for Ok, `e` for anything else,
/// including no string at all. `strstr` order: BufferFull is looked for first.
pub(crate) fn feed_verdict(reply: Option<&CStr>) -> u8 {
    let Some(reply) = reply else { return b'e' };
    let reply = reply.to_bytes();
    if contains(reply, b"BufferFull") {
        b'B'
    } else if contains(reply, b"Ok") {
        b'O'
    } else {
        b'e'
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s<const N: usize>(b: &CBuf<N>) -> &str {
        std::str::from_utf8(b.as_bytes()).unwrap()
    }

    #[test]
    fn feed_payload_matches_the_c_format() {
        let j = feed_json(Ptr(0xb6a0_1000), 4096, 1_234_567_890, 1);
        assert_eq!(
            s(&j),
            r#"{"bufferAddr":"0xb6a01000","bufferSize":4096,"pts":1234567890,"esData":1}"#
        );
        // The terminating NUL is in place for the FFI call.
        // SAFETY: as_ptr points at the live buffer, NUL-terminated by construction.
        let c = unsafe { CStr::from_ptr(j.as_ptr()) };
        assert_eq!(c.to_bytes(), j.as_bytes());
    }

    #[test]
    fn feed_payload_fits_its_buffer_at_the_extremes() {
        let j = feed_json(Ptr(u32::MAX as usize), u32::MAX, i64::MIN, c_int::MIN);
        assert_eq!(
            s(&j),
            r#"{"bufferAddr":"0xffffffff","bufferSize":4294967295,"pts":-9223372036854775808,"esData":-2147483648}"#
        );
    }

    #[test]
    fn pointers_print_like_glibc_percent_p() {
        assert_eq!(Ptr(0).to_string(), "(nil)");
        assert_eq!(Ptr(0x1f).to_string(), "0x1f");
    }

    #[test]
    fn the_small_payloads_match_the_c_format() {
        assert_eq!(s(&volume_json(0)), r#"{"volume":0}"#);
        assert_eq!(s(&volume_json(100)), r#"{"volume":100}"#);
        assert_eq!(s(&position_json(-5)), r#"{"position":-5}"#);
        assert_eq!(s(&seek_arg(90_000)), "90000");
        assert_eq!(s(&seek_arg(i64::MIN)), "-9223372036854775808");
    }

    #[test]
    fn atmos_descriptor_is_byte_exact() {
        let j = atmos_json(c"_Media_Id_7");
        assert_eq!(
            s(&j),
            "{\"audio\":{\"immersive\":\"ATMOS\"},\"context\":\"_Media_Id_7\"}\n"
        );
    }

    #[test]
    fn output_truncates_like_snprintf() {
        let long = std::ffi::CString::new(vec![b'x'; 300]).unwrap();
        let j = atmos_json(&long);
        assert_eq!(j.as_bytes().len(), 191);
        assert!(j.as_bytes().starts_with(b"{\"audio\""));
        let mut small = CBuf::<4>::new();
        small.push_bytes(b"abcdef");
        assert_eq!(small.as_bytes(), b"abc");
    }

    #[test]
    fn window_id_copy_keeps_63_bytes_and_a_nul() {
        let mut dst = [0xffu8; 64];
        copy_c_string(&mut dst, c"_Window_Id_3");
        assert_eq!(&dst[..13], b"_Window_Id_3\0");
        let long = std::ffi::CString::new(vec![b'w'; 100]).unwrap();
        copy_c_string(&mut dst, &long);
        assert!(dst[..63].iter().all(|&b| b == b'w'));
        assert_eq!(dst[63], 0);
    }

    #[test]
    fn feed_verdicts() {
        assert_eq!(feed_verdict(None), b'e');
        assert_eq!(feed_verdict(Some(c"Ok")), b'O');
        assert_eq!(feed_verdict(Some(c"BufferFull")), b'B');
        assert_eq!(feed_verdict(Some(c"{\"r\":\"BufferFull Ok\"}")), b'B');
        assert_eq!(feed_verdict(Some(c"")), b'e');
        assert_eq!(feed_verdict(Some(c"Error")), b'e');
    }
}
