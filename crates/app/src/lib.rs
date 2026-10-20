//! Application core: boot, the frame loop, remote input and the app lifecycle.
//!
//! `main` opens this run's logs (`logs`), brings up SDL and GL (`sdl`), opens the data
//! (`store`) and hands the screens to the `shell`. The loop then sleeps in SDL until the next
//! event or the shell's next wake, feeds it key edges and the Magic Remote's pointer (`input`)
//! and keyboard text, and draws and presents only when something changed or an animation still
//! moves. The boot path makes no network call and records nothing beyond the local logs.
//!
//! Built with `--features hostsim`, the same binary is the desktop simulator (`sim`).

mod backup;
mod input;
mod logos;
mod logs;
mod media;
mod player;
mod port;
mod sdl;
mod shell;
#[cfg(feature = "hostsim")]
mod sim;
mod store;
mod subtitles;
mod transport;

#[cfg(feature = "hostsim")]
use std::ops::ControlFlow;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use iptv::refresh::Transport;
use logs::log;
use sdl::{Event, Video};
use shell::Shell;
use tv_base::task::MainThread;

/// The process-shaped name: the window title, the simulator's data dir and the first line of the
/// event log.
const NAME: &str = "webosiptv";

/// The longest the loop sleeps without an event: the refresher's status and the player's
/// reports are read at least this often.
const MAX_WAIT_MS: u64 = 1_000;

/// One frame's time step while nothing was moving: the first frame of an animation.
const FIRST_DT: f32 = 1.0 / 60.0;

/// The whole app; the binary's `main` returns this.
pub fn main() -> ExitCode {
    #[cfg(feature = "hostsim")]
    let options = match sim::Options::start() {
        Ok(options) => options,
        Err(code) => return code,
    };
    // webOS passes the launch parameters as one JSON argument; nothing reads them yet.
    logs::start();
    log(&format!("{NAME} {} starting", env!("CARGO_PKG_VERSION")));
    // The platform (video sink, the set's probe), then the TV's language for every screen string.
    port::install();
    tv_platform::i18n::initialize(tv_platform::i18n::Preference::System, false);
    let video = match Video::open(NAME) {
        Ok(video) => video,
        Err(e) => {
            log(&format!("boot: {e}"));
            return ExitCode::FAILURE;
        }
    };
    log(&format!("GL: {}", video.gl_strings()));
    tv_starfish::set_event_log(log);
    tv_gfx::gfx::init_gl();
    tv_gfx::gfx::init_image();
    tv_gfx::text::init_text();
    // The process's User-Agent, then libcurl's process init, before any thread can make a request.
    let user_agent = format!("{NAME}/{}", env!("CARGO_PKG_VERSION"));
    if tv_net::net::set_user_agent(user_agent).is_err() {
        log("net: User-Agent refused");
    }
    if !tv_net::net::global_init() {
        log("net: no libcurl, https is off");
    }
    #[cfg(feature = "hostsim")]
    let store: Box<dyn store::Store> = match options.fake_data() {
        Some(fake) => Box::new(fake),
        None => Box::new(store::open()),
    };
    #[cfg(not(feature = "hostsim"))]
    let store: Box<dyn store::Store> = Box::new(store::open());
    // FFmpeg, then the player, which binds the video plane.
    tv_media::ff::boot();
    // SAFETY: `main` runs on the SDL main thread, the one every player call is made from.
    let player = Box::new(media::MediaPlayer::new(unsafe { MainThread::assume() }));
    // Transports spool a body (serialised TLS) into the private data dir, never RAM-backed /tmp.
    let spool = store.data_dir().map(Path::to_path_buf);
    let logo_net = net(spool.as_deref()).with_max_body(logos::MAX_BYTES);
    let logos = logos::LogoCache::new(Some(Box::new(logo_net)));
    logos.set_wake(sdl::wake);
    let refresh_net = move || -> Box<dyn Transport> { Box::new(net(spool.as_deref())) };
    let mut shell = Shell::new(store, player, Some(Box::new(refresh_net)), logos);
    #[cfg(feature = "hostsim")]
    let clock = match options.script(&video, &mut shell) {
        ControlFlow::Break(code) => return code,
        ControlFlow::Continue(clock) => clock,
    };
    #[cfg(not(feature = "hostsim"))]
    let clock = {
        shell.start(0);
        0
    };
    run(&video, &mut shell, clock);
    log("shutdown");
    ExitCode::SUCCESS
}

/// A transport over `tv_net`, spooling into `spool` when it has to.
fn net(spool: Option<&Path>) -> transport::NetTransport {
    let net = transport::NetTransport::new();
    match spool {
        Some(dir) => net.with_spool_dir(dir),
        None => net,
    }
}

/// The frame loop, its clock starting at `clock` ms. Returns when a screen asks to exit or the
/// system ends the app.
fn run(video: &Video, shell: &mut Shell, clock: u64) {
    let start = Instant::now();
    let ms = || clock + u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX / 2);
    let mut ev = Event::new();
    // Between a background edge and the next foreground the window is not ours: nothing is drawn.
    let mut background = false;
    let mut last_frame: Option<Instant> = None;
    loop {
        let now = ms();
        let wait = if shell.needs_frame() && !background {
            0
        } else {
            shell
                .next_wake(now)
                .map_or(MAX_WAIT_MS, |at| at.saturating_sub(now))
                .min(MAX_WAIT_MS)
        };
        let mut queued = video.wait(&mut ev, wait as u32);
        tv_platform::tv::window::pump_bus();
        while queued {
            match ev.kind() {
                sdl::SDL_QUIT | sdl::SDL_APP_TERMINATING => {
                    log("lifecycle: quit");
                    shell.shutdown();
                    return;
                }
                sdl::SDL_APP_WILLENTERBACKGROUND | sdl::SDL_APP_DIDENTERBACKGROUND => {
                    if !background {
                        log("lifecycle: background");
                        shell.suspend();
                        video.release_surface();
                        background = true;
                    }
                }
                sdl::SDL_APP_DIDENTERFOREGROUND => {
                    log("lifecycle: foreground");
                    background = false;
                    video.grab_surface();
                    shell.resume();
                }
                sdl::SDL_WINDOWEVENT => shell.redraw(),
                sdl::SDL_KEYDOWN | sdl::SDL_KEYUP => {
                    let key = input::decode(&ev, ms() as u32);
                    if let Some(line) = shell.key_line(&key) {
                        log(&line);
                    }
                    shell.key(key);
                }
                sdl::SDL_TEXTINPUT => shell.text(&input::decode_text(&ev), ms()),
                sdl::SDL_MOUSEMOTION
                | sdl::SDL_MOUSEBUTTONDOWN
                | sdl::SDL_MOUSEBUTTONUP
                | sdl::SDL_MOUSEWHEEL => {
                    if let Some(p) = input::decode_pointer(&ev, video.pointer_scale(), ms() as u32)
                    {
                        if let Some(line) = shell.pointer_line(&p) {
                            log(&line);
                        }
                        shell.pointer(p);
                    }
                }
                _ => {}
            }
            queued = video.poll(&mut ev);
        }
        shell.tick(ms());
        if let Some(on) = shell.take_text_input() {
            video.text_input(on);
        }
        if shell.exit() {
            log("exit");
            shell.shutdown();
            return;
        }
        if shell.needs_frame() && !background {
            let now = Instant::now();
            let dt = last_frame.map_or(FIRST_DT, |t| now.duration_since(t).as_secs_f32().min(0.1));
            frame(video, shell, dt);
            // The next frame steps by real time only while this one was part of an animation.
            last_frame = shell.needs_frame().then_some(now);
        }
    }
}

/// Step the animations by `dt` seconds, draw and present.
fn frame(video: &Video, shell: &mut Shell, dt: f32) {
    draw(video, shell, dt);
    video.swap();
}

/// [`frame`] without presenting it.
fn draw(video: &Video, shell: &mut Shell, dt: f32) {
    video.begin_frame();
    tv_gfx::text::begin_frame();
    shell.update(dt);
    shell.paint();
    // The simulator's decoded picture, under the UI (armed by the `simvideo` dev trigger).
    #[cfg(feature = "hostsim")]
    tv_media::player::sim_video::composite_under();
}
