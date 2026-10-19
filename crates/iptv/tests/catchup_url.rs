//! Catch-up URL cases ported from webOS IPTV Player `src/utils/catchup-url.test.ts`
//! (Apache-2.0), in the same order, then the archive URL policy. Date specifiers are local time
//! at an explicit UTC offset, so no test depends on the machine's time zone.

use iptv::catchup::{
    append_query, archive_url, fill, flussonic_source, in_archive, template, Times,
};
use iptv::m3u;
use iptv::model::{CatchupMode, Channel, Programme, Timestamp};

const TIMES: Times = Times {
    start: 1_700_000_000, // 2023-11-14 22:13:20 UTC
    end: 1_700_003_600,
    now: 1_700_010_000,
};

fn utc(_: Timestamp) -> i32 {
    0
}

/// UTC+3, which moves the start across midnight.
fn plus_3h(_: Timestamp) -> i32 {
    3 * 3_600
}

#[test]
fn mode_parsing_lower_cases_and_folds_the_fs_alias() {
    assert_eq!(CatchupMode::parse(" Shift "), Some(CatchupMode::Shift));
    assert_eq!(CatchupMode::parse("FS"), Some(CatchupMode::Flussonic));
    assert_eq!(CatchupMode::parse(""), None);
}

#[test]
fn append_query_joins_with_question_mark_or_ampersand() {
    assert_eq!(
        append_query("http://host/a.m3u8", "utc=1"),
        "http://host/a.m3u8?utc=1"
    );
    assert_eq!(
        append_query("http://host/a.m3u8?k=v", "utc=1"),
        "http://host/a.m3u8?k=v&utc=1"
    );
    assert_eq!(
        append_query("http://host/a.m3u8?k=v", "?utc=1"),
        "http://host/a.m3u8?k=v&utc=1"
    );
    assert_eq!(
        append_query("http://host/a.m3u8?", "&utc=1"),
        "http://host/a.m3u8?utc=1"
    );
}

#[test]
fn append_query_keeps_a_fragment_at_the_end() {
    assert_eq!(
        append_query("http://host/a.m3u8#x", "utc=1"),
        "http://host/a.m3u8?utc=1#x"
    );
}

#[test]
fn flussonic_turns_an_hls_playlist_into_the_fixed_window() {
    assert_eq!(
        flussonic_source("http://host/ch1/mono.m3u8?k=v").as_deref(),
        Some("http://host/ch1/mono-{utc}-{duration}.m3u8?k=v")
    );
    assert_eq!(
        flussonic_source("https://host/ch1/index.m3u8").as_deref(),
        Some("https://host/ch1/index-{utc}-{duration}.m3u8")
    );
}

#[test]
fn flussonic_turns_an_mpegts_stream_into_the_archive_file() {
    assert_eq!(
        flussonic_source("http://host/ch1/mpegts?k=v").as_deref(),
        Some("http://host/ch1/archive-{utc}-{duration}.ts?k=v")
    );
}

#[test]
fn flussonic_returns_nothing_for_a_url_without_its_shape() {
    assert_eq!(flussonic_source("http://host/ch1/stream.mp4"), None);
    assert_eq!(flussonic_source("not a url"), None);
}

const URL: &str = "http://host/ch1/mono.m3u8?k=v";

#[test]
fn template_appends_the_siptv_query_in_shift_mode() {
    assert_eq!(
        template(Some(CatchupMode::Shift), URL, None).as_deref(),
        Some("http://host/ch1/mono.m3u8?k=v&utc={utc}&lutc={lutc}")
    );
}

#[test]
fn template_appends_the_declared_source_in_append_mode() {
    assert_eq!(
        template(Some(CatchupMode::Append), URL, Some("&cutv={Y}-{m}-{d}")).as_deref(),
        Some("http://host/ch1/mono.m3u8?k=v&cutv={Y}-{m}-{d}")
    );
    assert_eq!(template(Some(CatchupMode::Append), URL, Some("")), None);
}

#[test]
fn template_derives_the_archive_url_in_flussonic_mode() {
    assert_eq!(
        template(Some(CatchupMode::Flussonic), URL, None).as_deref(),
        Some("http://host/ch1/mono-{utc}-{duration}.m3u8?k=v")
    );
}

#[test]
fn template_keeps_a_declared_source_over_a_derived_one() {
    let declared = Some("http://host/c?t={utc}");
    for mode in [CatchupMode::Shift, CatchupMode::Flussonic] {
        assert_eq!(template(Some(mode), URL, declared).as_deref(), declared);
    }
}

#[test]
fn template_uses_the_declared_source_as_is_in_default_mode_and_without_a_mode() {
    let declared = Some("http://host/c?t={utc}");
    assert_eq!(
        template(Some(CatchupMode::Default), URL, declared).as_deref(),
        declared
    );
    assert_eq!(template(None, URL, None), None);
}

#[test]
fn fill_fills_the_epoch_specifiers_and_their_aliases() {
    assert_eq!(
        fill(
            "a={utc}&b=${start}&c={utcend}&d=${end}&e={lutc}&f=${now}&g=${timestamp}",
            TIMES,
            utc
        ),
        "a=1700000000&b=1700000000&c=1700003600&d=1700003600&e=1700010000&f=1700010000&g=1700010000"
    );
}

#[test]
fn fill_fills_every_occurrence() {
    assert_eq!(fill("{utc}/{utc}", TIMES, utc), "1700000000/1700000000");
}

#[test]
fn fill_fills_duration_and_offset_with_optional_dividers() {
    assert_eq!(
        fill("{duration} ${duration} {duration:60}", TIMES, utc),
        "3600 3600 60"
    );
    assert_eq!(fill("${offset} {offset:60}", TIMES, utc), "10000 166");
}

#[test]
fn fill_treats_a_zero_divider_as_one() {
    assert_eq!(fill("{duration:0}", TIMES, utc), "3600");
}

#[test]
fn fill_fills_start_date_parts_in_local_time() {
    let parts = "{Y}-{m}-{d}T{H}:{M}:{S}";
    assert_eq!(fill(parts, TIMES, utc), "2023-11-14T22:13:20");
    assert_eq!(fill(parts, TIMES, plus_3h), "2023-11-15T01:13:20");
    assert_eq!(fill(parts, TIMES, |_| -5 * 3_600), "2023-11-14T17:13:20");
}

#[test]
fn fill_formats_timestamp_specifiers_that_carry_a_format_string() {
    assert_eq!(
        fill("?start={utc:YmdHM}&end=${end:Ymd-H-M}", TIMES, plus_3h),
        "?start=202311150113&end=20231115-02-13"
    );
}

#[test]
fn fill_leaves_unknown_placeholders_alone() {
    assert_eq!(
        fill("{start}/{channel-id}/{x}", TIMES, utc),
        "{start}/{channel-id}/{x}"
    );
}

#[test]
fn fill_writes_a_literal_dollar_only_where_the_template_has_one() {
    assert_eq!(fill("p=$x&t=${start}", TIMES, utc), "p=$x&t=1700000000");
}

// Beyond the ported cases.

#[test]
fn flussonic_keeps_a_fragment() {
    assert_eq!(
        flussonic_source("http://host/ch1/mono.m3u8?k=v#f").as_deref(),
        Some("http://host/ch1/mono-{utc}-{duration}.m3u8?k=v#f")
    );
}

#[test]
fn fill_reads_dollar_forms_as_the_ts_port_does() {
    // `${utc}` and `{start}` stay literal (Kodi's substring replace would fill the `{utc}`
    // inside `${utc}`); `{duration:N}` and date parts after a `$` keep the `$`.
    assert_eq!(fill("${utc}{start}", TIMES, utc), "${utc}{start}");
    assert_eq!(fill("${duration:60}${Y}", TIMES, utc), "$60$2023");
    assert_eq!(fill("{offset}{lutc:H}", TIMES, utc), "{offset}01");
}

#[test]
fn fill_takes_the_offset_of_each_instant_across_a_dst_change() {
    // UTC+1 until a switch between start and end, UTC+2 after it.
    let offset_at = |t: Timestamp| if t < 1_700_001_800 { 3_600 } else { 7_200 };
    assert_eq!(
        fill("{utc:Ymd-H}/{utcend:Ymd-H}/{H}", TIMES, offset_at),
        "20231114-23/20231115-01/23"
    );
}

fn parsed_channel(extinf: &str, url: &str) -> Channel {
    let text = format!("#EXTM3U\n#EXTINF:0 {extinf},Alpha\n{url}\n");
    m3u::parse(text.as_bytes()).unwrap().0.channels.remove(0)
}

#[test]
fn archive_url_prefers_the_flussonic_window_for_an_ended_programme() {
    for name in ["mono", "index"] {
        let ch = parsed_channel(
            "timeshift=\"7\"",
            &format!("http://host/ch1/{name}.m3u8?token=t#f"),
        );
        assert_eq!(
            archive_url(&ch, TIMES, utc).as_deref(),
            Some(format!("http://host/ch1/{name}-1700000000-3600.m3u8?token=t#f").as_str())
        );
    }
}

#[test]
fn archive_url_keeps_shift_on_air_on_other_servers_and_for_declared_sources() {
    let shift = "http://host/ch1/mono.m3u8?token=t&utc=1700000000&lutc=1700001000#f";
    let ch = parsed_channel("timeshift=\"7\"", "http://host/ch1/mono.m3u8?token=t#f");
    let on_air = Times {
        now: 1_700_001_000,
        ..TIMES
    };
    assert_eq!(archive_url(&ch, on_air, utc).as_deref(), Some(shift));

    let ch = parsed_channel("timeshift=\"7\"", "http://host/ch1/playlist.m3u8");
    assert_eq!(
        archive_url(&ch, TIMES, utc).as_deref(),
        Some("http://host/ch1/playlist.m3u8?utc=1700000000&lutc=1700010000")
    );

    let ch = parsed_channel(
        "catchup=\"shift\" catchup-source=\"http://host/a?s={utc}\" catchup-days=\"1\"",
        "http://host/ch1/mono.m3u8",
    );
    assert_eq!(
        archive_url(&ch, TIMES, utc).as_deref(),
        Some("http://host/a?s=1700000000")
    );
}

#[test]
fn archive_url_fills_the_channel_id() {
    let source = "catchup=\"default\" catchup-source=\"http://host/{channel-id}/{utc}\"";
    let ch = parsed_channel(&format!("{source} tvg-id=\"a b/c\""), "http://host/live");
    assert_eq!(
        archive_url(&ch, TIMES, utc).as_deref(),
        Some("http://host/a%20b%2Fc/1700000000")
    );
    let ch = parsed_channel(source, "http://host/live");
    assert_eq!(
        archive_url(&ch, TIMES, utc).as_deref(),
        Some("http://host/Alpha/1700000000")
    );
}

#[test]
fn archive_url_needs_a_template() {
    let ch = parsed_channel(
        "catchup=\"xc\" catchup-days=\"3\"",
        "http://host/live/u/p/1.ts",
    );
    assert_eq!(archive_url(&ch, TIMES, utc), None);
}

fn programme(start: Timestamp, stop: Timestamp) -> Programme {
    Programme {
        start,
        stop,
        ..Programme::default()
    }
}

#[test]
fn in_archive_counts_from_the_start_within_the_days() {
    let ch = parsed_channel("timeshift=\"1\"", "http://host/ch1/mono.m3u8");
    let (start, now) = (TIMES.start, TIMES.start + 86_400);
    // Ended, and started exactly one day ago.
    assert!(in_archive(&ch, &programme(start, start + 3_600), now));
    // Started a second too early.
    assert!(!in_archive(&ch, &programme(start - 1, start + 3_600), now));
    // On air: start-over plays it from the archive.
    assert!(in_archive(&ch, &programme(now - 60, now + 60), now));
    // Not started yet.
    assert!(!in_archive(&ch, &programme(now, now + 60), now));
}

#[test]
fn in_archive_needs_a_template() {
    let ch = parsed_channel(
        "catchup=\"xc\" catchup-days=\"3\"",
        "http://host/live/u/p/1.ts",
    );
    assert!(!in_archive(
        &ch,
        &programme(TIMES.start, TIMES.end),
        TIMES.now
    ));
}
