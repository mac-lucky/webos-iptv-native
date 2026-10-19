//! The webosbrew native toolchain (GCC 12, binutils, a glibc 2.12 sysroot carrying the TV's own
//! libraries). It is a build tool and is not redistributed: `setup` installs it outside the repo,
//! in `$WEBOS_SDK` (default `~/webos-ndk/arm-webos-linux-gnueabi_sdk-buildroot`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{fetch, run, untar, Result, TARGET};

/// The pinned release. The toolchain fixes the ABI the binary is built against (glibc,
/// libstdc++), so moving it is a port, not a routine bump.
pub const RELEASE: &str = "webos-d7ed7ee.6";
const SDK_DIR: &str = "arm-webos-linux-gnueabi_sdk-buildroot";
/// Host builds published for the release: (rust os, rust arch, asset suffix, sha256).
#[rustfmt::skip]
const HOSTS: &[(&str, &str, &str, &str)] = &[
    ("macos", "aarch64", "darwin-arm64", "954333694ab1fe1f7737e81a0be1e3188cdfffc1a119c81c86f5fcebf1857c45"),
    ("macos", "x86_64", "darwin-x86_64", "75e7451ac39604872ae7f07894cbe8edb62b2091664a1781cb193e7ebe56aa75"),
    ("linux", "aarch64", "linux-aarch64", "45a2d12ff557457d92cde4fddaa77a6f1090fca03adc43bb74397e5e0c379501"),
];
/// Tool name prefix of the NDK's binutils and compilers.
pub const PREFIX: &str = "arm-webos-linux-gnueabi-";
/// The ignored symlink in the workspace root that `.cargo/config.toml` reaches the NDK through.
const LINK: &str = ".webos-ndk";

/// Where the NDK is (or will be) installed.
pub fn sdk_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("WEBOS_SDK") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME").ok_or("HOME is not set; set WEBOS_SDK")?;
            Path::new(&home).join("webos-ndk").join(SDK_DIR)
        }
    };
    if !dir.is_absolute() {
        return Err("WEBOS_SDK must be an absolute path".into());
    }
    Ok(dir)
}

pub fn tool(sdk: &Path, name: &str) -> PathBuf {
    sdk.join("bin").join(format!("{PREFIX}{name}"))
}

/// Write `src` to `dst` without the symbols and sections a TV binary does not need.
pub fn strip(sdk: &Path, src: &Path, dst: &Path) -> Result {
    run(Command::new(tool(sdk, "strip"))
        .arg("--strip-unneeded")
        .arg("-o")
        .arg(dst)
        .arg(src))
}

/// The C compiler: the real gcc driver, for the reason `.cargo/config.toml` gives.
pub fn cc(sdk: &Path) -> PathBuf {
    tool(sdk, "gcc.br_real")
}

pub fn sysroot(sdk: &Path) -> PathBuf {
    sdk.join("arm-webos-linux-gnueabi").join("sysroot")
}

/// The installed NDK, with the workspace's `.webos-ndk` link pointing at it.
pub fn require() -> Result<PathBuf> {
    let sdk = sdk_dir()?;
    if !cc(&sdk).is_file() {
        return Err(format!(
            "no webOS NDK at {} (run `cargo xtask setup`, or set WEBOS_SDK)",
            sdk.display()
        ));
    }
    let link = crate::root().join(LINK);
    if fs::read_link(&link).ok().as_deref() != Some(sdk.as_path()) {
        if link.symlink_metadata().is_ok() {
            fs::remove_file(&link).map_err(|e| format!("{}: {e}", link.display()))?;
        }
        std::os::unix::fs::symlink(&sdk, &link).map_err(|e| format!("{}: {e}", link.display()))?;
        eprintln!("{LINK} -> {}", sdk.display());
    }
    Ok(sdk)
}

pub fn setup() -> Result {
    run(Command::new("rustup").args(["target", "add", TARGET]))?;
    let sdk = sdk_dir()?;
    if cc(&sdk).is_file() {
        eprintln!("NDK already at {}", sdk.display());
        return require().map(drop);
    }
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let &(_, _, asset, sha256) = HOSTS
        .iter()
        .find(|h| h.0 == os && h.1 == arch)
        .ok_or_else(|| format!("{RELEASE} has no NDK build for {os}-{arch}"))?;
    let parent = sdk.parent().ok_or("WEBOS_SDK has no parent directory")?;
    let name = format!("{SDK_DIR}_{asset}.tar.bz2");
    let url =
        format!("https://github.com/webosbrew/native-toolchain/releases/download/{RELEASE}/{name}");
    let tarball = parent.join("ndk-download").join(&name);
    fetch(&url, sha256, &tarball)?;

    let staging = parent.join(".ndk-extract");
    untar(&tarball, &staging)?;
    fs::rename(staging.join(SDK_DIR), &sdk).map_err(|e| format!("{}: {e}", sdk.display()))?;
    fs::remove_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    // Buildroot SDKs hardcode their build location; this rewrites it to where it now lives.
    run(Command::new("./relocate-sdk.sh").current_dir(&sdk))?;
    fs::remove_dir_all(parent.join("ndk-download")).map_err(|e| format!("ndk-download: {e}"))?;
    run(Command::new(cc(&sdk)).arg("--version"))?;
    require().map(drop)
}
