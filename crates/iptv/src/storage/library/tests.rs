use rusqlite::Connection;

use super::*;
use crate::model::{Channel, Playlist};
use crate::storage::{HistoryEntry, MIGRATIONS};

const NOW: i64 = 1_700_000_000;

fn channel(name: &str, group: &str) -> Channel {
    let url = format!("http://host.test/{name}.m3u8");
    Channel {
        key: ChannelKey::of_url(&url),
        name: name.into(),
        groups: vec![group.into()],
        url,
        ..Channel::default()
    }
}

fn playlist(channels: &[(&str, &str)]) -> Playlist {
    Playlist {
        channels: channels.iter().map(|(n, g)| channel(n, g)).collect(),
        ..Playlist::default()
    }
}

fn keys(storage: &Storage, id: PlaylistId) -> Vec<ChannelKey> {
    storage
        .channels(id)
        .unwrap()
        .into_iter()
        .map(|c| c.key)
        .collect()
}

/// The schema right before this module's migration.
fn before_library() -> (Connection, usize) {
    let at = MIGRATIONS
        .iter()
        .position(|m| m.contains("CREATE TABLE custom_group ("))
        .expect("the library migration is listed");
    let conn = Connection::open_in_memory().unwrap();
    for step in &MIGRATIONS[..at] {
        conn.execute_batch(step).unwrap();
    }
    conn.pragma_update(None, "user_version", at as i64).unwrap();
    (conn, at)
}

#[test]
fn an_upgrade_keeps_the_playlist_as_number_one_with_what_was_kept_for_it() {
    let (conn, _) = before_library();
    let key = ChannelKey::of_url("http://host.test/a.m3u8");
    conn.execute_batch(&format!(
        "INSERT INTO setting (key, value) VALUES ('playlist.url', 'http://host.test/list.m3u');
         INSERT INTO playlist (id, name, url) VALUES (1, 'Playlist', 'http://host.test/list.m3u');
         INSERT INTO channel (playlist_id, position, key, name, groups, url,
         tvg_shift_minutes, catchup_days, radio, http_headers, extra)
         VALUES (1, 0, {k}, 'a', '[\"News\"]', 'http://host.test/a.m3u8', 0, 0, 0, '[]', '[]');
         INSERT INTO favourite (key) VALUES ({k});
         INSERT INTO recent (key, watched_at) VALUES ({k}, {NOW});
         INSERT INTO history (key, start, stop, title, position_secs, duration_secs, completed,
         updated_at, expires_at) VALUES ({k}, {NOW}, {end}, 't', 60, 3600, 0, {NOW}, {far});",
        k = key.0,
        end = NOW + 3600,
        far = i64::MAX,
    ))
    .unwrap();
    let storage = Storage::init(conn, "memory").unwrap();
    let playlists = storage.playlists().unwrap();
    assert_eq!(playlists.len(), 1);
    assert_eq!(
        (playlists[0].id, playlists[0].url.as_str()),
        (1, "http://host.test/list.m3u")
    );
    assert_eq!(keys(&storage, MAIN_PLAYLIST), [key], "the bare URL key");
    assert_eq!(storage.favourites().unwrap(), [key]);
    assert_eq!(storage.recent().unwrap(), [(key, NOW)]);
    assert_eq!(storage.history(NOW).unwrap().len(), 1);
    assert_eq!(storage.custom_groups().unwrap(), []);
    // The next download of the same playlist keeps the key.
    let changed = storage
        .save_playlist(MAIN_PLAYLIST, &playlist(&[("a", "News")]), NOW)
        .unwrap();
    assert!(changed);
    assert_eq!(keys(&storage, MAIN_PLAYLIST), [key]);
}

#[test]
fn an_install_that_only_set_the_url_gets_playlist_one() {
    let (conn, _) = before_library();
    conn.execute_batch(
        "INSERT INTO setting (key, value) VALUES ('playlist.url', 'http://host.test/list.m3u');",
    )
    .unwrap();
    let storage = Storage::init(conn, "memory").unwrap();
    let playlists = storage.playlists().unwrap();
    assert_eq!(playlists.len(), 1);
    assert_eq!(playlists[0].id, MAIN_PLAYLIST);
    assert!(playlists[0].enabled);
    // No URL set: no row.
    let (conn, _) = before_library();
    let storage = Storage::init(conn, "memory").unwrap();
    assert!(storage.playlists().unwrap().is_empty());
}

#[test]
fn the_main_playlist_follows_the_settings_url() {
    let s = Storage::open_in_memory().unwrap();
    s.set_main_playlist("").unwrap();
    assert!(
        s.playlists().unwrap().is_empty(),
        "nothing for an empty URL"
    );
    s.set_main_playlist("http://a.test/1.m3u").unwrap();
    let other = s.add_playlist("Other", "http://b.test/2.m3u").unwrap();
    assert_ne!(other, MAIN_PLAYLIST);
    s.remove_playlist(MAIN_PLAYLIST).unwrap();
    s.set_main_playlist("http://a.test/new.m3u").unwrap();
    let ids: Vec<_> = s.playlists().unwrap().iter().map(|p| p.id).collect();
    assert_eq!(ids, [MAIN_PLAYLIST, other], "made again, first in order");
    s.set_main_playlist("").unwrap();
    assert_eq!(s.playlists().unwrap()[0].url, "", "cleared, not kept");
}

#[test]
fn playlists_enable_move_and_count_their_channels() {
    let s = Storage::open_in_memory().unwrap();
    let a = s.add_playlist("A", "http://a.test/1.m3u").unwrap();
    let b = s.add_playlist("B", "http://b.test/2.m3u").unwrap();
    let c = s.add_playlist("C", "http://c.test/3.m3u").unwrap();
    let order =
        |s: &Storage| -> Vec<PlaylistId> { s.playlists().unwrap().iter().map(|p| p.id).collect() };
    assert!(s.move_playlist(c, true).unwrap());
    assert_eq!(order(&s), [a, c, b]);
    assert!(!s.move_playlist(a, true).unwrap(), "already first");
    assert!(!s.move_playlist(b, false).unwrap(), "already last");
    assert!(!s.move_playlist(99, false).unwrap());
    assert!(s.move_playlist(a, false).unwrap());
    assert_eq!(order(&s), [c, a, b]);
    s.set_playlist_enabled(b, false).unwrap();
    let enabled: Vec<bool> = s.playlists().unwrap().iter().map(|p| p.enabled).collect();
    assert_eq!(enabled, [true, true, false]);
    s.set_playlist_url(b, "http://b.test/new.m3u").unwrap();
    assert_eq!(s.playlists().unwrap()[2].url, "http://b.test/new.m3u");
    s.save_playlist(a, &playlist(&[("x", "G"), ("y", "G")]), NOW)
        .unwrap();
    assert_eq!(s.channel_counts().unwrap(), HashMap::from([(a, 2)]));
}

#[test]
fn a_stream_in_two_playlists_is_two_channels_and_removal_forgets_only_its_own() {
    let s = Storage::open_in_memory().unwrap();
    s.set_main_playlist("http://a.test/1.m3u").unwrap();
    let other = s.add_playlist("Other", "http://b.test/2.m3u").unwrap();
    let same = playlist(&[("a", "News")]);
    s.save_playlist(MAIN_PLAYLIST, &same, NOW).unwrap();
    s.save_playlist(other, &same, NOW).unwrap();
    let (mine, theirs) = (keys(&s, MAIN_PLAYLIST)[0], keys(&s, other)[0]);
    assert_eq!(mine, same.channels[0].key);
    assert_eq!(theirs, same.channels[0].key.in_playlist(other));
    assert_ne!(mine, theirs);
    for key in [mine, theirs] {
        s.toggle_favourite(key).unwrap();
        s.touch_recent(key, NOW).unwrap();
        s.save_progress(
            &HistoryEntry {
                key,
                start: NOW,
                stop: NOW + 3600,
                title: "t".into(),
                icon: None,
                position_secs: 600,
                duration_secs: 3600,
                completed: false,
                updated_at: NOW,
            },
            NOW,
        )
        .unwrap();
    }
    assert!(s.create_group("Mine").unwrap());
    assert!(s.add_to_group("Mine", &[mine, theirs], None).unwrap());
    s.set_channel_order(&GroupRef::Playlist("News".into()), &[theirs, mine])
        .unwrap();
    s.remove_playlist(other).unwrap();
    assert_eq!(s.favourites().unwrap(), [mine]);
    assert_eq!(s.recent().unwrap(), [(mine, NOW)]);
    let history: Vec<ChannelKey> = s.history(NOW).unwrap().iter().map(|h| h.key).collect();
    assert_eq!(history, [mine]);
    assert_eq!(s.custom_groups().unwrap()[0].channels, [(mine.0, false)]);
    assert_eq!(s.channel_order().unwrap()["News"], [mine]);
}

#[test]
fn custom_groups_are_made_renamed_and_deleted_by_name() {
    let s = Storage::open_in_memory().unwrap();
    assert!(s.create_group("Kids").unwrap());
    assert!(!s.create_group("Kids").unwrap(), "taken");
    for bad in [
        "",
        " Kids",
        "Kids ",
        "a\nb",
        &"x".repeat(MAX_GROUP_NAME + 1),
    ] {
        assert!(!s.create_group(bad).unwrap(), "{bad:?}");
    }
    assert!(s.create_group("Sport").unwrap());
    s.set_group_order(&[
        GroupRef::Custom("Sport".into()),
        GroupRef::Playlist("News".into()),
        GroupRef::Custom("Kids".into()),
    ])
    .unwrap();
    assert!(!s.rename_group("Kids", "Sport").unwrap(), "taken");
    assert!(!s.rename_group("Nope", "Other").unwrap());
    assert!(s.rename_group("Kids", "Kids").unwrap());
    assert!(s.rename_group("Kids", "Family").unwrap());
    let names: Vec<String> = s
        .custom_groups()
        .unwrap()
        .into_iter()
        .map(|g| g.name)
        .collect();
    assert_eq!(names, ["Family", "Sport"]);
    assert_eq!(
        s.group_order().unwrap(),
        [
            GroupRef::Custom("Sport".into()),
            GroupRef::Playlist("News".into()),
            GroupRef::Custom("Family".into()),
        ],
        "the order follows the rename"
    );
    s.delete_group("Sport").unwrap();
    assert_eq!(s.custom_groups().unwrap().len(), 1);
    assert_eq!(
        s.group_order().unwrap(),
        [
            GroupRef::Playlist("News".into()),
            GroupRef::Custom("Family".into()),
        ]
    );
}

#[test]
fn channels_are_copied_or_moved_into_custom_groups() {
    let s = Storage::open_in_memory().unwrap();
    let [a, b, c] = [1, 2, 3].map(ChannelKey);
    let members = |s: &Storage, name: &str| {
        s.custom_groups()
            .unwrap()
            .into_iter()
            .find(|g| g.name == name)
            .unwrap()
            .channels
    };
    assert!(!s.add_to_group("Missing", &[a], None).unwrap());
    s.create_group("One").unwrap();
    s.create_group("Two").unwrap();
    // Copied: still in the playlist groups.
    assert!(s.add_to_group("One", &[a, b], None).unwrap());
    // Moved from a playlist group: leaves the playlist groups.
    let news = GroupRef::Playlist("News".into());
    assert!(s.add_to_group("One", &[c, a], Some(&news)).unwrap());
    assert_eq!(
        members(&s, "One"),
        [(a.0, true), (b.0, false), (c.0, true)],
        "a keeps its place"
    );
    // Moved from a custom group: leaves it, keeping whether it left its playlist groups.
    let one = GroupRef::Custom("One".into());
    assert!(s.add_to_group("Two", &[c, b], Some(&one)).unwrap());
    assert_eq!(members(&s, "One"), [(a.0, true)]);
    assert_eq!(members(&s, "Two"), [(c.0, true), (b.0, false)]);
    assert!(s
        .add_to_group("Two", &[c], Some(&GroupRef::Custom("Two".into())))
        .unwrap());
    assert_eq!(members(&s, "Two").len(), 2, "into its own group: nothing");
    s.remove_from_group("Two", &[c]).unwrap();
    assert_eq!(members(&s, "Two"), [(b.0, false)]);
    s.set_channel_order(&GroupRef::Custom("One".into()), &[])
        .unwrap();
    s.add_to_group("One", &[b, c], None).unwrap();
    s.set_channel_order(&one, &[c, ChannelKey(9), a]).unwrap();
    assert_eq!(
        members(&s, "One"),
        [(c.0, false), (a.0, true), (b.0, false)],
        "listed first, the rest after, keys not in it ignored"
    );
    s.delete_group("One").unwrap();
    assert_eq!(members(&s, "Two"), [(b.0, false)]);
}

#[test]
fn a_playlist_groups_channel_order_is_kept_per_group() {
    let s = Storage::open_in_memory().unwrap();
    let news = GroupRef::Playlist("News".into());
    s.set_channel_order(&news, &[ChannelKey(3), ChannelKey(1)])
        .unwrap();
    s.set_channel_order(&GroupRef::Playlist("Sport".into()), &[ChannelKey(2)])
        .unwrap();
    s.set_channel_order(&news, &[ChannelKey(1), ChannelKey(3)])
        .unwrap();
    let order = s.channel_order().unwrap();
    assert_eq!(order["News"], [ChannelKey(1), ChannelKey(3)]);
    assert_eq!(order["Sport"], [ChannelKey(2)]);
}

#[test]
fn the_user_layout_round_trips_and_refuses_what_the_app_cannot_make() {
    let s = Storage::open_in_memory().unwrap();
    s.create_group("Kids").unwrap();
    s.add_to_group(
        "Kids",
        &[ChannelKey(5)],
        Some(&GroupRef::Playlist("A".into())),
    )
    .unwrap();
    s.set_group_order(&[GroupRef::Custom("Kids".into())])
        .unwrap();
    s.set_channel_order(&GroupRef::Playlist("A".into()), &[ChannelKey(7)])
        .unwrap();
    let layout = s.user_layout().unwrap();
    assert_eq!(
        layout,
        UserLayout {
            custom_groups: vec![CustomGroup {
                name: "Kids".into(),
                channels: vec![(5, true)],
            }],
            group_order: vec![GroupRef::Custom("Kids".into())],
            channel_order: vec![("A".into(), vec![7])],
        }
    );
    let fresh = Storage::open_in_memory().unwrap();
    fresh.create_group("Old").unwrap();
    assert!(fresh.set_user_layout(&layout).unwrap());
    assert_eq!(fresh.user_layout().unwrap(), layout, "replaced, not merged");
    let twice = UserLayout {
        custom_groups: vec![layout.custom_groups[0].clone(); 2],
        ..UserLayout::default()
    };
    let bad_name = UserLayout {
        custom_groups: vec![CustomGroup {
            name: "\u{7}".into(),
            channels: vec![],
        }],
        ..UserLayout::default()
    };
    let too_many = UserLayout {
        channel_order: vec![("A".into(), vec![1; MAX_LAYOUT_ROWS])],
        ..UserLayout::default()
    };
    for bad in [twice, bad_name, too_many] {
        assert!(!fresh.set_user_layout(&bad).unwrap());
    }
    assert_eq!(
        fresh.user_layout().unwrap(),
        layout,
        "a refused one changes nothing"
    );
}
