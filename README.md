# webos-iptv-native

A native IPTV player for LG webOS TVs, written in Rust. It draws its interface on the TV's GPU and
hands video to the TV's hardware decoder, so it starts fast and stays responsive on TVs where web
apps struggle.

The app ships no channels or playlists. Add an M3U playlist from a provider you are allowed to use.

## What it does

- M3U playlists (several at once) with an XMLTV guide, groups, favourites, and custom or hidden
  groups
- a full-screen TV guide with a live preview, a channel list, an info panel and playback history
- catch-up from the guide, following the M3U conventions of Kodi's pvr.iptvsimple (`catchup`,
  `catchup-source`, `catchup-days`, and SIPTV `timeshift`): shift, Flussonic and template sources
- movies and series sections for playlists that carry VOD
- DVB and WebVTT subtitles, audio track selection, picture in picture
- programme reminders, channel renaming, a parental PIN, settings backup, accent colours
- English and Polish

It is developed on an LG C1 (webOS 6) and an LG C5 (webOS 25). Older TVs are untested.

## Installing

Download the `.ipk` from the [latest release](https://github.com/mac-lucky/webos-iptv-native/releases/latest)
and install it on a TV in Developer Mode or with the
[Homebrew Channel](https://github.com/webosbrew/webos-homebrew-channel), for example with
`ares-install`.

## Building

```sh
cargo check --workspace
cargo test --workspace
```

Host tests link SDL2 and SDL2_ttf (`brew install sdl2 sdl2_ttf` on macOS).

The desktop simulator runs the same app on the Mac. The keyboard stands in for the remote:
arrows, Enter = OK, Backspace = Back, PageUp/PageDown = CH+/CH-, digits, F1-F4 = colour keys.

```sh
cargo xtask sim [--keys up,ok,red] [--shot out.png]
```

## Building for the TV

Native webOS apps are built with the community
[webosbrew native toolchain](https://github.com/webosbrew/native-toolchain).

```sh
cargo xtask setup              # once: the Rust target and the webosbrew NDK (in ~/webos-ndk, or $WEBOS_SDK)
cargo xtask ffmpeg             # once per FFmpeg pin: the bundled LGPL FFmpeg
cargo xtask package            # target/ipk/io.github.maclucky.webosiptv_<version>_arm.ipk
cargo xtask verify             # webosbrew-ipk-verify for webOS 6 and newer (needs Docker)
cargo xtask release            # the release assets in target/release-assets/
```

The app id is `io.github.maclucky.webosiptv`. The package version lives in one place,
`version` in `pkg/appinfo.json`: three integers `X.Y.Z`, as webOS requires, and a release tag
`vX.Y.Z` matches it. `cargo xtask package --flavor debug` (or `beta`) builds
`io.github.maclucky.webosiptv.debug`, which installs beside the regular app with its own title.

## Layout

| Path | What it is |
|---|---|
| `crates/base`, `machine`, `net`, `platform`, `gfx`, `ui`, `media`, `storage` | platform, rendering, UI toolkit, network and player layers taken from StremioNative and PlxNative |
| `crates/starfish` | the LG Starfish, ACB and exported-window seam, a Rust port of StremioNative's `native/starfish.c` |
| `crates/iptv` | playlists, guide, catch-up URLs, storage |
| `crates/screens` | the TV screens |
| `crates/app` | boot, frame loop and wiring |
| `xtask` | build, package and release tooling (`cargo xtask <command>`) |

## Credits

This project is built on [StremioNative](https://github.com/Morpheus235/stremio-native), which is
built on [PlxNative](https://github.com/GLinnik21/plx-native). Their renderer, UI toolkit, network
transport, Starfish playback engine, FFmpeg demuxer and HLS client are the foundation of this
player. Thanks to their authors and contributors. Details: [NOTICE](NOTICE) and
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

Not affiliated with or endorsed by LG Electronics.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). The bundled FFmpeg is LGPL-2.1-or-later; its source is
attached to every release.
