//! The refresher's `Store` on top of storage.

use std::io::{self, Read};

use iptv::epg::Matcher;
use iptv::model::{Channel, ChannelKey, Playlist, Timestamp};
use iptv::refresh::{Store, Stored, Validators};
use iptv::storage::{RefreshStore, Storage};

const NOW: Timestamp = 1_780_000_000;
const GUIDE: &str = "http://guide.example/epg.xml";
const OTHER: &str = "http://guide.example/other.xml";
const PLAYLIST: &str = "http://playlist.example/list.m3u";

fn now() -> Timestamp {
    NOW
}

fn channel(name: &str, tvg_id: &str) -> Channel {
    let url = format!("http://stream.example/{name}");
    Channel {
        key: ChannelKey::of_url(&url),
        name: name.into(),
        url,
        tvg_id: (!tvg_id.is_empty()).then(|| tvg_id.into()),
        ..Default::default()
    }
}

fn playlist(channels: Vec<Channel>, guides: &[&str]) -> Playlist {
    Playlist {
        name: None,
        channels,
        epg_urls: guides.iter().map(|g| g.to_string()).collect(),
    }
}

fn store() -> RefreshStore {
    let storage = Storage::open_in_memory().unwrap();
    let id = storage.add_playlist("Main", PLAYLIST).unwrap();
    assert_eq!(id, 1);
    RefreshStore::new(storage).with_clock(now)
}

fn programme(channel: &str, title: &str) -> String {
    format!(
        r#"<programme channel="{channel}" start="20260528200000 +0000" stop="20260528210000 +0000"><title>{title}</title></programme>"#
    )
}

fn etag(value: &str) -> Validators {
    Validators {
        etag: Some(value.into()),
        last_modified: None,
    }
}

/// The titles on air at NOW for each playlist channel.
fn on_air(store: &RefreshStore) -> Vec<Option<String>> {
    let storage = store.storage();
    let id = storage.playlists().unwrap()[0].id;
    let channels = storage.channels(id).unwrap();
    let sources = storage.epg_sources().unwrap();
    let matcher = Matcher::new(&sources);
    channels
        .iter()
        .map(|c| {
            let found = matcher.find(id, c, None)?;
            storage.now_next(&found, NOW).unwrap().0.map(|p| p.title)
        })
        .collect()
}

#[test]
fn playlist_validators_round_trip_and_empty_ones_are_forgotten() {
    let mut store = store();
    assert_eq!(store.validators(PLAYLIST), Validators::default());
    let saved = Validators {
        etag: Some("\"pl1\"".into()),
        last_modified: Some("Wed, 21 Oct 2026 07:28:00 GMT".into()),
    };
    store.save_validators(PLAYLIST, &saved);
    assert_eq!(store.validators(PLAYLIST), saved);
    store.save_validators(PLAYLIST, &Validators::default());
    assert_eq!(store.validators(PLAYLIST), Validators::default());
}

#[test]
fn guide_urls_follow_the_playlist_header() {
    let mut store = store();
    assert!(store.guide_urls().is_empty());
    store
        .save_playlist(1, playlist(vec![channel("Alpha", "a")], &[OTHER, GUIDE]))
        .unwrap();
    assert_eq!(store.guide_urls(), [OTHER, GUIDE]);
    store
        .save_playlist(1, playlist(vec![channel("Alpha", "a")], &[GUIDE]))
        .unwrap();
    assert_eq!(store.guide_urls(), [GUIDE]);
}

#[test]
fn a_guide_two_playlists_name_is_fetched_once_with_both_filters() {
    let mut store = store();
    let other = store
        .storage()
        .add_playlist("Other", "http://playlist.example/2.m3u")
        .unwrap();
    store
        .save_playlist(1, playlist(vec![channel("Alpha", "a")], &[GUIDE]))
        .unwrap();
    store
        .save_playlist(
            other,
            playlist(vec![channel("Bravo", "b")], &[OTHER, GUIDE]),
        )
        .unwrap();
    assert_eq!(
        store.guide_urls(),
        [GUIDE, OTHER],
        "playlist order, each once"
    );
    let xml = format!(
        r#"<tv>{}{}{}</tv>"#,
        programme("a", "A now"),
        programme("b", "B now"),
        programme("z", "Z now")
    );
    store.import_guide(GUIDE, &mut xml.as_bytes()).unwrap();
    let storage = store.storage();
    let sources = storage.epg_sources().unwrap();
    let matcher = Matcher::new(&sources);
    let titles = |id| -> Vec<Option<String>> {
        storage
            .channels(id)
            .unwrap()
            .iter()
            .map(|c| {
                let found = matcher.find(id, c, None)?;
                storage.now_next(&found, NOW).unwrap().0.map(|p| p.title)
            })
            .collect()
    };
    assert_eq!(titles(1), [Some("A now".to_owned())]);
    assert_eq!(titles(other), [Some("B now".to_owned())], "both filters");
    // A disabled playlist is not fetched for: its guides go, and a guide only it uses is not
    // read.
    store.storage().set_playlist_enabled(other, false).unwrap();
    assert_eq!(store.guide_urls(), [GUIDE]);
    let other_xml = format!("<tv>{}</tv>", programme("b", "B other"));
    assert_eq!(
        store
            .import_guide(OTHER, &mut other_xml.as_bytes())
            .unwrap(),
        Stored::Same
    );
}

#[test]
fn a_guide_import_keeps_only_the_playlist_channels() {
    let mut store = store();
    store
        .save_playlist(
            1,
            playlist(vec![channel("Alpha", "a"), channel("Bravo", "")], &[GUIDE]),
        )
        .unwrap();
    let xml = format!(
        r#"<tv><channel id="a"><display-name>Alpha</display-name></channel><channel id="b"><display-name>Bravo</display-name></channel><channel id="z"><display-name>Zulu</display-name></channel>{}{}{}</tv>"#,
        programme("a", "A now"),
        programme("b", "B now"),
        programme("z", "Z now")
    );
    store.import_guide(GUIDE, &mut xml.as_bytes()).unwrap();
    assert_eq!(on_air(&store), [Some("A now".into()), Some("B now".into())]);
    let catalog = &store.storage().epg_sources().unwrap()[0].channels;
    let kept: Vec<_> = catalog
        .iter()
        .map(|c| (c.epg.id.as_str(), c.has_programmes))
        .collect();
    assert_eq!(kept, [("a", true), ("b", true), ("z", false)]);
}

#[test]
fn guide_validators_lapse_when_the_channel_filter_changes() {
    let mut store = store();
    store
        .save_playlist(1, playlist(vec![channel("Alpha", "a")], &[GUIDE]))
        .unwrap();
    store
        .import_guide(GUIDE, &mut "<tv></tv>".as_bytes())
        .unwrap();
    store.save_validators(GUIDE, &etag("\"g1\""));
    assert_eq!(store.validators(GUIDE), etag("\"g1\""));
    // The same playlist again: not rewritten, and the filter is still current.
    assert_eq!(
        store.save_playlist(1, playlist(vec![channel("Alpha", "a")], &[GUIDE])),
        Ok(Stored::Same)
    );
    assert_eq!(store.validators(GUIDE), etag("\"g1\""));
    store
        .save_playlist(
            1,
            playlist(vec![channel("Alpha", "a"), channel("Bravo", "b")], &[GUIDE]),
        )
        .unwrap();
    assert_eq!(store.validators(GUIDE), Validators::default());
}

#[test]
fn programmes_before_their_channel_heal_on_the_next_full_download() {
    let mut store = store();
    store
        .save_playlist(1, playlist(vec![channel("Alpha", "")], &[GUIDE]))
        .unwrap();
    let xml = format!(
        r#"<tv>{}<channel id="late"><display-name>Alpha</display-name></channel></tv>"#,
        programme("late", "Late")
    );
    // That import asks for a second pass: no conditional GET next time.
    let second_pass = Stored::New {
        cache_validators: false,
    };
    assert_eq!(
        store.import_guide(GUIDE, &mut xml.as_bytes()),
        Ok(second_pass)
    );
    assert_eq!(on_air(&store), [None]);
    let healed = Stored::New {
        cache_validators: true,
    };
    assert_eq!(store.import_guide(GUIDE, &mut xml.as_bytes()), Ok(healed));
    assert_eq!(on_air(&store), [Some("Late".into())]);
}

#[test]
fn an_unknown_guide_becomes_a_manual_guide_for_every_playlist() {
    let mut store = store();
    store
        .save_playlist(1, playlist(vec![channel("Alpha", "a")], &[]))
        .unwrap();
    let xml = format!("<tv>{}</tv>", programme("a", "Extra"));
    store.import_guide(OTHER, &mut xml.as_bytes()).unwrap();
    let sources = store.storage().guide_sources().unwrap();
    assert_eq!((sources[0].url.as_str(), sources[0].manual), (OTHER, true));
    assert_eq!(on_air(&store), [Some("Extra".into())]);
}

/// A body that fails after `ok` bytes, like a download cut by a cap or a timeout.
struct Failing<'a> {
    data: &'a [u8],
    ok: usize,
}

impl Read for Failing<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.ok == 0 {
            return Err(io::Error::other("cut"));
        }
        let n = buf.len().min(self.ok).min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        self.ok -= n;
        Ok(n)
    }
}

#[test]
fn a_cut_download_fails_and_keeps_the_previous_guide() {
    let mut store = store();
    store
        .save_playlist(1, playlist(vec![channel("Alpha", "a")], &[GUIDE]))
        .unwrap();
    let good = format!("<tv>{}</tv>", programme("a", "Good"));
    store.import_guide(GUIDE, &mut good.as_bytes()).unwrap();
    let next = format!(
        "<tv>{}{}</tv>",
        programme("a", "Broken"),
        programme("a", "More")
    );
    let mut cut = Failing {
        data: next.as_bytes(),
        ok: 40,
    };
    let error = store.import_guide(GUIDE, &mut cut).unwrap_err();
    assert!(error.to_string().contains("cut"), "{error}");
    assert_eq!(on_air(&store), [Some("Good".into())]);
}

#[test]
fn a_guide_serving_no_channel_is_not_read() {
    let mut store = store();
    store.save_playlist(1, playlist(vec![], &[GUIDE])).unwrap();
    let mut never = Failing { data: b"", ok: 0 };
    assert_eq!(store.import_guide(GUIDE, &mut never), Ok(Stored::Same));
}

#[test]
fn a_cut_or_empty_guide_keeps_the_stored_one() {
    let mut store = store();
    store
        .save_playlist(1, playlist(vec![channel("Alpha", "a")], &[GUIDE]))
        .unwrap();
    let good = format!("<tv>{}</tv>", programme("a", "Good"));
    store.import_guide(GUIDE, &mut good.as_bytes()).unwrap();
    let cut = format!("<tv>{}", programme("a", "Cut"));
    assert_eq!(
        store.import_guide(GUIDE, &mut cut.as_bytes()),
        Ok(Stored::Kept("the guide ended early"))
    );
    assert_eq!(
        store.import_guide(GUIDE, &mut "<tv></tv>".as_bytes()),
        Ok(Stored::Kept("the guide has no programmes"))
    );
    assert_eq!(on_air(&store), [Some("Good".into())]);
}
