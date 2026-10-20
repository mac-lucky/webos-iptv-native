//! The desktop simulator's own pieces (`--features hostsim`): its command line, scripted key
//! presses and pointer moves, and the screenshot a review looks at.
//!
//! It is the television's app linked against Homebrew's SDL2 and the Mac's GL: the same boot,
//! loop and screens, with desktop keys standing in for the remote (`input::decode`).

use std::ffi::{c_int, c_ulong};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use iptv::model::CatchupMode;
use tv_screens::data::fake::FakeData;

use crate::input::{Edge, KeyEvent, PointerEvent, PointerKind, REMOTE};
use crate::sdl::Video;
use crate::shell::Shell;

const USAGE: &str = "\
usage: webosiptv [--fake [--streams BASE]] [--keys KEY,KEY,...] [--wait MS] [--shot FILE.png]
  --fake     synthetic channels and guide instead of the data dir's storage
  --streams  play the synthetic channels from BASE/live.m3u8, their archive from
             BASE/archive.m3u8?utc=..&lutc=.., the movies from BASE/movie.mp4?item=..
             and the episodes from BASE/vod.m3u8?item=.. (a local test origin)
  --keys     press these keys first: Up Down Left Right OK Back Exit CH+ CH- Red Green Yellow
             Blue 0-9 Del Clear; long-KEY holds it; text=WORD types WORD on the TV keyboard;
             wait=MS lets MS milliseconds pass; point=X:Y moves the pointer there (canvas
             pixels), click=X:Y clicks there, wheel=up or wheel=down turns the wheel a step
  --wait     let MS more milliseconds pass after the keys (streams start, panels time out)
  --shot     write a PNG of the frame once the keys are done and the screen has settled, then exit
video: touch /tmp/webosiptv-clocksink (the host sink plays) and /tmp/webosiptv-simvideo (the
decoded picture under the UI, needs ffmpeg)
keyboard: arrows, Enter = OK, Backspace = Back, Page Up/Down = CH+/CH-, 0-9, F1-F4 = Red Green
Yellow Blue; with the TV keyboard up, typing types, Delete deletes, keypad Clear clears
mouse: the Magic Remote's pointer: a click is OK, the wheel steps Up and Down";

/// How long a scripted press is held: a short one, and one past the longest long-press setting.
const SHORT_MS: u32 = 100;
const LONG_MS: u32 = 1_200;
/// From one scripted step's end to the next one.
const GAP_MS: u32 = 400;
/// After the last step, before the screenshot: slides and scrolls come to rest.
const SETTLE_MS: u32 = 1_500;
/// One simulated frame.
const FRAME_MS: u64 = 16;

#[derive(Debug, PartialEq)]
enum Step {
    /// `(sym, wcode)` held for `hold_ms`.
    Press {
        sym: u32,
        wcode: u32,
        hold_ms: u32,
    },
    Text(String),
    /// Let this many milliseconds pass.
    Wait(u32),
    /// The pointer to `(x, y)` on the canvas, then a click there when `click`.
    Point {
        x: f32,
        y: f32,
        click: bool,
    },
    /// One step of the wheel.
    Wheel {
        up: bool,
    },
}

#[derive(Debug, Default, PartialEq)]
pub struct Options {
    steps: Vec<Step>,
    shot: Option<PathBuf>,
    fake: bool,
    streams: Option<String>,
    wait_ms: u32,
}

impl Options {
    /// Read the command line and point the payload dir at the repository's `pkg/` unless
    /// `WEBOSIPTV_APP_DIR` names another one (before any other thread exists, and before
    /// `tv_base::paths` first resolves). `Err` is the status to exit with after the usage text.
    pub fn start() -> Result<Self, ExitCode> {
        let options = Self::parse(std::env::args().skip(1)).map_err(|e| {
            if e.is_empty() {
                eprintln!("{USAGE}");
                ExitCode::SUCCESS
            } else {
                eprintln!("{e}\n{USAGE}");
                ExitCode::from(2)
            }
        })?;
        if std::env::var_os("WEBOSIPTV_APP_DIR").is_none() {
            std::env::set_var(
                "WEBOSIPTV_APP_DIR",
                concat!(env!("CARGO_MANIFEST_DIR"), "/../../pkg"),
            );
        }
        eprintln!(
            "simulator: event log {}",
            tv_base::eventlog::events_log().display()
        );
        Ok(options)
    }

    /// `--fake`: synthetic channels and guide, played from `--streams` when given.
    pub fn fake_data(&self) -> Option<FakeData> {
        if !self.fake {
            return None;
        }
        let mut data = FakeData::new(crate::store::unix_now(), 30);
        if let Some(base) = &self.streams {
            let base = base.trim_end_matches('/');
            for c in &mut data.channels {
                c.url = format!("{base}/live.m3u8");
                if c.catchup.available() {
                    c.catchup.mode = Some(CatchupMode::Shift);
                    c.catchup.source =
                        Some(format!("{base}/archive.m3u8?utc={{utc}}&lutc={{lutc}}"));
                }
            }
        }
        // Movies and shows for the VOD sections, played from BASE with --streams.
        data.vod = tv_screens::vod::fake::items(self.streams.as_deref().unwrap_or(""));
        Some(data)
    }

    /// Start the shell and play the scripted steps in real time, then take the screenshot if
    /// one was asked for: `Break` ends the run with that status, `Continue` hands over to the
    /// interactive loop at that clock.
    pub fn script(&self, video: &Video, shell: &mut Shell) -> ControlFlow<ExitCode, u64> {
        shell.start(0);
        let mut clock = Clock {
            now: 0,
            start: Instant::now(),
            video,
            shell,
        };
        let mut at = 0;
        for step in &self.steps {
            match step {
                Step::Press {
                    sym,
                    wcode,
                    hold_ms,
                } => {
                    for (edge, t) in [(Edge::Down, at), (Edge::Up, at + hold_ms)] {
                        clock.run_to(u64::from(t));
                        clock.shell.key(KeyEvent {
                            sym: *sym,
                            wcode: *wcode,
                            edge,
                            at_ms: t,
                        });
                        clock.keyboard();
                    }
                    at += hold_ms + GAP_MS;
                }
                Step::Text(text) => {
                    clock.run_to(u64::from(at));
                    clock.shell.text(text, u64::from(at));
                    clock.keyboard();
                    at += GAP_MS;
                }
                Step::Wait(ms) => at += ms,
                Step::Point { x, y, click } => {
                    // From 200 px away first, as a hand moves it: past the keys' hold on the focus.
                    let from = if *x >= 200.0 { x - 200.0 } else { x + 200.0 };
                    let mut edges = vec![
                        (PointerKind::Move, from, at),
                        (PointerKind::Move, *x, at + 16),
                    ];
                    if *click {
                        edges.push((PointerKind::Down, *x, at + 32));
                        edges.push((PointerKind::Up, *x, at + 32 + SHORT_MS));
                    }
                    for (kind, x, t) in edges {
                        clock.run_to(u64::from(t));
                        clock.shell.pointer(PointerEvent {
                            kind,
                            x,
                            y: *y,
                            at_ms: t,
                        });
                        clock.keyboard();
                    }
                    at += 32 + SHORT_MS + GAP_MS;
                }
                Step::Wheel { up } => {
                    clock.run_to(u64::from(at));
                    let kind = PointerKind::Wheel { up: *up };
                    clock.shell.pointer(PointerEvent {
                        kind,
                        x: 0.0,
                        y: 0.0,
                        at_ms: at,
                    });
                    at += GAP_MS;
                }
            }
            if clock.shell.exit() {
                return ControlFlow::Break(ExitCode::SUCCESS);
            }
        }
        let Some(path) = &self.shot else {
            return ControlFlow::Continue(clock.now);
        };
        clock.run_to(u64::from(at + SETTLE_MS + self.wait_ms));
        crate::draw(video, shell, 1.0 / 60.0);
        ControlFlow::Break(match save_png(path, video.read_rgb()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("screenshot: {e}");
                ExitCode::FAILURE
            }
        })
    }

    /// `Err("")` asks for the usage text.
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Options::default();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--keys" => {
                    for k in args.next().ok_or("--keys needs a list")?.split(',') {
                        options.steps.push(step(k.trim())?);
                    }
                }
                "--shot" => options.shot = Some(args.next().ok_or("--shot needs a file")?.into()),
                "--fake" => options.fake = true,
                "--streams" => options.streams = Some(args.next().ok_or("--streams needs a URL")?),
                "--wait" => {
                    let ms = args.next().ok_or("--wait needs milliseconds")?;
                    options.wait_ms = ms.parse().map_err(|_| format!("bad --wait {ms:?}"))?;
                }
                "-h" | "--help" => return Err(String::new()),
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        Ok(options)
    }
}

/// One `--keys` token.
fn step(token: &str) -> Result<Step, String> {
    if let Some(text) = token.strip_prefix("text=") {
        return Ok(Step::Text(text.to_owned()));
    }
    if let Some(ms) = token.strip_prefix("wait=") {
        return ms
            .parse()
            .map(Step::Wait)
            .map_err(|_| format!("bad {token:?}"));
    }
    match token.strip_prefix("wheel=") {
        Some("up") => return Ok(Step::Wheel { up: true }),
        Some("down") => return Ok(Step::Wheel { up: false }),
        Some(_) => return Err(format!("bad {token:?}")),
        None => {}
    }
    let at = token.strip_prefix("point=").map(|xy| (xy, false));
    if let Some((xy, click)) = at.or_else(|| token.strip_prefix("click=").map(|xy| (xy, true))) {
        let xy = xy
            .split_once(':')
            .and_then(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?)));
        let (x, y) = xy.ok_or_else(|| format!("bad {token:?}"))?;
        return Ok(Step::Point { x, y, click });
    }
    let (name, hold_ms) = match token.strip_prefix("long-") {
        Some(name) => (name, LONG_MS),
        None => (token, SHORT_MS),
    };
    let key = REMOTE.iter().find(|r| r.0.eq_ignore_ascii_case(name));
    let &(_, sym, wcode) = key.ok_or_else(|| format!("unknown key {token:?}"))?;
    Ok(Step::Press {
        sym,
        wcode,
        hold_ms,
    })
}

/// The script's clock: real time since the script started, so streams play as they would.
struct Clock<'a> {
    now: u64,
    start: Instant,
    video: &'a Video,
    shell: &'a mut Shell,
}

impl Clock<'_> {
    /// Run to `to` ms: every wake the shell asks for on the way, and a drawn (not presented)
    /// frame every 16 ms while something moves.
    fn run_to(&mut self, to: u64) {
        while self.now < to {
            let next = if self.shell.needs_frame() {
                self.now + FRAME_MS
            } else {
                self.shell
                    .next_wake(self.now)
                    .unwrap_or(to)
                    .max(self.now + 1)
            };
            let next = next.min(to);
            let real = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
            if next > real {
                std::thread::sleep(Duration::from_millis(next - real));
            }
            self.now = next;
            self.shell.tick(self.now);
            self.keyboard();
            if self.shell.needs_frame() {
                crate::draw(self.video, self.shell, FRAME_MS as f32 / 1000.0);
            }
        }
    }

    /// Raise or dismiss the keyboard as the shell asks, as the loop does.
    fn keyboard(&mut self) {
        if let Some(on) = self.shell.take_text_input() {
            self.video.text_input(on);
        }
    }
}

/// Write `(width, height, RGB rows)` to `path` as a PNG.
fn save_png(path: &Path, (w, h, rgb): (usize, usize, Vec<u8>)) -> Result<(), String> {
    if w == 0 || h == 0 {
        return Err("nothing was drawn".into());
    }
    let png = png(w, h, &rgb).ok_or("zlib refused to compress the frame")?;
    std::fs::write(path, png).map_err(|e| format!("{}: {e}", path.display()))
}

#[link(name = "z")]
extern "C" {
    fn compressBound(len: c_ulong) -> c_ulong;
    fn compress2(
        dst: *mut u8,
        dst_len: *mut c_ulong,
        src: *const u8,
        src_len: c_ulong,
        level: c_int,
    ) -> c_int;
}

/// 8-bit RGB, unfiltered rows, one zlib stream from the system's zlib.
fn png(w: usize, h: usize, rgb: &[u8]) -> Option<Vec<u8>> {
    let mut rows = Vec::with_capacity((w * 3 + 1) * h);
    for row in rgb.chunks_exact(w * 3) {
        rows.push(0);
        rows.extend_from_slice(row);
    }
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&u32::try_from(w).ok()?.to_be_bytes());
    ihdr.extend_from_slice(&u32::try_from(h).ok()?.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib(&rows)?);
    chunk(&mut out, b"IEND", &[]);
    Some(out)
}

fn zlib(data: &[u8]) -> Option<Vec<u8>> {
    let src_len = c_ulong::try_from(data.len()).ok()?;
    // SAFETY: compressBound is pure arithmetic; compress2 reads `src_len` bytes of `data` and
    // writes at most `len` bytes into `out`, which holds exactly `len`.
    unsafe {
        let mut len = compressBound(src_len);
        let mut out = vec![0u8; usize::try_from(len).ok()?];
        if compress2(out.as_mut_ptr(), &mut len, data.as_ptr(), src_len, 6) != 0 {
            return None;
        }
        out.truncate(usize::try_from(len).ok()?);
        Some(out)
    }
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// CRC-32 (ISO-HDLC), the one PNG chunks carry.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[link(name = "z")]
    extern "C" {
        fn uncompress(
            dst: *mut u8,
            dst_len: *mut c_ulong,
            src: *const u8,
            src_len: c_ulong,
        ) -> c_int;
    }

    fn args(s: &str) -> impl Iterator<Item = String> + '_ {
        s.split_whitespace().map(String::from)
    }

    #[test]
    fn the_command_line_names_keys_long_presses_text_and_a_shot() {
        let line = "--fake --streams http://127.0.0.1:1/ --wait 250 --shot out.png \
                    --keys up,long-OK,ch+,text=news,wait=900,7,point=960:540,click=10:20,wheel=down";
        let o = Options::parse(args(line)).unwrap();
        assert!(o.fake);
        assert_eq!(o.wait_ms, 250);
        let data = o.fake_data().unwrap();
        assert_eq!(data.channels[0].url, "http://127.0.0.1:1/live.m3u8");
        let archive = data.channels[0].catchup.source.as_deref().unwrap();
        assert_eq!(
            archive,
            "http://127.0.0.1:1/archive.m3u8?utc={utc}&lutc={lutc}"
        );
        assert!(
            !data.channels[2].catchup.available(),
            "no archive stays none"
        );
        assert_eq!(data.vod[0].url, "http://127.0.0.1:1/movie.mp4?item=0");
        let named = |s: &Step| match s {
            Step::Press { wcode, hold_ms, .. } => {
                format!("{} {hold_ms}", crate::input::name(*wcode).unwrap())
            }
            Step::Text(t) => format!("text {t}"),
            Step::Wait(ms) => format!("wait {ms}"),
            Step::Point { x, y, click } => format!("point {x} {y} {click}"),
            Step::Wheel { up } => format!("wheel {up}"),
        };
        let steps: Vec<_> = o.steps.iter().map(named).collect();
        assert_eq!(
            steps,
            [
                "Up 100",
                "OK 1200",
                "CH+ 100",
                "text news",
                "wait 900",
                "7 100",
                "point 960 540 false",
                "point 10 20 true",
                "wheel false"
            ]
        );
        assert_eq!(o.shot.as_deref(), Some(Path::new("out.png")));
        assert!(Options::parse(args("--keys up,nope"))
            .unwrap_err()
            .contains("nope"));
        assert!(Options::parse(args("--keys long-nope")).is_err());
        for bad in ["point=1", "click=a:2", "wheel=left"] {
            assert!(
                Options::parse(args(&format!("--keys {bad}"))).is_err(),
                "{bad}"
            );
        }
        assert!(Options::parse(args("--shot")).is_err());
        assert_eq!(Options::parse(args("--help")), Err(String::new()));
        assert_eq!(Options::parse(args("")), Ok(Options::default()));
    }

    #[test]
    fn a_png_carries_its_size_and_the_pixels_round_trip() {
        let (w, h) = (3, 2);
        let rgb: Vec<u8> = (0..w * h * 3).map(|i| i as u8 * 10).collect();
        let png = png(w, h, &rgb).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(png[16..24], [0, 0, 0, 3, 0, 0, 0, 2]);
        assert_eq!(
            png[png.len() - 12..],
            [0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]
        );
        let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        assert_eq!(&png[37..41], b"IDAT");
        let mut rows = vec![0u8; (w * 3 + 1) * h];
        let mut len = rows.len() as c_ulong;
        let rc = unsafe {
            uncompress(
                rows.as_mut_ptr(),
                &mut len,
                png[41..].as_ptr(),
                idat_len as c_ulong,
            )
        };
        assert_eq!((rc, len as usize), (0, rows.len()));
        assert_eq!((rows[0], &rows[1..10]), (0, &rgb[..9]));
        assert_eq!((rows[10], &rows[11..]), (0, &rgb[9..]));
    }

    #[test]
    fn the_crc_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
