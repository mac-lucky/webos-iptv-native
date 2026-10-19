//! `licenses/rust-crates.txt`: the Rust code linked into the TV binary. First the standard
//! library, then each third-party crate with its version, declared licence and repository, then
//! the licence files each one ships, in full.
//!
//! The crate set is `cargo tree`'s for the normal dependencies of every workspace crate but xtask
//! on the TV target, with the features the build resolves: the app's set, or more. Procedural
//! macros and everything only they use are left out: they run on the build machine and are not
//! linked. Each crate's licence fields and source directory come from `cargo metadata`. No path
//! from this machine is written, so one Cargo.lock and one toolchain always give the same file.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::ipk::read;
use crate::{cargo, output, root, Result, TARGET};

/// File name prefixes of licence texts, matched ignoring case.
const PREFIXES: &[&str] = &["LICENSE", "LICENCE", "COPYING", "NOTICE"];
/// The standard library's own crates, linked into every binary.
const STD_CRATES: &str = "std, core, alloc, panic_unwind, unwind and compiler_builtins";
/// The crates.io crates std vendors for backtraces and symbol demangling. Any other crate the
/// binary names under `/rust/deps/` is listed with them.
const STD_VENDORED: &[&str] = &["addr2line", "gimli", "object", "rustc-demangle"];
/// The standard library's licence texts, from the toolchain's `share/doc/rust/licenses` (the
/// rustc component, always installed): rust-lang's MIT and Apache-2.0, the LLVM exception that
/// compiler-rt (C code in compiler_builtins) adds to Apache-2.0, and Unicode-3.0 for core's tables.
const STD_TEXTS: &[&str] = &[
    "MIT.txt",
    "Apache-2.0.txt",
    "LLVM-exception.txt",
    "Unicode-3.0.txt",
];

/// The standard library as linked: `rustc -V`, the vendored crates and the licence texts.
struct Std {
    rustc: String,
    vendored: Vec<String>,
    texts: Vec<(String, String)>,
}

struct Crate {
    name: String,
    version: String,
    license: Option<String>,
    repository: Option<String>,
    /// (file name, text) of each licence file, sorted by name.
    files: Vec<(String, String)>,
    /// Without licence files: (source path, text) of the copyright notice its library opens with.
    notice: Option<(String, String)>,
}

/// The text of `licenses/rust-crates.txt` for the TV binary `bin`.
pub fn rust_crates(bin: &Path) -> Result<String> {
    let tree = output(cargo().args([
        "tree",
        "--locked",
        "--workspace",
        "--exclude",
        "xtask",
        "--target",
        TARGET,
        "--edges",
        "normal,no-proc-macro",
        "--prefix",
        "none",
        "--format",
        "{p}",
    ]))?;
    let linked = parse_tree(&String::from_utf8_lossy(&tree))?;
    let meta = output(cargo().args([
        "metadata",
        "--locked",
        "--format-version",
        "1",
        "--filter-platform",
        TARGET,
    ]))?;
    let meta: Value = serde_json::from_slice(&meta).map_err(|e| format!("cargo metadata: {e}"))?;
    let members: BTreeSet<&str> = meta["workspace_members"]
        .as_array()
        .ok_or("cargo metadata: no workspace_members")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let packages = meta["packages"]
        .as_array()
        .ok_or("cargo metadata: no packages")?;

    let mut crates = Vec::new();
    for (name, version) in &linked {
        let mut found = packages
            .iter()
            .filter(|p| p["name"] == name.as_str() && p["version"] == version.as_str());
        let (Some(package), None) = (found.next(), found.next()) else {
            return Err(format!(
                "cargo metadata has no single package {name} {version}"
            ));
        };
        if members.contains(package["id"].as_str().unwrap_or_default()) {
            continue;
        }
        let dir = package["manifest_path"]
            .as_str()
            .and_then(|m| Path::new(m).parent())
            .ok_or_else(|| format!("cargo metadata: no manifest_path for {name} {version}"))?;
        let files = licence_files(dir, package["license_file"].as_str())?;
        let notice = if files.is_empty() {
            source_notice(dir, package)?
        } else {
            None
        };
        if files.is_empty() {
            let quoted = notice.as_ref().map_or(String::new(), |n| {
                format!(" (quoting the notice in its {})", n.0)
            });
            eprintln!("note: {name} {version} ships no licence file{quoted}");
        }
        crates.push(Crate {
            name: name.clone(),
            version: version.clone(),
            license: package["license"].as_str().map(String::from),
            repository: package["repository"].as_str().map(String::from),
            files,
            notice,
        });
    }
    crates.sort_by(|a, b| {
        (&a.name, version_key(&a.version)).cmp(&(&b.name, version_key(&b.version)))
    });
    Ok(render(&standard_library(bin)?, &crates))
}

fn standard_library(bin: &Path) -> Result<Std> {
    let rustc = || {
        let mut cmd = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
        cmd.current_dir(root());
        cmd
    };
    let version = String::from_utf8_lossy(&output(rustc().arg("-V"))?)
        .trim()
        .to_string();
    let sysroot = output(rustc().args(["--print", "sysroot"]))?;
    let docs =
        PathBuf::from(String::from_utf8_lossy(&sysroot).trim()).join("share/doc/rust/licenses");
    let texts = STD_TEXTS
        .iter()
        .map(|name| Ok((name.to_string(), text(&read(&docs.join(name))?))))
        .collect::<Result<_>>()?;
    Ok(Std {
        rustc: version,
        vendored: vendored(&read(bin)?),
        texts,
    })
}

/// `name version` of each crate std vendored into `bin`, from the `/rust/deps/<name>-<version>/`
/// paths its panic messages carry, and each of `STD_VENDORED` that left no path, without a version.
fn vendored(bin: &[u8]) -> Vec<String> {
    const MARK: &[u8] = b"/rust/deps/";
    let mut found = BTreeSet::new();
    let mut rest = bin;
    while let Some(at) = rest.windows(MARK.len()).position(|w| w == MARK) {
        rest = &rest[at + MARK.len()..];
        let len = rest
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(b))
            .count();
        let dir = String::from_utf8_lossy(&rest[..len]);
        // The version starts at the first `-` followed by a digit: `rustc-demangle-0.1.28`.
        let split = dir
            .char_indices()
            .find(|&(i, c)| c == '-' && dir[i + 1..].starts_with(|d: char| d.is_ascii_digit()));
        if let (Some((i, _)), Some(b'/')) = (split, rest.get(len)) {
            found.insert((dir[..i].to_string(), dir[i + 1..].to_string()));
        }
    }
    let mut names: Vec<String> = found.iter().map(|(n, v)| format!("{n} {v}")).collect();
    for name in STD_VENDORED {
        if !found.iter().any(|(n, _)| n == name) {
            names.push(format!("{name} (no version recorded in the binary)"));
        }
    }
    names.sort();
    names
}

/// (name, version) of every package in `cargo tree --prefix none --format {p}` output.
fn parse_tree(text: &str) -> Result<BTreeSet<(String, String)>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut words = line.split_whitespace();
            match (words.next(), words.next().and_then(|v| v.strip_prefix('v'))) {
                (Some(name), Some(version)) => Ok((name.to_string(), version.to_string())),
                _ => Err(format!("cargo tree: unexpected line {line:?}")),
            }
        })
        .collect()
}

/// Orders `1.10.0` after `1.9.0`; parts that are not numbers compare as text after the numbers.
fn version_key(version: &str) -> Vec<(u64, &str)> {
    version
        .split(['.', '-', '+'])
        .map(|part| (part.parse().unwrap_or(u64::MAX), part))
        .collect()
}

/// The licence texts a crate ships: the files at its root named like `PREFIXES`, plus the one its
/// `license-file` names when that is somewhere else.
fn licence_files(dir: &Path, license_file: Option<&str>) -> Result<Vec<(String, String)>> {
    let mut paths = BTreeMap::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let upper = name.to_ascii_uppercase();
        if PREFIXES.iter().any(|p| upper.starts_with(p)) && path.is_file() {
            paths.insert(name, path);
        }
    }
    if let Some(file) = license_file {
        paths
            .entry(file.to_string())
            .or_insert_with(|| dir.join(file));
    }
    paths
        .into_iter()
        .map(|(name, path)| Ok((name, text(&read(&path)?))))
        .collect()
}

/// A licence file as quoted: LF line ends, no trailing blank lines.
fn text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    text.trim_end().to_string()
}

/// For a crate without licence files: the comment its library's root file opens with, when that
/// comment carries a copyright notice (qrcodegen keeps its MIT notice only there).
fn source_notice(dir: &Path, package: &Value) -> Result<Option<(String, String)>> {
    let lib = package["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| {
            t["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|k| k == "lib" || k == "rlib"))
        });
    let Some(src) = lib.and_then(|t| t["src_path"].as_str()).map(Path::new) else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&read(src)?).replace("\r\n", "\n");
    let comment = leading_comment(&text);
    if !comment.to_ascii_lowercase().contains("copyright") {
        return Ok(None);
    }
    let name = src.strip_prefix(dir).unwrap_or(src).display().to_string();
    Ok(Some((name, comment.to_string())))
}

/// The `/* ... */` block or run of `//` lines a source file opens with.
fn leading_comment(text: &str) -> &str {
    let text = text.trim_start();
    if text.starts_with("/*") {
        return text.find("*/").map_or("", |end| &text[..end + 2]);
    }
    let mut end = 0;
    for line in text.split_inclusive('\n') {
        if !line.trim_start().starts_with("//") {
            break;
        }
        end += line.len();
    }
    text[..end].trim_end()
}

fn render(std: &Std, crates: &[Crate]) -> String {
    let mut out = String::from(
        "Rust code in the webosiptv executable\n\n\
         The Rust standard library and the third-party Rust crates linked into the app's executable,\n\
         from its toolchain and Cargo.lock: each crate's version, declared licence and repository,\n\
         then the standard library's entry and each crate's licence files. The app's own crates are\n\
         GPL-3.0-or-later (LICENSE); FFmpeg is described in ffmpeg.txt.\n\n",
    );
    let declared = |c: &Crate| {
        c.license
            .clone()
            .unwrap_or_else(|| "(none declared)".into())
    };
    let id = |c: &Crate| format!("{} {}", c.name, c.version);
    let id_width = crates.iter().map(|c| id(c).len()).max().unwrap_or(0);
    let licence_width = crates.iter().map(|c| declared(c).len()).max().unwrap_or(0);
    for c in crates {
        let line = format!(
            "{:id_width$}  {:licence_width$}  {}",
            id(c),
            declared(c),
            c.repository.as_deref().unwrap_or("")
        );
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.push_str(&format!(
        "\n{}\nThe Rust standard library, {}\n\
         Crates: {STD_CRATES}, and the crates.io crates std vendors: {}\n\
         Licence: MIT OR Apache-2.0; core's Unicode tables (library/core/src/unicode): Unicode-3.0; \
         the compiler-rt code in compiler_builtins: Apache-2.0 WITH LLVM-exception\n\
         Copyright: The Rust Project Developers (see https://thanks.rust-lang.org) and the authors \
         of each vendored crate; Unicode, Inc.; the LLVM Project contributors\n\
         Repository: https://github.com/rust-lang/rust\n",
        "=".repeat(78),
        std.rustc,
        std.vendored.join(", ")
    ));
    for (name, text) in &std.texts {
        out.push_str(&format!("\n--- {name} ---\n\n{text}\n"));
    }
    for c in crates {
        out.push_str(&format!("\n{}\n{}\n", "=".repeat(78), id(c)));
        out.push_str(&format!("Licence: {}\n", declared(c)));
        if let Some(repository) = &c.repository {
            out.push_str(&format!("Repository: {repository}\n"));
        }
        if c.files.is_empty() {
            out.push_str(&match &c.notice {
                Some((name, text)) => format!(
                    "\nThe published crate ships no licence file; its notice, from {name}:\n\n{text}\n"
                ),
                None => "\nThe published crate ships no licence file; see its declared licence.\n"
                    .into(),
            });
        }
        for (name, text) in &c.files {
            out.push_str(&format!("\n--- {name} ---\n\n{text}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tree_takes_name_and_version() {
        let text = "webosiptv-app v0.0.0 (/w/crates/app)\nlibc v0.2.170\nlibc v0.2.170 (*)\n\n";
        let got: Vec<_> = parse_tree(text).unwrap().into_iter().collect();
        assert_eq!(
            got,
            [
                ("libc".to_string(), "0.2.170".to_string()),
                ("webosiptv-app".to_string(), "0.0.0".to_string()),
            ]
        );
        assert!(parse_tree("[build-dependencies]\n").is_err());
    }

    #[test]
    fn versions_order_numerically() {
        assert!(version_key("1.10.0") > version_key("1.9.0"));
        assert!(version_key("0.2.0") < version_key("0.10.0"));
    }

    #[test]
    fn licence_files_match_prefixes_ignoring_case() {
        let dir = std::env::temp_dir().join(format!("xtask-licences-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("LICENSES")).unwrap();
        for (name, text) in [
            ("LICENSE-MIT", "mit\r\n\r\n"),
            ("license.txt", "lower"),
            ("COPYING", "copying"),
            ("README.md", "no"),
            ("other/x", ""),
        ] {
            let path = dir.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let files = licence_files(&dir, Some("other/x")).unwrap();
        let names: Vec<_> = files.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(names, ["COPYING", "LICENSE-MIT", "license.txt", "other/x"]);
        assert_eq!(files[1].1, "mit");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_lists_then_quotes() {
        let crates = [
            Crate {
                name: "a".into(),
                version: "1.0.0".into(),
                license: Some("MIT".into()),
                repository: Some("https://example.invalid/a".into()),
                files: vec![("LICENSE".into(), "text".into())],
                notice: None,
            },
            Crate {
                name: "bb".into(),
                version: "0.1.0".into(),
                license: None,
                repository: None,
                files: Vec::new(),
                notice: None,
            },
            Crate {
                name: "c".into(),
                version: "2.0.0".into(),
                license: Some("MIT".into()),
                repository: None,
                files: Vec::new(),
                notice: Some(("src/lib.rs".into(), "/* Copyright */".into())),
            },
        ];
        let std = Std {
            rustc: "rustc 1.0.0 (abc 2015-05-15)".into(),
            vendored: vec!["gimli 0.1.0".into(), "object (no version)".into()],
            texts: vec![("MIT.txt".into(), "mit".into())],
        };
        let text = render(&std, &crates);
        assert!(text.contains(
            "\nThe Rust standard library, rustc 1.0.0 (abc 2015-05-15)\nCrates: std, core, alloc, \
             panic_unwind, unwind and compiler_builtins, and the crates.io crates std vendors: \
             gimli 0.1.0, object (no version)\n"
        ));
        assert!(text
            .contains("Repository: https://github.com/rust-lang/rust\n\n--- MIT.txt ---\n\nmit\n"));
        assert!(text.find("The Rust standard library,") < text.find("\na 1.0.0\n"));
        assert!(text.contains("\na 1.0.0   MIT              https://example.invalid/a\n"));
        assert!(text.contains("\nbb 0.1.0  (none declared)\n"));
        assert!(text.contains("\na 1.0.0\nLicence: MIT\nRepository: https://example.invalid/a\n\n--- LICENSE ---\n\ntext\n"));
        assert!(text.contains(
            "\nbb 0.1.0\nLicence: (none declared)\n\nThe published crate ships no licence file; see"
        ));
        assert!(text.contains("no licence file; its notice, from src/lib.rs:\n\n/* Copyright */\n"));
        assert!(text.is_ascii());
    }

    #[test]
    fn vendored_reads_rust_deps_paths() {
        let bin = b"\0/rust/deps/gimli-0.34.0/src/read/x.rs\0/rust/deps/rustc-demangle-0.1.28/src/\
                    \0/rust/deps/gimli-0.34.0/src/y.rs\0/rust/deps/no-version/\0/rust/deps/";
        assert_eq!(
            vendored(bin),
            [
                "addr2line (no version recorded in the binary)",
                "gimli 0.34.0",
                "object (no version recorded in the binary)",
                "rustc-demangle 0.1.28",
            ]
        );
    }

    #[test]
    fn leading_comment_takes_the_opening_block() {
        assert_eq!(
            leading_comment("\n/* a\n * b */\n/* c */\nfn x() {}"),
            "/* a\n * b */"
        );
        assert_eq!(leading_comment("// a\n// b\n\n//! doc\n"), "// a\n// b");
        assert_eq!(leading_comment("//! doc\nuse x;"), "//! doc");
        assert_eq!(leading_comment("fn x() {} // c"), "");
        assert_eq!(leading_comment("/* open"), "");
    }
}
