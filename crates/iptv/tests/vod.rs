//! VOD end to end on synthetic playlists: each shape a playlist marks movies and
//! episodes with is told apart from its live channels by the parser and the classifier, and a
//! refresh stores them apart, keeping the live channels as they were.

use iptv::m3u;
use iptv::model::Timestamp;
use iptv::refresh::{Store, Stored};
use iptv::storage::{RefreshStore, Storage};
use iptv::vod::{self, VodItem, VodKind};

const NOW: Timestamp = 1_780_000_000;

fn now() -> Timestamp {
    NOW
}

/// The VOD a playlist's text gives, and the names of the live channels left.
fn split(text: &str) -> (Vec<VodItem>, Vec<String>) {
    let (mut playlist, _) = m3u::parse(text.as_bytes()).unwrap();
    let vod = vod::split(&mut playlist.channels);
    let live = playlist.channels.into_iter().map(|c| c.name).collect();
    (vod, live)
}

fn kinds(vod: &[VodItem]) -> Vec<(VodKind, String)> {
    vod.iter().map(|v| (v.kind, v.label())).collect()
}

#[test]
fn an_xtream_style_export_splits_by_path() {
    let (vod, live) = split(
        "#EXTM3U\n\
         #EXTINF:-1 tvg-id=\"news\" group-title=\"News\",News HD\n\
         http://xc.example:8080/live/user/pass/1.ts\n\
         #EXTINF:-1 tvg-logo=\"http://xc.example/p/2.jpg\" group-title=\"Filmy\",Heat (1995)\n\
         http://xc.example:8080/movie/user/pass/2.mkv\n\
         #EXTINF:-1 group-title=\"Seriale\",Ranczo S01 E02\n\
         http://xc.example:8080/series/user/pass/3.mp4\n",
    );
    assert_eq!(live, ["News HD"]);
    assert_eq!(
        kinds(&vod),
        [
            (VodKind::Movie, "Heat".to_owned()),
            (VodKind::Episode, "S01 E02".to_owned())
        ]
    );
    assert_eq!(vod[0].poster.as_deref(), Some("http://xc.example/p/2.jpg"));
    assert_eq!(vod[1].series.as_deref(), Some("Ranczo"));
}

#[test]
fn types_extensions_groups_and_episode_names_each_mark_vod() {
    let (vod, live) = split(
        "#EXTM3U\n\
         #EXTINF:-1 tvg-type=\"movie\",Typed movie\n\
         http://cdn.example/watch?id=1\n\
         #EXTINF:-1 tvg-type=\"series\",Typed S02E03 Finale\n\
         http://cdn.example/watch?id=2\n\
         #EXTINF:-1 group-title=\"Kino\",File movie (2001)\n\
         http://cdn.example/files/film.avi\n\
         #EXTINF:-1 group-title=\"PL | VOD\",Playlist movie\n\
         http://cdn.example/hls/movie/index.m3u8\n\
         #EXTINF:-1 group-title=\"TV Shows\",Show S01 E01 Pilot\n\
         http://cdn.example/hls/show/1.m3u8\n\
         #EXTINF:-1 group-title=\"Movies\",Film Channel\n\
         http://cdn.example/live/films.m3u8\n\
         #EXTINF:-1 tvg-type=\"live\" group-title=\"VOD\",Live despite the group\n\
         http://cdn.example/live/x.mp4\n",
    );
    assert_eq!(live, ["Film Channel", "Live despite the group"]);
    assert_eq!(
        kinds(&vod),
        [
            (VodKind::Movie, "Typed movie".to_owned()),
            (VodKind::Episode, "S02 E03 Finale".to_owned()),
            (VodKind::Movie, "File movie".to_owned()),
            (VodKind::Movie, "Playlist movie".to_owned()),
            (VodKind::Episode, "S01 E01 Pilot".to_owned()),
        ]
    );
    assert_eq!(vod[2].year, Some(2001));
}

#[test]
fn an_archive_keeps_a_file_live_but_not_an_xtream_vod_path() {
    let (vod, live) = split(
        "#EXTM3U catchup=\"shift\" catchup-days=\"7\"\n\
         #EXTINF:-1,Channel with archive\n\
         http://tv.example/ch1/mono.m3u8\n\
         #EXTINF:-1,Recorded loop\n\
         http://tv.example/loop.mp4\n\
         #EXTINF:-1,Movie\n\
         http://tv.example/movie/u/p/9.mkv\n",
    );
    assert_eq!(live, ["Channel with archive", "Recorded loop"]);
    assert_eq!(kinds(&vod), [(VodKind::Movie, "Movie".to_owned())]);
}

#[test]
fn a_refresh_stores_vod_apart_from_the_channels() {
    let storage = Storage::open_in_memory().unwrap();
    let id = storage
        .add_playlist("Main", "http://playlist.example/list.m3u")
        .unwrap();
    let mut store = RefreshStore::new(storage).with_clock(now);
    let text = "#EXTM3U\n\
                #EXTINF:-1 group-title=\"News\",News\n\
                http://tv.example/news.m3u8\n\
                #EXTINF:-1 group-title=\"VOD\",Film\n\
                http://tv.example/film.mkv\n";
    let parse = || m3u::parse(text.as_bytes()).unwrap().0;
    assert!(matches!(
        store.save_playlist(id, parse()).unwrap(),
        Stored::New { .. }
    ));
    let names: Vec<_> = store
        .storage()
        .channels(id)
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, ["News"]);
    let vod = store.storage().vod_items(id).unwrap();
    assert_eq!(kinds(&vod), [(VodKind::Movie, "Film".to_owned())]);
    // The same download again changes nothing.
    assert_eq!(store.save_playlist(id, parse()).unwrap(), Stored::Same);
    // Only the VOD changed: still new data for the screens.
    let more =
        format!("{text}#EXTINF:-1 group-title=\"VOD\",Second\nhttp://tv.example/second.mp4\n");
    let (playlist, _) = m3u::parse(more.as_bytes()).unwrap();
    assert!(matches!(
        store.save_playlist(id, playlist).unwrap(),
        Stored::New { .. }
    ));
    assert_eq!(store.storage().vod_items(id).unwrap().len(), 2);
}
