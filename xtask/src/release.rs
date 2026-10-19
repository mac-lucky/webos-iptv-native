//! `cargo xtask release`: the stable package and the files a GitHub release of the public repo
//! carries beside it, all in `target/release-assets/`.
//!
//! The webOS Homebrew Channel reads `releases/latest/download/<id>.manifest.json` and resolves its
//! bare `ipkUrl` next to it, so the ipk and the manifest must be assets of the same release. The
//! TV checks the download against the manifest's sha256 and nothing earlier does, so the hash is
//! taken here from the very file written beside it. The FFmpeg tarball is the corresponding source
//! for the LGPL libraries in the package, and `rust-crates.txt` the crate notices it carries.
//!
//! A release builds only from a clean tree and the committed Cargo.lock, so what is published is
//! the source of one commit.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::ipk::{json_str, read};
use crate::{ffmpeg, ipk, output, root, sha256_file, target_dir, Opts, Result};

/// The public GitHub repository releases are published from.
const REPO: &str = "mac-lucky/webos-iptv-native";

pub fn release() -> Result {
    let dirty = output(git().args(["status", "--porcelain"]))?;
    if !dirty.is_empty() {
        return Err(format!(
            "uncommitted changes; a release builds only a clean tree:\n{}",
            String::from_utf8_lossy(&dirty).trim_end()
        ));
    }
    let version = json_str(&appinfo()?, "version")?;
    let tag = git()
        .args(["describe", "--exact-match", "--tags"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    if tag.as_deref() != Some(format!("v{version}").as_str()) {
        eprintln!(
            "warning: HEAD is {}, not v{version}; tag it before publishing",
            tag.map_or("untagged".into(), |t| format!("tagged {t}"))
        );
    }
    ffmpeg::require()?;
    ipk::package(&Opts {
        release: true,
        flavor: "stable".into(),
        locked: true,
    })?;
    let out = target_dir().join("release-assets");
    if out.exists() {
        fs::remove_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;

    let ipk = ipk::path("stable")?;
    let ipk_name = file_name(&ipk);
    let published = out.join(&ipk_name);
    copy(&ipk, &published)?;
    let (manifest_name, manifest) = manifest(&ipk_name, &sha256_file(&published)?)?;
    write(&out.join(manifest_name), manifest.as_bytes())?;
    let tarball = ffmpeg::source_tarball()?;
    copy(&tarball, &out.join(file_name(&tarball)))?;
    // The copy `package` wrote beside the ipk, the same text as inside it.
    copy(
        &ipk.with_file_name("rust-crates.txt"),
        &out.join("rust-crates.txt"),
    )?;
    let sums = sha256sums(&out)?;
    write(&out.join("SHA256SUMS"), sums.as_bytes())?;

    eprint!("{sums}");
    eprintln!("wrote {}", out.display());
    Ok(())
}

/// The Homebrew Channel manifest (webosbrew's `PackageManifest` schema) with the fields
/// `webosbrew-gen-manifest` writes, and its file name. The id, version, type, title and
/// description are the package's own, from `pkg/appinfo.json`.
fn manifest(ipk_name: &str, sha256: &str) -> Result<(String, String)> {
    let appinfo = appinfo()?;
    let field = |key| json_str(&appinfo, key).map_err(|e| format!("pkg/appinfo.json: {e}"));
    let (id, version) = (field("id")?, field("version")?);
    // The Channel keys an install on the id and offers an update when the version string differs,
    // so both must be the ones inside the package; its file name, from the same appinfo, agrees.
    if ipk_name != format!("{id}_{version}_arm.ipk") {
        return Err(format!(
            "{ipk_name} is not the package of {id} {version} (pkg/appinfo.json)"
        ));
    }
    let manifest = serde_json::json!({
        "id": id,
        "version": version,
        "type": field("type")?,
        "title": field("title")?,
        "appDescription": field("appDescription")?,
        "iconUri": format!("https://raw.githubusercontent.com/{REPO}/main/pkg/largeIcon.png"),
        "sourceUrl": format!("https://github.com/{REPO}"),
        "rootRequired": false,
        "ipkUrl": ipk_name,
        "ipkHash": { "sha256": sha256 },
    });
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    Ok((format!("{id}.manifest.json"), text + "\n"))
}

fn appinfo() -> Result<String> {
    String::from_utf8(read(&root().join("pkg/appinfo.json"))?)
        .map_err(|e| format!("pkg/appinfo.json: {e}"))
}

fn git() -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(root());
    cmd
}

/// `sha256sum` output for every file in `dir`, sorted by name.
fn sha256sums(dir: &Path) -> Result<String> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        names.push(file_name(&entry.map_err(|e| e.to_string())?.path()));
    }
    names.sort();
    let mut sums = String::new();
    for name in names {
        sums.push_str(&format!("{}  {name}\n", sha256_file(&dir.join(&name))?));
    }
    Ok(sums)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn copy(from: &Path, to: &Path) -> Result {
    fs::copy(from, to)
        .map(drop)
        .map_err(|e| format!("{} -> {}: {e}", from.display(), to.display()))
}

fn write(path: &Path, bytes: &[u8]) -> Result {
    fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_names_the_package_and_its_hash() {
        let appinfo = String::from_utf8(read(&root().join("pkg/appinfo.json")).unwrap()).unwrap();
        let (id, version) = (
            json_str(&appinfo, "id").unwrap(),
            json_str(&appinfo, "version").unwrap(),
        );
        let ipk = format!("{id}_{version}_arm.ipk");
        let (name, text) = manifest(&ipk, &"ab".repeat(32)).unwrap();
        assert_eq!(name, format!("{id}.manifest.json"));
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["ipkUrl"], ipk.as_str());
        assert_eq!(json["ipkHash"]["sha256"], "ab".repeat(32).as_str());
        assert_eq!(json["rootRequired"], false);
        assert_eq!(json["type"], "native");
        assert!(manifest("other_0.0.1_arm.ipk", "00").is_err());
    }

    #[test]
    fn sha256sums_is_sorted_sha256sum_format() {
        let dir = std::env::temp_dir().join(format!("xtask-sums-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("b"), "abc").unwrap();
        fs::write(dir.join("a"), "").unwrap();
        assert_eq!(
            sha256sums(&dir).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  a\n\
             ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  b\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
