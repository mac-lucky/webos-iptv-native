// Copyright (C) 2026 the StremioNative authors
// Copyright (C) 2026 the webos-iptv-native authors
// SPDX-License-Identifier: GPL-3.0-or-later
// Provenance: this module and everything under hls/ is StremioNative's HLS client
// (crates/media/src/hls, commit 008c6ac, GPL-3.0-or-later), carried over with its tests and
// changed here for IPTV playlists: MPEG audio variants, a segment size cap, the no-archive stub.

//! **HLS (RFC 8216) in Rust**, for the streams the bundled FFmpeg cannot open itself: it has no
//! `hls` demuxer and no network protocols (`--disable-network`; its demuxers are matroska, mov,
//! mpegts, h264, hevc, avi, asf, mpegps and rm). Live-TV addons answer with exactly this - an `.m3u8` master or media
//! playlist of MPEG-TS segments (sometimes fMP4 with an `EXT-X-MAP`, sometimes AES-128), behind
//! request headers the origin insists on for every playlist, key and segment.
//!
//! So the playlist work happens here and the demuxer never learns there was a playlist: the
//! [`source::HlsSource`] resolves the master playlist, picks the variant this television can
//! decode ([`select`], against `tv_platform::devcaps`), follows the media playlist (live reload
//! and window per RFC 8216 §6.3, in [`live`]), fetches the segments one after another through the
//! curl media plane with the stream's [`RequestHeaders`](crate::headers::RequestHeaders),
//! decrypts them ([`aes`]), and hands `ff::demux` ONE continuous byte stream - the initialization
//! section first for fMP4 - through the same AVIO read seam the progressive curl source uses.
//! libavformat then probes mpegts or mov from the bytes.
//!
//! - [`playlist`]: the parser (master and media playlists, every URI resolved absolute).
//! - [`select`]: variant selection against the device's codec table.
//! - [`live`]: the live window and reload arithmetic, pure.
//! - [`aes`]: AES-128-CBC/PKCS#7, verified against FIPS-197 and SP 800-38A.
//! - [`source`]: the client, over an injectable transport and clock.
//! - [`timeline`]: one timeline across the stream's discontinuities, for the demuxer, pure.
//!
//! **Seeking.** A live stream does not seek: the pump drops the request. A VOD
//! stream (a catch-up programme) seeks by segment: the pump reloads the engine at the target and
//! `ff::open_hls` opens the playlist at the segment holding it, because neither demuxer can be
//! repositioned inside a byte stream that has no byte addresses (the AVIO is unseekable on
//! purpose). Its positions are playlist time (`Shared::hls_vod_base_ns`), whatever timestamps the
//! segments carry. An archive of the programme on air ends where the archive did when it was asked
//! for; the playback has it followed past that end ([`source::HlsSource::follow_until`]).

pub mod aes;
pub mod live;
pub mod playlist;
pub mod select;
pub mod source;
pub mod timeline;

/// Also the loopback origin `ff_hls_tests.rs` serves its streams from.
#[cfg(test)]
#[path = "hls/loopback_tests.rs"]
pub(crate) mod loopback_tests;

pub use source::{CurlHls, CurlTransport, FetchError, HlsError, HlsSource, SystemClock};
pub use timeline::Timeline;

/// Does `url` name an HLS playlist by its path (`....m3u8`, case-insensitive, query and fragment
/// ignored)? The cheap test, made before anything is requested.
pub fn is_playlist_url(url: &str) -> bool {
    let (_, path) = tv_net::net::origin::split(url);
    let path = path.split(['?', '#']).next().unwrap_or("");
    path.len() >= 5 && path[path.len() - 5..].eq_ignore_ascii_case(".m3u8")
}

/// Was a playlist request for `requested` answered, after a redirect, by an archive server's
/// "no archive" stub (`.../noarchive.m3u8`, also spelled `no_archive` or `no-archive`)? Flussonic
/// answers an archive request outside the recorded window this way, with a short VOD playlist
/// and no HTTP error, so the only sign is where the redirect went. Not a redirect: never a stub.
pub fn is_no_archive_stub(requested: &str, effective: &str) -> bool {
    let path = |u: &str| -> String { tv_net::net::origin::split(u).1.split(['?', '#']).next().unwrap_or("").to_owned() };
    let (asked, answered) = (path(requested), path(effective));
    if asked == answered {
        return false;
    }
    let file = answered.rsplit('/').next().unwrap_or("");
    let stem = file.split('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(stem.as_str(), "noarchive" | "no_archive" | "no-archive")
}

/// Is `content_type` an HLS playlist's media type (`application/vnd.apple.mpegurl`,
/// `application/x-mpegurl`, and the `audio/` spellings servers also send)? Parameters ignored.
pub fn is_playlist_content_type(content_type: &str) -> bool {
    let t = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    matches!(
        t.as_str(),
        "application/vnd.apple.mpegurl" | "application/x-mpegurl" | "audio/mpegurl" | "audio/x-mpegurl"
    )
}

/// The last HLS failure of this process's playback, for the layer above to name in its read-out
/// (`player::error_now` reports it as a media-source failure; this says which). Cleared when an
/// HLS source opens.
static LAST_FAILURE: std::sync::Mutex<Option<HlsError>> = std::sync::Mutex::new(None);

pub fn last_failure() -> Option<HlsError> {
    LAST_FAILURE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub(crate) fn note_failure(e: Option<HlsError>) {
    *LAST_FAILURE.lock().unwrap_or_else(|e| e.into_inner()) = e;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_urls_and_media_types() {
        assert!(is_playlist_url("https://live.example/ch/12/index.m3u8"));
        assert!(is_playlist_url("http://live.example:8080/play/X.M3U8?token=a.b&x=.mp4"));
        assert!(!is_playlist_url("https://cdn.example/movie.mkv?list=a.m3u8"), "the query is not the path");
        assert!(!is_playlist_url("https://cdn.example/m3u8"));
        assert!(!is_playlist_url("https://cdn.example/playlist.m3u"), "a plain M3U channel list is not HLS");
        assert!(is_playlist_content_type("application/vnd.apple.mpegurl"));
        assert!(is_playlist_content_type("Application/X-MpegURL; charset=UTF-8"));
        assert!(is_playlist_content_type("audio/mpegurl"));
        assert!(!is_playlist_content_type("video/mp2t"));
        assert!(!is_playlist_content_type("text/plain"));
    }

    #[test]
    fn only_a_redirect_to_a_no_archive_playlist_is_the_stub() {
        let asked = "http://archive.example/ch1/mono-1760000000-3600.m3u8?token=t";
        assert!(is_no_archive_stub(asked, "http://stub.example/new_stub/noarchive.m3u8"));
        assert!(is_no_archive_stub(asked, "https://archive.example:443/x/No_Archive.M3U8?a=1"));
        assert!(!is_no_archive_stub(asked, "http://archive.example/ch1/mono-1760000000-3600.m3u8?token=other"), "same path: no redirect");
        assert!(!is_no_archive_stub(asked, "http://edge.example/ch1/mono-1760000000-3600.m3u8"), "a redirect to the stream itself");
        assert!(!is_no_archive_stub(asked, "http://edge.example/noarchive/index.m3u8"), "only the file name counts");
        assert!(!is_no_archive_stub("http://a.example/noarchive.m3u8", "http://a.example/noarchive.m3u8"));
    }
}
