# Third-party notices

Paths are relative to the repository. The package installs this file, `NOTICE`, `LICENSE` and
`licenses/` at the same paths in the app directory, and the files under `pkg/` at its top level.

## Code this project is derived from

| Component | Source | Licence |
|---|---|---|
| Layer crates `base`, `machine`, `net`, `platform`, `gfx`, `ui`, `media`, `storage`, `build_support` | StremioNative, https://github.com/Morpheus235/stremio-native, commit `008c6ac` | GPL-3.0-or-later |
| The same crates, upstream of StremioNative | PlxNative, https://github.com/GLinnik21/plx-native | GPL-3.0-or-later |
| `crates/starfish`, a Rust port of StremioNative's `native/starfish.c` (the LG Starfish, ACB and exported-window seam) | StremioNative, commit `008c6ac`, derived from PlxNative's `src/starfish.c` | GPL-3.0-or-later |
| Parts of `crates/iptv` (M3U and XMLTV parsing, guide matching, channel identity, storage rules) and of its tests, and Polish UI strings in `locales/pl`, as listed under [webOS IPTV Player](#webos-iptv-player) | webOS IPTV Player, Copyright 2026 lennylxx, https://github.com/lennylxx/webos-iptv-player, commit `7d8fd30` | Apache-2.0 (`licenses/Apache-2.0.txt`) |

Crate names were changed from `sn_*` to `tv_*`, the `SNATIVE_*` and `SN_*` environment variables
to `WEBOSIPTV_*`, the remaining `sn_`/`SN_`/`sn-` symbol, shader and file-name prefixes to
`wi_`/`WI_`/`wi-`, and product names and app ids were replaced. The upstream C sources (boot
shim, Starfish seam, nanosvg rasterizer, software audio decoder) were not imported: the Starfish
seam is ported to Rust in `crates/starfish`, the audio decoder in `crates/media/src/ff_adec.rs`,
the boot shim in `crates/app`, and the SVG rasterizer is now resvg (`crates/gfx/src/svg.rs`).

## webOS IPTV Player

Ported to Rust from webOS IPTV Player at commit `7d8fd30` and changed for this project. Only what
is listed here comes from it; the rest of these files is this project's own.

- `crates/iptv/src/m3u.rs`, from `src/parsers/m3u-parser.ts`: the `#EXTINF` attribute scanner,
  the entry mapping, the directive switch, the guide URL keys, the name of a bare URL and the list
  of known attributes are translated. Line decoding (BOM, UTF-16), group, option and HTTP header
  handling, format detection and `tvg-shift` follow its rules in new code.
- `crates/iptv/src/model.rs`, from `src/utils/channel.ts`: the stable stream URL, its list of
  volatile query parameters and the channel key, with the hash widened to 64 bits.
- `crates/iptv/src/xmltv.rs`, from `src/parsers/xmltv-parser.ts` and `xmltv-loader.ts`: the
  timestamp parser and the channel and programme checks (filter, time window, budget) with their
  statistics are translated. Element and entity reading, display-name retention, the
  programme-before-channel retry, the source name and gzip and UTF-8 decoding follow its rules in
  a new quick-xml reader.
- `crates/iptv/src/epg.rs`, from `src/services/epg-service.ts`: the match order
  (`findBaseChannelId`), source applicability (`sourceAppliesToChannel`) and the parse filter
  (`filterFor`) are translated; channel offsets follow `applyChannelOffset` with a different
  design.
- `crates/iptv/src/storage.rs`, from `src/services/storage-service.ts`: saving catch-up history
  follows `setCatchupProgress` closely; favourites, recent channels and history reads follow its
  rules. The SQLite schema, settings, guide import, search and size limits are new.
- `crates/iptv/src/catchup.rs`: the `{channel-id}` substitution follows `src/components/player.ts`.
  The rest of that file, `crates/iptv/tests/catchup_url.rs` and the SIPTV and header catch-up
  modes in `m3u.rs` come from a catch-up patch written for this project and offered to webOS IPTV
  Player as https://github.com/lennylxx/webos-iptv-player/pull/87; they are not in `7d8fd30`.
- Tests, as each file's header or comments mark them: 36 cases in `crates/iptv/tests/m3u_parser.rs`
  (from `m3u-parser.test.ts`), the first 23 in `xmltv_parser.rs` (`xmltv-parser.test.ts`), 12 in
  `epg_matching.rs` (`epg-service.test.ts`), 14 in `storage_user.rs` and two unit tests in
  `storage.rs` (`storage-service.test.ts`), and five unit tests in `model.rs` (`channel.test.ts`).
- `locales/pl`, from `src/i18n/pl.ts`: 48 of the 377 Polish strings are reused as they are and 16
  more with small edits; some key names follow its message keys.

## Assets

- **Fonts** `pkg/appfont.ttf`, `pkg/appfont-bold.ttf`: Inter, Copyright 2020 The Inter Project
  Authors, SIL Open Font License 1.1 (`pkg/OFL.txt`).
- **Icons** under `assets/icons`, as credited by PlxNative:
  - `user.svg`: Google Material Design Icons, Copyright Google LLC, Apache License 2.0
    (`licenses/Apache-2.0.txt`); modified (bounding-box path removed, white fill).
  - `backspace.svg`: Feather Icons, Copyright (c) 2013-2023 Cole Bemis, MIT (`licenses/MIT.txt`);
    modified (stroke width, white stroke). `chevron.svg`, `chevron-down.svg` and `chevron-up.svg`
    share Feather's vertex coordinates and are credited under the same notice.
  - `check.svg`: Heroicons, Copyright (c) Tailwind Labs, Inc., MIT (`licenses/MIT.txt`); modified
    (stroke width, white stroke).
  - The other icons are original work of the PlxNative and StremioNative authors (GPL-3.0-or-later).
- **CA roots** `pkg/le-roots.pem`: Let's Encrypt (ISRG) root certificates, public.

## Libraries shipped in the package

- **FFmpeg 9.0.2** (libavutil, libavcodec, libavformat, libswresample), Copyright (C) 2000-2026
  the FFmpeg developers, LGPL-2.1-or-later (`licenses/LGPL-2.1.txt`), built without any GPL or
  nonfree component from the unmodified upstream source. That source is attached to every release
  as `ffmpeg-9.0.2.tar.xz`, and the complete configure line is in `licenses/ffmpeg.txt` inside the
  package.
- **Rust code**: the Rust standard library (MIT OR Apache-2.0, with Unicode-3.0 tables in `core`
  and Apache-2.0 WITH LLVM-exception compiler-rt code in `compiler_builtins`) and every
  third-party crate linked into the executable, each with its version, licence and repository,
  followed by the full licence texts, are in `licenses/rust-crates.txt` inside the package and
  attached to every release as `rust-crates.txt`. `cargo xtask package` generates it from the
  toolchain and `Cargo.lock`.
