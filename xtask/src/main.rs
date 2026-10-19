//! `cargo xtask <command>`: the project's build tooling, in Rust.
//!
//! External programs are used only where a crate would be a new dependency: `curl` for downloads,
//! `tar` for the NDK's bzip2 and FFmpeg's xz archives, and the NDK's own compiler and `make` for
//! FFmpeg. Every download is checked against a pinned sha256 before anything reads it.

mod backup;
mod elf;
mod ffmpeg;
mod ipk;
mod ndk;
mod provision;
mod replay;
mod verify;

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use tv_base::sha256::Sha256;

const USAGE: &str = "usage: cargo xtask <command>

  setup                         add the rustup target and install the pinned webOS NDK
  build [--release]             cross-build the TV binary, then check its glibc symbol versions
  package [--release] [--flavor stable|debug|beta]
                                build, then write target/ipk/<id>_<version>_arm.ipk
  ffmpeg [--host]               cross-build the bundled LGPL FFmpeg into target/ffmpeg/lib
                                (--host: the same build for this Mac, staged into pkg/)
  verify [<ipk>]                webosbrew-ipk-verify in Docker for webOS 6+ (default: stable ipk)
  sim [<args>]                  run the desktop simulator; args go to the app (--keys, --shot)
  provision <ssh-alias> [--from <file>] [--epg-from <file>] [--flavor stable|debug|beta]
                                send the playlist URL in <file> (or $WEBOSIPTV_PLAYLIST_FILE)
                                and/or the guide URL in the --epg-from file to the TV app over
                                ssh stdin
  backup <ssh-alias> pull|push <file> [--flavor stable|debug|beta]
                                copy the app's backup (Settings > General > Backup) from the
                                TV into <file> (0600), or <file> onto the TV, over ssh
  tv-replay <ssh-alias> <script> [--out <dir>] [--flavor stable|debug|beta]
                                press the script's remote keys on the TV (only with our app or
                                the home screen in front), keep screenshots and the event log
                                in target/tv-replay/
  glibc <elf>                  list an ARM binary's versioned glibc needs (fails above 2.28)
  check                         rustfmt and clippy -D warnings on our own crates, all host tests";

pub type Result<T = ()> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(args: &[String]) -> Result {
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("setup") => ndk::setup(),
        Some("build") => build(&Opts::parse(rest)?).map(drop),
        Some("package") => ipk::package(&Opts::parse(rest)?),
        Some("ffmpeg") => ffmpeg::build(rest),
        Some("glibc") => match rest {
            [elf] => elf::check_glibc(Path::new(elf)),
            _ => Err("glibc takes one path".into()),
        },
        Some("verify") => match rest {
            [] => verify::verify(&ipk::path("stable")?),
            [ipk] => verify::verify(Path::new(ipk)),
            _ => Err("verify takes at most one path".into()),
        },
        Some("sim") => run(cargo()
            .args([
                "run",
                "-p",
                "webosiptv-app",
                "--features",
                "hostsim",
                "--bin",
                BIN,
                "--",
            ])
            .args(rest)),
        Some("provision") => provision::run(rest),
        Some("backup") => backup::run(rest),
        Some("tv-replay") => replay::run(rest),
        Some("check") => check(),
        _ => Err(USAGE.into()),
    }
}

/// Flags shared by `build` and `package`.
pub struct Opts {
    pub release: bool,
    pub flavor: String,
}

impl Opts {
    fn parse(args: &[String]) -> Result<Opts> {
        let mut opts = Opts {
            release: false,
            flavor: "stable".into(),
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--release" => opts.release = true,
                "--flavor" => opts.flavor = it.next().ok_or("--flavor needs a value")?.clone(),
                other => return Err(format!("unknown flag {other}\n\n{USAGE}")),
            }
        }
        Ok(opts)
    }

    /// The cargo profile: `tvdev` (release codegen, no fat LTO) unless `--release`.
    pub fn profile(&self) -> &'static str {
        if self.release {
            "release"
        } else {
            "tvdev"
        }
    }
}

/// The TV binary and the Rust target it is built for.
pub const BIN: &str = "webosiptv";
pub const TARGET: &str = "armv7-unknown-linux-gnueabi";

/// Cross-build the workspace for the TV and return the path of the checked binary.
pub fn build(opts: &Opts) -> Result<PathBuf> {
    ndk::require()?;
    // Every library crate too, so a crate that does not build for the TV fails here even before
    // the binary reaches it. xtask is a host tool and is left out.
    run(cargo()
        .args([
            "build",
            "--workspace",
            "--exclude",
            "xtask",
            "--lib",
            "--bin",
            BIN,
        ])
        .args(["--target", TARGET, "--profile", opts.profile()])
        .args(["--config", &remap_paths()]))?;
    let bin = target_dir().join(TARGET).join(opts.profile()).join(BIN);
    elf::check_glibc(&bin)?;
    Ok(bin)
}

/// A `--config` adding `--remap-path-prefix` to the TV target's rustflags (cargo appends it to
/// `.cargo/config.toml`'s), so the binary's panic locations name `/build/...` and `~/...` rather
/// than this machine's paths. The later prefix wins where both match: the workspace inside $HOME.
fn remap_paths() -> String {
    let mut flags = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        flags.push(format!(
            "--remap-path-prefix={}=~",
            Path::new(&home).display()
        ));
    }
    flags.push(format!("--remap-path-prefix={}=/build", root().display()));
    format!("target.{TARGET}.rustflags={flags:?}")
}

/// Packages written for this project. The crates imported from upstream are left unformatted and
/// unlinted so their diffs against upstream stay readable; their tests still run.
const OWN: &[&str] = &[
    "iptv",
    "tv_screens",
    "tv_starfish",
    "webosiptv-app",
    "xtask",
];

fn check() -> Result {
    let packages: Vec<String> = OWN.iter().map(|p| format!("--package={p}")).collect();
    run(cargo().args(["fmt", "--check"]).args(&packages))?;
    run(cargo()
        .args(["clippy", "--all-targets", "--no-deps"])
        .args(&packages)
        .args(["--", "-D", "warnings"]))?;
    run(cargo().args(["test", "--workspace"]))
}

fn cargo() -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root());
    cmd
}

/// The workspace root (xtask lives one level below it).
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent")
        .to_path_buf()
}

/// Cargo's target directory, honouring `CARGO_TARGET_DIR` like cargo itself does.
pub fn target_dir() -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => root().join(dir),
        None => root().join("target"),
    }
}

/// Run a command to completion, failing on a non-zero exit.
pub fn run(cmd: &mut Command) -> Result {
    let shown = format!("{} {}", cmd.get_program().to_string_lossy(), {
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy()).collect();
        args.join(" ")
    });
    eprintln!("+ {shown}");
    let status = cmd.status().map_err(|e| format!("{shown}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{shown}: {status}"))
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish().iter().map(|b| format!("{b:02x}")).collect())
}

/// Download `url` to `dest` unless `dest` already has the pinned hash. The file only takes its
/// final name after the hash matched, so a partial or tampered download is never used.
pub fn fetch(url: &str, sha256: &str, dest: &Path) -> Result {
    if dest.is_file() && sha256_file(dest)? == sha256 {
        return Ok(());
    }
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let part = dest.with_file_name(format!(
        "{}.part",
        dest.file_name().unwrap_or_default().to_string_lossy()
    ));
    run(Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "3",
        ])
        .args(["--proto", "=https", "--proto-redir", "=https", "--output"])
        .arg(&part)
        .arg(url))?;
    let got = sha256_file(&part)?;
    if got != sha256 {
        let _ = fs::remove_file(&part);
        return Err(format!(
            "sha256 mismatch for {url}\n  expected {sha256}\n  got      {got}"
        ));
    }
    fs::rename(&part, dest).map_err(|e| format!("{}: {e}", dest.display()))
}

/// Unpack a verified archive into `into`, which must not exist yet: the archive's contents never
/// mix with anything already on disk. `tar` reads both bzip2 and xz on macOS and Linux.
pub fn untar(archive: &Path, into: &Path) -> Result {
    if into.exists() {
        fs::remove_dir_all(into).map_err(|e| format!("{}: {e}", into.display()))?;
    }
    fs::create_dir_all(into).map_err(|e| format!("{}: {e}", into.display()))?;
    run(Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into))
}

#[cfg(test)]
mod tests {
    #[test]
    fn sha256_file_is_the_hex_digest() {
        let path = std::env::temp_dir().join(format!("xtask-sha256-{}", std::process::id()));
        std::fs::write(&path, "abc").unwrap();
        // FIPS 180-4's one-block example.
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(super::sha256_file(&path).unwrap(), abc);
        let _ = std::fs::remove_file(&path);
    }
}
