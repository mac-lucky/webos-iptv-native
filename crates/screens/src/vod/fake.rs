//! Synthetic movies and episodes for tests and the simulator: made-up titles on a synthetic
//! host, built as a playlist's entries would be ([`iptv::vod::item`]).

use iptv::model::{Channel, ChannelKey};
use iptv::vod::{self, VodItem, VodKind};

/// Movies, then episodes of three shows, under `base` (`http://vod.example` when it is empty):
/// every movie is `base/movie.mp4` and every episode `base/vod.m3u8` (a progressive file and a
/// VOD playlist), told apart by an `item` parameter, so two test files play them all.
pub fn items(base: &str) -> Vec<VodItem> {
    let base = match base.trim_end_matches('/') {
        "" => "http://vod.example",
        b => b,
    };
    let movies = [
        ("Northern Lights (2019)", "Drama"),
        ("Harbour City (2011)", "Action"),
        ("Quiet Hours (2021)", "Drama"),
        ("Paper Moons (1998)", "Comedy"),
        ("Iron Valley (2015)", "Action"),
        ("Summer Rain (2008)", "Romance"),
        ("Last Train Home (2013)", "Drama"),
        ("The Long Road (2004)", "Comedy"),
    ];
    let episodes = [
        ("Harbour Watch S01 E01 Pilot", "Crime"),
        ("Harbour Watch S01 E02 Low Tide", "Crime"),
        ("Harbour Watch S01 E03 Fog", "Crime"),
        ("Harbour Watch S02 E01 Return", "Crime"),
        ("Harbour Watch S02 E02 Undertow", "Crime"),
        ("The Archive S01 E01", "Documentary"),
        ("The Archive S01 E02", "Documentary"),
        ("The Archive S01 E03", "Documentary"),
        ("Kitchen Stories S03 E07 Bread", "Lifestyle"),
        ("Kitchen Stories S03 E08 Salt", "Lifestyle"),
    ];
    let entry = |i: usize, name: &str, group: &str, file: &str| {
        let url = format!("{base}/{file}?item={i}");
        Channel {
            key: ChannelKey::of_url(&url),
            name: name.to_owned(),
            groups: vec![group.to_owned()],
            url,
            ..Channel::default()
        }
    };
    let mut out: Vec<VodItem> = movies
        .iter()
        .enumerate()
        .map(|(i, (name, group))| vod::item(entry(i, name, group, "movie.mp4"), VodKind::Movie))
        .collect();
    out.extend(episodes.iter().enumerate().map(|(i, (name, group))| {
        vod::item(entry(100 + i, name, group, "vod.m3u8"), VodKind::Episode)
    }));
    out
}
