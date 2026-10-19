use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use iptv::model::{Channel, Playlist};
use iptv::refresh::{NetError, Response, Transport};
use tv_screens::data::TvData;

use super::*;
use crate::store::{make_dir, open_at, Channels};

fn channel(name: &str, group: &str, number: u32) -> Channel {
    let url = format!("http://host.test/{name}/index.m3u8");
    Channel {
        key: ChannelKey::of_url(&url),
        name: name.into(),
        number: Some(number),
        groups: vec![group.into()],
        url,
        ..Channel::default()
    }
}

/// Playlist 1 with Beta and Gamma in News and alpha in Sport.
fn store() -> StoreData {
    let storage = Storage::open_in_memory().unwrap();
    storage
        .set_main_playlist("http://host.test/list.m3u")
        .unwrap();
    let playlist = Playlist {
        channels: vec![
            channel("Beta", "News", 3),
            channel("alpha", "Sport", 1),
            channel("Gamma", "News", 2),
        ],
        ..Playlist::default()
    };
    storage
        .save_playlist(MAIN_PLAYLIST, &playlist, 1_700_000_000)
        .unwrap();
    StoreData::new(storage)
}

fn names(list: Channels) -> Vec<String> {
    list.iter().map(|c| c.name.clone()).collect()
}

fn key(s: &StoreData, name: &str) -> ChannelKey {
    s.channels(&Group::All)
        .iter()
        .find(|c| c.name == name)
        .unwrap()
        .key
}

fn custom(name: &str) -> Group {
    Group::Custom(name.into())
}

fn playlist(name: &str) -> Group {
    Group::Playlist(name.into())
}

const BUILT_IN: [Group; 3] = [Group::Favorites, Group::All, Group::History];

#[test]
fn custom_groups_follow_the_built_in_ones_in_the_viewers_order() {
    let mut s = store();
    s.edit_library(LibraryEdit::CreateGroup("Mine".into()));
    assert_eq!(
        s.groups()[3..],
        [custom("Mine"), playlist("News"), playlist("Sport")],
        "a new custom group leads the playlist groups"
    );
    let (beta, gamma) = (key(&s, "Beta"), key(&s, "Gamma"));
    s.edit_library(LibraryEdit::AddToGroup {
        to: "Mine".into(),
        channels: vec![gamma, beta],
        from: None,
    });
    assert_eq!(names(s.channels(&custom("Mine"))), ["Gamma", "Beta"]);
    assert_eq!(
        names(s.channels(&playlist("News"))),
        ["Beta", "Gamma"],
        "copied: still in News"
    );
    s.edit_library(LibraryEdit::SetGroupOrder(vec![
        playlist("Sport"),
        custom("Mine"),
        playlist("News"),
    ]));
    assert_eq!(
        s.groups()[3..],
        [playlist("Sport"), custom("Mine"), playlist("News")]
    );
    // Hidden groups leave the groups panel but not Manage groups.
    let mut settings = s.settings();
    settings.set(keys::GROUPS_HIDDEN, "Sport".into());
    settings.set(keys::GROUPS_SHOW_HISTORY, "false".into());
    s.save_setting(keys::GROUPS_HIDDEN, &settings);
    s.save_setting(keys::GROUPS_SHOW_HISTORY, &settings);
    assert_eq!(
        s.groups(),
        [
            Group::Favorites,
            Group::All,
            custom("Mine"),
            playlist("News")
        ]
    );
    assert_eq!(s.all_groups()[..3], BUILT_IN);
    assert_eq!(
        s.all_groups()[3..],
        [playlist("Sport"), custom("Mine"), playlist("News")]
    );
    s.edit_library(LibraryEdit::RenameGroup {
        from: "Mine".into(),
        to: "Ours".into(),
    });
    assert_eq!(names(s.channels(&custom("Ours"))), ["Gamma", "Beta"]);
    s.edit_library(LibraryEdit::RemoveFromGroup {
        group: "Ours".into(),
        channels: vec![gamma],
    });
    assert_eq!(names(s.channels(&custom("Ours"))), ["Beta"]);
    s.edit_library(LibraryEdit::DeleteGroup("Ours".into()));
    assert_eq!(s.all_groups()[3..], [playlist("Sport"), playlist("News")]);
}

#[test]
fn a_moved_channel_leaves_its_playlist_group_and_an_emptied_group_goes() {
    let mut s = store();
    s.edit_library(LibraryEdit::CreateGroup("Mine".into()));
    let alpha = key(&s, "alpha");
    s.edit_library(LibraryEdit::AddToGroup {
        to: "Mine".into(),
        channels: vec![alpha],
        from: Some(playlist("Sport")),
    });
    assert_eq!(s.groups()[3..], [custom("Mine"), playlist("News")]);
    assert_eq!(names(s.channels(&playlist("Sport"))), Vec::<String>::new());
    assert!(names(s.channels(&Group::All)).contains(&"alpha".to_owned()));
    // Out of the custom group: back in Sport.
    s.edit_library(LibraryEdit::DeleteGroup("Mine".into()));
    assert_eq!(names(s.channels(&playlist("Sport"))), ["alpha"]);
}

#[test]
fn a_playlist_groups_own_order_shows_under_the_playlist_sorting() {
    let mut s = store();
    let (beta, gamma) = (key(&s, "Beta"), key(&s, "Gamma"));
    s.edit_library(LibraryEdit::SetChannelOrder {
        group: playlist("News"),
        channels: vec![gamma, beta],
    });
    assert_eq!(names(s.channels(&playlist("News"))), ["Gamma", "Beta"]);
    let mut settings = s.settings();
    settings.set(keys::GROUPS_CHANNEL_SORT, "name".into());
    s.save_setting(keys::GROUPS_CHANNEL_SORT, &settings);
    assert_eq!(names(s.channels(&playlist("News"))), ["Beta", "Gamma"]);
}

#[test]
fn playlists_are_added_enabled_sorted_and_removed() {
    let mut s = store();
    // Built, not written out: a token-looking literal trips the secret scanners.
    let token = ["q9w8", "e7r6", "t5y4"].concat();
    let second = format!("http://b.example/pl/{token}/list.m3u");
    s.edit_library(LibraryEdit::AddPlaylist(SecretUrl(second.clone())));
    let rows: Vec<(i64, String, bool)> = s
        .playlists()
        .into_iter()
        .map(|p| (p.id, p.name, p.enabled))
        .collect();
    assert_eq!(
        rows,
        [
            (1, "Playlist".to_owned(), true),
            (2, "b.example".to_owned(), true)
        ]
    );
    assert_eq!(s.playlists()[0].channels, 3);
    assert!(
        !redact::text(&second).contains(&token),
        "masked from now on"
    );
    let fetched = |s: &StoreData| -> Vec<PlaylistId> {
        s.refresh_config()
            .map(|c| c.playlists.iter().map(|p| p.0).collect())
            .unwrap_or_default()
    };
    assert_eq!(fetched(&s), [1, 2]);
    s.edit_library(LibraryEdit::SetPlaylistEnabled {
        playlist: 1,
        enabled: false,
    });
    assert_eq!(fetched(&s), [2], "a disabled playlist is not fetched");
    assert!(s.channels(&Group::All).is_empty(), "nor shown");
    s.edit_library(LibraryEdit::SetPlaylistEnabled {
        playlist: 1,
        enabled: true,
    });
    assert_eq!(s.channels(&Group::All).len(), 3, "its channels were kept");
    let before = s.refresh_config();
    s.edit_library(LibraryEdit::MovePlaylist {
        playlist: 2,
        up: true,
    });
    assert_eq!(s.playlists()[0].id, 2, "moved up");
    assert!(
        s.refresh_config() == before,
        "the same fetch: a refresh under way goes on"
    );
    let new = "http://b.example/other.m3u";
    s.edit_library(LibraryEdit::SetPlaylistUrl {
        playlist: 2,
        url: SecretUrl(new.into()),
    });
    assert_eq!(s.playlists()[0].url.0, new);
    s.edit_library(LibraryEdit::RemovePlaylist(2));
    assert_eq!(fetched(&s), [1]);
    assert_eq!(s.playlists().len(), 1);
}

#[test]
fn without_playlist_one_a_new_playlist_becomes_the_settings_url() {
    let mut s = StoreData::new(Storage::open_in_memory().unwrap());
    assert_eq!(s.refresh_config().map(|c| c.playlists.len()), None);
    let url = "http://a.example/list.m3u";
    s.edit_library(LibraryEdit::AddPlaylist(SecretUrl(url.into())));
    assert_eq!(s.settings().get(keys::PLAYLIST_URL), url);
    assert_eq!(s.playlists()[0].id, MAIN_PLAYLIST);
    // A Settings URL stored by a build before several playlists makes playlist 1 at start.
    let storage = Storage::open_in_memory().unwrap();
    storage.set_setting(keys::PLAYLIST_URL, url).unwrap();
    let s = StoreData::new(storage);
    assert_eq!(s.playlists()[0].id, MAIN_PLAYLIST);
    assert_eq!(s.playlists()[0].url.0, url);
}

/// Serves a two-channel playlist (News: One, Two) at any URL.
struct Same(Arc<AtomicUsize>);

impl Transport for Same {
    fn get(
        &self,
        _url: &str,
        _headers: &[String],
        _budget: std::time::Duration,
        _cancel: &Arc<AtomicBool>,
    ) -> Result<Response, NetError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let m3u = "#EXTM3U\n#EXTINF:-1 group-title=\"News\",One\nhttp://host.test/1.m3u8\n\
                   #EXTINF:-1 group-title=\"News\",Two\nhttp://host.test/2.m3u8\n";
        Ok(Response {
            status: 200,
            validators: Default::default(),
            location: None,
            body: Box::new(std::io::Cursor::new(m3u.as_bytes().to_vec())),
        })
    }
}

#[test]
fn two_playlists_refresh_into_merged_groups_and_keep_their_own_channels() {
    let dir = std::env::temp_dir().join(format!("webosiptv-library-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(make_dir(&dir));
    let mut s = open_at(&dir);
    let mut settings = s.settings();
    settings.set(keys::PLAYLIST_URL, "http://a.example/list.m3u".into());
    s.save_setting(keys::PLAYLIST_URL, &settings);
    s.edit_library(LibraryEdit::AddPlaylist(SecretUrl(
        "http://b.example/list.m3u".into(),
    )));
    let gets = Arc::new(AtomicUsize::new(0));
    let refresher = s
        .refresher(Box::new(Same(Arc::clone(&gets))), false)
        .unwrap();
    let start = std::time::Instant::now();
    while refresher.generation() == 0 && start.elapsed().as_secs() < 10 {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(refresher);
    assert_eq!(gets.load(Ordering::SeqCst), 2, "one fetch per playlist");
    s.reload();
    assert_eq!(names(s.channels(&Group::All)), ["One", "Two", "One", "Two"]);
    assert_eq!(
        s.groups()[3..],
        [playlist("News")],
        "one group for both playlists"
    );
    assert_eq!(s.channels(&playlist("News")).len(), 4);
    let all = s.channels(&Group::All);
    let keys: HashSet<ChannelKey> = all.iter().map(|c| c.key).collect();
    assert_eq!(
        keys.len(),
        4,
        "the same stream in two playlists is two channels"
    );
    let (first, second) = (all[0].key, all[2].key);
    s.toggle_favorite(second);
    assert!(s.is_favorite(second) && !s.is_favorite(first));
    assert_eq!(
        s.playlists().iter().map(|p| p.channels).collect::<Vec<_>>(),
        [2, 2]
    );
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_simulators_library_changes_in_memory() {
    let mut d = FakeData::new(1_700_000_000, 6);
    let one = d.channels[0].key;
    let two = d.channels[1].key;
    d.edit_library(LibraryEdit::CreateGroup("Kids".into()));
    d.edit_library(LibraryEdit::CreateGroup("Late".into()));
    d.edit_library(LibraryEdit::AddToGroup {
        to: "Kids".into(),
        channels: vec![two, one],
        from: None,
    });
    assert_eq!(d.channels(&custom("Kids")).len(), 2);
    d.edit_library(LibraryEdit::AddToGroup {
        to: "Late".into(),
        channels: vec![one],
        from: Some(custom("Kids")),
    });
    assert_eq!(d.custom_groups[0].1, [two]);
    assert_eq!(d.custom_groups[1].1, [one]);
    d.edit_library(LibraryEdit::SetGroupOrder(vec![
        custom("Late"),
        custom("Kids"),
    ]));
    assert_eq!(d.groups()[3..5], [custom("Late"), custom("Kids")]);
    d.edit_library(LibraryEdit::AddPlaylist(SecretUrl(
        "http://c.example/x.m3u".into(),
    )));
    d.edit_library(LibraryEdit::MovePlaylist {
        playlist: 3,
        up: true,
    });
    let ids: Vec<i64> = d.playlists().iter().map(|p| p.id).collect();
    assert_eq!(ids, [1, 3, 2]);
    d.edit_library(LibraryEdit::RemovePlaylist(3));
    d.edit_library(LibraryEdit::SetPlaylistEnabled {
        playlist: 2,
        enabled: true,
    });
    assert!(d.playlists().iter().all(|p| p.enabled));
    d.edit_library(LibraryEdit::DeleteGroup("Kids".into()));
    assert_eq!(d.custom_groups.len(), 1);
}

#[test]
fn a_new_playlist_is_named_by_its_host() {
    assert_eq!(
        name_of("http://viewer@tv.example:8080/get.php?x=1"),
        "tv.example"
    );
    assert_eq!(name_of("https://b.example"), "b.example");
    assert_eq!(name_of("http:///x"), "Playlist");
}
