//! M3U parser cases ported to Rust from webOS IPTV Player `src/parsers/m3u-parser.test.ts`
//! (commit 7d8fd30, Copyright 2026 lennylxx, Apache-2.0; see THIRD-PARTY-NOTICES.md) and changed,
//! in the same order, then playlist shapes seen in the field and hostile input. Cases marked
//! "Adapted" test the same behaviour through this crate's API, which has no parse hooks, keeps
//! no header attributes and leaves wrapping a single stream to the caller. Cases marked "Ours"
//! are this project's, some from its catch-up patch (lennylxx/webos-iptv-player#87).

use std::io::BufReader;

use iptv::m3u::{parse, parse_with, Limits, M3uError, NotM3u, ParseReport};
use iptv::model::{CatchupMode, Channel, Playlist};

fn parsed(text: &str) -> (Playlist, ParseReport) {
    parse(text.as_bytes()).expect("parses")
}

fn channels(text: &str) -> Vec<Channel> {
    parsed(text).0.channels
}

fn channel(text: &str) -> Channel {
    channels(text).remove(0)
}

fn not_m3u(text: &str) -> NotM3u {
    match parse(text.as_bytes()) {
        Err(M3uError::NotM3u(kind)) => kind,
        other => panic!("expected NotM3u, got {other:?}"),
    }
}

fn catchup(ch: &Channel) -> (Option<CatchupMode>, u16, Option<&str>) {
    (
        ch.catchup.mode,
        ch.catchup.days,
        ch.catchup.source.as_deref(),
    )
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|&(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

#[test]
fn parses_a_basic_channel_with_url() {
    let chs = channels("#EXTM3U\n#EXTINF:-1,Channel One\nhttp://example.com/1.m3u8");
    assert_eq!(chs.len(), 1);
    assert_eq!(chs[0].name, "Channel One");
    assert_eq!(chs[0].url, "http://example.com/1.m3u8");
}

#[test]
fn extracts_tvg_attributes_and_group_title() {
    let ch = channel(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1 tvg-id=\"c1\" tvg-name=\"News HD\" tvg-logo=\"http://logo/1.png\" ",
        "group-title=\"News\",News\n",
        "http://example.com/news.m3u8",
    ));
    assert_eq!(ch.tvg_id.as_deref(), Some("c1"));
    // Deviation: the display title wins over tvg-name, as in Kodi (the TS preferred tvg-name).
    assert_eq!(ch.name, "News");
    assert_eq!(ch.tvg_name.as_deref(), Some("News HD"));
    assert_eq!(ch.logo.as_deref(), Some("http://logo/1.png"));
    assert_eq!(ch.groups, ["News"]);
    assert!(ch.extra.is_empty());
}

#[test]
fn accepts_single_quoted_unquoted_and_spaced_attributes() {
    let (playlist, _) = parsed(concat!(
        "#EXTM3U url-tvg = http://host/guide.xml\n",
        "#EXTINF:-1 tvg-id = 'ch1' tvg-name=Alpha group-title = \"Local\" catchup-days = 7,Display\n",
        "http://host/a",
    ));
    assert_eq!(playlist.epg_urls, ["http://host/guide.xml"]);
    let ch = &playlist.channels[0];
    assert_eq!(ch.tvg_id.as_deref(), Some("ch1"));
    assert_eq!(ch.name, "Display");
    assert_eq!(ch.tvg_name.as_deref(), Some("Alpha"));
    assert_eq!(ch.groups, ["Local"]);
    assert_eq!(ch.catchup.days, 7);
}

#[test]
fn keeps_commas_and_equals_inside_quoted_values_and_the_display_title() {
    let ch = channel(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1 tvg-id=\"ch1\" group-title=\"Alpha, Bravo\" ",
        "catchup-source='http://host/a?x=1,y=2',Display, Name\n",
        "http://host/a",
    ));
    assert_eq!(ch.groups, ["Alpha, Bravo"]);
    assert_eq!(ch.catchup.source.as_deref(), Some("http://host/a?x=1,y=2"));
    assert_eq!(ch.name, "Display, Name");
}

#[test]
fn keeps_catchup_xc_without_inferring_an_xtream_source() {
    // Adapted: Xtream archive inference belongs to the Xtream account support.
    let ch = channel(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1 catchup=\"xc\" catchup-days=\"3\",Alpha\n",
        "http://host:8080/live/u1/p1/42.m3u8",
    ));
    assert_eq!(catchup(&ch), (Some(CatchupMode::Xc), 3, None));
}

#[test]
fn keeps_an_explicit_catchup_source_for_catchup_xc() {
    let ch = channel(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1 catchup=\"xc\" catchup-source=\"http://host/archive/{utc}\",Alpha\n",
        "http://host/live/u1/p1/42.ts",
    ));
    assert_eq!(ch.catchup.mode, Some(CatchupMode::Xc));
    assert_eq!(
        ch.catchup.source.as_deref(),
        Some("http://host/archive/{utc}")
    );
}

#[test]
fn reads_siptv_timeshift_as_catchup_days_in_shift_mode() {
    // Ours, from the catch-up patch.
    let chs = channels(concat!(
        "#EXTM3U\n",
        "#EXTINF:0 tvg-id=\"ch1\" timeshift=\"7\",Alpha\n",
        "http://host/ch1/mono.m3u8?k=v\n",
        "#EXTINF:0 tvg-id=\"ch2\" timeshift=\"0\",Bravo\n",
        "http://host/ch2/mono.m3u8",
    ));
    assert_eq!(
        catchup(&chs[0]),
        (
            Some(CatchupMode::Shift),
            7,
            Some("http://host/ch1/mono.m3u8?k=v&utc={utc}&lutc={lutc}")
        )
    );
    assert_eq!(chs[0].tvg_shift_minutes, 0);
    assert_eq!(catchup(&chs[1]), (None, 0, None));
}

#[test]
fn reads_tvg_rec_like_siptv_timeshift() {
    // Ours, from the catch-up patch.
    let ch = channel("#EXTM3U\n#EXTINF:0 tvg-rec=\"3\",Alpha\nhttp://host/ch1/index.m3u8");
    assert_eq!(
        catchup(&ch),
        (
            Some(CatchupMode::Shift),
            3,
            Some("http://host/ch1/index.m3u8?utc={utc}&lutc={lutc}")
        )
    );
}

#[test]
fn keeps_an_explicit_mode_and_catchup_days_over_siptv_timeshift() {
    // Ours, from the catch-up patch.
    let ch = channel(concat!(
        "#EXTM3U\n",
        "#EXTINF:0 catchup=\"default\" catchup-days=\"2\" timeshift=\"7\" ",
        "catchup-source=\"http://host/c/{utc}\",Alpha\n",
        "http://host/ch1/mono.m3u8",
    ));
    assert_eq!(
        catchup(&ch),
        (Some(CatchupMode::Default), 2, Some("http://host/c/{utc}"))
    );
}

#[test]
fn builds_sources_for_append_shift_and_flussonic_modes() {
    // Ours, from the catch-up patch.
    let chs = channels(concat!(
        "#EXTM3U\n",
        "#EXTINF:0 catchup=\"append\" catchup-source=\"?cutv={Y}-{m}-{d}\",Alpha\n",
        "http://host/a.m3u8?k=v\n",
        "#EXTINF:0 catchup=\"shift\" catchup-days=\"1\",Bravo\n",
        "http://host/b.m3u8\n",
        "#EXTINF:0 catchup=\"fs\",Charlie\n",
        "http://host/ch3/mono.m3u8?k=v\n",
        "#EXTINF:0 catchup=\"flussonic\",Delta\n",
        "http://host/ch4/mpegts",
    ));
    let got: Vec<_> = chs
        .iter()
        .map(|ch| (ch.catchup.mode, ch.catchup.source.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            (
                Some(CatchupMode::Append),
                Some("http://host/a.m3u8?k=v&cutv={Y}-{m}-{d}")
            ),
            (
                Some(CatchupMode::Shift),
                Some("http://host/b.m3u8?utc={utc}&lutc={lutc}")
            ),
            (
                Some(CatchupMode::Flussonic),
                Some("http://host/ch3/mono-{utc}-{duration}.m3u8?k=v")
            ),
            (
                Some(CatchupMode::Flussonic),
                Some("http://host/ch4/archive-{utc}-{duration}.ts")
            ),
        ]
    );
}

#[test]
fn applies_header_catchup_defaults_to_entries_without_their_own() {
    // Ours, from the catch-up patch.
    let chs = channels(concat!(
        "#EXTM3U catchup-type=\"flussonic\" catchup-time=\"604800\"\n",
        "#EXTINF:0 tvg-id=\"ch1\",Alpha\n",
        "http://host/ch1/mono.m3u8?k=v\n",
        "#EXTINF:0 tvg-id=\"ch2\" catchup=\"shift\" catchup-days=\"3\",Bravo\n",
        "http://host/ch2/mono.m3u8",
    ));
    assert_eq!(
        catchup(&chs[0]),
        (
            Some(CatchupMode::Flussonic),
            7,
            Some("http://host/ch1/mono-{utc}-{duration}.m3u8?k=v")
        )
    );
    assert_eq!(
        catchup(&chs[1]),
        (
            Some(CatchupMode::Shift),
            3,
            Some("http://host/ch2/mono.m3u8?utc={utc}&lutc={lutc}")
        )
    );
}

#[test]
fn inherits_a_header_catchup_source_only_for_the_header_mode() {
    // Ours, from the catch-up patch.
    let chs = channels(concat!(
        "#EXTM3U catchup=\"default\" catchup-source=\"http://host/c/{utc}\" catchup-days=\"5\"\n",
        "#EXTINF:0,Alpha\n",
        "http://host/a.m3u8\n",
        "#EXTINF:0 catchup=\"shift\",Bravo\n",
        "http://host/b.m3u8",
    ));
    assert_eq!(
        catchup(&chs[0]),
        (Some(CatchupMode::Default), 5, Some("http://host/c/{utc}"))
    );
    assert_eq!(chs[1].catchup.mode, Some(CatchupMode::Shift));
    assert_eq!(
        chs[1].catchup.source.as_deref(),
        Some("http://host/b.m3u8?utc={utc}&lutc={lutc}")
    );
}

fn correction_secs(text: &str) -> Vec<i32> {
    channels(text)
        .iter()
        .map(|c| c.catchup.correction_secs)
        .collect()
}

#[test]
fn reads_catchup_correction_hours_from_the_entry_else_the_header() {
    // Ours.
    let chs = channels(concat!(
        "#EXTM3U catchup-correction=\"-2.5\"\n",
        "#EXTINF:0 catchup=\"shift\" catchup-correction=\"1.5\",Entry\n",
        "http://host/a.m3u8\n",
        "#EXTINF:0 catchup=\"shift\",Header\n",
        "http://host/b.m3u8\n",
        "#EXTINF:0 catchup-correction=\"\",Empty\n",
        "http://host/c.m3u8\n",
        // Declared but not a number: 0, as Kodi's atof reads it, and the header does not apply.
        "#EXTINF:0 catchup-correction=\"soon\",Malformed\n",
        "http://host/d.m3u8\n",
    ));
    let secs: Vec<i32> = chs.iter().map(|c| c.catchup.correction_secs).collect();
    assert_eq!(secs, [5_400, -9_000, -9_000, 0]);
    assert!(chs[0].extra.is_empty(), "{:?}", chs[0].extra);
}

#[test]
fn catchup_correction_is_zero_when_absent() {
    // Ours.
    assert_eq!(
        correction_secs("#EXTM3U\n#EXTINF:0 catchup=\"shift\",Alpha\nhttp://host/a.m3u8"),
        [0]
    );
    assert_eq!(
        correction_secs("#EXTINF:0 catchup=\"shift\",Alpha\nhttp://host/a.m3u8"),
        [0]
    );
}

#[test]
fn reads_catchup_correction_like_atof_within_a_day() {
    // Ours.
    for (value, secs) in [
        ("-2", -7_200),
        ("+1", 3_600),
        ("-4.0", -14_400),
        ("0.25", 900),
        ("-.5", -1_800),
        ("2.", 7_200),
        (" 3", 10_800),
        ("1.5h", 5_400),
        ("1.2.3", 4_320),
        // A second's fraction is truncated, as Kodi casts to int.
        ("0.0001", 0),
        ("-0.0001", 0),
        ("24", 86_400),
        ("-24", -86_400),
        ("24.5", 0),
        ("99999999999999999999", 0),
        ("-", 0),
        (".", 0),
    ] {
        let text = format!("#EXTM3U\n#EXTINF:0 catchup-correction=\"{value}\",A\nhttp://host/a");
        assert_eq!(correction_secs(&text), [secs], "{value:?}");
    }
}

#[test]
fn matches_attribute_names_without_regard_to_case() {
    let (playlist, _) = parsed(concat!(
        "#EXTM3U X-TVG-URL=\"http://host/guide.xml\"\n",
        "#EXTINF:-1 TVG-ID=\"ch1\" TVG-NAME=\"Alpha\" GROUP-TITLE=\"Bravo\",Display\n",
        "http://host/a",
    ));
    assert_eq!(playlist.epg_urls, ["http://host/guide.xml"]);
    let ch = &playlist.channels[0];
    assert_eq!(ch.tvg_id.as_deref(), Some("ch1"));
    assert_eq!(ch.tvg_name.as_deref(), Some("Alpha"));
    assert_eq!(ch.groups, ["Bravo"]);
}

#[test]
fn falls_back_to_the_display_title_and_no_group() {
    let ch = channel("#EXTM3U\n#EXTINF:-1,Bare Channel\nhttp://example.com/bare.m3u8");
    assert_eq!(ch.name, "Bare Channel");
    assert!(ch.groups.is_empty());
}

#[test]
fn keeps_a_provider_group_named_uncategorized_apart_from_ungrouped() {
    // Adapted: ungrouped is an empty list rather than a sentinel group.
    let (playlist, _) = parsed(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1 group-title=\"Uncategorized\",One\n",
        "http://host/1\n",
        "#EXTINF:-1,Two\n",
        "http://host/2",
    ));
    assert_eq!(playlist.channels[0].groups, ["Uncategorized"]);
    assert!(playlist.channels[1].groups.is_empty());
    assert_eq!(playlist.groups(), ["Uncategorized"]);
}

#[test]
fn collects_the_distinct_set_of_groups() {
    let (playlist, _) = parsed(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1 group-title=\"A\",One\nhttp://e/1\n",
        "#EXTINF:-1 group-title=\"B\",Two\nhttp://e/2\n",
        "#EXTINF:-1 group-title=\"A\",Three\nhttp://e/3",
    ));
    assert_eq!(playlist.groups(), ["A", "B"]);
}

#[test]
fn reads_the_embedded_epg_url_from_the_header() {
    let epg = |text: &str| parsed(text).0.epg_urls;
    assert_eq!(
        epg("#EXTM3U url-tvg=\"http://epg/guide.xml\""),
        ["http://epg/guide.xml"]
    );
    assert_eq!(
        epg("#EXTM3U x-tvg-url=\"http://epg/alt.xml\""),
        ["http://epg/alt.xml"]
    );
}

#[test]
fn collects_multiple_epg_urls() {
    // Adapted: other header attributes (max-conn, custom ones) are not kept.
    let (playlist, report) = parsed(concat!(
        "#EXTM3U url-tvg=\"http://host/a.xml,http://host/b.xml\" ",
        "tvg-url=\"http://host/c.xml\" max-conn=\"2\" custom-header=\"v\"",
    ));
    assert_eq!(
        playlist.epg_urls,
        [
            "http://host/a.xml",
            "http://host/b.xml",
            "http://host/c.xml"
        ]
    );
    assert!(report.has_header);
}

#[test]
fn lets_extgrp_override_the_group_title() {
    let ch = channel("#EXTM3U\n#EXTINF:-1 group-title=\"Old\",Ch\n#EXTGRP:Movies\nhttp://e/1");
    assert_eq!(ch.groups, ["Movies"]);
}

#[test]
fn maps_extvlcopt_headers_and_keeps_kodiprop_as_extra() {
    let ch = channel(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1,Ch\n",
        "#EXTVLCOPT:http-user-agent=Mozilla\n",
        "#KODIPROP:inputstream.adaptive.license_type=clearkey\n",
        "http://e/1",
    ));
    assert_eq!(ch.http_headers, pairs(&[("User-Agent", "Mozilla")]));
    assert_eq!(
        ch.extra,
        pairs(&[("kodiprop:inputstream.adaptive.license_type", "clearkey")])
    );
}

#[test]
fn preserves_unknown_attributes_and_extended_channel_metadata() {
    let ch = channel(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1 tvg-id=\"ch1\" group-title=\"Alpha;Bravo\" custom-field=\"v\" ",
        "tvg-chno=\"12\" tvg-shift=\"-1.5\" radio=\"true\",Channel\n",
        "http://host/a",
    ));
    assert_eq!(ch.extra, pairs(&[("custom-field", "v")]));
    assert_eq!(ch.groups, ["Alpha", "Bravo"]);
    assert_eq!(ch.number, Some(12));
    assert_eq!(ch.tvg_shift_minutes, -90);
    assert!(ch.radio);
}

#[test]
fn parses_exthttp_headers() {
    let mut ch = channel(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1,Channel\n",
        "#EXTHTTP:{\"User-Agent\":\"Agent\",\"Referer\":\"http://host/a\",\"X-Test\":\"v\"}\n",
        "http://host/a",
    ));
    ch.http_headers.sort();
    assert_eq!(
        ch.http_headers,
        pairs(&[
            ("Referer", "http://host/a"),
            ("User-Agent", "Agent"),
            ("X-Test", "v")
        ])
    );
}

#[test]
fn reports_malformed_exthttp_without_dropping_the_channel() {
    let (playlist, report) = parsed("#EXTM3U\n#EXTINF:-1,Channel\n#EXTHTTP:{bad\nhttp://host/a");
    assert_eq!(playlist.channels.len(), 1);
    assert_eq!(report.bad_headers, 1);
}

#[test]
fn ignores_blank_lines_and_crlf_line_endings() {
    assert_eq!(
        channels("#EXTM3U\r\n\r\n#EXTINF:-1,Ch\r\nhttp://e/1\r\n").len(),
        1
    );
}

#[test]
fn preserves_parser_semantics_across_arbitrary_chunk_boundaries() {
    let m3u = [
        "#EXTM3U url-tvg=\"http://host/guide.xml\"",
        "#EXTINF:-1 tvg-id=\"ch1\" group-title=\"Alpha;Bravo\" custom=\"v\",Channel",
        "#EXTVLCOPT:http-user-agent=Agent",
        "#KODIPROP:inputstream.adaptive.license_type=clearkey",
        "#EXTHTTP:{\"Referer\":\"http://host/a\"}",
        "http://host/live/u1/p1/42.ts",
    ]
    .join("\r\n");
    let whole = parse(m3u.as_bytes()).unwrap();
    let bytewise = parse(BufReader::with_capacity(1, m3u.as_bytes())).unwrap();
    assert_eq!(bytewise, whole);
    assert_eq!(whole.0.channels[0].http_headers.len(), 2);
}

#[test]
fn filtering_channels_after_parsing_keeps_groups_consistent() {
    // Adapted: no post-filter hook; groups are derived from the channels that remain.
    let (mut playlist, _) = parsed(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1 group-title=\"Dropped\",Drop\nhttp://host/drop\n",
        "#EXTINF:-1 group-title=\"Kept\",Keep\nhttp://host/keep",
    ));
    playlist.channels.retain(|ch| ch.url.ends_with("/keep"));
    assert_eq!(playlist.channels[0].name, "Keep");
    assert_eq!(playlist.groups(), ["Kept"]);
}

#[test]
fn a_rejected_location_drops_its_entry_and_metadata() {
    // Adapted: the location filter is the http(s) rule rather than a hook.
    let (playlist, report) = parsed(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1 group-title=\"Dropped\" custom-field=\"value\",Movie\n",
        "#EXTHTTP:{bad\n",
        "rtmp://host/movie/u1/p1/201.mp4\n",
        "#EXTINF:-1 group-title=\"Live\",Alpha\n",
        "#EXTHTTP:{\"User-Agent\":\"Agent\"}\n",
        "http://host/live/u1/p1/101.ts",
    ));
    assert_eq!(playlist.channels.len(), 1);
    let ch = &playlist.channels[0];
    assert_eq!(ch.name, "Alpha");
    assert_eq!(ch.groups, ["Live"]);
    assert_eq!(ch.http_headers, pairs(&[("User-Agent", "Agent")]));
    assert!(ch.extra.is_empty());
    assert_eq!(playlist.groups(), ["Live"]);
    assert_eq!((report.dropped, report.bad_headers), (1, 1));
}

#[test]
fn keeps_an_entry_with_a_malformed_exthttp() {
    let (playlist, report) = parsed(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1 group-title=\"Movies\" custom-field=\"value\",Movie\n",
        "#EXTHTTP:{bad\n",
        "http://host/movie/u1/p1/201.mp4",
    ));
    let ch = &playlist.channels[0];
    assert_eq!(
        (ch.name.as_str(), ch.groups[0].as_str()),
        ("Movie", "Movies")
    );
    assert_eq!(ch.extra, pairs(&[("custom-field", "value")]));
    assert_eq!(report.bad_headers, 1);
}

#[test]
fn accepts_lone_cr_line_endings() {
    assert_eq!(channels("#EXTM3U\r#EXTINF:-1,Ch\rhttp://host/a\r").len(), 1);
}

#[test]
fn returns_an_empty_result_for_input_with_no_entries() {
    let (playlist, report) = parsed("#EXTM3U");
    assert!(playlist.channels.is_empty());
    assert!(playlist.groups().is_empty());
    assert!(report.has_header);
}

#[test]
fn accepts_a_bare_url_playlist_and_reports_the_missing_header() {
    let (playlist, report) = parsed("http://host/alpha.ts\nhttp://host/bravo.ts");
    let names: Vec<_> = playlist
        .channels
        .iter()
        .map(|ch| ch.name.as_str())
        .collect();
    assert_eq!(names, ["alpha", "bravo"]);
    assert!(!report.has_header);
}

#[test]
fn does_not_turn_arbitrary_text_into_a_channel() {
    let (playlist, report) = parsed("Access denied");
    assert!(playlist.channels.is_empty());
    assert_eq!(report.dropped, 1);
}

#[test]
fn bounds_channels() {
    let text = concat!(
        "#EXTM3U\n",
        "#EXTINF:-1,Missing\n",
        "#EXTINF:-1,Alpha\nhttp://host/a\n",
        "#EXTINF:-1,Bravo\nhttp://host/b",
    );
    let limits = Limits {
        max_channels: 1,
        ..Limits::default()
    };
    let (playlist, report) = parse_with(text.as_bytes(), &limits).unwrap();
    assert_eq!(playlist.channels.len(), 1);
    assert_eq!((report.orphans, report.truncated), (1, true));
}

#[test]
fn reports_a_bare_hls_master_playlist() {
    // Adapted: the caller, which knows the URL, wraps a single stream as a channel.
    let hls = concat!(
        "#EXTM3U url-tvg=\"http://host/guide.xml\" custom=\"v\"\n",
        "#PLAYLIST:Alpha\n",
        "#EXT-X-VERSION:3\n",
        "#EXT-X-STREAM-INF:BANDWIDTH=1000000\n",
        "https://cdn/inner.m3u8",
    );
    assert_eq!(not_m3u(hls), NotM3u::Hls);
}

#[test]
fn reports_a_bare_hls_media_playlist_instead_of_segment_rows() {
    let media = concat!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-TARGETDURATION:10\n",
        "#EXTINF:10.000, no desc\nseg-100.ts\n",
        "#EXTINF:10.000, no desc\nseg-101.ts\n",
    );
    assert_eq!(not_m3u(media), NotM3u::Hls);
}

#[test]
fn never_turns_hls_variants_into_channels() {
    assert_eq!(
        not_m3u("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nhttps://cdn/inner.m3u8"),
        NotM3u::Hls
    );
}

#[test]
fn reports_a_bare_dash_manifest() {
    let mpd = concat!(
        "<?xml version=\"1.0\"?>\n",
        "<!-- manifest -->\n",
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"dynamic\"></MPD>",
    );
    assert_eq!(not_m3u(mpd), NotM3u::Dash);
}

#[test]
fn reports_a_one_line_dash_manifest() {
    assert_eq!(not_m3u("<MPD type=\"static\"></MPD>"), NotM3u::Dash);
}

#[test]
fn parses_a_normal_channel_list() {
    let chs = channels("#EXTM3U\n#EXTINF:-1,Ch\nhttp://e/1");
    assert_eq!(chs.len(), 1);
    assert_eq!(chs[0].name, "Ch");
}

#[test]
fn detects_hls_and_common_wrong_document_formats() {
    assert_eq!(channels("#EXTM3U\n#EXTINF:-1,Ch\nhttp://host/a").len(), 1);
    assert_eq!(
        not_m3u("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1"),
        NotM3u::Hls
    );
    assert_eq!(not_m3u("#EXTM3U\n#EXT-X-TARGETDURATION:5"), NotM3u::Hls);
    assert_eq!(not_m3u("#extm3u\n#ext-x-endlist"), NotM3u::Hls);
    assert_eq!(not_m3u("<?xml version=\"1.0\"?><MPD/>"), NotM3u::Dash);
    assert_eq!(not_m3u("<tv></tv>"), NotM3u::Xmltv);
    assert_eq!(not_m3u("<html></html>"), NotM3u::Markup);
    assert_eq!(not_m3u("{\"error\":true}"), NotM3u::Json);
}

#[test]
fn rejects_a_recognized_non_playlist_document() {
    assert_eq!(not_m3u("<html><body>Denied</body></html>"), NotM3u::Markup);
}

#[test]
fn decodes_utf8_bom_and_utf16_byte_playlists() {
    let source = "#EXTM3U\n#EXTINF:-1,Alpha\nhttp://host/a";
    let utf8_bom = [&[0xef, 0xbb, 0xbf][..], source.as_bytes()].concat();
    let utf16 = |little_endian: bool| -> Vec<u8> {
        let units = std::iter::once(0xfeff).chain(source.encode_utf16());
        units
            .flat_map(|u| match little_endian {
                true => u.to_le_bytes(),
                false => u.to_be_bytes(),
            })
            .collect()
    };
    for bytes in [utf8_bom, utf16(true), utf16(false)] {
        let whole = parse(bytes.as_slice()).unwrap();
        assert_eq!(whole.0.channels[0].name, "Alpha");
        assert!(whole.1.has_header);
        // The byte-order mark split across reads.
        assert_eq!(
            parse(BufReader::with_capacity(1, bytes.as_slice())).unwrap(),
            whole
        );
    }
}

// Playlist shapes seen in the field (synthetic hosts and tokens).

#[test]
fn siptv_playlist_with_extgrp_and_no_header_attributes() {
    let (playlist, _) = parsed(concat!(
        "#EXTM3U\n",
        "#EXTINF:0 tvg-id=\"alpha\" tvg-logo=\"http://host/a.png\" timeshift=\"7\",Alpha\n",
        "#EXTGRP:1. News\n",
        "http://host/ch1/mono.m3u8?token=t1\n",
        "#EXTINF:0 tvg-id=\"bravo\" timeshift=\"3\",Bravo\n",
        "#EXTGRP:2. Sport\n",
        "http://host/ch2/index.m3u8?token=t2\n",
        "#EXTINF:0 tvg-id=\"charlie\",Charlie\n",
        "#EXTGRP:2. Sport\n",
        "http://host/ch3/mono.m3u8?token=t3\n",
    ));
    let chs = &playlist.channels;
    assert_eq!(
        catchup(&chs[0]),
        (
            Some(CatchupMode::Shift),
            7,
            Some("http://host/ch1/mono.m3u8?token=t1&utc={utc}&lutc={lutc}")
        )
    );
    assert_eq!(chs[1].catchup.days, 3);
    assert!(!chs[2].catchup.available());
    assert_eq!(playlist.groups(), ["1. News", "2. Sport"]);
}

#[test]
fn playlist_with_a_guide_url_and_tvg_name() {
    let (playlist, _) = parsed(concat!(
        "#EXTM3U url-tvg=\"https://epg.host/guide.xml.gz\"\n",
        "#EXTINF:0 tvg-name=\"Alpha\" timeshift=\"7\",Alpha HD\n",
        "#EXTGRP:1. News\n",
        "https://host/ch1/mono.m3u8?token=t1\n",
    ));
    assert_eq!(playlist.epg_urls, ["https://epg.host/guide.xml.gz"]);
    let ch = &playlist.channels[0];
    assert_eq!(
        (ch.name.as_str(), ch.tvg_name.as_deref()),
        ("Alpha HD", Some("Alpha"))
    );
    assert_eq!(
        catchup(ch),
        (
            Some(CatchupMode::Shift),
            7,
            Some("https://host/ch1/mono.m3u8?token=t1&utc={utc}&lutc={lutc}")
        )
    );
}

#[test]
fn header_flussonic_playlist_without_a_space_after_the_duration() {
    let (playlist, _) = parsed(concat!(
        "#EXTM3U catchup-type=\"flussonic\" catchup-time=\"604800\" ",
        "url-tvg=\"https://epg.host/guide.xml\" generation-date=\"2026-01-01 00:00 UTC\"\n",
        "#EXTINF:0tvg-id=\"alpha\" group-title=\"News\",Alpha\n",
        "https://host/ch1/mono.m3u8?token=t1\n",
    ));
    let ch = &playlist.channels[0];
    assert_eq!(ch.tvg_id.as_deref(), Some("alpha"));
    assert_eq!(ch.groups, ["News"]);
    assert_eq!(
        catchup(ch),
        (
            Some(CatchupMode::Flussonic),
            7,
            Some("https://host/ch1/mono-{utc}-{duration}.m3u8?token=t1")
        )
    );
}

#[test]
fn name_falls_back_to_tvg_name_then_tvg_id() {
    let names: Vec<_> = channels(concat!(
        "#EXTM3U\n",
        "#EXTINF:-1 tvg-name=\"Alpha\" tvg-id=\"a\", \nhttp://host/1\n",
        "#EXTINF:-1 tvg-id=\"b\",\nhttp://host/2\n",
        "#EXTINF:-1\nhttp://host/3\n",
    ))
    .into_iter()
    .map(|ch| ch.name)
    .collect();
    assert_eq!(names, ["Alpha", "b", "Unknown"]);
}

#[test]
fn catchup_time_floors_to_whole_days_but_at_least_one() {
    let days = |seconds: &str| {
        let text = format!(
            "#EXTM3U catchup=\"shift\" catchup-time=\"{seconds}\"\n#EXTINF:0,A\nhttp://h/a"
        );
        channel(&text).catchup.days
    };
    assert_eq!((days("90000"), days("3600"), days("604800")), (1, 1, 7));
    // No archive length at all: the default for a mode.
    assert_eq!(days("0"), 5);
}

#[test]
fn a_mode_without_days_gets_the_default() {
    let chs = channels(concat!(
        "#EXTM3U catchup-type=\"flussonic\"\n",
        "#EXTINF:0,A\nhttp://host/a/mono.m3u8\n",
        "#EXTINF:0 catchup=\"append\" catchup-source=\"?t={utc}\",B\nhttp://host/b\n",
        "#EXTINF:0 catchup=\"\",C\nhttp://host/c\n",
    ));
    let days: Vec<_> = chs.iter().map(|ch| ch.catchup.days).collect();
    assert_eq!(days, [5, 5, 5]);
    let ch = channel("#EXTM3U\n#EXTINF:0,A\nhttp://host/a");
    assert_eq!(catchup(&ch), (None, 0, None));
}

#[test]
fn an_unknown_catchup_tag_overrides_the_header_mode() {
    let chs = channels(concat!(
        "#EXTM3U catchup=\"shift\" catchup-days=\"3\"\n",
        "#EXTINF:0 catchup=\"disabled\",A\nhttp://host/a/mono.m3u8\n",
        "#EXTINF:0 catchup=\"disabled\" timeshift=\"7\",B\nhttp://host/b/mono.m3u8\n",
        "#EXTINF:0,C\nhttp://host/c/mono.m3u8\n",
    ));
    assert_eq!(chs[0].catchup.mode, None);
    assert!(!chs[0].catchup.available());
    // SIPTV still implies shift.
    assert_eq!(
        (chs[1].catchup.mode, chs[1].catchup.days),
        (Some(CatchupMode::Shift), 7)
    );
    assert_eq!(
        (chs[2].catchup.mode, chs[2].catchup.days),
        (Some(CatchupMode::Shift), 3)
    );
}

#[test]
fn the_header_tvg_shift_applies_to_entries_without_their_own() {
    let shifts: Vec<_> = channels(concat!(
        "#EXTM3U tvg-shift=\"2\"\n",
        "#EXTINF:0,A\nhttp://host/a\n",
        "#EXTINF:0 tvg-shift=\"-1\",B\nhttp://host/b\n",
        "#EXTINF:0 tvg-shift=\"x\",C\nhttp://host/c\n",
    ))
    .iter()
    .map(|ch| ch.tvg_shift_minutes)
    .collect();
    assert_eq!(shifts, [120, -60, 0]);
}

#[test]
fn a_repeated_attribute_in_another_case_keeps_the_last_value() {
    let ch = channel(
        "#EXTM3U\n#EXTINF:0 TVG-ID=\"a\" tvg-id=\"b\" X-Y=\"1\" x-y=\"2\",A\nhttp://host/a",
    );
    assert_eq!(ch.tvg_id.as_deref(), Some("b"));
    assert_eq!(ch.extra, pairs(&[("x-y", "2")]));
}

// Hostile input.

#[test]
fn only_http_urls_become_channels() {
    let (playlist, report) = parsed(concat!(
        "#EXTM3U url-tvg=\"file:///etc/guide.xml,http://host/g.xml\"\n",
        "#EXTINF:-1,A\nrtmp://host/a\n",
        "#EXTINF:-1,B\nudp://@239.0.0.1:1234\n",
        "#EXTINF:-1,C\nfile:///etc/passwd\n",
        "#EXTINF:-1,D\n/relative/path.ts\n",
        "#EXTINF:-1,E\nhttp:///no-host\n",
        "#EXTINF:-1,F\nhttp://host/a\u{1}b\n",
        "#EXTINF:-1 tvg-logo=\"file:///etc/a.png\" catchup=\"default\" ",
        "catchup-source=\"file:///archive/{utc}\" catchup-days=\"1\",G\n",
        "HTTPS://host/ok\n",
    ));
    assert_eq!(playlist.epg_urls, ["http://host/g.xml"]);
    assert_eq!(playlist.channels.len(), 1);
    assert_eq!(playlist.channels[0].logo, None);
    assert_eq!(playlist.channels[0].catchup.source, None);
    assert_eq!(playlist.channels[0].name, "G");
    assert_eq!((report.dropped, report.orphans), (6, 0));
}

#[test]
fn header_injection_is_refused() {
    let (playlist, report) = parsed(concat!(
        "#EXTM3U\n#EXTINF:-1,A\n",
        "#EXTHTTP:{\"X-A\":\"ok\",\"X-B\":\"a\\r\\nInjected: 1\",\"Bad Name\":\"v\",\"X-N\":1}\n",
        "#EXTVLCOPT:http-referrer=http://host/\u{0}\n",
        "#EXTHTTP:{\"X-C\":\"ok\"}\n",
        "http://host/a",
    ));
    let mut headers = playlist.channels[0].http_headers.clone();
    headers.sort();
    assert_eq!(headers, pairs(&[("X-A", "ok"), ("X-C", "ok")]));
    assert_eq!(report.bad_headers, 3);
}

#[test]
fn header_policy_is_left_to_playback() {
    // The parser checks framing only; which headers a stream may send is decided at playback.
    let ch = channel(concat!(
        "#EXTM3U\n#EXTINF:-1,A\n",
        "#EXTHTTP:{\"Range\":\"bytes=0-\",\"Host\":\"other\"}\n",
        "http://host/a",
    ));
    let mut headers = ch.http_headers;
    headers.sort();
    assert_eq!(headers, pairs(&[("Host", "other"), ("Range", "bytes=0-")]));
}

#[test]
fn urls_the_fetcher_would_refuse_are_dropped() {
    let (playlist, report) = parsed(concat!(
        "#EXTM3U url-tvg=\"http://host/a b.xml,http://u@/g.xml\"\n",
        "#EXTINF:-1,A\nhttp://host/a b.ts\n",
        "#EXTINF:-1,B\nhttp://user@/b.ts\n",
        "#EXTINF:-1 tvg-logo=\"http://host/a b.png\",C\nhttp://host/c.ts\n",
    ));
    assert!(playlist.epg_urls.is_empty());
    assert_eq!(playlist.channels.len(), 1);
    assert_eq!(playlist.channels[0].logo, None);
    assert_eq!(report.dropped, 2);
}

#[test]
fn line_and_total_caps_are_errors() {
    let limits = Limits {
        max_line_bytes: 16,
        ..Limits::default()
    };
    let long = "#EXTM3U\n#EXTINF:-1,0123456789\n";
    assert!(matches!(
        parse_with(long.as_bytes(), &limits),
        Err(M3uError::LineTooLong { line: 2 })
    ));
    let utf16: Vec<u8> = [0xff, 0xfe]
        .into_iter()
        .chain([b'a', 0].repeat(9))
        .collect();
    assert!(matches!(
        parse_with(utf16.as_slice(), &limits),
        Err(M3uError::LineTooLong { line: 1 })
    ));
    let limits = Limits {
        max_total_bytes: 10,
        ..Limits::default()
    };
    assert!(matches!(
        parse_with("#EXTM3U\nhttp://host/a".as_bytes(), &limits),
        Err(M3uError::TooLarge)
    ));
}

#[test]
fn absurd_numbers_saturate_or_are_ignored() {
    let ch = channel(concat!(
        "#EXTM3U\n#EXTINF:-1 catchup=\"shift\" catchup-days=\"99999999999999999999999\" ",
        "tvg-chno=\"99999999999\" tvg-shift=\"1e300\",A\nhttp://host/a",
    ));
    assert_eq!(ch.catchup.days, u16::MAX);
    assert_eq!((ch.number, ch.tvg_shift_minutes), (None, 0));
}

#[test]
fn attributes_per_entry_are_capped() {
    let attrs: String = (0..2_000).map(|i| format!("k{i}=v ")).collect();
    let ch = channel(&format!("#EXTM3U\n#EXTINF:-1 {attrs},A\nhttp://host/a"));
    assert_eq!(ch.extra.len(), 64);
    assert_eq!(ch.name, "A");
}

#[test]
fn deeply_nested_exthttp_is_rejected_without_overflow() {
    let nested = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    let text = format!("#EXTM3U\n#EXTINF:-1,A\n#EXTHTTP:{{\"a\":{nested}}}\nhttp://host/a");
    let (playlist, report) = parsed(&text);
    assert_eq!((playlist.channels.len(), report.bad_headers), (1, 1));
}

#[test]
fn random_input_never_panics() {
    const PIECES: &[&[u8]] = &[
        b"#EXTM3U ",
        b"#EXTINF:",
        b"-1",
        b"0",
        b" ",
        b"\t",
        b"tvg-id=",
        b"catchup=",
        b"shift",
        b"fs",
        b"timeshift=",
        b"catchup-source=",
        b"catchup-time=",
        b"group-title=",
        b"99999999999999999999",
        b"\"",
        b"'",
        b",",
        b"=",
        b";",
        b"\n",
        b"\r",
        b"\r\n",
        b"http://h/",
        b"https://",
        b"mono.m3u8",
        b"mpegts",
        b"?",
        b"#",
        b"{utc:",
        b"{",
        b"}",
        b"$",
        b"#EXTGRP:",
        b"#EXTHTTP:",
        b"#EXTVLCOPT:",
        b"#KODIPROP:",
        b"#PLAYLIST:",
        b"#EXT-X-ENDLIST",
        b"%",
        b"<",
        b"\xef\xbb\xbf",
        b"\xff\xfe",
        b"\xff",
        b"\xc3",
        b"\x00",
    ];
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..3_000 {
        let len = next() % 64;
        let input: Vec<u8> = (0..len)
            .flat_map(|_| {
                PIECES[(next() % PIECES.len() as u64) as usize]
                    .iter()
                    .copied()
            })
            .collect();
        let chunk = 1 + (next() % 7) as usize;
        if let Ok((playlist, _)) = parse(BufReader::with_capacity(chunk, input.as_slice())) {
            for ch in &playlist.channels {
                let start = next() as i64;
                let times = iptv::catchup::Times {
                    start,
                    end: start.wrapping_add(next() as i64 % 100_000),
                    now: next() as i64,
                };
                let offset = (next() % 100_000) as i32 - 50_000;
                let _ = iptv::catchup::archive_url(ch, times, |_| offset);
            }
        }
    }
}
