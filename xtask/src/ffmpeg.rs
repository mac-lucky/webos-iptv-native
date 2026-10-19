//! `cargo xtask ffmpeg`: the FFmpeg the app ships, cross-built with the NDK.
//!
//! The TVs' own FFmpeg differs per firmware (the C1 has 4.0) and LG ships no headers, so the app
//! bundles one fixed build. It demuxes with it and decodes only the audio the TV's buffer feed
//! cannot take (crates/media `ff_adec.rs`); every other decode is the TV's hardware. The
//! libraries carry a `-wi` SONAME suffix and are opened by absolute path from the app directory,
//! so the firmware's copies are never picked up instead.
//!
//! `cargo xtask ffmpeg --host` builds the same version with the same component list for this
//! machine and stages it in `pkg/`, where the desktop simulator and the media crate's FFmpeg tests
//! load it from.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ipk::read;
use crate::{elf, fetch, ndk, root, run, target_dir, untar, Result};

const VERSION: &str = "9.0.2";
const URL: &str = "https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz";
const SHA256: &str = "8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e";

/// LGPL-2.1-or-later only: no `--enable-gpl`/`--enable-nonfree`, and `--disable-autodetect` keeps
/// out every external library. Only what live IPTV needs: MPEG-TS and fMP4 HLS segments, raw
/// audio segments, and elementary H.264/HEVC, demuxed with the parsers filling the stream
/// parameters; plus the IPTV audio decoders and libswresample for the software decode -> PCM
/// path: the C1 plays nothing for MP2 declared as MP3, and on webOS 10+ (the C5) the pipeline
/// plays only PCM.
///
/// MPEG audio uses the fixed-point decoders, which have ARM asm: built beside them, the float
/// twins are dead code for MP2 and, being listed first, what `avcodec_find_decoder` picks for
/// MP3. AAC and AC-3 use the float ones: E-AC-3 and LATM exist only on top of them, and the
/// TVs' cores have VFP and NEON.
const CONFIGURE: &[&str] = &[
    "--build-suffix=-wi",
    "--enable-shared",
    "--disable-static",
    "--enable-pic",
    "--disable-programs",
    "--disable-doc",
    "--disable-avdevice",
    "--disable-avfilter",
    "--disable-swscale",
    "--disable-network",
    "--disable-debug",
    "--enable-small",
    "--disable-everything",
    "--disable-autodetect",
    "--enable-swresample",
    "--enable-protocol=file",
    "--enable-demuxer=mpegts,mov,aac,ac3,eac3,mp3,h264,hevc",
    "--enable-parser=h264,hevc,aac,aac_latm,ac3,mpegaudio",
    "--enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata",
    "--enable-decoder=mp2,mp3,aac,aac_latm,ac3,eac3",
];

/// The prefix the libraries are configured for. A stand-in, so no build path is recorded in them;
/// `make install` is redirected with DESTDIR.
const PREFIX: &str = "/wi";

fn dir() -> PathBuf {
    target_dir().join("ffmpeg")
}

/// `cargo xtask ffmpeg [--host]`.
pub fn build(args: &[String]) -> Result {
    match args {
        [] => cross(),
        [flag] if flag == "--host" => host(),
        _ => Err("ffmpeg takes at most --host".into()),
    }
}

/// The unpacked, verified source tree.
fn source() -> Result<PathBuf> {
    let tarball = dir().join(format!("ffmpeg-{VERSION}.tar.xz"));
    fetch(URL, SHA256, &tarball)?;
    let src = dir().join("src");
    let source = src.join(format!("ffmpeg-{VERSION}"));
    if !source.join("configure").is_file() {
        untar(&tarball, &src)?;
    }
    Ok(source)
}

/// Configure (again only when its arguments change) and make out of tree in `build`, then install
/// into `install`; returns the installed `lib` directory.
fn compile(build: &Path, install: &Path, mut args: Vec<String>) -> Result<PathBuf> {
    let source = source()?;
    fs::create_dir_all(build).map_err(|e| format!("{}: {e}", build.display()))?;
    args.insert(0, format!("--prefix={PREFIX}"));
    args.extend(CONFIGURE.iter().map(|s| s.to_string()));
    let stamp = build.join(".configure-args");
    if fs::read_to_string(&stamp).ok().as_deref() != Some(args.join("\n").as_str()) {
        run(Command::new(source.join("configure"))
            .args(&args)
            .current_dir(build))?;
        fs::write(&stamp, args.join("\n")).map_err(|e| format!("{}: {e}", stamp.display()))?;
    }
    let jobs = std::thread::available_parallelism().map_or(4, |n| n.get());
    run(Command::new("make")
        .arg(format!("-j{jobs}"))
        .current_dir(build))?;
    if install.exists() {
        fs::remove_dir_all(install).map_err(|e| format!("{}: {e}", install.display()))?;
    }
    run(Command::new("make")
        .arg("install")
        .arg(format!("DESTDIR={}", install.display()))
        .current_dir(build))?;
    Ok(install.join(PREFIX.trim_start_matches('/')).join("lib"))
}

/// The TV build, staged in target/ffmpeg/lib for the package.
fn cross() -> Result {
    let sdk = ndk::require()?;
    let installed = compile(
        &dir().join("build"),
        &dir().join("install"),
        vec![
            "--enable-cross-compile".into(),
            "--target-os=linux".into(),
            "--arch=arm".into(),
            "--cpu=cortex-a9".into(),
            format!(
                "--cross-prefix={}",
                sdk.join("bin").join(ndk::PREFIX).display()
            ),
            format!("--cc={}", ndk::cc(&sdk).display()),
            format!("--sysroot={}", ndk::sysroot(&sdk).display()),
            "--host-cc=cc".into(),
            // glibc 2.12's assert.h predates C11 static_assert.
            "--extra-cflags=-Dstatic_assert=_Static_assert".into(),
            // Same polyfill exclusion and build id as the app's link (.cargo/config.toml).
            "--extra-ldflags=-tno-glibc-polyfill -Wl,--build-id=sha1".into(),
        ],
    )?;

    // Each real library (not the install's symlinks) is staged stripped, under its SONAME, which
    // is the file name the app opens.
    let lib = dir().join("lib");
    if lib.exists() {
        fs::remove_dir_all(&lib).map_err(|e| format!("{}: {e}", lib.display()))?;
    }
    fs::create_dir_all(&lib).map_err(|e| format!("{}: {e}", lib.display()))?;
    for entry in fs::read_dir(&installed).map_err(|e| format!("{}: {e}", installed.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if !path
            .symlink_metadata()
            .map_err(|e| e.to_string())?
            .is_file()
            || !path.to_string_lossy().contains(".so.")
        {
            continue;
        }
        let staged = lib.join(elf::soname(&read(&path)?)?);
        ndk::strip(&sdk, &path, &staged)?;
        elf::check_glibc(&staged)?;
    }
    eprintln!("ffmpeg {VERSION}: libraries in {}", lib.display());
    Ok(())
}

/// The same build for this Mac, staged in `pkg/` under the names ff.rs opens there
/// (`libavcodec-wi.63.dylib`). The package never takes them: ipk.rs lists its `pkg/` files.
///
/// The install names carry the stand-in prefix (`/wi/lib/libavutil-wi.61.dylib`), which does not
/// exist, and dyld resolves a dependency by install name, so each one is rewritten to
/// `@loader_path` and the file ad-hoc signed again (Apple silicon refuses a dylib whose load
/// commands changed after signing).
fn host() -> Result {
    if !cfg!(target_os = "macos") {
        return Err("--host stages macOS dylibs; on another host build FFmpeg yourself and point SN_FFMPEG_DIR at it".into());
    }
    let installed = compile(
        &dir().join("host-build"),
        &dir().join("host-install"),
        Vec::new(),
    )?;
    // The major-named symlinks (`libavcodec-wi.63.dylib`, two dots) are the install names.
    let mut names = Vec::new();
    for entry in fs::read_dir(&installed).map_err(|e| format!("{}: {e}", installed.display()))? {
        let name = entry
            .map_err(|e| e.to_string())?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name.ends_with(".dylib") && name.matches('.').count() == 2 {
            names.push(name);
        }
    }
    names.sort();
    let pkg = root().join("pkg");
    for name in &names {
        let staged = pkg.join(name);
        // fs::copy follows the symlink to the real library.
        fs::copy(installed.join(name), &staged)
            .map_err(|e| format!("{}: {e}", staged.display()))?;
        let mut tool = Command::new("install_name_tool");
        tool.arg("-id").arg(format!("@loader_path/{name}"));
        for dep in &names {
            tool.arg("-change")
                .arg(format!("{PREFIX}/lib/{dep}"))
                .arg(format!("@loader_path/{dep}"));
        }
        run(tool.arg(&staged))?;
        run(Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&staged))?;
    }
    eprintln!(
        "ffmpeg {VERSION} (host): {} in {}",
        names.join(" "),
        pkg.display()
    );
    Ok(())
}

/// The package's FFmpeg files: each library under its SONAME at the app root, the LGPL text and
/// the record of what was built from where (the source offer). Empty, with a note, before
/// `cargo xtask ffmpeg`.
pub fn payload() -> Result<Vec<(String, Vec<u8>)>> {
    let lib = dir().join("lib");
    let Ok(entries) = fs::read_dir(&lib) else {
        eprintln!(
            "note: no FFmpeg in {} (run `cargo xtask ffmpeg`); packaging without it",
            lib.display()
        );
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        files.push((name, read(&path)?));
    }
    files.push((
        "licenses/LGPL-2.1.txt".into(),
        read(&root().join("licenses/LGPL-2.1.txt"))?,
    ));
    files.push(("licenses/ffmpeg.txt".into(), notice().into_bytes()));
    Ok(files)
}

/// What the About screen shows for FFmpeg: licence, unmodified source and how it was configured.
fn notice() -> String {
    format!(
        "FFmpeg {VERSION}, licensed under the GNU LGPL version 2.1 or later (LGPL-2.1.txt).\n\
         Built unmodified from {URL}\n(sha256 {SHA256}),\n\
         cross-compiled for armv7 (cortex-a9) with the webosbrew native toolchain, configured with:\n  {}\n",
        CONFIGURE.join(" ")
    )
}
