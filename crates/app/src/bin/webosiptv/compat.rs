//! glibc functions Rust's std calls directly that the NDK's glibc 2.12 sysroot does not define.
//!
//! The binary links against that sysroot, so a symbol it lacks must come from the binary itself;
//! the NDK's own replacement archive is excluded (`.cargo/config.toml`). Each function here does
//! what the Linux man page documents and nothing more.

use libc::c_ulong;
use std::sync::OnceLock;

const WORD: usize = std::mem::size_of::<c_ulong>();

/// `getauxval(3)` (glibc 2.16), used by std's CPU feature detection. Reads `/proc/self/auxv` once.
/// A missing key returns 0 with errno `ENOENT`, as glibc does.
#[no_mangle]
pub extern "C" fn getauxval(key: c_ulong) -> c_ulong {
    static AUXV: OnceLock<Vec<(c_ulong, c_ulong)>> = OnceLock::new();
    let auxv = AUXV.get_or_init(|| {
        let bytes = std::fs::read("/proc/self/auxv").unwrap_or_default();
        let (pairs, _) = bytes.as_chunks::<{ 2 * WORD }>();
        pairs
            .iter()
            .map(|pair| (word(&pair[..WORD]), word(&pair[WORD..])))
            .take_while(|&(k, _)| k != 0)
            .collect()
    });
    match auxv.iter().find(|&&(k, _)| k == key) {
        Some(&(_, value)) => value,
        None => {
            // SAFETY: errno is this thread's own slot.
            unsafe { *libc::__errno_location() = libc::ENOENT };
            0
        }
    }
}

fn word(bytes: &[u8]) -> c_ulong {
    let mut b = [0; WORD];
    b.copy_from_slice(bytes);
    c_ulong::from_ne_bytes(b)
}
