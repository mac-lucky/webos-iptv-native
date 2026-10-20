//! EPG matching tests. The multi-source cases are ported to Rust from webOS IPTV Player
//! `src/services/epg-service.test.ts` (commit 7d8fd30, Copyright 2026 lennylxx, Apache-2.0; see
//! THIRD-PARTY-NOTICES.md) and changed; the rest cover tvg-shift and diagnostics.

use std::collections::{HashMap, HashSet};

use iptv::epg::{filter, GuideChannel, GuideSource, Matcher, Override, PlaylistId, SourceId, Via};
use iptv::model::{Channel, ChannelKey, EpgChannel};

const A: PlaylistId = 1;
const B: PlaylistId = 2;

fn channel(name: &str, tvg_id: &str, url: &str) -> Channel {
    Channel {
        key: ChannelKey::of_url(url),
        name: name.into(),
        url: url.into(),
        tvg_id: (!tvg_id.is_empty()).then(|| tvg_id.into()),
        ..Default::default()
    }
}

fn guide(row: i64, id: &str, names: &[&str]) -> GuideChannel {
    GuideChannel {
        row,
        epg: EpgChannel {
            id: id.into(),
            names: names.iter().map(|n| n.to_string()).collect(),
            icon: None,
        },
        has_programmes: true,
    }
}

fn source(
    id: SourceId,
    manual: bool,
    playlists: &[PlaylistId],
    channels: Vec<GuideChannel>,
) -> GuideSource {
    GuideSource {
        id,
        manual,
        playlists: playlists.to_vec(),
        offset_minutes: 0,
        channels,
    }
}

fn mapped(source: SourceId, id: &str) -> Override {
    Override {
        guide: Some((source, id.into())),
        shift_minutes: None,
    }
}

fn set(values: &[&str]) -> HashSet<String> {
    values.iter().map(|v| v.to_string()).collect()
}

// --- ported: EpgService multi-source matching ------------------------------------------------

#[test]
fn reports_stale_manual_mappings() {
    let sources = [source(
        1,
        false,
        &[A],
        vec![guide(10, "available", &["Alpha"])],
    )];
    let alpha = channel("Alpha", "", "http://host/a");
    let overrides = HashMap::from([(alpha.key, mapped(1, "missing"))]);
    let matches = Matcher::new(&sources).match_playlist(A, &[alpha], &overrides);
    assert_eq!(matches.stale_overrides, [0]);
    // Matching falls back to the name.
    assert_eq!(matches.channels[0].as_ref().unwrap().via, Via::Name);
}

#[test]
fn applies_a_source_offset() {
    let sources = [GuideSource {
        offset_minutes: 60,
        ..source(1, false, &[A], vec![guide(10, "a", &["Alpha"])])
    }];
    let found = Matcher::new(&sources)
        .find(A, &channel("Alpha", "a", "http://host/a"), None)
        .unwrap();
    assert_eq!(
        (found.guide_channel, found.shift_minutes, found.via),
        (10, 60, Via::TvgId)
    );
}

#[test]
fn overrides_a_source_offset_for_only_the_customized_channel() {
    let mut sources = [GuideSource {
        offset_minutes: 60,
        ..source(1, false, &[A], vec![guide(10, "shared", &["Alpha"])])
    }];
    let customized = channel("Alpha", "shared", "http://host/a");
    let inherited = channel("Alpha", "shared", "http://host/b");
    let shift = |minutes| Override {
        guide: None,
        shift_minutes: Some(minutes),
    };
    let channels = [customized.clone(), inherited];
    let shifts = |sources: &[GuideSource], overrides: &HashMap<ChannelKey, Override>| {
        Matcher::new(sources)
            .match_playlist(A, &channels, overrides)
            .channels
            .iter()
            .map(|m| m.as_ref().unwrap().shift_minutes)
            .collect::<Vec<_>>()
    };

    let overrides = HashMap::from([(customized.key, shift(60))]);
    assert_eq!(shifts(&sources, &overrides), [120, 60]);

    let overrides = HashMap::from([(customized.key, shift(-30))]);
    assert_eq!(shifts(&sources, &overrides), [30, 60]);

    sources[0].offset_minutes = 90;
    assert_eq!(shifts(&sources, &overrides), [60, 90]);
}

#[test]
fn keeps_colliding_xmltv_ids_isolated_by_the_owning_playlist() {
    let sources = [
        source(1, false, &[A], vec![guide(10, "shared", &["Alpha"])]),
        source(2, false, &[B], vec![guide(20, "shared", &["Bravo"])]),
    ];
    let matcher = Matcher::new(&sources);
    let a = matcher
        .find(A, &channel("Alpha", "shared", "http://host/a"), None)
        .unwrap();
    let b = matcher
        .find(B, &channel("Bravo", "shared", "http://host/b"), None)
        .unwrap();
    assert_eq!((a.source, a.guide_channel), (1, 10));
    assert_eq!((b.source, b.guide_channel), (2, 20));
}

#[test]
fn falls_back_to_a_case_insensitive_name_within_the_owning_feed() {
    let sources = [source(
        1,
        false,
        &[A],
        vec![guide(10, "epg.5", &["Alpha HD"])],
    )];
    let found = Matcher::new(&sources)
        .find(A, &channel("alpha hd", "missing", "http://host/a"), None)
        .unwrap();
    assert_eq!((found.guide_channel, found.via), (10, Via::Name));
}

#[test]
fn matches_a_renamed_channel_through_its_tvg_name() {
    let sources = [source(1, false, &[A], vec![guide(10, "epg.6", &["Alpha"])])];
    let renamed = Channel {
        tvg_name: Some("Alpha".into()),
        ..channel("My Alpha", "missing", "http://host/a")
    };
    assert_eq!(
        Matcher::new(&sources)
            .find(A, &renamed, None)
            .map(|m| m.guide_channel),
        Some(10)
    );
}

#[test]
fn matches_through_a_secondary_display_name() {
    let sources = [source(
        1,
        false,
        &[A],
        vec![guide(10, "epg.7", &["Alpha", "Alpha HD"])],
    )];
    let found = Matcher::new(&sources).find(A, &channel("alpha hd", "", "http://host/a"), None);
    assert_eq!(found.map(|m| m.guide_channel), Some(10));
}

#[test]
fn does_not_match_against_an_unrelated_playlist_feed() {
    let sources = [source(1, false, &[A], vec![guide(10, "same", &["Alpha"])])];
    let found = Matcher::new(&sources).find(B, &channel("Alpha", "same", "http://host/a"), None);
    assert_eq!(found, None);
}

#[test]
fn gives_a_manual_feed_priority_over_playlist_feeds() {
    // Listed after the playlist feed on purpose: the matcher orders manual feeds first.
    let sources = [
        source(1, false, &[A], vec![guide(10, "same", &["Alpha"])]),
        source(2, true, &[], vec![guide(20, "same", &["Alpha"])]),
    ];
    let found = Matcher::new(&sources).find(A, &channel("Alpha", "same", "http://host/a"), None);
    assert_eq!(found.map(|m| m.source), Some(2));
}

#[test]
fn limits_a_manual_feed_to_its_selected_playlists() {
    let sources = [
        source(1, true, &[A], vec![guide(10, "same", &["Alpha"])]),
        source(2, true, &[B], vec![guide(20, "same", &["Alpha"])]),
    ];
    let matcher = Matcher::new(&sources);
    let alpha = channel("Alpha", "same", "http://host/a");
    assert_eq!(matcher.find(A, &alpha, None).map(|m| m.source), Some(1));
    assert_eq!(matcher.find(B, &alpha, None).map(|m| m.source), Some(2));
}

#[test]
fn uses_a_manual_mapping_before_id_and_name_matching() {
    let sources = [source(
        1,
        false,
        &[A],
        vec![
            guide(10, "epg.8", &["Bravo"]),
            guide(11, "wrong", &["Alpha"]),
        ],
    )];
    let alpha = channel("Alpha", "wrong", "http://host/a");
    let overrides = HashMap::from([(alpha.key, mapped(1, "epg.8"))]);
    let found = Matcher::new(&sources)
        .find(A, &alpha, overrides.get(&alpha.key))
        .unwrap();
    assert_eq!((found.guide_channel, found.via), (10, Via::Override));
    let channels = [alpha];
    let (ids, names) = filter(&sources[0], &[(A, &channels)], &overrides);
    assert_eq!(ids, set(&["wrong", "epg.8"]));
    assert_eq!(names, set(&["alpha"]));
}

#[test]
fn ignores_a_mapping_whose_source_no_longer_serves_the_channel() {
    let sources = [source(1, true, &[B], vec![guide(10, "epg.8", &["Bravo"])])];
    let alpha = channel("Alpha", "wrong", "http://host/a");
    let overrides = HashMap::from([(alpha.key, mapped(1, "epg.8"))]);
    let channels = [alpha];
    let matches = Matcher::new(&sources).match_playlist(A, &channels, &overrides);
    assert_eq!(matches.channels, [None]);
    assert_eq!(matches.stale_overrides, [0]);
    // Nothing of this playlist would be parsed from that source.
    let (ids, names) = filter(&sources[0], &[(A, &channels)], &overrides);
    assert!(ids.is_empty() && names.is_empty());
}

// --- new ---------------------------------------------------------------------------------

#[test]
fn id_and_name_matches_need_programmes_but_a_mapping_does_not() {
    let empty = |row, id: &str| GuideChannel {
        has_programmes: false,
        ..guide(row, id, &["Alpha"])
    };
    let sources = [
        source(1, true, &[], vec![empty(10, "x")]),
        source(2, false, &[A], vec![guide(20, "x", &["Alpha"])]),
    ];
    let matcher = Matcher::new(&sources);
    let by_id = channel("Other", "x", "http://host/a");
    let by_name = channel("Alpha", "", "http://host/b");
    assert_eq!(
        matcher.find(A, &by_id, None).map(|m| m.guide_channel),
        Some(20)
    );
    assert_eq!(
        matcher.find(A, &by_name, None).map(|m| m.guide_channel),
        Some(20)
    );
    let picked = mapped(1, "x");
    assert_eq!(
        matcher
            .find(A, &by_id, Some(&picked))
            .map(|m| m.guide_channel),
        Some(10)
    );
}

#[test]
fn tvg_shift_is_applied_and_a_user_shift_replaces_it() {
    let sources = [GuideSource {
        offset_minutes: 15,
        ..source(1, false, &[A], vec![guide(10, "a", &["Alpha"])])
    }];
    let shifted = Channel {
        tvg_shift_minutes: 120,
        ..channel("Alpha", "a", "http://host/a")
    };
    let matcher = Matcher::new(&sources);
    assert_eq!(matcher.find(A, &shifted, None).unwrap().shift_minutes, 135);
    let user = Override {
        guide: None,
        shift_minutes: Some(-60),
    };
    assert_eq!(
        matcher
            .find(A, &shifted, Some(&user))
            .unwrap()
            .shift_minutes,
        -45
    );
}

#[test]
fn unmatched_channels_are_listed() {
    let sources = [source(1, false, &[A], vec![guide(10, "a", &["Alpha"])])];
    let channels = [
        channel("Nope", "", "http://host/1"),
        channel("Alpha", "", "http://host/2"),
        channel("Still nope", "zz", "http://host/3"),
    ];
    let matches = Matcher::new(&sources).match_playlist(A, &channels, &HashMap::new());
    assert_eq!(matches.unmatched().collect::<Vec<_>>(), [0, 2]);
    assert!(matches.stale_overrides.is_empty());
    assert_eq!(
        matches.channels[1].as_ref().map(|m| m.guide_channel),
        Some(10)
    );
}

#[test]
fn the_filter_covers_only_served_playlists() {
    let owned = source(1, false, &[A], vec![]);
    let global = source(2, true, &[], vec![]);
    let a = [Channel {
        tvg_name: Some("Alpha Source".into()),
        ..channel("Alpha", "a.id", "http://host/a")
    }];
    let b = [channel("Bravo", "", "http://host/b")];
    let playlists = [(A, &a[..]), (B, &b[..])];
    assert_eq!(
        filter(&owned, &playlists, &HashMap::new()),
        (set(&["a.id"]), set(&["alpha", "alpha source"]))
    );
    assert_eq!(
        filter(&global, &playlists, &HashMap::new()),
        (set(&["a.id"]), set(&["alpha", "alpha source", "bravo"]))
    );
}
