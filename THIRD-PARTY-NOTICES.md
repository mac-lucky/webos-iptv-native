# Third-party notices

## Code this project is derived from

| Component | Source | Licence |
|---|---|---|
| Layer crates `base`, `machine`, `net`, `platform`, `gfx`, `ui`, `media`, `storage`, `build_support` | StremioNative, https://github.com/Morpheus235/stremio-native, commit `008c6ac` | GPL-3.0-or-later |
| The same crates, upstream of StremioNative | PlxNative, https://github.com/GLinnik21/plx-native | GPL-3.0-or-later |
| Polish UI translations in `locales/pl` reused from its message catalog (`src/i18n/pl.ts`), some modified | webos-iptv-player, Copyright 2026 lennylxx, https://github.com/lennylxx/webos-iptv-player, commit `7d8fd30` | Apache-2.0 (`licenses/Apache-2.0.txt`) |

Crate names were changed from `sn_*` to `tv_*` and product names and app ids were replaced.
The upstream C sources (boot shim, Starfish seam, nanosvg rasterizer, software audio decoder)
were not imported; their roles move to Rust. The SVG rasterizer is now resvg (`crates/gfx/src/svg.rs`).

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

The bundled FFmpeg build (LGPL-2.1-or-later, `licenses/LGPL-2.1.txt`) and the full list of Rust
crate licences are added here when packaging lands.
