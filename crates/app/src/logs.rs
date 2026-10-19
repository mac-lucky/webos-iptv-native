//! This run's logs, opened before anything else: the event log (fresh each launch, the last one
//! kept as `.prev`), stderr captured to a file on the television, and a panic hook that records
//! the message and a backtrace in the append-only crash log. Nothing is sent anywhere.
//!
//! Every sink is in the runtime dir (`/tmp` on the television, which is shared and sticky) and is
//! opened through `tv_base::eventlog::open_log_append`: no symlinks, a regular file this uid owns
//! with one link, mode 0640. Every line is masked first ([`mask`]): a URL a message happens to
//! carry reaches the disk without its tokens or credentials.

use std::path::Path;

use tv_base::eventlog::{append_record, open_log_append, scrub::scrub_local};
use tv_base::paths::{in_runtime_dir, runtime_file};

/// Append a line to the event log with every URL masked. The app's only log call: lines from the
/// crates below reach `tv_base::eventlog::log` directly and get its `scrub_local` pass alone.
pub fn log(m: &str) {
    // `eventlog::log` runs `scrub_local` itself, so the file gets exactly `mask(m)`.
    tv_base::eventlog::log(&iptv::redact::text(m));
}

/// A line as it reaches a log file. `iptv::redact::text` masks URL tokens: userinfo, every query
/// value, token-like runs and Xtream user/password segments. tv_base's `scrub_local` then masks
/// what redact keeps on purpose (hosts, bare addresses) and JSON secret fields.
fn mask(line: &str) -> String {
    scrub_local(&iptv::redact::text(line))
}

/// Start this run's logs and install the panic hook. Call first in `main`.
pub fn start() {
    rotate(
        &in_runtime_dir(runtime_file::EVENTS),
        &in_runtime_dir(runtime_file::EVENTS_PREV),
    );
    #[cfg(not(feature = "hostsim"))]
    capture_stderr();
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let place = info
            .location()
            .map_or_else(|| "?".into(), |l| format!("{}:{}", l.file(), l.line()));
        let head = format!(
            "PANIC [{}] at {place}: {}",
            thread.name().unwrap_or("?"),
            info.payload_as_str().unwrap_or("?")
        );
        log(&head);
        let backtrace = format!(
            "{}{:#}",
            image_line(),
            std::backtrace::Backtrace::force_capture()
        );
        let record = panic_record(&head, &backtrace, iptv::model::unix_now());
        let _ = append_record(&in_runtime_dir(runtime_file::CRASH), &record);
        eprintln!("{record}");
    }));
}

/// Keep the last run's event log as `prev` and start an empty one. `rename` acts on the entry,
/// never through a symlink, and the sticky runtime dir refuses it for a name another uid owns; a
/// log that could not be moved is emptied in place if it is ours, and left alone if it is not.
fn rotate(events: &Path, prev: &Path) {
    if std::fs::rename(events, prev).is_err() && events.exists() {
        let _ = open_log_append(events).and_then(|f| f.set_len(0));
    }
}

/// Point fd 2 at the stderr log, so what SDL, GL and the TV's libraries print is kept. The file is
/// checked before it is used, which a reopen by path would not be.
#[cfg(not(feature = "hostsim"))]
fn capture_stderr() {
    use std::os::fd::AsRawFd;
    let Ok(file) = open_log_append(&in_runtime_dir(runtime_file::STDERR)) else {
        return;
    };
    if file.set_len(0).is_ok() {
        // SAFETY: dup2 on two open descriptors; fd 2 keeps the file open after `file` closes its own.
        unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) };
    }
}

/// One crash-log record: the panic line, masked like every log line (a panic message is whatever
/// the failing code was formatting, a URL included), the time and the backtrace, which holds only
/// code addresses, symbols and source paths.
fn panic_record(head: &str, backtrace: &str, unix_secs: iptv::model::Timestamp) -> String {
    format!("{}\ntime: {unix_secs}\n{backtrace}", mask(head))
}

/// Where the executable is mapped, so the backtrace's raw addresses can be resolved against the
/// unstripped binary of a position-independent build.
fn image_line() -> String {
    #[cfg(target_os = "linux")]
    if let (Ok(exe), Ok(maps)) = (
        std::fs::read_link("/proc/self/exe"),
        std::fs::read_to_string("/proc/self/maps"),
    ) {
        if let Some(line) = exe
            .to_str()
            .and_then(|exe| maps.lines().find(|l| l.ends_with(exe)))
        {
            return format!("image: {line}\n");
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("webosiptv-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_last_event_log_is_kept_and_a_fresh_one_starts() {
        let dir = scratch("rotate");
        let (events, prev) = (dir.join("events.log"), dir.join("events.prev.log"));
        rotate(&events, &prev);
        assert!(
            !events.exists() && !prev.exists(),
            "a first launch has nothing to keep"
        );
        append_record(&events, "run one").unwrap();
        rotate(&events, &prev);
        assert_eq!(std::fs::read_to_string(&prev).unwrap(), "run one\n");
        assert!(!events.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Synthetic credentials, assembled at run time so no credential-shaped literal sits here.
    fn secret(prefix: &str, len: usize) -> String {
        format!("{prefix}{}", "x7Q".repeat(len))
            .chars()
            .take(len)
            .collect()
    }

    #[test]
    fn a_logged_url_keeps_no_credential() {
        let (user, pass, token) = (secret("u", 8), secret("p", 10), secret("t", 13));
        let line = format!(
            "open http://iptv.example:8080/live/{user}/{pass}/1234.ts via 10.0.0.7, \
             playlist http://pl.example/pl/13/{token}/playlist.m3u8"
        );
        let out = mask(&line);
        for s in [
            &*user,
            &pass,
            &token,
            "iptv.example",
            "pl.example",
            "10.0.0.7",
        ] {
            assert!(!out.contains(s), "{s} survived in {out}");
        }
        assert!(out.contains("/live/<masked>/<masked>/1234.ts"), "{out}");
        assert!(out.contains("/pl/13/<masked>/playlist.m3u8"), "{out}");
    }

    #[test]
    fn a_panic_record_masks_the_message_and_carries_time_and_backtrace() {
        let token = secret("t", 20);
        let record = panic_record(
            &format!("PANIC [main] at x.rs:1: bad http://h.example/x.ts?token={token}"),
            "   0: webosiptv_app::frame\n",
            1_750_000_000,
        );
        assert!(!record.contains(&token), "{record}");
        assert!(record.starts_with("PANIC [main] at x.rs:1: "), "{record}");
        assert!(record.contains("\ntime: 1750000000\n"), "{record}");
        assert!(record.ends_with("webosiptv_app::frame\n"), "{record}");
    }

    #[test]
    fn a_configured_urls_path_token_never_reaches_a_log_line() {
        // Letters only, so no token heuristic would catch it: only the registration does.
        let token: String = "quiet".chars().chain("meadowfx".chars()).collect();
        assert_eq!(token.len(), 13);
        let control = "softwindowsky";
        assert!(
            mask(&format!("owner {control}")).contains(control),
            "an unregistered word of the same shape stays"
        );
        let mut settings = tv_screens::panels::Settings::new();
        let url = format!("http://iptv.example/{token}/get/list.m3u");
        assert!(settings.set(tv_screens::panels::keys::PLAYLIST_URL, url.clone()));
        for line in [
            format!("refresh: {url} failed"),
            format!("refresh: playlist owner {token}, retrying"),
            format!("refresh: http://other.example/a/{token}?x=1"),
        ] {
            assert!(!mask(&line).contains(&token), "{}", mask(&line));
        }
    }
}
