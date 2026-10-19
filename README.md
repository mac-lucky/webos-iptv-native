# webos-iptv-rs

A native IPTV player for LG webOS TVs, written in Rust, with a TV guide and remote-control model
in the style of TiviMate. Early development: the foundation is in place, the player is not
usable yet.

The goal is a fast player for M3U playlists with an XMLTV guide and catch-up, built as a native
webOS app instead of a web app. Catch-up follows the M3U conventions of Kodi's pvr.iptvsimple
(`catchup`, `catchup-source`, `catchup-days`, and SIPTV `timeshift`).

First scope:

- one M3U playlist, groups, and an XMLTV guide
- live playback on the TV's hardware decoder, in full screen and in the guide's preview window
- catch-up from the guide (shift, Flussonic and template sources)
- guide, channel list, info and playback panels, history and favourites

Out of scope at first: video on demand catalogs, DRM, recording.

Native webOS apps are built with the community
[webosbrew native toolchain](https://github.com/webosbrew/native-toolchain) and installed through
the Homebrew Channel or developer mode. LG does not distribute native third-party apps.

## Layout

| Path | What it is |
|---|---|
| `crates/base`, `machine`, `net`, `platform`, `gfx`, `ui`, `media`, `storage` | platform, rendering, UI toolkit, network and player layers taken from StremioNative and PlxNative |
| `crates/iptv` | playlists, guide, catch-up URLs, storage |
| `crates/screens` | the TV screens |
| `crates/app` | boot, frame loop and wiring |
| `xtask` | build, package and deploy tooling (`cargo xtask <command>`) |

## Building

```sh
cargo check --workspace
cargo test --workspace
```

Host tests link SDL2 and SDL2_ttf (`brew install sdl2 sdl2_ttf` on macOS).

The desktop simulator runs the same app on the Mac. The keyboard stands in for the remote:
arrows, Enter = OK, Backspace = Back, PageUp/PageDown = CH+/CH-, digits, F1-F4 = colour keys.

```sh
cargo run -p webosiptv-app --features hostsim --bin webosiptv -- [--keys up,ok,red] [--shot out.png]
```

## Building for the TV

```sh
cargo xtask setup              # once: the Rust target and the webosbrew NDK (in ~/webos-ndk, or $WEBOS_SDK)
cargo xtask ffmpeg             # once per FFmpeg pin: the bundled LGPL FFmpeg
cargo xtask package            # target/ipk/io.github.maclucky.webosiptv_<version>_arm.ipk
cargo xtask verify             # webosbrew-ipk-verify for webOS 6 and newer (needs Docker)
cargo xtask sim [--keys up,ok] [--shot out.png]   # desktop simulator
```

The app id is `io.github.maclucky.webosiptv`. The package version lives in one place,
`version` in `pkg/appinfo.json`: three integers `X.Y.Z`, as webOS requires. It stays `0.y.z`
until the player is usable; a minor bump marks new features, a patch bump fixes, and a release
tag `vX.Y.Z` matches it. `cargo xtask package --flavor debug` (or `beta`) builds
`io.github.maclucky.webosiptv.debug`, which installs beside the regular app with its own title.

## Credits

This project is built on [StremioNative](https://github.com/Morpheus235/stremio-native), which is
built on [PlxNative](https://github.com/GLinnik21/plx-native). Their renderer, UI toolkit, network
transport, Starfish playback engine, FFmpeg demuxer and HLS client are the foundation of this
player. Thanks to their authors and contributors. Details: [NOTICE](NOTICE) and
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
