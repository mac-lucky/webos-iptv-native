//! `cargo xtask backup <ssh-alias> pull|push <file> [--flavor ...]`: move the app's backup file
//! (`backup.json` in its data dir, written by Settings > General > Backup > Export and read by
//! Import) from a TV into a local file, or from a local file onto a TV.
//!
//! The file holds the playlist and guide URLs with their tokens. It travels only over ssh's
//! stdout (pull) or stdin (push), never in argv or through a local shell, and is never printed:
//! messages give its size, never its content. A pulled file is created 0600 through an exclusive
//! temp file and a rename. On the TV the data dir is found and trusted as `provision` finds it
//! (the dir holding the database; a link, or a dir not owned like its database, is refused); a
//! pulled backup must be a regular file owned like the dir, and a pushed one is made by `mktemp`
//! (0600), given to the dir's owner and renamed into place.
//!
//! A USB stick is not a destination: the app reads and writes the backup only in its private data
//! dir, where it keeps the same tokens in its database.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::ipk::app_id;
use crate::provision::{candidates, destination, in_data_dir, stdin_into, NO_DATA_DIR, UNTRUSTED};
use crate::Result;

/// The backup file in the app's data dir (`crates/app/src/backup.rs`).
const BACKUP_FILE: &str = "backup.json";
/// The largest backup the app reads or writes (`backup::MAX_BYTES` there).
const MAX_BYTES: usize = 2 << 20;
/// What every backup carries in its `format` field.
const FORMAT: &str = "\"webosiptv-backup\"";
/// The pull script's exit status when the data dir holds no backup.
const NO_BACKUP: i32 = 5;

pub fn run(args: &[String]) -> Result {
    let opts = parse(args)?;
    let id = app_id(&opts.flavor)?;
    let ssh = std::env::var_os("XTASK_SSH").unwrap_or_else(|| "ssh".into());
    let (ssh, alias, file) = (Path::new(&ssh), &opts.alias, Path::new(&opts.file));
    match opts.op {
        Op::Pull => {
            let bytes = pull(ssh, alias, &id, file)?;
            println!(
                "backup: {bytes} bytes from {alias} written to {} (0600)",
                file.display()
            );
        }
        Op::Push => {
            let dir = push(ssh, alias, &id, file)?;
            println!(
                "backup: {} sent to {alias} ({dir}); import it in Settings > General > Backup",
                file.display()
            );
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
enum Op {
    Pull,
    Push,
}

#[derive(Debug, PartialEq)]
struct Opts {
    alias: String,
    op: Op,
    file: String,
    flavor: String,
}

const ARGS: &str = "backup takes <ssh-alias> pull|push <file> [--flavor stable|debug|beta]";

fn parse(args: &[String]) -> Result<Opts> {
    let mut it = args.iter();
    let alias = it.next().filter(|a| destination(a)).ok_or(ARGS)?;
    let op = match it.next().map(String::as_str) {
        Some("pull") => Op::Pull,
        Some("push") => Op::Push,
        _ => return Err(ARGS.into()),
    };
    let file = it.next().filter(|f| !f.starts_with('-')).ok_or(ARGS)?;
    let mut opts = Opts {
        alias: alias.clone(),
        op,
        file: file.clone(),
        flavor: "stable".to_owned(),
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--flavor" => opts.flavor = it.next().ok_or("--flavor needs a value")?.clone(),
            other => return Err(format!("unknown argument {other}\n{ARGS}")),
        }
    }
    Ok(opts)
}

/// `ssh <alias> <script>` with the options every call here uses.
fn ssh_command(ssh: &Path, alias: &str, script: &str) -> Command {
    let mut cmd = Command::new(ssh);
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=6", alias])
        .arg(script);
    cmd
}

/// What a remote script's exit status means, for a call that did not succeed.
fn failure(code: Option<i32>, alias: &str, id: &str) -> String {
    match code {
        Some(NO_DATA_DIR) => {
            format!("{id} has no private data dir on {alias}: launch the app once")
        }
        Some(UNTRUSTED) => format!(
            "{alias}: the data dir or its {BACKUP_FILE} is a link or not owned like the database"
        ),
        Some(NO_BACKUP) => {
            format!("{alias} has no {BACKUP_FILE}: export one in Settings > General > Backup first")
        }
        Some(code) => format!("ssh {alias}: exit status {code}"),
        None => format!("ssh {alias}: killed by a signal"),
    }
}

/// The TV's backup into `file`; returns its size.
fn pull(ssh: &Path, alias: &str, id: &str, file: &Path) -> Result<usize> {
    let mut child = ssh_command(ssh, alias, &pull_script(&candidates(id)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", ssh.display()))?;
    let mut bytes = Vec::new();
    let mut stdout = child.stdout.take().ok_or("ssh has no stdout")?;
    let read = (&mut stdout)
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() > MAX_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return Err(match read {
            Err(e) => format!("ssh {alias}: {:?}", e.kind()),
            Ok(_) => format!("{alias}: the {BACKUP_FILE} there is larger than a backup may be"),
        });
    }
    drop(stdout);
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(failure(status.code(), alias, id));
    }
    check(&bytes).map_err(|e| format!("{alias}: {BACKUP_FILE} {e}"))?;
    write_private(file, &bytes)?;
    Ok(bytes.len())
}

/// `file` onto the TV; returns the data dir it landed in.
fn push(ssh: &Path, alias: &str, id: &str, file: &Path) -> Result<String> {
    let bytes = read_backup(file)?;
    let mut child = ssh_command(ssh, alias, &push_script(&candidates(id)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", ssh.display()))?;
    let mut stdin = child.stdin.take().ok_or("ssh has no stdin")?;
    // A write error means ssh is gone; its exit status below says why.
    let _ = stdin.write_all(&bytes);
    drop(stdin);
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(failure(out.status.code(), alias, id));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// What a backup looks like before the app reads it strictly: text, a JSON object, carrying
/// the format marker. The errors never quote it.
fn check(bytes: &[u8]) -> Result {
    let text = std::str::from_utf8(bytes).map_err(|_| "is not text")?;
    if !text.trim_start().starts_with('{') || !text.contains(FORMAT) {
        return Err("is not a webosiptv backup".into());
    }
    Ok(())
}

/// The backup in `file`: a regular file of at most [`MAX_BYTES`] that looks like one.
fn read_backup(file: &Path) -> Result<Vec<u8>> {
    let shown = file.display();
    let meta = std::fs::metadata(file).map_err(|e| format!("{shown}: {:?}", e.kind()))?;
    if !meta.is_file() || meta.len() > MAX_BYTES as u64 {
        return Err(format!("{shown}: not a file of at most {MAX_BYTES} bytes"));
    }
    let bytes = std::fs::read(file).map_err(|e| format!("{shown}: {:?}", e.kind()))?;
    check(&bytes).map_err(|e| format!("{shown} {e}"))?;
    Ok(bytes)
}

/// `bytes` as `file`, 0600: an exclusive temp file beside it, renamed over it (a link at the
/// name is replaced, never written through).
fn write_private(file: &Path, bytes: &[u8]) -> Result {
    use std::os::unix::fs::OpenOptionsExt;
    let shown = file.display();
    let name = file
        .file_name()
        .ok_or_else(|| format!("{shown}: not a file name"))?;
    let temp = file.with_file_name(format!(
        ".{}.{}.part",
        name.to_string_lossy(),
        std::process::id()
    ));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&temp, file));
    written.map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("{shown}: {:?}", e.kind())
    })
}

/// The command the TV runs to print its backup: a regular file owned like the data dir, never a
/// link.
fn pull_script(dirs: &[PathBuf]) -> String {
    in_data_dir(
        dirs,
        &format!(
            "f=\"$d/{BACKUP_FILE}\"; \
             if [ -L \"$f\" ]; then exit {UNTRUSTED}; fi; \
             if [ ! -e \"$f\" ]; then exit {NO_BACKUP}; fi; \
             fo=$(ls -ln \"$f\" | awk '{{print $3\":\"$4}}'); \
             if [ ! -f \"$f\" ] || [ \"$fo\" != \"$o\" ]; then exit {UNTRUSTED}; fi; \
             cat \"$f\"; exit $?;"
        ),
    )
}

/// The command the TV runs to take a backup from stdin.
fn push_script(dirs: &[PathBuf]) -> String {
    in_data_dir(dirs, &stdin_into(BACKUP_FILE, ".backup"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provision::DB_FILE;
    use std::os::unix::fs::PermissionsExt;

    const ID: &str = "io.github.maclucky.webosiptv";

    /// A scratch dir with a fake `ssh` that records its argv and stdin and acts as `script`.
    fn rig(name: &str, script: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("xtask-backup-{name}-{}", std::process::id()));
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

    /// A backup carrying a token-shaped path segment, built at run time.
    fn backup() -> (String, String) {
        let token: String = "Wq4z".repeat(5).chars().take(15).collect();
        let text = format!(
            "{{\"format\":{FORMAT},\"version\":1,\"settings\":{{\"playlist.url\":\
             \"https://iptv.example/pl/{token}/list.m3u\"}}}}\n"
        );
        (text, token)
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn arguments() {
        let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert_eq!(
            parse(&args("lg-c5 pull out.json --flavor debug")).unwrap(),
            Opts {
                alias: "lg-c5".into(),
                op: Op::Pull,
                file: "out.json".into(),
                flavor: "debug".into()
            }
        );
        assert_eq!(parse(&args("tv push b.json")).unwrap().op, Op::Push);
        for bad in [
            "",
            "tv",
            "tv pull",
            "tv copy b.json",
            "-oProxyCommand=x pull b.json",
            "tv;rm pull b.json",
            "tv pull --flavor",
            "tv pull b.json --flavor",
            "tv pull b.json extra",
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn push_sends_the_file_over_stdin_and_nowhere_else() {
        let (dir, ssh) = rig("push", "echo /media/developer/x-data");
        let (text, token) = backup();
        let file = dir.join("b.json");
        std::fs::write(&file, &text).unwrap();
        let landed = push(&ssh, "tv-1", ID, &file).unwrap();
        assert_eq!(landed, "/media/developer/x-data");
        assert_eq!(std::fs::read_to_string(dir.join("stdin")).unwrap(), text);
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        assert!(!argv.contains(&token) && !argv.contains("iptv.example"));
        assert!(argv.starts_with("-o\nBatchMode=yes\n-o\nConnectTimeout=6\ntv-1\n"));
        assert!(argv.contains(&format!("'/media/developer/{ID}-data'")));
        assert!(argv.contains(BACKUP_FILE));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_a_backup_is_refused_before_ssh_runs_and_never_quoted() {
        let (dir, ssh) = rig("bad", "exit 0");
        let (text, token) = backup();
        let file = dir.join("b.json");
        let big = format!("{text}{}", " ".repeat(MAX_BYTES));
        for content in [
            text.replace(FORMAT, "\"other\""),
            format!("[{text}]"),
            big,
            String::new(),
        ] {
            std::fs::write(&file, &content).unwrap();
            let e = push(&ssh, "tv-2", ID, &file).unwrap_err();
            assert!(!e.contains(&token) && !e.contains("iptv.example"), "{e}");
        }
        std::fs::write(&file, b"\xff{\"webosiptv-backup\"").unwrap();
        assert!(push(&ssh, "tv-2", ID, &file)
            .unwrap_err()
            .contains("not text"));
        let missing = push(&ssh, "tv-2", ID, &dir.join("none")).unwrap_err();
        assert!(missing.contains("NotFound"), "{missing}");
        assert!(
            push(&ssh, "tv-2", ID, &dir).is_err(),
            "a dir is not a backup"
        );
        assert!(!dir.join("argv").exists(), "ssh never ran");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pull_writes_the_file_owner_only_and_prints_nothing_of_it() {
        let (dir, ssh) = rig("pull", "cat \"$(dirname \"$0\")/remote.json\"");
        let (text, token) = backup();
        std::fs::write(dir.join("remote.json"), &text).unwrap();
        let out = dir.join("out.json");
        // An old file there is replaced, and a link at the name is replaced, not followed.
        let target = dir.join("target");
        std::fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, &out).unwrap();
        assert_eq!(pull(&ssh, "tv-1", ID, &out).unwrap(), text.len());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), text);
        assert_eq!(mode(&out), 0o600);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        assert!(!argv.contains(&token) && argv.contains("tv-1\n"));
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".part")
            })
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_or_odd_pull_writes_nothing() {
        let (_, token) = backup();
        let cases = [
            ("exit 5", "export one"),
            ("exit 3", "launch the app once"),
            ("exit 4", "is a link"),
            ("echo '{\"format\":\"x\"}'", "not a webosiptv backup"),
            ("head -c 2097153 /dev/zero | tr '\\0' ' '", "larger than"),
        ];
        for (i, (script, said)) in cases.into_iter().enumerate() {
            let (dir, ssh) = rig(&format!("fail{i}"), script);
            let out = dir.join("out.json");
            let e = pull(&ssh, "tv-1", ID, &out).unwrap_err();
            assert!(e.contains(said), "{script}: {e}");
            assert!(!e.contains(&token));
            assert!(!out.exists(), "{script}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Run a remote script locally with `stdin`: (exit status, stdout).
    fn remote(script: &str, stdin: &[u8]) -> (Option<i32>, Vec<u8>) {
        let mut sh = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        sh.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = sh.wait_with_output().unwrap();
        (out.status.code(), out.stdout)
    }

    #[test]
    fn the_remote_scripts_use_the_dir_holding_the_database() {
        let root = std::env::temp_dir().join(format!("xtask-backup-sh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (empty, live) = (root.join("first"), root.join("second"));
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join(DB_FILE), "").unwrap();
        let dirs = [empty.clone(), live.clone()];
        let (text, _) = backup();
        assert_eq!(remote(&pull_script(&dirs), b"").0, Some(NO_BACKUP));
        let (code, out) = remote(&push_script(&dirs), text.as_bytes());
        assert_eq!(code, Some(0));
        assert_eq!(
            String::from_utf8(out).unwrap().trim(),
            live.to_str().unwrap()
        );
        let file = live.join(BACKUP_FILE);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        assert_eq!(mode(&file), 0o600);
        assert!(!empty.join(BACKUP_FILE).exists());
        assert_eq!(
            remote(&pull_script(&dirs), b""),
            (Some(0), text.clone().into_bytes())
        );
        // A backup that is a link is never read, whatever it points at.
        std::fs::remove_file(&file).unwrap();
        let target = root.join("secret");
        std::fs::write(&target, "not for ssh").unwrap();
        std::os::unix::fs::symlink(&target, &file).unwrap();
        assert_eq!(
            remote(&pull_script(&dirs), b""),
            (Some(UNTRUSTED), Vec::new())
        );
        // A dir there is not a file.
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert_eq!(remote(&pull_script(&dirs), b"").0, Some(UNTRUSTED));
        assert_eq!(remote(&pull_script(&[empty]), b"").0, Some(NO_DATA_DIR));
        let _ = std::fs::remove_dir_all(&root);
    }
}
