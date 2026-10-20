//! `cargo xtask tv-replay <ssh-alias> <script> [--out DIR] [--flavor stable|debug|beta]`: play a
//! remote key script on a television and keep what it showed: a screenshot per `shot` step and
//! the end of the app's event log, in `target/tv-replay/<alias>-<unix time>/` or `--out DIR`.
//!
//! One ssh session does the whole run. A small Python program ([`PROGRAM`], python2.7 on the C1,
//! python3 on the C5) goes over ssh's stdin with the steps in it, so the only argv on either side
//! is `python -` and no shell ever parses the script. On the television it:
//! - refuses unless the foreground app is ours or the home screen, and launches ours from home;
//! - finds the remote's input device by name and refuses key codes it does not declare (the
//!   kernel drops those without a word: KEY_BACK, 158, is one, so BACK is sent as 412);
//! - reads the foreground app again before every key and stops when it is no longer ours;
//! - writes each key as evdev events, releasing it even when the run is killed;
//! - takes a luna capture at every `shot` and sends it back on stdout at once, then the end of
//!   the app's event log, each as a framed file (`WIFILE <name> <bytes>` and the bytes).
//!
//! Here the frames are accepted only under the names the script gave, size-capped, and written
//! into a fresh output dir; the event log is scrubbed again and every URL in it, and in anything
//! the television prints, is cut to its scheme before it is written or shown.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::ipk::app_id;
use crate::Result;

/// The remote's keys by script name, as evdev codes on the television's remote input device.
const KEYS: &[(&str, u16)] = &[
    ("Up", 103),
    ("Down", 108),
    ("Left", 105),
    ("Right", 106),
    ("OK", 28),
    ("Back", 412),
    ("Exit", 174),
    ("CH+", 402),
    ("CH-", 403),
    ("Red", 398),
    ("Green", 399),
    ("Yellow", 400),
    ("Blue", 401),
    ("1", 2),
    ("2", 3),
    ("3", 4),
    ("4", 5),
    ("5", 6),
    ("6", 7),
    ("7", 8),
    ("8", 9),
    ("9", 10),
    ("0", 11),
    ("Play", 207),
    ("Pause", 119),
    ("Rewind", 168),
    ("FF", 208),
];

/// The input device that magic_mapper resends the remote's keys to.
const DEVICE: &str = "LGE M-RCU - Builtin [2]";
/// The home screen's app id on both televisions: the one other foreground app a run starts from.
const HOME: &str = "com.webos.app.home";

/// How long a short press holds the key down: well under the shortest long-press threshold
/// (Settings > Remote, 300 ms).
const PRESS_MS: u32 = 100;
/// How long `long` holds it: past the longest threshold (1 s).
const LONG_MS: u32 = 1_500;
const MAX_MS: u32 = 60_000;
const MAX_REPEAT: u32 = 50;
const MAX_STEPS: usize = 1_000;
const MAX_NAME: usize = 40;

/// The most a capture or the event log may take on the way back.
const MAX_PNG: usize = 32 << 20;
const MAX_LOG: usize = 8 << 20;
/// What the television sends of the event log: its end.
const LOG_TAIL: usize = 4 << 20;
/// The name the event log comes back under.
const LOG_NAME: &str = "events.log";

/// The program's exit statuses (keep in step with [`PROGRAM`]).
const NOT_OURS: i32 = 10;
const NO_DEVICE: i32 = 11;
const UNDECLARED: i32 = 12;
const CHANGED: i32 = 13;
const NO_LAUNCH: i32 = 14;
const NO_FOREGROUND: i32 = 15;
const NO_PYTHON: i32 = 9;

/// The command ssh runs: the first Python there is, reading its program from stdin.
const REMOTE: &str = "p=$(command -v python3 || command -v python) || exit 9; exec \"$p\" -u -";

pub fn run(args: &[String]) -> Result {
    let opts = parse_args(args)?;
    let text = std::fs::read_to_string(&opts.script)
        .map_err(|e| format!("{}: {e}", opts.script.display()))?;
    let steps = parse(&text).map_err(|e| format!("{}: {e}", opts.script.display()))?;
    let id = app_id(&opts.flavor)?;
    let log = tv_base::paths::installed_runtime_dir(&id)
        .join(tv_base::paths::runtime_file::EVENTS)
        .display()
        .to_string();
    let program = program(&steps, &id, &Target::television(log));
    let out = opts.out.unwrap_or_else(|| {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        crate::target_dir()
            .join("tv-replay")
            .join(format!("{}-{secs}", opts.alias))
    });
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let ssh = std::env::var_os("XTASK_SSH").unwrap_or_else(|| "ssh".into());
    let saved = replay(Path::new(&ssh), &opts.alias, &program, &steps, &out)?;
    println!(
        "tv-replay: {} file(s) from {} in {}",
        saved.len(),
        opts.alias,
        out.display()
    );
    Ok(())
}

#[derive(Debug, PartialEq)]
struct Opts {
    alias: String,
    script: PathBuf,
    out: Option<PathBuf>,
    flavor: String,
}

fn parse_args(args: &[String]) -> Result<Opts> {
    let mut it = args.iter();
    let alias = it
        .next()
        .filter(|a| crate::provision::destination(a))
        .ok_or("tv-replay takes an ssh destination (alias or user@host) first")?;
    let script = it
        .next()
        .filter(|s| !s.starts_with('-'))
        .ok_or("tv-replay takes a key script after the ssh destination")?;
    let mut opts = Opts {
        alias: alias.clone(),
        script: PathBuf::from(script),
        out: None,
        flavor: "stable".to_owned(),
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => opts.out = Some(it.next().ok_or("--out needs a dir")?.into()),
            "--flavor" => opts.flavor = it.next().ok_or("--flavor needs a value")?.clone(),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(opts)
}

/// One step of a key script.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    /// Hold the key with this evdev code down for `hold_ms`, then release it.
    Key {
        name: &'static str,
        code: u16,
        hold_ms: u32,
    },
    Wait(u32),
    /// A capture of the screen, saved as `<name>.png`.
    Shot(String),
}

/// A key script: one step per line, `#` starts a comment.
///
/// ```text
/// Left            a short press
/// Left x3         three of them
/// long OK         held past the long-press threshold
/// hold Down 2000  held 2 s (a key that repeats acts again while held)
/// wait 1500       nothing for 1.5 s
/// shot groups     a screenshot, saved as NN-groups.png (NN counts the shots)
/// ```
fn parse(text: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    let mut shots = 0;
    for (n, line) in text.lines().enumerate() {
        let err = |what: String| format!("line {}: {what}", n + 1);
        let words: Vec<&str> = line
            .split('#')
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .collect();
        match words[..] {
            [] => {}
            ["wait", ms] => steps.push(Step::Wait(millis(ms).map_err(err)?)),
            ["shot"] | ["shot", _] => {
                let name = words.get(1).copied().unwrap_or("shot");
                let plain = name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c));
                if !plain || name.len() > MAX_NAME {
                    return Err(err(format!(
                        "a shot name is up to {MAX_NAME} letters, digits, _ and -"
                    )));
                }
                shots += 1;
                steps.push(Step::Shot(format!("{shots:02}-{name}")));
            }
            ["long", key, ref rest @ ..] => press(&mut steps, key, LONG_MS, rest).map_err(err)?,
            ["hold", key, ms, ref rest @ ..] => {
                let ms = millis(ms).map_err(err)?;
                press(&mut steps, key, ms, rest).map_err(err)?
            }
            [key, ref rest @ ..] => press(&mut steps, key, PRESS_MS, rest).map_err(err)?,
        }
        if steps.len() > MAX_STEPS {
            return Err(err(format!("more than {MAX_STEPS} steps")));
        }
    }
    if !steps.iter().any(|s| matches!(s, Step::Key { .. })) {
        return Err("the script presses no key".into());
    }
    Ok(steps)
}

/// `key`, `rest` times (`xN`, else once), each held `hold_ms`.
fn press(steps: &mut Vec<Step>, key: &str, hold_ms: u32, rest: &[&str]) -> Result {
    let Some(&(name, code)) = KEYS.iter().find(|k| k.0 == key) else {
        let names: Vec<&str> = KEYS.iter().map(|k| k.0).collect();
        return Err(format!("unknown key {key:?} (keys: {})", names.join(" ")));
    };
    let times = match rest {
        [] => 1,
        [n] => n
            .strip_prefix('x')
            .and_then(|n| n.parse().ok())
            .filter(|n| (1..=MAX_REPEAT).contains(n))
            .ok_or_else(|| format!("a repeat count is x1 to x{MAX_REPEAT}"))?,
        _ => return Err(format!("too many words after {key}")),
    };
    for _ in 0..times {
        steps.push(Step::Key {
            name,
            code,
            hold_ms,
        });
    }
    Ok(())
}

fn millis(ms: &str) -> Result<u32> {
    ms.parse()
        .ok()
        .filter(|ms| (1..=MAX_MS).contains(ms))
        .ok_or_else(|| format!("milliseconds are 1 to {MAX_MS}, not {ms:?}"))
}

/// The file a shot step comes back as.
fn shot_file(name: &str) -> String {
    format!("{name}.png")
}

/// Where the program looks and how long it waits; tests point it at a scratch dir.
struct Target {
    devices: String,
    input: String,
    luna: String,
    tmp: String,
    log: String,
    /// Seconds: after each key; before a capture; after the app came to the foreground; between
    /// looks at the foreground while it launches.
    gap: f32,
    settle: f32,
    boot: f32,
    poll: f32,
}

impl Target {
    fn television(log: String) -> Self {
        Target {
            devices: "/proc/bus/input/devices".into(),
            input: "/dev/input".into(),
            luna: "luna-send".into(),
            tmp: "/tmp".into(),
            log,
            gap: 0.7,
            settle: 0.8,
            boot: 4.0,
            poll: 1.0,
        }
    }
}

/// A Python string literal (ASCII; quotes and backslashes escaped).
fn py(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            ' '..='~' => out.push(c),
            _ => out.push('?'),
        }
    }
    out.push('"');
    out
}

/// The program sent to the television: the run's settings and steps, then [`PROGRAM`].
fn program(steps: &[Step], app: &str, t: &Target) -> String {
    let steps: Vec<String> = steps
        .iter()
        .map(|s| match s {
            Step::Key {
                name,
                code,
                hold_ms,
            } => format!("(\"key\", {code}, {hold_ms}, {})", py(name)),
            Step::Wait(ms) => format!("(\"wait\", {ms})"),
            Step::Shot(name) => format!("(\"shot\", {})", py(&shot_file(name))),
        })
        .collect();
    let settings = [
        ("APP", py(app)),
        ("HOME", py(HOME)),
        ("DEVICE", py(DEVICE)),
        ("DEVICES", py(&t.devices)),
        ("INPUT", py(&t.input)),
        ("LUNA", py(&t.luna)),
        ("TMP", py(&t.tmp)),
        ("LOG", py(&t.log)),
        ("LOG_TAIL", LOG_TAIL.to_string()),
        ("MAX_PNG", MAX_PNG.to_string()),
        ("GAP", t.gap.to_string()),
        ("SETTLE", t.settle.to_string()),
        ("BOOT", t.boot.to_string()),
        ("POLL", t.poll.to_string()),
    ];
    let mut out = String::from("# cargo xtask tv-replay\n");
    for (name, value) in settings {
        out.push_str(&format!("{name} = {value}\n"));
    }
    out.push_str("STEPS = [\n");
    for s in steps {
        out.push_str(&format!("    {s},\n"));
    }
    out.push_str("]\n");
    out.push_str(PROGRAM);
    out
}

/// Run `program` on `alias` through `ssh` and save what comes back into `dir`; the paths saved.
fn replay(
    ssh: &Path,
    alias: &str,
    program: &str,
    steps: &[Step],
    dir: &Path,
) -> Result<Vec<PathBuf>> {
    let mut expected: Vec<String> = steps
        .iter()
        .filter_map(|s| match s {
            Step::Shot(name) => Some(shot_file(name)),
            _ => None,
        })
        .collect();
    expected.push(LOG_NAME.to_owned());
    let mut child = Command::new(ssh)
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=6",
            alias,
            REMOTE,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", ssh.display()))?;
    let mut stdin = child.stdin.take().ok_or("ssh has no stdin")?;
    // A write error means ssh is gone; its exit status below says why.
    let _ = stdin.write_all(program.as_bytes());
    drop(stdin);
    let stderr = child.stderr.take().ok_or("ssh has no stderr")?;
    let shown = std::thread::spawn(move || {
        for line in BufReader::new(stderr).split(b'\n').map_while(|l| l.ok()) {
            eprintln!("{}", mask_urls(&String::from_utf8_lossy(&line)));
        }
    });
    let stdout = child.stdout.take().ok_or("ssh has no stdout")?;
    let received = receive(BufReader::new(stdout), &expected, dir);
    if received.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = shown.join();
    let saved = received?;
    let why = match status.code() {
        Some(0) => return Ok(saved),
        Some(NOT_OURS) => "another app is in the foreground; nothing was sent",
        Some(NO_FOREGROUND) => "the foreground app could not be read; nothing was sent",
        Some(NO_LAUNCH) => "the app did not come to the foreground; nothing was sent",
        Some(NO_DEVICE) => "the remote's input device is not there; nothing was sent",
        Some(UNDECLARED) => "the input device drops some of the script's keys; nothing was sent",
        Some(CHANGED) => "the foreground app changed during the run, which stopped there",
        Some(NO_PYTHON) => "the television has no python",
        _ => "the run failed (see above)",
    };
    let kept = if saved.is_empty() {
        String::new()
    } else {
        format!(" ({} file(s) kept in {})", saved.len(), dir.display())
    };
    Err(format!("tv-replay on {alias}: {why} [{status}]{kept}"))
}

/// Save the framed files on `from` into `dir`: only names in `expected`, each once and capped.
/// The event log is scrubbed line by line first.
fn receive(mut from: impl BufRead, expected: &[String], dir: &Path) -> Result<Vec<PathBuf>> {
    let mut saved = Vec::new();
    loop {
        let mut header = Vec::new();
        (&mut from)
            .take(128)
            .read_until(b'\n', &mut header)
            .map_err(|e| e.to_string())?;
        if header.is_empty() {
            return Ok(saved);
        }
        let header = String::from_utf8_lossy(&header);
        let frame = header
            .trim_end_matches('\n')
            .strip_prefix("WIFILE ")
            .and_then(|f| f.split_once(' '))
            .and_then(|(name, len)| Some((name, len.parse::<usize>().ok()?)));
        let Some((name, len)) = frame else {
            return Err("the television sent something that is not a file".into());
        };
        let path = dir.join(name);
        if !expected.iter().any(|e| e == name) || saved.contains(&path) {
            return Err(format!("the television sent an unexpected file {name:?}"));
        }
        let cap = if name == LOG_NAME { MAX_LOG } else { MAX_PNG };
        if len > cap {
            return Err(format!("{name}: {len} bytes, more than {cap}"));
        }
        let mut body = vec![0; len];
        from.read_exact(&mut body)
            .map_err(|e| format!("{name}: cut short ({e})"))?;
        if name == LOG_NAME {
            body = scrub_log(&body).into_bytes();
        }
        std::fs::write(&path, &body).map_err(|e| format!("{}: {e}", path.display()))?;
        saved.push(path);
    }
}

/// The event log as it is kept here: every line through the app's own local scrubber, then
/// every URL cut to its scheme.
fn scrub_log(log: &[u8]) -> String {
    String::from_utf8_lossy(log)
        .lines()
        .map(|l| mask_urls(&tv_base::eventlog::scrub::scrub_local(l)) + "\n")
        .collect()
}

/// `line` with every URL cut to its scheme (`https://...`), up to the next space or quote:
/// playlist and stream URLs carry tokens, in the path too (the scrubber's `<host>` is cut with
/// the rest).
fn mask_urls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("://") {
        out.push_str(&rest[..at + 3]);
        out.push_str("...");
        let tail = &rest[at + 3..];
        let end = tail
            .find(|c: char| c.is_whitespace() || "\"'`".contains(c))
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// What runs on the television, after the settings and STEPS [`program`] puts above it. Python
/// 2.7 and 3: the C1 has only a stripped python2.7 (os, sys, time, struct, json, subprocess,
/// fcntl, signal are there; tempfile is not relied on).
const PROGRAM: &str = r#"
import json
import os
import signal
import struct
import subprocess
import sys
import time

SERVICES = ("com.webos.applicationManager", "com.webos.service.applicationmanager")
EV_KEY = 1
EVENT = "llHHi"
OUT = getattr(sys.stdout, "buffer", sys.stdout)
# luna-send's stdin, never this program's own: that is ssh's pipe, at its end once python read
# the program, and the C5's luna-send prints nothing when its stdin is a pipe that hung up.
NULL = os.open(os.devnull, os.O_RDONLY)


def say(text):
    sys.stderr.write("tv-replay: %s\n" % text)
    sys.stderr.flush()


def stop(code, text):
    say(text)
    sys.exit(code)


def quit_on_signal(signum, frame):
    # A killed run still releases the key it holds (see press).
    raise SystemExit(1)


def token():
    return "".join("%02x" % b for b in bytearray(os.urandom(8)))


def luna(url, payload):
    """One luna-send reply as a dict, or None. The reply goes to a new file and is waited for:
    in a pipe it can be lost, and it can come after luna-send returned."""
    path = os.path.join(TMP, "wi-replay.%s.json" % token())
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    child = None
    try:
        child = subprocess.Popen([LUNA, "-n", "1", url, payload], stdin=NULL, stdout=fd, stderr=fd)
        deadline = time.time() + 5
        while time.time() < deadline:
            time.sleep(0.1)
            with open(path) as f:
                text = f.read()
            start, end = text.find("{"), text.rfind("}")
            if start >= 0 and end > start:
                try:
                    return json.loads(text[start:end + 1])
                except ValueError:
                    pass
        return None
    finally:
        os.close(fd)
        if child is not None and child.poll() is None:
            child.kill()
        os.remove(path)


def foreground():
    for service in SERVICES:
        reply = luna("luna://%s/getForegroundAppInfo" % service, "{}")
        if reply and reply.get("returnValue", True) and "appId" in reply:
            return reply["appId"]
    return None


def launch():
    for service in SERVICES:
        reply = luna("luna://%s/launch" % service, json.dumps({"id": APP}))
        if reply and reply.get("returnValue"):
            break
    deadline = time.time() + 20
    while time.time() < deadline:
        time.sleep(POLL)
        if foreground() == APP:
            time.sleep(BOOT)
            return True
    return False


def device():
    """The event node of the input device named DEVICE."""
    with open(DEVICES) as f:
        blocks = f.read().split("\n\n")
    for block in blocks:
        name, handlers = None, []
        for line in block.splitlines():
            if line.startswith("N: Name="):
                name = line[len("N: Name="):].strip().strip('"')
            elif line.startswith("H: Handlers="):
                handlers = line[len("H: Handlers="):].split()
        if name == DEVICE:
            for h in handlers:
                if h.startswith("event") and h[5:].isdigit():
                    return os.path.join(INPUT, h)
    return None


def undeclared(fd, codes):
    """The codes the device does not declare (the kernel drops those silently); None when it
    cannot be asked. EVIOCGBIT(EV_KEY) answers a byte bitmap whatever the word size."""
    try:
        import fcntl
    except ImportError:
        return None
    size = 96
    request = (2 << 30) | (size << 16) | (ord("E") << 8) | (0x20 + EV_KEY)
    for req in (request, request - (1 << 32)):
        try:
            bits = bytearray(fcntl.ioctl(fd, req, b"\0" * size))
        except OverflowError:
            continue
        except (IOError, OSError):
            return None
        return sorted(c for c in set(codes) if not (bits[c // 8] >> (c % 8)) & 1)
    return None


def emit(fd, code, value):
    os.write(fd, struct.pack(EVENT, 0, 0, EV_KEY, code, value) + struct.pack(EVENT, 0, 0, 0, 0, 0))


def press(fd, code, hold_ms):
    emit(fd, code, 1)
    try:
        time.sleep(hold_ms / 1000.0)
    finally:
        emit(fd, code, 0)


def read_file(path, limit, tail):
    """Up to `limit` bytes of the regular file at `path` (its end when `tail`), never through a
    link; None when there is none."""
    try:
        fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    except OSError:
        return None
    try:
        st = os.fstat(fd)
        if st.st_mode & 0o170000 != 0o100000:
            return None
        if st.st_size > limit:
            if not tail:
                return None
            os.lseek(fd, st.st_size - limit, 0)
        chunks, got = [], 0
        while got < limit:
            chunk = os.read(fd, min(65536, limit - got))
            if not chunk:
                break
            chunks.append(chunk)
            got += len(chunk)
        return b"".join(chunks)
    finally:
        os.close(fd)


def shot():
    """A capture of the screen as PNG bytes, or None."""
    time.sleep(SETTLE)
    path = os.path.join(TMP, "wi-replay.%s.png" % token())
    payload = {"path": path, "format": "PNG", "width": 1920, "height": 1080}
    try:
        luna("luna://com.webos.service.capture/executeOneShot", json.dumps(payload))
        return read_file(path, MAX_PNG, False)
    finally:
        try:
            os.remove(path)
        except OSError:
            pass


def send(name, data):
    OUT.write(("WIFILE %s %d\n" % (name, len(data))).encode("ascii"))
    OUT.write(data)
    OUT.flush()


def main():
    for signum in (signal.SIGHUP, signal.SIGTERM, signal.SIGINT):
        signal.signal(signum, quit_on_signal)
    app = foreground()
    if app is None:
        stop(15, "the foreground app could not be read")
    if app != APP:
        if app != HOME:
            stop(10, "the foreground app is %s, not %s or the home screen" % (app, APP))
        say("launching %s" % APP)
        if not launch():
            stop(14, "%s did not come to the foreground" % APP)
    path = device()
    if path is None:
        stop(11, "no input device named %s" % DEVICE)
    fd = os.open(path, os.O_WRONLY)
    missing = undeclared(fd, [s[1] for s in STEPS if s[0] == "key"])
    if missing is None:
        say("could not ask %s which keys it declares; going on" % path)
    elif missing:
        stop(12, "%s does not declare the key codes %s" % (path, missing))
    total = len(STEPS)
    for i, step in enumerate(STEPS):
        if step[0] == "key":
            now = foreground()
            if now != APP:
                stop(13, "step %d: the foreground app is now %s" % (i + 1, now))
            say("%d/%d %s %d ms" % (i + 1, total, step[3], step[2]))
            press(fd, step[1], step[2])
            time.sleep(GAP)
        elif step[0] == "wait":
            time.sleep(step[1] / 1000.0)
        elif step[0] == "shot":
            png = shot()
            if png:
                send(step[1], png)
                say("%d/%d %s" % (i + 1, total, step[1]))
            else:
                say("%d/%d %s: no picture" % (i + 1, total, step[1]))
    os.close(fd)
    log = read_file(LOG, LOG_TAIL, True)
    if log is not None:
        send("events.log", log)


main()
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn steps(text: &str) -> Vec<Step> {
        parse(text).unwrap()
    }

    fn key(name: &'static str, code: u16, hold_ms: u32) -> Step {
        Step::Key {
            name,
            code,
            hold_ms,
        }
    }

    #[test]
    fn scripts_parse_into_presses_waits_and_shots() {
        let s = steps(
            "# comment\n\nLeft\nRight x2  # trailing comment\nlong OK\nhold Down 2000\n\
             wait 1500\nshot\nshot groups-panel\nCH+\n",
        );
        assert_eq!(
            s,
            [
                key("Left", 105, PRESS_MS),
                key("Right", 106, PRESS_MS),
                key("Right", 106, PRESS_MS),
                key("OK", 28, LONG_MS),
                key("Down", 108, 2000),
                Step::Wait(1500),
                Step::Shot("01-shot".into()),
                Step::Shot("02-groups-panel".into()),
                key("CH+", 402, PRESS_MS),
            ]
        );
        assert_eq!(steps("hold OK 700 x2").len(), 2);
    }

    #[test]
    fn bad_lines_are_refused_with_their_number() {
        for (text, what) in [
            ("Left\nLft", "line 2: unknown key \"Lft\""),
            ("Left x0", "repeat count"),
            ("Left x51", "repeat count"),
            ("Left 3", "repeat count"),
            ("Left x2 x2", "too many words"),
            ("Left\nwait", "unknown key \"wait\""),
            ("Left\nwait 0", "milliseconds"),
            ("Left\nwait 60001", "milliseconds"),
            ("hold Left", "unknown key \"hold\""),
            ("Left\nshot a/b", "shot name"),
            ("Left\nshot ../x", "shot name"),
            ("Left\nshot \u{e9}", "shot name"),
            ("long", "unknown key \"long\""),
            ("# nothing\nwait 10\nshot", "presses no key"),
            ("", "presses no key"),
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(what), "{text:?}: {e}");
        }
        assert!(parse(&"Left x50\n".repeat(21)).is_err(), "step cap");
    }

    #[test]
    fn back_is_sent_as_412_and_every_code_is_distinct() {
        let back = KEYS.iter().find(|k| k.0 == "Back").unwrap();
        assert_eq!(
            back.1, 412,
            "158 (KEY_BACK) is dropped by the remote's device"
        );
        for (i, a) in KEYS.iter().enumerate() {
            assert!(KEYS[i + 1..].iter().all(|b| b.0 != a.0 && b.1 != a.1));
            assert!(usize::from(a.1) < 96 * 8, "inside the EVIOCGBIT bitmap");
        }
    }

    /// The guide Left/Back sequence of the C1 trial, as shipped for televisions.
    #[test]
    fn the_example_script_is_the_c1_trial_sequence() {
        let path = crate::root().join("xtask/replay/guide-left-back.keys");
        let s = steps(&std::fs::read_to_string(path).unwrap());
        // The keys alone, as `name`, `long name` or `hold name`.
        let keys: Vec<String> = s
            .iter()
            .filter_map(|s| match s {
                Step::Key { name, hold_ms, .. } => Some(match *hold_ms {
                    PRESS_MS => name.to_string(),
                    LONG_MS => format!("long {name}"),
                    _ => format!("hold {name}"),
                }),
                _ => None,
            })
            .collect();
        let keys = keys.join(" ");
        for seq in ["Left Left Left hold Left", "Back Back Back Left Back"] {
            assert!(keys.contains(seq), "{seq:?} in {keys:?}");
        }
        assert!(s.iter().filter(|s| matches!(s, Step::Shot(_))).count() >= 4);
    }

    #[test]
    fn the_program_carries_the_steps_and_no_unsafe_text() {
        let t = Target::television("/tmp/webosiptv-events.log".into());
        let p = program(
            &steps("Left\nBack\nlong OK\nshot a"),
            "io.github.maclucky.webosiptv",
            &t,
        );
        assert!(p.starts_with("# cargo xtask tv-replay\nAPP = \"io.github.maclucky.webosiptv\"\n"));
        assert!(p.contains("    (\"key\", 105, 100, \"Left\"),\n"));
        assert!(p.contains("    (\"key\", 412, 100, \"Back\"),\n"));
        assert!(p.contains("    (\"key\", 28, 1500, \"OK\"),\n"));
        assert!(p.contains("    (\"shot\", \"01-a.png\"),\n"));
        assert!(p.contains("DEVICE = \"LGE M-RCU - Builtin [2]\"\n"));
        assert!(p.contains("LOG = \"/tmp/webosiptv-events.log\"\n"));
        assert!(!p.contains("158") && !p.contains("pkill"));
        assert!(p.is_ascii());
        assert_eq!(py("a\"b\\c\u{e9}"), "\"a\\\"b\\\\c?\"");
    }

    #[test]
    fn urls_are_cut_to_their_scheme() {
        assert_eq!(
            mask_urls("play https://h.example/pl/abc123/x.m3u8?t=1 ok \"rtmp://u:p@h/x\""),
            "play https://... ok \"rtmp://...\""
        );
        assert_eq!(mask_urls("a http://<host>/t0k/x b"), "a http://... b");
        assert_eq!(mask_urls("no url here"), "no url here");
        assert_eq!(mask_urls("x://"), "x://...");
        let log = scrub_log(b"a http://host.example/tok3n/list.m3u8\nb\xff c\n");
        assert_eq!(log, "a http://...\nb\u{fffd} c\n");
    }

    #[test]
    fn options() {
        let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert_eq!(
            parse_args(&args("lg-c5 s.keys --out o --flavor debug")).unwrap(),
            Opts {
                alias: "lg-c5".into(),
                script: "s.keys".into(),
                out: Some("o".into()),
                flavor: "debug".into(),
            }
        );
        assert!(parse_args(&args("-oProxyCommand=x s.keys")).is_err());
        assert!(parse_args(&args("tv;rm s.keys")).is_err());
        assert!(parse_args(&args("lg-c5")).is_err());
        assert!(parse_args(&args("lg-c5 --out o")).is_err());
        assert!(parse_args(&args("lg-c5 s.keys --nope")).is_err());
    }

    /// A scratch dir and an executable `name` in it with `body`.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-replay-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("out")).unwrap();
        dir
    }

    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// A fake `ssh` that records its argv and stdin, prints `stdout` (a file it is given),
    /// writes `stderr` and exits with `code`.
    fn fake_ssh(dir: &Path, stdout: &[u8], stderr: &str, code: i32) -> PathBuf {
        std::fs::write(dir.join("reply"), stdout).unwrap();
        let ssh = dir.join("ssh");
        let d = dir.display();
        script(
            &ssh,
            &format!(
                "printf '%s\\n' \"$@\" > '{d}/argv'\ncat > '{d}/stdin'\ncat '{d}/reply'\n\
                 printf '%s\\n' '{stderr}' >&2\nexit {code}"
            ),
        );
        ssh
    }

    fn frame(name: &str, body: &[u8]) -> Vec<u8> {
        let mut f = format!("WIFILE {name} {}\n", body.len()).into_bytes();
        f.extend_from_slice(body);
        f
    }

    #[test]
    fn a_run_goes_over_stdin_and_brings_back_the_shots_and_a_scrubbed_log() {
        let dir = scratch("ok");
        let s = steps("Left\nshot start\nBack\nshot groups");
        let mut reply = frame("01-start.png", b"\x89PNG one");
        reply.extend(frame("02-groups.png", b"\x89PNG two"));
        reply.extend(frame(
            LOG_NAME,
            b"remote: Left\nplay https://p.example/T0KEN/x.m3u8\n",
        ));
        let ssh = fake_ssh(&dir, &reply, "tv-replay: at https://p.example/T0KEN", 0);
        let program = program(&s, "a.b", &Target::television("/tmp/l".into()));
        let out = dir.join("out");
        let saved = replay(&ssh, "tv-1", &program, &s, &out).unwrap();
        assert_eq!(saved.len(), 3);
        assert_eq!(
            std::fs::read(out.join("01-start.png")).unwrap(),
            b"\x89PNG one"
        );
        assert_eq!(
            std::fs::read(out.join("02-groups.png")).unwrap(),
            b"\x89PNG two"
        );
        let log = std::fs::read_to_string(out.join(LOG_NAME)).unwrap();
        assert_eq!(log, "remote: Left\nplay https://...\n");
        assert_eq!(std::fs::read_to_string(dir.join("stdin")).unwrap(), program);
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        assert_eq!(
            argv,
            format!("-o\nBatchMode=yes\n-o\nConnectTimeout=6\ntv-1\n{REMOTE}\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refusal_or_a_stray_file_is_an_error() {
        let dir = scratch("refused");
        let s = steps("Left\nshot a");
        let ssh = fake_ssh(
            &dir,
            b"",
            "tv-replay: the foreground app is org.example",
            10,
        );
        let out = dir.join("out");
        let e = replay(&ssh, "tv-1", "", &s, &out).unwrap_err();
        assert!(e.contains("another app is in the foreground"), "{e}");
        assert!(std::fs::read_dir(&out).unwrap().next().is_none());
        // Only the names the script gave, each once, capped; nothing outside the dir.
        for reply in [
            frame("../x.png", b"x"),
            frame("01-b.png", b"x"),
            [frame("01-a.png", b"x"), frame("01-a.png", b"y")].concat(),
            format!("WIFILE 01-a.png {}\n", MAX_PNG + 1).into_bytes(),
            b"WIFILE 01-a.png 10\nshort".to_vec(),
            b"garbage\n".to_vec(),
        ] {
            let ssh = fake_ssh(&dir, &reply, "", 0);
            assert!(replay(&ssh, "tv-1", "", &s, &out).is_err());
        }
        assert!(!dir.join("x.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The program itself, run here by the local python3 through a fake ssh, against a fake
    /// `luna-send`, input device list and event node. Skipped without python3.
    #[test]
    fn the_program_runs_and_refuses_as_documented() {
        let Some(python) = [
            "/usr/bin/python3",
            "/opt/homebrew/bin/python3",
            "/usr/local/bin/python3",
        ]
        .into_iter()
        .map(Path::new)
        .find(|p| p.is_file()) else {
            eprintln!("no python3 here: the program's own run is not tested");
            return;
        };
        let dir = scratch("python");
        let d = dir.display();
        let app = "io.example.app";
        std::fs::write(
            dir.join("devices"),
            "I: Bus=0000\nN: Name=\"LGE M-RCU - Builtin [0]\"\nH: Handlers=kbd event0\n\n\
             I: Bus=0000\nN: Name=\"LGE M-RCU - Builtin [2]\"\nH: Handlers=sysrq kbd event7\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("events.log"),
            "boot\nplay http://p.example/S3CRET/list.m3u8\n",
        )
        .unwrap();
        let luna = dir.join("luna-send");
        script(
            &luna,
            &format!(
                "url=$3; payload=$4\n\
                 [ -p /dev/stdin ] && exit 0\n\
                 case \"$url\" in\n\
                 */getForegroundAppInfo)\n\
                   if [ -f '{d}/flip' ]; then n=$(($(cat '{d}/flip') - 1)); echo $n > '{d}/flip';\n\
                     if [ $n -le 0 ]; then printf other.app > '{d}/fg'; fi; fi\n\
                   printf '{{\"returnValue\":true,\"appId\":\"%s\"}}\\n' \"$(cat '{d}/fg')\" ;;\n\
                 */launch) printf '{app}' > '{d}/fg'; echo '{{\"returnValue\":true}}' ;;\n\
                 */executeOneShot)\n\
                   p=$(printf '%s' \"$payload\" | sed 's/.*\"path\": *\"\\([^\"]*\\)\".*/\\1/')\n\
                   printf 'PNG %s' \"$(wc -c < '{d}/input/event7' | tr -d ' ')\" > \"$p\"; echo '{{\"returnValue\":true}}' ;;\n\
                 esac"
            ),
        );
        std::fs::create_dir_all(dir.join("input")).unwrap();
        std::fs::create_dir_all(dir.join("tmp")).unwrap();
        let ssh = dir.join("ssh");
        script(&ssh, &format!("exec '{}' -u -", python.display()));
        let target = Target {
            devices: format!("{d}/devices"),
            input: format!("{d}/input"),
            luna: luna.display().to_string(),
            tmp: format!("{d}/tmp"),
            log: format!("{d}/events.log"),
            gap: 0.0,
            settle: 0.0,
            boot: 0.0,
            poll: 0.05,
        };
        let s = steps("hold Left 5\nhold Back 5\nshot one\nwait 1");
        let program = program(&s, app, &target);
        let run = |fg: &str, flip: Option<u32>| {
            std::fs::write(dir.join("fg"), fg).unwrap();
            let _ = std::fs::remove_file(dir.join("flip"));
            if let Some(n) = flip {
                std::fs::write(dir.join("flip"), n.to_string()).unwrap();
            }
            std::fs::write(dir.join("input/event7"), "").unwrap();
            let out = dir.join("out");
            let _ = std::fs::remove_dir_all(&out);
            std::fs::create_dir_all(&out).unwrap();
            let result = replay(&ssh, "tv", &program, &s, &out);
            (result, std::fs::read(dir.join("input/event7")).unwrap())
        };
        // Started from home: launched, both keys pressed and released, the shot and the log.
        let (saved, events) = run(HOME, None);
        let saved = saved.unwrap();
        assert_eq!(saved.len(), 2);
        // struct input_event: two longs (the time), type, code, value.
        let long = std::mem::size_of::<usize>();
        let size = 2 * long + 8;
        let decoded: Vec<(u16, u16, i32)> = events
            .chunks(size)
            .map(|e| {
                let at = 2 * long;
                (
                    u16::from_ne_bytes([e[at], e[at + 1]]),
                    u16::from_ne_bytes([e[at + 2], e[at + 3]]),
                    i32::from_ne_bytes(e[at + 4..at + 8].try_into().unwrap()),
                )
            })
            .collect();
        let syn = (0, 0, 0);
        assert_eq!(
            decoded,
            [
                (1, 105, 1),
                syn,
                (1, 105, 0),
                syn,
                (1, 412, 1),
                syn,
                (1, 412, 0),
                syn
            ]
        );
        let shot = std::fs::read_to_string(dir.join("out/01-one.png")).unwrap();
        assert_eq!(shot, format!("PNG {}", 8 * size), "taken after both keys");
        let log = std::fs::read_to_string(dir.join("out/events.log")).unwrap();
        assert_eq!(log, "boot\nplay http://...\n");
        assert_eq!(
            std::fs::read_dir(dir.join("tmp")).unwrap().count(),
            0,
            "no leftovers"
        );
        // Already in front: the same run.
        assert!(run(app, None).0.is_ok());
        // Another app in front: nothing is sent.
        let (refused, events) = run("org.example.player", None);
        assert!(refused.unwrap_err().contains("another app"));
        assert!(events.is_empty());
        // The foreground changes before the second key: the run stops there.
        let (stopped, events) = run(app, Some(3));
        assert!(stopped.unwrap_err().contains("changed during the run"));
        assert_eq!(events.len(), 4 * size, "the first key only, released");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
