# webos-iptv-rs

A native IPTV player for LG webOS TVs, written in Rust. Planning stage: there is no code yet.

The goal is a small player for M3U playlists with an XMLTV guide and catch-up, built as a
native webOS app instead of a web app. Catch-up follows the M3U conventions of Kodi's
pvr.iptvsimple (`catchup`, `catchup-source`, `catchup-days`, and SIPTV `timeshift`).

Planned first scope:

- one M3U playlist, groups, and an XMLTV guide
- live playback through the TV's own media pipeline
- catch-up from the guide (shift, Flussonic and template sources)

Out of scope at first: video on demand catalogs, DRM, parental controls.

Native webOS apps are built with the community
[webosbrew native toolchain](https://github.com/webosbrew/native-toolchain) and installed through
the Homebrew Channel or developer mode. LG does not distribute native third-party apps.
