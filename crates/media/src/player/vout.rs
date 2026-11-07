//! player::vout - instruments for a picture that stays black although the pipeline reports frames
//! displayed (webOS 10+): the video output's own account of its MAIN sink after each Load's first
//! frame, and the `try` experiment switches.
//!
//! A switch is one word in the `try` dev trigger, read at every Load, so a run can change it
//! between zaps. Without the trigger file nothing here changes a Load, and a build without
//! `devtriggers` cannot arm one at all (`devtrig::read` is `None` there at compile time).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use serde_json::Value;
use tv_base::task::MainThread;

use super::log;

/// One experiment switch, named by its trigger word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Try {
    /// `mute-off`: once the exported window is placed, set its `mute` property to `off`.
    MuteOff,
    /// `set-window-id`: hand the exported window id to the media object (`setWindowId`) before
    /// Load, beside `option.windowId`.
    SetWindowId,
    /// `provider-chrome`: Load with `provider` "Chrome", as a browser's player does.
    ProviderChrome,
}

fn parse(word: &str) -> Option<Try> {
    match word {
        "mute-off" => Some(Try::MuteOff),
        "set-window-id" => Some(Try::SetWindowId),
        "provider-chrome" => Some(Try::ProviderChrome),
        _ => None,
    }
}

/// The trigger's switch, read now; `None` without the trigger file. Logged whenever the file is
/// there, so a run's log says which switch each Load ran under.
fn armed() -> Option<Try> {
    let word = tv_base::devtrig::read("try")?;
    let armed = parse(&word);
    if armed.is_some() {
        log(&format!("try: {word}"));
    } else {
        let word: String = word.chars().take(32).collect();
        log(&format!("try: unknown switch {word:?} ignored"));
    }
    armed
}

/// Whether the current Load runs under `mute-off`, for its placement.
static MUTE_OFF: AtomicBool = AtomicBool::new(false);

/// The Load-time switches, applied to the final payload (app id and windowId already in it) on
/// the main thread before the Load is issued.
pub(super) fn at_load(mt: &MainThread, payload: String) -> String {
    let armed = armed();
    MUTE_OFF.store(armed == Some(Try::MuteOff), Relaxed);
    match armed {
        Some(Try::ProviderChrome) => with_provider_chrome(&payload),
        Some(Try::SetWindowId) => {
            // SAFETY: main thread (the token), before this Load is issued.
            let rv = unsafe { super::sink().arm_window_id(mt) };
            log(&format!("try: set-window-id armed={rv}"));
            payload
        }
        _ => payload,
    }
}

fn with_provider_chrome(payload: &str) -> String {
    payload.replace(r#""provider":"webosiptv""#, r#""provider":"Chrome""#)
}

/// Right after the Load's exported window is placed at loadCompleted.
pub(super) fn after_place(mt: &MainThread) {
    if MUTE_OFF.load(Relaxed) {
        // SAFETY: main thread (the token); two NUL-terminated literals.
        let rv =
            unsafe { super::sink().set_window_property(mt, c"mute".as_ptr(), c"off".as_ptr()) };
        log(&format!("try: mute-off rv={rv}"));
    }
}

/// The last Load epoch whose first frame was probed; epochs start at 1.
static PROBED: AtomicU32 = AtomicU32::new(0);

/// Called on every pump tick that has a frame. On webOS 10+, the first such tick of each Load asks
/// the video output about MAIN on a worker, so the LS2 round trip never runs on the frame thread.
pub(super) fn after_frame(epoch: u32) {
    if due(tv_platform::tv::device::info().major, &PROBED, epoch) {
        tv_base::task::spawn_small("vout", || {
            log(&main_line(
                tv_platform::tv::video_output_status().as_deref(),
            ))
        });
    }
}

fn due(major: u32, probed: &AtomicU32, epoch: u32) -> bool {
    major >= 10 && probed.swap(epoch, Relaxed) != epoch
}

/// `vout: MAIN connected=<bool> muted=<bool> appId=<id>`, `?` for a field the reply lacks, or the
/// reply's head when it has no MAIN sink, so an unexpected shape is still on record.
fn main_line(reply: Option<&str>) -> String {
    let Some(reply) = reply else {
        return "vout: getStatus gave no reply".into();
    };
    let value = serde_json::from_str::<Value>(reply).ok();
    // The video sinks when the reply lists them: keys iterate alphabetically, so a MAIN of another
    // list (audio) would otherwise win.
    let sinks = value.as_ref().map(|v| v.get("video").unwrap_or(v));
    let Some(main) = sinks.and_then(find_main) else {
        let head: String = reply.chars().take(400).collect();
        return format!("vout: no MAIN sink in the reply: {head}");
    };
    let flag = |key: &str| {
        main.get(key)
            .and_then(Value::as_bool)
            .map_or("?".to_string(), |b| b.to_string())
    };
    format!(
        "vout: MAIN connected={} muted={} appId={}",
        flag("connected"),
        flag("muted"),
        main.get("appId").and_then(Value::as_str).unwrap_or("?")
    )
}

/// The object whose `sink` is "MAIN", wherever the reply nests it.
fn find_main(v: &Value) -> Option<&serde_json::Map<String, Value>> {
    match v {
        Value::Object(o) if o.get("sink").and_then(Value::as_str) == Some("MAIN") => Some(o),
        Value::Object(o) => o.values().find_map(find_main),
        Value::Array(a) => a.iter().find_map(find_main),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_switch_word_parses_and_nothing_else_does() {
        assert_eq!(parse("mute-off"), Some(Try::MuteOff));
        assert_eq!(parse("set-window-id"), Some(Try::SetWindowId));
        assert_eq!(parse("provider-chrome"), Some(Try::ProviderChrome));
        for word in ["", "auto", "primer", "Mute-Off", "mute-off set-window-id"] {
            assert_eq!(parse(word), None, "{word:?}");
        }
    }

    const PAYLOAD: &str = r#"{"args":[{"option":{"appId":"app.example","windowId":"_Window_Id_1","externalStreamingInfo":{"contents":{"codec":{"video":"H264"},"format":"RAW","provider":"webosiptv"}}}}]}"#;

    /// Each Load under the switch the trigger names when it is read, and under none without it:
    /// its payload, whether `mute-off` holds, and what it logged.
    fn load_under(word: Option<&str>) -> (String, bool, String) {
        tv_base::devtrig::with_private_triggers(|| {
            if let Some(word) = word {
                std::fs::write(tv_base::devtrig::path("try"), word).unwrap();
            }
            tv_base::eventlog::with_private_log(|| {
                // SAFETY: a test thread stands in for the main thread; the installed test sink
                // (NoSink, or HostSink under hostsim) answers arm_window_id with the trait default
                // and holds no state.
                let mt = unsafe { MainThread::assume() };
                let payload = at_load(&mt, PAYLOAD.to_string());
                let log = std::fs::read_to_string(tv_base::eventlog::events_log());
                (payload, MUTE_OFF.load(Relaxed), log.unwrap_or_default())
            })
        })
    }

    #[test]
    fn a_load_is_unchanged_without_the_trigger_and_follows_its_word_with_it() {
        let _serial = tv_base::testlock::serial();
        let unchanged = (PAYLOAD.to_string(), false, String::new());
        assert_eq!(load_under(None), unchanged);
        if !tv_base::devtrig::ENABLED {
            return; // a release build reads no trigger
        }
        let ran = |word: &str, mute: bool, line: &str| {
            let (payload, muted, log) = load_under(Some(word));
            assert_eq!((payload.as_str(), muted), (PAYLOAD, mute), "{word:?}");
            assert!(log.contains(line), "{word:?}: {log}");
        };
        ran("unknown", false, r#"try: unknown switch "unknown" ignored"#);
        ran("mute-off\n", true, "try: mute-off");
        // The test sink has no window to arm, so the verb's answer is 0.
        ran("set-window-id", false, "try: set-window-id armed=0");
        let (chrome, mute, log) = load_under(Some("provider-chrome"));
        assert_eq!(
            chrome,
            PAYLOAD.replace(r#""provider":"webosiptv""#, r#""provider":"Chrome""#)
        );
        assert!(!mute && log.contains("try: provider-chrome"), "{log}");
        assert_eq!(load_under(None), unchanged, "the next Load forgets it");
    }

    #[test]
    fn the_probe_runs_once_per_load_and_only_on_webos_10_or_later() {
        let probed = AtomicU32::new(0);
        assert!(!due(6, &probed, 1));
        assert!(due(10, &probed, 1));
        assert!(!due(10, &probed, 1), "the same Load again");
        assert!(due(10, &probed, 2), "the next Load");
        assert!(!due(4, &probed, 3));
    }

    #[test]
    fn the_main_line_reads_the_main_sink_of_a_get_status_reply() {
        let black = r#"{"returnValue":true,"video":[{"sink":"MAIN","connected":false,"muted":true,"appId":"unknown","displayOutput":{"x":0,"y":0,"width":0,"height":0}},{"sink":"SUB0","connected":false,"muted":true}]}"#;
        assert_eq!(
            main_line(Some(black)),
            "vout: MAIN connected=false muted=true appId=unknown"
        );
        let playing = r#"{"video":[{"sink":"SUB0","connected":true,"muted":false,"appId":"other.example"},{"sink":"MAIN","connected":true,"muted":false,"appId":"app.example"}],"returnValue":true}"#;
        assert_eq!(
            main_line(Some(playing)),
            "vout: MAIN connected=true muted=false appId=app.example"
        );
        let with_audio = r#"{"audio":[{"sink":"MAIN","connected":true,"muted":false}],"video":[{"sink":"MAIN","connected":false,"muted":true,"appId":"unknown"}]}"#;
        assert_eq!(
            main_line(Some(with_audio)),
            "vout: MAIN connected=false muted=true appId=unknown",
            "the video MAIN, not a MAIN that sorts first"
        );
        let unlisted = r#"{"sinks":{"main":{"sink":"MAIN","connected":true,"muted":false,"appId":"app.example"}}}"#;
        assert_eq!(
            main_line(Some(unlisted)),
            "vout: MAIN connected=true muted=false appId=app.example",
            "no video list: found wherever it is"
        );
    }

    #[test]
    fn a_reply_without_main_or_its_fields_still_says_what_came_back() {
        assert_eq!(main_line(None), "vout: getStatus gave no reply");
        assert_eq!(
            main_line(Some(r#"{"video":[{"sink":"MAIN","appId":null}]}"#)),
            "vout: MAIN connected=? muted=? appId=?"
        );
        let refused = r#"{"returnValue":false,"errorText":"denied"}"#;
        assert_eq!(
            main_line(Some(refused)),
            format!("vout: no MAIN sink in the reply: {refused}")
        );
        assert_eq!(
            main_line(Some("not json")),
            "vout: no MAIN sink in the reply: not json"
        );
        let long = "x".repeat(1000);
        assert_eq!(
            main_line(Some(&long)).len(),
            "vout: no MAIN sink in the reply: ".len() + 400
        );
    }
}
