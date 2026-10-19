//! The TV executable, and with `--features hostsim` the desktop simulator: all of it is the
//! application crate's `main`.

#[cfg(all(target_arch = "arm", target_os = "linux"))]
mod compat;

fn main() -> std::process::ExitCode {
    webosiptv_app::main()
}
