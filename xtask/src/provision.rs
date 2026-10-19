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
//! dir the app can write, so nothing there is trusted, and the app's uid may race the script:
//! - a dir or database that is a link, a dir not owned like its database, and candidates whose
//!   databases have different owners (another uid's plant in an earlier candidate) are refused;
//! - the script then enters the data dir (`cd -P`) and names files there relatively, so a dir
//!   swapped for a link after the checks cannot steer it;
//! - the file is written in a fresh dir only root can enter (`mktemp -d`), and only once that dir
//!   is checked to be ours, empty and inside the data dir; it is given to the dir's owner there
//!   and hard-linked into place with `ln -nf`, which replaces a link at the name instead of
//!   following it. Root never opens or chowns a path the app's uid can change.
//!
//! Candidates under a world-writable parent (`/tmp`, `/media/developer/temp`) are left out: any
//! user on the TV could plant a database there.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::ipk::app_id;
use crate::Result;

/// The database file the app keeps in its data dir (`iptv::storage::DB_FILE`).
pub(crate) const DB_FILE: &str = "iptv.sqlite3";
/// What the app imports at start (`crates/app/src/store.rs`).
const PROVISION_FILE: &str = "provision.url";
/// The remote script's exit status when no candidate holds the database.
pub(crate) const NO_DATA_DIR: i32 = 3;
/// The remote script's exit status for a data dir it does not trust.
pub(crate) const UNTRUSTED: i32 = 4;
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
        .filter(|a| destination(a))
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

/// An ssh destination (alias or user@host), never an option: a leading `-` would be read as one.
pub(crate) fn destination(a: &str) -> bool {
    !a.starts_with('-')
        && !a.is_empty()
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-@".contains(c))
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
            "{alias}: a data dir is a link or not owned like its database, or two are owned \
             differently; nothing written"
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
pub(crate) fn candidates(id: &str) -> Vec<PathBuf> {
    let dirs = INSTALL_ROOTS.map(|root| Path::new(root).join(id));
    let dirs = dirs.each_ref().map(PathBuf::as_path);
    let runtime = tv_base::paths::installed_runtime_dir(id);
    let mut all = tv_base::paths::installed_data_candidates(id, &dirs, &runtime);
    all.retain(|d| !SHARED.iter().any(|s| d.starts_with(s)));
    all
}

/// The command the TV runs: find and check the data dir, write stdin into `provision.url` there
/// for its owner ([`stdin_into`]) and print the dir. Only paths built from the app id appear in
/// it.
fn remote_script(dirs: &[PathBuf]) -> String {
    in_data_dir(dirs, &stdin_into(PROVISION_FILE, ".provision"))
}

/// A command for the TV that runs `then` in the data dir: the first of `dirs` holding the
/// database, with `$d` its path, `$o` its owner (`uid:gid`) and the working directory in it
/// (`cd -P`), so `then` names files there relatively. Refused ([`UNTRUSTED`]) when a candidate
/// holding a database is a link, holds a link or is owned unlike its database, when two such
/// candidates have different owners (the app's own data dir is never told from another uid's
/// plant by position alone), or when the dir entered is not the one checked; [`NO_DATA_DIR`]
/// when none holds one. `then` ends with an `exit`. Files it makes are 0600 (`umask 077`).
pub(crate) fn in_data_dir(dirs: &[PathBuf], then: &str) -> String {
    let dirs: Vec<String> = dirs.iter().map(|d| format!("'{}'", d.display())).collect();
    format!(
        "umask 077; w=; for d in {dirs}; do \
         if [ -f \"$d/{DB_FILE}\" ]; then \
         if [ -L \"$d\" ] || [ -L \"$d/{DB_FILE}\" ]; then exit {UNTRUSTED}; fi; \
         o=$(ls -ldn \"$d\" | awk '{{print $3\":\"$4}}'); \
         db=$(ls -ln \"$d/{DB_FILE}\" | awk '{{print $3\":\"$4}}'); \
         if [ -z \"$o\" ] || [ \"$o\" != \"$db\" ]; then exit {UNTRUSTED}; fi; \
         if [ -z \"$w\" ]; then w=$d; wo=$o; \
         elif [ \"$o\" != \"$wo\" ]; then exit {UNTRUSTED}; fi; \
         fi; done; \
         if [ -z \"$w\" ]; then exit {NO_DATA_DIR}; fi; \
         d=$w; cd -P \"$d\" || exit {UNTRUSTED}; \
         o=$(ls -ldn . | awk '{{print $3\":\"$4}}'); \
         db=$(ls -ln {DB_FILE} | awk '{{print $3\":\"$4}}'); \
         if [ -L {DB_FILE} ] || [ ! -f {DB_FILE} ] || [ \"$o\" != \"$wo\" ] \
         || [ \"$db\" != \"$wo\" ]; then exit {UNTRUSTED}; fi; \
         {then}",
        dirs = dirs.join(" ")
    )
}

/// For [`in_data_dir`]: stdin into `<name>` in the data dir for its owner, then print the dir.
/// The file is written in a fresh root-only dir (`<temp>.XXXXXX`, `mktemp -d`) entered with
/// `cd -P` and refused ([`UNTRUSTED`]) unless it is this uid's, empty and inside the data dir
/// (its parent's inode), so the app's uid cannot swap a link in for it; given to `$o` there; and
/// hard-linked into place with `ln -nf`, which replaces a link or a file at the name and never
/// follows one (`mv` would move into a dir a link at the name points to).
pub(crate) fn stdin_into(name: &str, temp: &str) -> String {
    format!(
        "p=$(ls -di . | awk '{{print $1}}'); \
         t=$(mktemp -d \"./{temp}.XXXXXX\") || exit 1; \
         cd -P \"$t\" || exit 1; \
         if [ ! -O . ] || [ -n \"$(ls -A)\" ] \
         || [ \"$(ls -di .. | awk '{{print $1}}')\" != \"$p\" ]; then exit {UNTRUSTED}; fi; \
         if cat > f && chown -h \"$o\" f && ln -nf f \"../{name}\"; then r=0; else r=1; fi; \
         rm -f f; cd -P .. && rmdir \"$t\"; \
         if [ $r = 0 ]; then echo \"$d\"; fi; exit $r;"
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
        remote_in(dirs, url, &[])
    }

    /// [`remote`] with `env` set for the script.
    fn remote_in(dirs: &[PathBuf], url: &[u8], env: &[(&str, String)]) -> (Option<i32>, String) {
        let mut sh = Command::new("sh")
            .arg("-c")
            .arg(remote_script(dirs))
            .envs(env.iter().map(|(k, v)| (k, v)))
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
        // Nor is one pointing at a dir entered: `mv` would have moved the file in there.
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::remove_file(&written).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &written).unwrap();
        assert_eq!(
            remote(std::slice::from_ref(&live), b"http://h/z\n").0,
            Some(0)
        );
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        assert_eq!(std::fs::read_to_string(&written).unwrap(), "http://h/z\n");
        assert_eq!(leftovers(&live), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The app's uid wins the race after `mktemp`: the temp dir's name now holds a link to a dir
    /// elsewhere (a `mktemp` on PATH does the swap). Nothing is written through it.
    #[test]
    fn a_temp_dir_swapped_for_a_link_is_refused() {
        let root =
            std::env::temp_dir().join(format!("xtask-provision-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (live, attack, bin) = (root.join("data"), root.join("attack"), root.join("bin"));
        for d in [&live, &attack, &bin] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(live.join(DB_FILE), "").unwrap();
        let shim = bin.join("mktemp");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\np=$(/usr/bin/mktemp \"$@\") || exit 1\nmv \"$p\" \"$p.kept\"\n\
                 ln -s '{}' \"$p\"\necho \"$p\"\n",
                attack.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let (code, out) = remote_in(
            std::slice::from_ref(&live),
            b"http://h/x\n",
            &[("PATH", path)],
        );
        assert_eq!((code, out.as_str()), (Some(UNTRUSTED), ""));
        assert_eq!(std::fs::read_dir(&attack).unwrap().count(), 0);
        assert!(!live.join(PROVISION_FILE).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A database in an earlier candidate owned unlike the one in a later candidate: one of them
    /// is another uid's plant, so neither is written to. (Owners differ here by group, which a
    /// test can set without root.)
    #[test]
    fn data_dirs_owned_differently_are_refused() {
        use std::os::unix::fs::MetadataExt;
        let root = std::env::temp_dir().join(format!("xtask-provision-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (planted, live) = (root.join("first"), root.join("second"));
        for d in [&planted, &live] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join(DB_FILE), "").unwrap();
        }
        let dirs = [planted.clone(), live.clone()];
        let gid = std::fs::metadata(&live).unwrap().gid();
        let groups = Command::new("id").arg("-G").output().unwrap().stdout;
        let other = String::from_utf8_lossy(&groups)
            .split_whitespace()
            .filter_map(|g| g.parse::<u32>().ok())
            .find(|&g| g != gid);
        let Some(other) = other else {
            eprintln!("skipped: this user is in one group only");
            return;
        };
        for p in [planted.clone(), planted.join(DB_FILE)] {
            std::os::unix::fs::chown(&p, None, Some(other)).unwrap();
        }
        assert_eq!(remote(&dirs, b"http://h/x\n").0, Some(UNTRUSTED));
        assert!(!planted.join(PROVISION_FILE).exists());
        assert!(!live.join(PROVISION_FILE).exists());
        // Alike again, the first is the data dir.
        for p in [planted.clone(), planted.join(DB_FILE)] {
            std::os::unix::fs::chown(&p, None, Some(gid)).unwrap();
        }
        let (code, out) = remote(&dirs, b"http://h/x\n");
        assert_eq!((code, out.as_str()), (Some(0), planted.to_str().unwrap()));
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
