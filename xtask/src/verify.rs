//! `cargo xtask verify`: webosbrew-ipk-verify, which checks every ELF in the package against
//! firmware symbol data for webOS 6 (C1) and newer (C5 is webOS 25, firmware 10.x).
//!
//! The tool ships only as Debian packages, so it runs in a pinned Debian container with no
//! network; the packages are fetched and sha256-checked like every other download.

use std::path::Path;
use std::process::Command;

use crate::{fetch, run, target_dir, Result};

const RELEASE: &str =
    "https://github.com/webosbrew/dev-toolbox-cli/releases/download/v20261017-2d5a7b6";
/// (Debian arch, package, sha256) for toolbox 0.10.0.
#[rustfmt::skip]
const DEBS: &[(&str, &str, &str)] = &[
    ("arm64", "webosbrew-toolbox-fw-symbols_0.10.0-1_arm64.deb", "8652b32e08e8e71c55b572ce6f1e6e9b98f87998a2cae11912d8ff4fb51bab12"),
    ("arm64", "webosbrew-toolbox-ipk-verify_0.10.0-1_arm64.deb", "fea1eccba0de1e13ab2a7934904e8cae51451b39c229aff98ac6c86d5eeda38b"),
    ("amd64", "webosbrew-toolbox-fw-symbols_0.10.0-1_amd64.deb", "25fee8952f02ea570009cfb3aeaa101759036f24245fc6069d9d454e81a605b9"),
    ("amd64", "webosbrew-toolbox-ipk-verify_0.10.0-1_amd64.deb", "eb0faf0f4bf616b14ffb6bdec811a474bb551eff345251aa594066883a8987cf"),
];
const IMAGE: &str =
    "debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587";

pub fn verify(ipk: &Path) -> Result {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => return Err(format!("no webosbrew toolbox build for {other}")),
    };
    let debs = target_dir().join("toolbox");
    let mut install = Vec::new();
    for &(_, name, sha256) in DEBS.iter().filter(|d| d.0 == arch) {
        fetch(&format!("{RELEASE}/{name}"), sha256, &debs.join(name))?;
        install.push(format!("/debs/{name}"));
    }
    let ipk = ipk
        .canonicalize()
        .map_err(|e| format!("{}: {e}", ipk.display()))?;
    let (dir, file) = (
        ipk.parent().unwrap_or(Path::new("/")),
        ipk.file_name().unwrap_or_default(),
    );
    let script = format!(
        "dpkg -i {} >/dev/null && webosbrew-ipk-verify --details --fw-releases '>=6.0' \"$1\"",
        install.join(" ")
    );
    run(Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            "none",
            "--platform",
            &format!("linux/{arch}"),
        ])
        .arg("-v")
        .arg(format!("{}:/debs:ro", debs.display()))
        .arg("-v")
        .arg(format!("{}:/ipk:ro", dir.display()))
        .args([IMAGE, "sh", "-c", &script, "verify"])
        .arg(Path::new("/ipk").join(file)))
}
