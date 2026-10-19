//! tv_media: the media layer of the WebosIptv application core.
//!
//! Demux (`ff`, the bundled FFmpeg), the HTTPS media pull source (`curlio`), HLS (`hls`), the access-unit
//! queues (`aq`), the stream's request headers (`headers`), what a stream is and how it is fed
//! (`stream`), the seam the layer above starts a playback through (`session`), the lock-free
//! preference cells the player reads (`prefs`) and the buffer-feed engine (`player`). It uses `base`,
//! `machine`, `platform`, `net` and (for the simulator's picture only) `gfx`, and nothing above
//! them: what to play arrives as a `session::PlaybackStart`, and what happened leaves through its
//! `session::PlaybackObserver`.
//!
//! The Starfish/ACB surface is declared here and nowhere else: `sf_on_event` and `acb_on_event`
//! (`player`), which `native/starfish.c` calls, and the externs of `player::ffi`. The crate links
//! nothing itself; the final link resolves them.
//!
//! `test-support` exposes the `cfg(test)` seams the layers above test through (`player::preview`,
//! `player::restore_state_for_test`, ...). The application crate enables it in
//! `[dev-dependencies]` only, so no shipped build sees it.

pub mod aq;
pub mod curlio; // the HTTPS media plane: a remote file pulled by byte range over libcurl-multi (stream.rs is the plaintext-socket twin)
#[macro_use]
pub mod ff; // THE demuxer -- the FFmpeg 9.0 this app BUNDLES and pins (majors 63/63/61), dlopen'd by absolute path beside the binary, never the television's
pub mod hls; // HLS in Rust (the bundled FFmpeg has no hls demuxer): playlists, variant choice, live reload, AES-128, one byte stream for the demuxer
pub mod headers; // a stream's request headers (the playlist's `#EXTVLCOPT` / `#EXTHTTP`), checked once
pub mod player; // buffer-feed video engine
pub mod playback; // IPTV playback: the API the application shell drives (live, catch-up, watchdog)
pub mod prefs; // the viewer's playback preferences, as the player reads them
pub mod session; // PlaybackStart / PlaybackSession / PlaybackObserver: the seam to the layer above
pub mod stream; // the stream declaration, Dolby Vision presentation and the audio-feed policy
pub mod subtitles; // the subtitle tracks, the viewer's pick and the decoded cues waiting for their time
