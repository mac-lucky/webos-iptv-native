//! `cargo xtask provision <ssh-alias> [--from FILE] [--epg-from FILE] [--flavor ...]`: hand a TV
//! the playlist URL kept in a local file (`--from`, else `WEBOSIPTV_PLAYLIST_FILE`) and/or a guide
//! URL (`--epg-from`), so nobody types a tokenised URL with the remote.
//!
//! The URLs go over ssh's stdin into `provision.url` in the app's data dir, as `playlist <url>`
//! and `epg <url>` lines, which the app reads, stores as its settings and deletes at its next
//! start. They are never in argv (here or on the TV), never in a shell (no local shell runs), and
//! never printed: errors say what is wrong with a file, not what is in it.
//!
//! The data dir is the first of the app's private data dirs (`tv_base::paths::
//! private_data_candidates` on the TV) that holds its database, so the file lands where the app
//! actually keeps its data; a TV where the app has never run is refused. ssh runs as root in a
//! dir the app can write, so nothing there is trusted: a dir or database that is a link, or a dir
//! not owned like its database, is refused, the file is created by `mktemp` (exclusive, 0600),
//! handed to the dir's owner with `chown -h`, and renamed into place (a rename replaces a link at
//! the destination instead of following it). Candidates under a world-writable parent (`/tmp`,
//! `/media/developer/temp`) are left out: any user on the TV could plant a database there.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::ipk::app_id;
use crate::Result;

/// The database file the app keeps in its data dir (`iptv::storage::DB_FILE`).
const DB_FILE: &str = "iptv.sqlite3";
/// What the app imports at start (`crates/app/src/store.rs`).
const PROVISION_FILE: &str = "provision.url";
/// The remote script's exit status when no candidate holds the database.
const NO_DATA_DIR: i32 = 3;
/// The remote script's exit status for a data dir it does not trust.
const UNTRUSTED: i32 = 4;
/// The environment variable naming the playlist file when `--from` does not.
const FILE_ENV: &str = "WEBOSIPTV_PLAYLIST_FILE";
/// Longest playlist URL accepted, as the app's settings accept.
const MAX_URL: usize = 4096;

pub fn run(args: &[String]) -> Result {
    let opts = parse(args)?;
    let id = app_id(&opts.flavor)?;
    let playlist = opts
        .from
        .map(PathBuf::from)
        .or_else(|| std::env::var_os(FILE_ENV).map(PathBuf::from));
    let epg = opts.epg_from.map(PathBuf::from);
    if playlist.is_none() && epg.is_none() {
        return Err(format!(
            "name the playlist file with --from FILE (or {FILE_ENV}) and/or the guide file with \
             --epg-from FILE"
        ));
    }
    let alias = opts.alias;
    let ssh = std::env::var_os("XTASK_SSH").unwrap_or_else(|| "ssh".into());
    let dir = provision(
        Path::new(&ssh),
        &alias,
        &id,
        playlist.as_deref(),
        epg.as_deref(),
    )?;
    println!("provision: URLs sent to {alias} ({dir}); the app takes them at its next start");
    Ok(())
}

#[derive(Debug, PartialEq)]
struct Opts {
    alias: String,
    from: Option<String>,
    epg_from: Option<String>,
    flavor: String,
}

fn parse(args: &[String]) -> Result<Opts> {
    let mut it = args.iter();
    // An ssh destination, never an option: a leading `-` would be read as one.
    let alias = it
        .next()
        .filter(|a| {
            !a.starts_with('-')
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-@".contains(c))
        })
        .ok_or("provision takes an ssh destination (alias or user@host) first")?;
    let mut opts = Opts {
        alias: alias.clone(),
        from: None,
        epg_from: None,
        flavor: "stable".to_owned(),
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--from" => opts.from = Some(it.next().ok_or("--from needs a file")?.clone()),
            "--epg-from" => {
                opts.epg_from = Some(it.next().ok_or("--epg-from needs a file")?.clone())
            }
            "--flavor" => opts.flavor = it.next().ok_or("--flavor needs a value")?.clone(),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(opts)
}

/// Send the URLs in the `playlist` and `epg` files to `alias` through `ssh`; returns the data dir
/// they landed in.
fn provision(
    ssh: &Path,
    alias: &str,
    id: &str,
    playlist: Option<&Path>,
    epg: Option<&Path>,
) -> Result<String> {
    let mut body = String::new();
    for (name, file) in [("playlist", playlist), ("epg", epg)] {
        if let Some(file) = file {
            body.push_str(&format!("{name} {}\n", read_url(file)?));
        }
    }
    let mut child = Command::new(ssh)
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=6", alias])
        .arg(remote_script(&candidates(id)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", ssh.display()))?;
    let mut stdin = child.stdin.take().ok_or("ssh has no stdin")?;
    // A write error means ssh is gone; its exit status below says why.
    let _ = stdin.write_all(body.as_bytes());
    drop(stdin);
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    match out.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned()),
        Some(NO_DATA_DIR) => Err(format!(
            "{id} has no private data dir on {alias}: launch the app once, then provision \
             (one under /tmp or /media/developer/temp is never used; enter the URL in Settings)"
        )),
        Some(UNTRUSTED) => Err(format!(
            "{alias}: the data dir is a link or not owned like its database; nothing written"
        )),
        _ => Err(format!("ssh {alias}: {}", out.status)),
    }
}

/// The URL in `file`: one http or https line, nothing else. The errors never quote the file.
fn read_url(file: &Path) -> Result<String> {
    let text =
        std::fs::read_to_string(file).map_err(|e| format!("{}: {:?}", file.display(), e.kind()))?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let (Some(line), None) = (lines.next(), lines.next()) else {
        return Err(format!("{}: expected exactly one URL line", file.display()));
    };
    let url = line.trim();
    let scheme = url.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
    let plain = url.len() <= MAX_URL && !url.chars().any(|c| c.is_whitespace() || c.is_control());
    if !plain || !matches!(scheme.as_deref(), Some("http" | "https")) {
        return Err(format!("{}: not a single http(s) URL", file.display()));
    }
    Ok(url.to_owned())
}

/// Where an app is installed: the Homebrew Channel, then Developer Mode (`tv_base::paths`).
const INSTALL_ROOTS: [&str; 2] = [
    "/media/cryptofs/apps/usr/palm/applications",
    "/media/developer/apps/usr/palm/applications",
];

/// World-writable dirs on the television: a data dir under one is never written to.
const SHARED: [&str; 2] = ["/tmp", "/media/developer/temp"];

/// The app's private data dirs on the television, best first, as
/// `tv_base::paths::private_data_candidates` resolves them for an app installed either way,
/// without those under a [`SHARED`] dir.
fn candidates(id: &str) -> Vec<PathBuf> {
    let dirs = INSTALL_ROOTS.map(|root| Path::new(root).join(id));
    let dirs = dirs.each_ref().map(PathBuf::as_path);
    let runtime = tv_base::paths::installed_runtime_dir(id);
    let mut all = tv_base::paths::installed_data_candidates(id, &dirs, &runtime);
    all.retain(|d| !SHARED.iter().any(|s| d.starts_with(s)));
    all
}

/// The command the TV runs: refuse an untrusted data dir, write stdin to an exclusive temp file
/// in the first candidate holding the database, give it to the dir's owner and rename it into
/// place; print the dir. Only paths built from the app id appear in it.
fn remote_script(dirs: &[PathBuf]) -> String {
    let dirs: Vec<String> = dirs.iter().map(|d| format!("'{}'", d.display())).collect();
    format!(
        "umask 077; for d in {dirs}; do \
         if [ -f \"$d/{DB_FILE}\" ]; then \
         if [ -L \"$d\" ] || [ -L \"$d/{DB_FILE}\" ]; then exit {UNTRUSTED}; fi; \
         o=$(ls -ldn \"$d\" | awk '{{print $3\":\"$4}}'); \
         db=$(ls -ln \"$d/{DB_FILE}\" | awk '{{print $3\":\"$4}}'); \
         if [ -z \"$o\" ] || [ \"$o\" != \"$db\" ]; then exit {UNTRUSTED}; fi; \
         t=$(mktemp \"$d/.provision.XXXXXX\") || exit 1; \
         if cat > \"$t\" && chown -h \"$o\" \"$t\" && mv -f \"$t\" \"$d/{PROVISION_FILE}\"; \
         then echo \"$d\"; exit 0; fi; \
         rm -f \"$t\"; exit 1; fi; done; exit {NO_DATA_DIR}",
        dirs = dirs.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A scratch dir with a fake `ssh` that records its argv and stdin and acts as `script`.
    fn rig(name: &str, script: &str) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("xtask-provision-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ssh = dir.join("ssh");
        let body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{d}/argv'\ncat > '{d}/stdin'\n{script}\n",
            d = dir.display()
        );
        std::fs::write(&ssh, body).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, ssh)
    }

    /// A playlist URL with a token-shaped path segment, built at run time.
    fn url() -> String {
        let token: String = "q7Rk".repeat(4).chars().take(13).collect();
        format!("https://iptv.example/pl/{token}/list.m3u8")
    }

    #[test]
    fn the_urls_go_over_stdin_and_nowhere_else() {
        let (dir, ssh) = rig("ok", "echo /media/developer/x-data");
        let file = dir.join(".playlist");
        std::fs::write(&file, format!("\n{}\n", url())).unwrap();
        let guide = dir.join(".epg");
        std::fs::write(&guide, "https://guide.example/epg.xml.gz\n").unwrap();
        let id = "io.github.maclucky.webosiptv";
        let landed = provision(&ssh, "tv-1", id, Some(&file), Some(&guide)).unwrap();
        assert_eq!(landed, "/media/developer/x-data");
        let stdin = std::fs::read_to_string(dir.join("stdin")).unwrap();
        assert_eq!(
            stdin,
            format!("playlist {}\nepg https://guide.example/epg.xml.gz\n", url())
        );
        provision(&ssh, "tv-1", id, None, Some(&guide)).unwrap();
        let stdin = std::fs::read_to_string(dir.join("stdin")).unwrap();
        assert_eq!(stdin, "epg https://guide.example/epg.xml.gz\n");
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        assert!(!argv.contains("guide.example"));
        provision(&ssh, "tv-1", id, Some(&file), None).unwrap();
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        assert!(!argv.contains(&url()) && !argv.contains("iptv.example"));
        assert!(argv.contains("tv-1\n") && argv.contains("BatchMode=yes"));
        assert!(argv.contains("'/media/developer/io.github.maclucky.webosiptv-data'"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_file_is_refused_before_ssh_runs_and_never_quoted() {
        let (dir, ssh) = rig("bad", "exit 0");
        let file = dir.join(".playlist");
        let secret = url();
        for content in [
            format!("{secret}\n{secret}\n"),
            secret.replace("https", "ftp"),
            format!("{secret} extra"),
            String::new(),
        ] {
            std::fs::write(&file, &content).unwrap();
            let e = provision(&ssh, "tv-2", "a.b", Some(&file), None).unwrap_err();
            assert!(!e.contains("iptv.example"), "{e}");
        }
        let missing = provision(&ssh, "tv-2", "a.b", None, Some(&dir.join("none"))).unwrap_err();
        assert!(missing.contains("NotFound"), "{missing}");
        assert!(!dir.join("argv").exists(), "ssh never ran");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tv_where_the_app_never_ran_is_refused() {
        let (dir, ssh) = rig("nodir", "exit 3");
        let file = dir.join(".playlist");
        std::fs::write(&file, url()).unwrap();
        let e = provision(&ssh, "tv-1", "a.b", Some(&file), None).unwrap_err();
        assert!(e.contains("launch the app once"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Run the remote script locally over `dirs` with `url` on stdin: (exit status, stdout).
    fn remote(dirs: &[PathBuf], url: &[u8]) -> (Option<i32>, String) {
        let mut sh = Command::new("sh")
            .arg("-c")
            .arg(remote_script(dirs))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        sh.stdin.take().unwrap().write_all(url).unwrap();
        let out = sh.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (out.status.code(), stdout)
    }

    fn leftovers(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".provision")
            })
            .count()
    }

    #[test]
    fn the_remote_script_writes_into_the_dir_holding_the_database() {
        let root = std::env::temp_dir().join(format!("xtask-provision-sh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (empty, live) = (root.join("first"), root.join("second"));
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join(DB_FILE), "").unwrap();
        let (code, out) = remote(&[empty.clone(), live.clone()], b"http://h/x\n");
        assert_eq!((code, out.as_str()), (Some(0), live.to_str().unwrap()));
        let written = live.join(PROVISION_FILE);
        assert_eq!(std::fs::read_to_string(&written).unwrap(), "http://h/x\n");
        let mode = std::fs::metadata(&written).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!empty.join(PROVISION_FILE).exists());
        assert_eq!(leftovers(&live), 0, "the temp file was renamed");
        // A provision.url that is a link is replaced, never written through.
        let target = root.join("target");
        std::fs::write(&target, "untouched").unwrap();
        std::fs::remove_file(&written).unwrap();
        std::os::unix::fs::symlink(&target, &written).unwrap();
        assert_eq!(
            remote(std::slice::from_ref(&live), b"http://h/y\n").0,
            Some(0)
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        assert!(!std::fs::symlink_metadata(&written)
            .unwrap()
            .file_type()
            .is_symlink());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_linked_data_dir_or_database_is_refused() {
        let root = std::env::temp_dir().join(format!("xtask-provision-ln-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join(DB_FILE), "").unwrap();
        let linked = root.join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        assert_eq!(remote(&[linked], b"http://h/x\n").0, Some(UNTRUSTED));
        assert!(!real.join(PROVISION_FILE).exists());
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(real.join(DB_FILE), other.join(DB_FILE)).unwrap();
        assert_eq!(
            remote(std::slice::from_ref(&other), b"http://h/x\n").0,
            Some(UNTRUSTED)
        );
        assert!(!other.join(PROVISION_FILE).exists());
        assert_eq!(leftovers(&other), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn flavours_and_aliases() {
        assert_eq!(app_id("stable").unwrap(), "io.github.maclucky.webosiptv");
        assert_eq!(
            app_id("debug").unwrap(),
            "io.github.maclucky.webosiptv.debug"
        );
        assert!(app_id("nope").is_err());
        let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert_eq!(
            parse(&args("tv-1 --from f.txt --epg-from g.txt --flavor beta")).unwrap(),
            Opts {
                alias: "tv-1".into(),
                from: Some("f.txt".into()),
                epg_from: Some("g.txt".into()),
                flavor: "beta".into()
            }
        );
        assert!(parse(&args("tv-1 --epg-from")).is_err());
        assert!(
            parse(&args("-oProxyCommand=x")).is_err(),
            "never an ssh option"
        );
        assert!(parse(&args("tv;rm")).is_err());
        assert!(parse(&args("")).is_err());
    }

    /// Both install roots are covered; nothing under a world-writable dir is.
    #[test]
    fn candidates_cover_both_installs_and_skip_shared_dirs() {
        let id = "io.github.maclucky.webosiptv";
        let apps = Path::new("/media/cryptofs/apps/usr/palm/applications").join(id);
        let dev = Path::new("/media/developer/apps/usr/palm/applications").join(id);
        let all = candidates(id);
        assert_eq!(all.len(), 4);
        assert_eq!(all[2], apps.join("data"));
        assert_eq!(all[3], dev.join("data"));
        for id in [id, "a.b"] {
            for d in candidates(id) {
                assert!(!SHARED.iter().any(|s| d.starts_with(s)), "{}", d.display());
            }
        }
    }
}
