//! `cargo xtask package`: the .ipk, written in Rust so one commit always gives the same bytes.
//!
//! An ipk is an `ar` archive of `debian-binary`, `control.tar.gz` and `data.tar.gz`. The member
//! names are written bare: GNU ar's `name/` form makes the TV's installer fail the whole package.
//! Tar and gzip headers are normalised (root owner, fixed mtime, sorted paths, fixed modes).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use flate2::{Compression, GzBuilder};

use crate::{ndk, root, sha256_file, target_dir, Opts, Result, BIN};

/// Any fixed time works; 2010-01-01 is inside what old opkg builds accept.
const EPOCH: u64 = 1_262_304_000;
/// Files installed in the app directory: (source under the repo, path in the app dir). The notices
/// keep their repository paths, so every path THIRD-PARTY-NOTICES.md and NOTICE cite resolves.
const PAYLOAD: &[(&str, &str)] = &[
    ("pkg/icon.png", "icon.png"),
    ("pkg/largeIcon.png", "largeIcon.png"),
    ("pkg/appfont.ttf", "appfont.ttf"),
    ("pkg/appfont-bold.ttf", "appfont-bold.ttf"),
    ("pkg/OFL.txt", "OFL.txt"),
    ("pkg/le-roots.pem", "le-roots.pem"),
    ("LICENSE", "LICENSE"),
    ("NOTICE", "NOTICE"),
    ("THIRD-PARTY-NOTICES.md", "THIRD-PARTY-NOTICES.md"),
    ("licenses/MIT.txt", "licenses/MIT.txt"),
    ("licenses/Apache-2.0.txt", "licenses/Apache-2.0.txt"),
];

/// Archive path -> (contents, executable).
type Tree = BTreeMap<String, (Vec<u8>, bool)>;

pub fn package(opts: &Opts) -> Result {
    let app = App::load(&opts.flavor)?;
    let bin = crate::build(opts)?;
    let sdk = ndk::require()?;
    let out_dir = target_dir().join("ipk");
    fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;

    let dir = format!("usr/palm/applications/{}", app.id);
    let mut data = Tree::new();
    data.insert(
        format!("{dir}/appinfo.json"),
        (app.appinfo.clone().into_bytes(), false),
    );
    let stripped = out_dir.join(BIN);
    ndk::strip(&sdk, &bin, &stripped)?;
    data.insert(format!("{dir}/{}", app.main), (read(&stripped)?, true));
    for (src, name) in PAYLOAD {
        data.insert(format!("{dir}/{name}"), (read(&root().join(src))?, false));
    }
    // Also kept beside the ipk, where `release` takes it from.
    let crates = crate::licenses::rust_crates(&bin)?;
    let crates_txt = out_dir.join("rust-crates.txt");
    fs::write(&crates_txt, &crates).map_err(|e| format!("{}: {e}", crates_txt.display()))?;
    data.insert(
        format!("{dir}/licenses/rust-crates.txt"),
        (crates.into_bytes(), false),
    );
    for (name, bytes) in crate::ffmpeg::payload()? {
        data.insert(format!("{dir}/{name}"), (bytes, false));
    }
    data.insert(
        format!("usr/palm/packages/{}/packageinfo.json", app.id),
        (app.packageinfo().into_bytes(), false),
    );

    let kib = data
        .values()
        .map(|(b, _)| b.len())
        .sum::<usize>()
        .div_ceil(1024);
    let mut control = Tree::new();
    control.insert("control".into(), (app.control(kib).into_bytes(), false));

    let ipk = path(&opts.flavor)?;
    let archive = ar(&[
        ("debian-binary", b"2.0\n"),
        ("control.tar.gz", &targz(&control)?),
        ("data.tar.gz", &targz(&data)?),
    ]);
    fs::write(&ipk, archive).map_err(|e| format!("{}: {e}", ipk.display()))?;

    for (path, (bytes, exec)) in &data {
        eprintln!(
            "  {} {:>9}  {path}",
            if *exec { "x" } else { "-" },
            bytes.len()
        );
    }
    eprintln!(
        "wrote {} (Installed-Size {kib} KiB, sha256 {})",
        ipk.display(),
        sha256_file(&ipk)?
    );
    Ok(())
}

/// Where `package` writes a flavour's ipk.
pub fn path(flavor: &str) -> Result<PathBuf> {
    let app = App::load(flavor)?;
    Ok(target_dir()
        .join("ipk")
        .join(format!("{}_{}_arm.ipk", app.id, app.version)))
}

pub(crate) fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The app id of `flavor`, from `pkg/install-identities.json`. The id is a directory name on the
/// TV and goes into the provisioning script, so only letters, digits, `.` and `-` pass.
pub(crate) fn app_id(flavor: &str) -> Result<String> {
    let identities = String::from_utf8(read(&root().join("pkg/install-identities.json"))?)
        .map_err(|e| e.to_string())?;
    let id = identities
        .split('{')
        .find(|obj| json_str(obj, "name").is_ok_and(|n| n == flavor))
        .map(|obj| json_str(obj, "app_id"))
        .ok_or_else(|| format!("unknown flavour {flavor:?} (pkg/install-identities.json)"))??;
    if id.is_empty()
        || id.starts_with('.')
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
    {
        return Err(format!("bad app id {id:?}"));
    }
    Ok(id)
}

/// The package identity: `pkg/appinfo.json` re-pointed at a flavour from
/// `pkg/install-identities.json`. Only `id` and `title` move, so two installs sit side by side.
struct App {
    id: String,
    /// The executable's path in the app directory (`main` in appinfo.json).
    main: String,
    version: String,
    title: String,
    vendor: String,
    appinfo: String,
}

impl App {
    fn load(flavor: &str) -> Result<App> {
        let mut appinfo = String::from_utf8(read(&root().join("pkg/appinfo.json"))?)
            .map_err(|e| e.to_string())?;
        let (stable, version, mut title) = (
            json_str(&appinfo, "id")?,
            json_str(&appinfo, "version")?,
            json_str(&appinfo, "title")?,
        );
        let id = app_id(flavor)?;
        if flavor != "stable" {
            appinfo = replace_once(
                &appinfo,
                &format!("\"id\": \"{stable}\""),
                &format!("\"id\": \"{id}\""),
            )?;
            let flavored = format!("{title} {flavor}");
            appinfo = replace_once(
                &appinfo,
                &format!("\"title\": \"{title}\""),
                &format!("\"title\": \"{flavored}\""),
            )?;
            title = flavored;
        }
        // `main` is a path inside the app dir, and webOS takes only X.Y.Z integer versions.
        let main = json_str(&appinfo, "main")?;
        let main_path = Path::new(&main);
        if main_path.file_name() != Some(BIN.as_ref())
            || !main_path
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(format!(
                "appinfo main {main:?} must be a relative path to {BIN}"
            ));
        }
        let parts: Vec<&str> = version.split('.').collect();
        if parts.len() != 3
            || !parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(format!("version {version:?} is not X.Y.Z"));
        }
        let vendor = json_str(&appinfo, "vendor")?;
        Ok(App {
            id,
            main,
            version,
            title,
            vendor,
            appinfo,
        })
    }

    /// webOS's package descriptor: which app the package owns. Without it nothing registers.
    fn packageinfo(&self) -> String {
        let App {
            id,
            version,
            title,
            vendor,
            ..
        } = self;
        format!(
            "{{\n  \"app\": \"{id}\",\n  \"id\": \"{id}\",\n  \"loc_name\": \"{title}\",\n  \"package_format_version\": 2,\n  \
             \"vendor\": \"{vendor}\",\n  \"version\": \"{version}\"\n}}\n"
        )
    }

    fn control(&self, kib: usize) -> String {
        let App {
            id,
            version,
            title,
            vendor,
            ..
        } = self;
        format!(
            "Package: {id}\nVersion: {version}\nSection: misc\nPriority: optional\nArchitecture: arm\n\
             Installed-Size: {kib}\nMaintainer: {vendor}\nDescription: {title}\n\
             webOS-Package-Format-Version: 2\nwebOS-Packager-Version: webosiptv-xtask/1\n"
        )
    }
}

/// The string value of `"key": "value"` in one of our own flat JSON files. Values with escapes are
/// refused rather than half-parsed.
pub(crate) fn json_str(text: &str, key: &str) -> Result<String> {
    let missing = || format!("no string \"{key}\" in JSON");
    let rest = text
        .split_once(&format!("\"{key}\":"))
        .ok_or_else(missing)?
        .1
        .trim_start();
    let value = rest
        .strip_prefix('"')
        .and_then(|r| r.split_once('"'))
        .ok_or_else(missing)?
        .0;
    if value.contains('\\') {
        return Err(format!(
            "\"{key}\" has an escape; keep package metadata plain"
        ));
    }
    Ok(value.to_string())
}

fn replace_once(text: &str, from: &str, to: &str) -> Result<String> {
    match text.matches(from).count() {
        1 => Ok(text.replacen(from, to, 1)),
        n => Err(format!(
            "expected {from} once in pkg/appinfo.json, found {n}"
        )),
    }
}

/// A gzipped tar of `tree`, with every parent directory listed before its contents.
fn targz(tree: &Tree) -> Result<Vec<u8>> {
    let mut entries: BTreeMap<String, Option<&(Vec<u8>, bool)>> = BTreeMap::new();
    for (path, file) in tree {
        let mut dir = String::new();
        for part in path
            .split('/')
            .rev()
            .skip(1)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            dir = format!("{dir}{part}/");
            entries.insert(dir.clone(), None);
        }
        entries.insert(path.clone(), Some(file));
    }
    let mut tar = tar::Builder::new(Vec::new());
    for (path, file) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_path(&path).map_err(|e| format!("{path}: {e}"))?;
        h.set_mtime(EPOCH);
        h.set_uid(0);
        h.set_gid(0);
        h.set_username("root")
            .and_then(|()| h.set_groupname("root"))
            .map_err(|e| e.to_string())?;
        let bytes: &[u8] = match file {
            None => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(0o755);
                &[]
            }
            Some((bytes, exec)) => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(if *exec { 0o755 } else { 0o644 });
                bytes
            }
        };
        h.set_size(bytes.len() as u64);
        h.set_cksum();
        tar.append(&h, bytes).map_err(|e| format!("{path}: {e}"))?;
    }
    let raw = tar.into_inner().map_err(|e| e.to_string())?;
    // mtime 0 and no file name keep the gzip header itself reproducible.
    let mut gz = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::best());
    gz.write_all(&raw).map_err(|e| e.to_string())?;
    gz.finish().map_err(|e| e.to_string())
}

/// A common-format `ar` archive: 60-byte headers, names verbatim (no GNU `/`), 2-byte alignment.
fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, data) in members {
        assert!(name.len() <= 16, "{name} needs a long-name table");
        out.extend(
            format!(
                "{name:<16}{EPOCH:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                0,
                0,
                "100644",
                data.len()
            )
            .as_bytes(),
        );
        out.extend(*data);
        if data.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ar_headers_are_bare_and_aligned() {
        let a = ar(&[("debian-binary", b"2.0\n"), ("x", b"odd")]);
        assert!(a.starts_with(
            b"!<arch>\ndebian-binary   1262304000  0     0     100644  4         `\n2.0\n"
        ));
        assert!(a.ends_with(b"`\nodd\n"));
        assert_eq!((a.len() - 8) % 2, 0);
    }

    #[test]
    fn targz_is_reproducible_and_lists_parents() {
        let mut t = Tree::new();
        t.insert("a/b/c.txt".into(), (b"hi".to_vec(), false));
        t.insert("a/run".into(), (b"#!".to_vec(), true));
        let gz = targz(&t).unwrap();
        assert_eq!(gz, targz(&t).unwrap());
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&gz[..]));
        let listed: Vec<(String, u32, u64)> = archive
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let h = e.header();
                (
                    e.path().unwrap().display().to_string(),
                    h.mode().unwrap(),
                    h.mtime().unwrap(),
                )
            })
            .collect();
        let names: Vec<_> = listed.iter().map(|l| l.0.as_str()).collect();
        assert_eq!(names, ["a/", "a/b/", "a/b/c.txt", "a/run"]);
        assert!(listed.iter().all(|l| l.2 == EPOCH));
        assert_eq!(listed[3].1, 0o755);
        assert_eq!(listed[2].1, 0o644);
    }

    #[test]
    fn json_str_reads_flat_strings() {
        let text = "{\n  \"id\": \"a.b\",\n  \"title\": \"T\"\n}";
        assert_eq!(json_str(text, "id").unwrap(), "a.b");
        assert_eq!(json_str(text, "title").unwrap(), "T");
        assert!(json_str(text, "version").is_err());
        assert!(json_str("{\"id\": \"a\\\"b\"}", "id").is_err());
    }

    #[test]
    fn flavours_move_only_id_and_title() {
        let stable = App::load("stable").unwrap();
        let debug = App::load("debug").unwrap();
        assert_eq!(debug.id, format!("{}.debug", stable.id));
        assert_eq!(debug.title, format!("{} debug", stable.title));
        assert_eq!(debug.version, stable.version);
        let changed: Vec<_> = stable
            .appinfo
            .lines()
            .zip(debug.appinfo.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(changed.len(), 2);
        assert!(App::load("nope").is_err());
    }
}
