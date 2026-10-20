//! XMLTV parser tests. The first 23 are ported to Rust from webOS IPTV Player
//! `src/parsers/xmltv-parser.test.ts` (commit 7d8fd30, Copyright 2026 lennylxx, Apache-2.0; see
//! THIRD-PARTY-NOTICES.md) and changed; the rest cover the input adapter and review regressions.

mod common;

use std::collections::HashSet;
use std::convert::Infallible;
use std::io::{self, BufRead, Read};

use common::{date, gzip};
use iptv::model::{EpgChannel, Programme, Timestamp};
use iptv::xmltv::{self, Options, Sink, Stats, Summary};

const NOW: Timestamp = 1_780_000_000;
const HOUR: i64 = 3600;
const DAY: i64 = 86_400;

/// The parsed guide in the web app's shape: channels and programme lists by id, in first-seen
/// order, each list sorted by start (stable, so equal starts keep document order).
#[derive(Debug, Default, PartialEq)]
struct Guide {
    channels: Vec<(String, EpgChannel)>,
    programmes: Vec<(String, Vec<Programme>)>,
}

impl Guide {
    fn channel_ids(&self) -> Vec<&str> {
        self.channels.iter().map(|(id, _)| id.as_str()).collect()
    }

    fn channel(&self, id: &str) -> Option<&EpgChannel> {
        self.channels.iter().find(|(c, _)| c == id).map(|(_, c)| c)
    }

    fn programme_ids(&self) -> Vec<&str> {
        self.programmes.iter().map(|(id, _)| id.as_str()).collect()
    }

    fn titles(&self, id: &str) -> Option<Vec<&str>> {
        let (_, list) = self.programmes.iter().find(|(c, _)| c == id)?;
        Some(list.iter().map(|p| p.title.as_str()).collect())
    }
}

#[derive(Default)]
struct Collect {
    /// Keep channels outside the filter too (the web app's `retainChannelCatalog`).
    catalog: bool,
    guide: Guide,
    /// Ids of the channels sent as wanted, for the order retry.
    wanted: HashSet<String>,
}

impl Sink for Collect {
    type Error = Infallible;

    fn channel(&mut self, channel: EpgChannel, wanted: bool) -> Result<(), Infallible> {
        if wanted {
            self.wanted.insert(channel.id.clone());
        }
        if wanted || self.catalog {
            self.guide.channels.push((channel.id.clone(), channel));
        }
        Ok(())
    }

    fn programme(&mut self, channel_id: &str, programme: Programme) -> Result<(), Infallible> {
        let programmes = &mut self.guide.programmes;
        match programmes.iter_mut().find(|(id, _)| id == channel_id) {
            Some((_, list)) => list.push(programme),
            None => programmes.push((channel_id.to_owned(), vec![programme])),
        }
        Ok(())
    }
}

fn run(input: impl BufRead, options: &Options, catalog: bool) -> (Guide, Summary) {
    let (guide, summary, _) = run_wanted(input, options, catalog);
    (guide, summary)
}

/// [`run`], and the ids of the channels it sent as wanted.
fn run_wanted(
    input: impl BufRead,
    options: &Options,
    catalog: bool,
) -> (Guide, Summary, HashSet<String>) {
    let mut sink = Collect {
        catalog,
        ..Collect::default()
    };
    let summary = match xmltv::parse(input, options, &mut sink) {
        Ok(summary) => summary,
        Err(xmltv::Error::Read(e)) => panic!("read failed: {e}"),
    };
    let mut guide = sink.guide;
    for (_, list) in &mut guide.programmes {
        list.sort_by_key(|p| p.start);
    }
    (guide, summary, sink.wanted)
}

/// One pass plus the order retry, like the web app's `parseXMLTVWithStats`.
fn parse_with(xml: &str, options: &Options, catalog: bool) -> (Guide, Summary) {
    let (guide, summary, wanted) = run_wanted(xml.as_bytes(), options, catalog);
    if !summary.retry {
        return (guide, summary);
    }
    let retry = Options {
        ids: options.ids.union(&wanted).cloned().collect(),
        ..options.clone()
    };
    run(xml.as_bytes(), &retry, catalog)
}

fn parse(xml: &str) -> (Guide, Summary) {
    parse_with(xml, &Options::new(NOW), false)
}

fn filter(ids: &[&str], names: &[&str]) -> Options {
    Options {
        ids: ids.iter().map(|s| s.to_string()).collect(),
        names: names.iter().map(|s| s.to_string()).collect(),
        ..Options::new(NOW)
    }
}

/// Serves `data` up to `split` first, then the rest `step` bytes at a time.
struct Chunked<'a> {
    data: &'a [u8],
    pos: usize,
    split: usize,
    step: usize,
}

impl<'a> Chunked<'a> {
    fn new(data: &'a [u8], split: usize, step: usize) -> Self {
        Chunked {
            data,
            pos: 0,
            split,
            step,
        }
    }
}

impl Read for Chunked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Chunked<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        let end = if self.pos < self.split {
            self.split
        } else {
            self.pos.saturating_add(self.step)
        };
        Ok(&self.data[self.pos..end.min(self.data.len())])
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
    }
}

fn one_hour(channel: &str, title: &str) -> String {
    format!(
        r#"<programme channel="{channel}" start="{}" stop="{}"><title>{title}</title></programme>"#,
        date(NOW),
        date(NOW + HOUR)
    )
}

// --- parseXMLTV ---------------------------------------------------------------------------

#[test]
fn reads_a_source_name_from_the_root() {
    let (_, summary) = parse(
        r#"<?xml version="1.0"?>
      <tv source-info-name="Guide Alpha" generator-info-name="Generator">
      </tv>"#,
    );
    assert_eq!(summary.source_name.as_deref(), Some("Guide Alpha"));
}

#[test]
fn falls_back_to_the_generator_name() {
    let (_, summary) = parse(r#"<tv generator-info-name="Generator Alpha"></tv>"#);
    assert_eq!(summary.source_name.as_deref(), Some("Generator Alpha"));
}

#[test]
fn parses_channels_with_display_names_and_icons() {
    let (guide, _) = parse(
        r#"<?xml version="1.0"?>
      <tv>
        <channel id="chan1">
          <display-name>Channel One</display-name>
          <icon src="http://logo/chan1.png"/>
        </channel>
      </tv>"#,
    );
    assert_eq!(
        guide.channel("chan1"),
        Some(&EpgChannel {
            id: "chan1".into(),
            names: vec!["Channel One".into()],
            icon: Some("http://logo/chan1.png".into()),
        })
    );
}

#[test]
fn parses_programmes_inside_the_window() {
    let xml = format!(
        r#"<?xml version="1.0"?>
      <tv>
        <channel id="c1"><display-name>Chan</display-name></channel>
        <programme channel="c1" start="{}" stop="{}">
          <title>The Show</title>
          <desc>An episode</desc>
        </programme>
      </tv>"#,
        date(NOW),
        date(NOW + HOUR)
    );
    let (guide, _) = parse(&xml);
    assert_eq!(guide.titles("c1"), Some(vec!["The Show"]));
    let programme = &guide.programmes[0].1[0];
    assert_eq!(programme.description.as_deref(), Some("An episode"));
    assert_eq!((programme.start, programme.stop), (NOW, NOW + HOUR));
}

#[test]
fn drops_programmes_far_outside_the_window() {
    let (guide, summary) = parse(
        r#"<?xml version="1.0"?>
      <tv>
        <channel id="c1"><display-name>Chan</display-name></channel>
        <programme channel="c1" start="20000101000000 +0000" stop="20000101010000 +0000">
          <title>Ancient</title>
        </programme>
      </tv>"#,
    );
    assert_eq!(guide.titles("c1"), None);
    assert_eq!(summary.stats.skipped_range, 1);
}

#[test]
fn decodes_entities_cdata_nested_text_and_stray_ampersands() {
    let xml = format!(
        r#"<tv>
      <channel id="c&amp;1">
        <display-name><![CDATA[Alpha & Bravo]]></display-name>
        <icon src="http://host/a?x=1&amp;y=2"/>
      </channel>
      <programme channel="c&amp;1" start="{}" stop="{}">
        <title>Alpha & Bravo &amp; <b>Charlie</b></title>
        <desc><![CDATA[One &amp; Two]]></desc>
        <category>News &#x26; Talk</category>
      </programme>
    </tv>"#,
        date(NOW),
        date(NOW + HOUR)
    );
    let (guide, _) = parse(&xml);
    assert_eq!(
        guide.channel("c&1"),
        Some(&EpgChannel {
            id: "c&1".into(),
            names: vec!["Alpha & Bravo".into()],
            icon: Some("http://host/a?x=1&y=2".into()),
        })
    );
    let programme = &guide.programmes[0].1[0];
    assert_eq!(guide.programmes[0].0, "c&1");
    assert_eq!(programme.title, "Alpha & Bravo & Charlie");
    assert_eq!(programme.description.as_deref(), Some("One &amp; Two"));
    assert_eq!(programme.category.as_deref(), Some("News & Talk"));
}

#[test]
fn accepts_single_quotes_and_whitespace_around_attribute_equals() {
    let xml = format!(
        "<tv>
      <channel id = 'c1'><display-name>Alpha</display-name></channel>
      <programme channel = 'c1' start = '{}' stop = '{}'>
        <title>First</title>
      </programme>
    </tv>",
        date(NOW),
        date(NOW + HOUR)
    );
    assert_eq!(parse(&xml).0.titles("c1"), Some(vec!["First"]));
}

#[test]
fn parses_every_possible_chunk_boundary_identically() {
    let xml = format!(
        r#"<?xml version="1.0"?><!DOCTYPE tv [<!ELEMENT tv ANY>]><tv>
      <!-- <programme channel="ignored"> -->
      <channel id="c1"><display-name>Alpha</display-name><icon src="http://host/a"/></channel>
      <programme channel="c1" start="{}" stop="{}">
        <title>First &amp; Best</title><desc>Long enough description</desc>
      </programme>
    </tv>"#,
        date(NOW),
        date(NOW + HOUR)
    );
    let options = Options::new(NOW);
    let expected = run(xml.as_bytes(), &options, false);
    assert_eq!(expected.0.titles("c1"), Some(vec!["First & Best"]));
    assert_eq!(expected.0.channel_ids(), ["c1"]);
    for split in 1..xml.len() {
        let got = run(
            Chunked::new(xml.as_bytes(), split, usize::MAX),
            &options,
            false,
        );
        assert_eq!(got, expected, "split at {split}");
    }
}

#[test]
fn filters_channels_before_retaining_their_programmes() {
    let xml = format!(
        r#"<tv>
      <channel id="c1"><display-name>Alpha</display-name></channel>
      <channel id="c2"><display-name>Bravo</display-name></channel>
      {}{}
    </tv>"#,
        one_hour("c1", "One"),
        one_hour("c2", "Two")
    );
    let (guide, _) = parse_with(&xml, &filter(&["c2"], &[]), false);
    assert_eq!(guide.channel_ids(), ["c2"]);
    assert_eq!(guide.programme_ids(), ["c2"]);
}

#[test]
fn can_retain_the_channel_catalog_while_filtering_programmes() {
    let xml = format!(
        r#"<tv>
      <channel id="c1"><display-name>Alpha</display-name></channel>
      <channel id="c2"><display-name>Bravo</display-name></channel>
      {}{}
    </tv>"#,
        one_hour("c1", "One"),
        one_hour("c2", "Two")
    );
    let (guide, _) = parse_with(&xml, &filter(&["c2"], &[]), true);
    assert_eq!(guide.channel_ids(), ["c1", "c2"]);
    assert_eq!(guide.programme_ids(), ["c2"]);
}

#[test]
fn sorts_unordered_schedules_and_keeps_equal_starts_in_input_order() {
    let half = |start: Timestamp, title: &str| {
        format!(
            r#"<programme channel="c1" start="{}" stop="{}"><title>{title}</title></programme>"#,
            date(start),
            date(start + HOUR / 2)
        )
    };
    let xml = format!(
        "<tv>{}{}{}</tv>",
        half(NOW, "Later A"),
        half(NOW - HOUR, "Earlier"),
        half(NOW, "Later B")
    );
    assert_eq!(
        parse(&xml).0.titles("c1"),
        Some(vec!["Earlier", "Later A", "Later B"])
    );
}

#[test]
fn captures_the_first_retained_timezone_and_validates_dates() {
    let start = date(NOW).replace("+0000", "+0530");
    let stop = date(NOW + HOUR).replace("+0000", "+0530");
    let xml = format!(
        r#"<tv>
      <programme channel="bad" start="20260230000000 +0000" stop="{stop}"><title>Bad</title></programme>
      <programme channel="c1" start="{start}" stop="{stop}"><title>Good</title></programme>
    </tv>"#
    );
    let (guide, summary) = parse(&xml);
    assert_eq!(summary.tz_offset_minutes, Some(330));
    assert_eq!(guide.titles("bad"), None);
    assert_eq!(guide.programmes[0].1[0].start, NOW - 330 * 60);
}

#[test]
fn continues_past_malformed_and_incomplete_elements() {
    let xml = format!(
        r#"<tv>
      <programme channel="c1" start="bad" stop="bad"><title>Bad</title></programme>
      {}
      <programme channel="c1"><title>Incomplete"#,
        one_hour("c1", "Good")
    );
    let (guide, summary) = run(xml.as_bytes(), &Options::new(NOW), false);
    assert_eq!(guide.titles("c1"), Some(vec!["Good"]));
    assert_eq!(summary.stats.skipped_date, 1);
    assert!(summary.stats.malformed > 0);
}

// --- parseXMLTV channel filter ------------------------------------------------------------

fn filter_feed() -> String {
    format!(
        r#"<tv>
      <channel id="c1"><display-name>Alpha</display-name></channel>
      <channel id="c2"><display-name>Bravo</display-name><display-name>Bravo HD</display-name></channel>
      <channel id="c3"><display-name>Charlie</display-name></channel>
      {}{}{}
    </tv>"#,
        one_hour("c1", "A"),
        one_hour("c2", "B"),
        one_hour("c3", "C")
    )
}

#[test]
fn keeps_only_the_requested_ids() {
    let (guide, summary) = run(filter_feed().as_bytes(), &filter(&["c1"], &[]), false);
    assert_eq!(guide.channel_ids(), ["c1"]);
    assert_eq!(guide.programme_ids(), ["c1"]);
    assert_eq!(summary.stats.programmes_seen, 3);
    assert_eq!(summary.stats.programmes_kept, 1);
    assert!(summary.stats.skipped_filter > 0);
}

#[test]
fn admits_a_channel_matched_by_any_display_name_with_its_programmes() {
    let (guide, _) = parse_with(&filter_feed(), &filter(&[], &["bravo hd"]), false);
    assert_eq!(guide.channel_ids(), ["c2"]);
    assert_eq!(guide.channel("c2").unwrap().names, ["Bravo", "Bravo HD"]);
    assert_eq!(guide.titles("c2"), Some(vec!["B"]));
}

#[test]
fn matches_programmes_declared_before_their_channel() {
    let xml = format!(
        r#"<tv>
      {}
      <channel id="c2"><display-name>Bravo</display-name><display-name>Bravo HD</display-name></channel>
    </tv>"#,
        one_hour("c2", "B")
    );
    let options = filter(&[], &["bravo hd"]);
    let (first, summary, wanted) = run_wanted(xml.as_bytes(), &options, false);
    assert_eq!(first.titles("c2"), None);
    assert!(summary.retry);
    assert_eq!(wanted, ["c2".to_string()].into_iter().collect());
    let (guide, summary) = parse_with(&xml, &options, false);
    assert_eq!(guide.titles("c2"), Some(vec!["B"]));
    assert!(!summary.retry);
}

#[test]
fn matches_and_retains_a_requested_display_name_past_the_cap() {
    let xml = format!(
        r#"<tv>
      <channel id="c4">
        <display-name>One</display-name><display-name>Two</display-name>
        <display-name>Three</display-name><display-name>Four</display-name>
        <display-name>Five</display-name>
      </channel>
      {}
    </tv>"#,
        one_hour("c4", "D")
    );
    let (guide, _) = parse_with(&xml, &filter(&[], &["five"]), false);
    assert_eq!(
        guide.channel("c4").unwrap().names,
        ["One", "Two", "Three", "Four", "Five"]
    );
    assert_eq!(guide.titles("c4"), Some(vec!["D"]));
}

#[test]
fn counts_filter_matches_before_discarding_out_of_range_programmes() {
    let options = Options {
        now: NOW + 30 * DAY,
        ..filter(&[], &["bravo hd"])
    };
    let (_, summary) = run(filter_feed().as_bytes(), &options, false);
    assert_eq!(summary.stats.channels_kept, 1);
    assert_eq!(summary.stats.programmes_matched(), 1);
    assert_eq!(summary.stats.programmes_kept, 0);
}

#[test]
fn unions_ids_and_names() {
    let (guide, _) = parse_with(&filter_feed(), &filter(&["c1"], &["charlie"]), false);
    let mut ids = guide.programme_ids();
    ids.sort_unstable();
    assert_eq!(ids, ["c1", "c3"]);
}

#[test]
fn keeps_everything_when_no_filter_is_configured() {
    let (guide, _) = parse_with(&filter_feed(), &filter(&[], &[]), false);
    let mut ids = guide.programme_ids();
    ids.sort_unstable();
    assert_eq!(ids, ["c1", "c2", "c3"]);
}

// --- parseXMLTV retention ceiling ---------------------------------------------------------

fn ceiling_feed(count: i64) -> String {
    let mut xml = String::from("<tv>");
    for i in 0..count {
        xml += &format!(
            r#"<channel id="c{i}"><display-name>Ch {i}</display-name></channel><programme channel="c{i}" start="{}" stop="{}"><title>P{i}</title></programme>"#,
            date(NOW + i * 60),
            date(NOW + (i + 1) * 60)
        );
    }
    xml + "</tv>"
}

fn budget(max: usize) -> Options {
    Options {
        max_programmes: max,
        ..Options::new(NOW)
    }
}

#[test]
fn stops_retaining_past_the_ceiling_and_counts_what_it_dropped() {
    let (guide, summary) = parse_with(&ceiling_feed(10), &budget(4), false);
    assert_eq!(summary.stats.programmes_kept, 4);
    assert_eq!(summary.stats.dropped_budget, 6);
    assert_eq!(summary.stats.programmes_seen, 10);
    assert_eq!(guide.programme_ids(), ["c0", "c1", "c2", "c3"]);
}

#[test]
fn leaves_the_channel_catalog_complete_past_the_ceiling() {
    let (guide, _) = parse_with(&ceiling_feed(10), &budget(4), false);
    assert_eq!(guide.channels.len(), 10);
}

#[test]
fn retains_everything_with_the_default_ceiling() {
    let (_, summary) = parse(&ceiling_feed(10));
    assert_eq!(summary.stats.programmes_kept, 10);
    assert_eq!(summary.stats.dropped_budget, 0);
}

// --- input adapter ------------------------------------------------------------------------

fn small_guide() -> String {
    format!(
        "<tv><channel id='ch1'><display-name>Alpha &#233; \u{17c}</display-name></channel>{}</tv>",
        one_hour("ch1", "Bravo \u{e9} \u{15b}\u{107}")
    )
}

#[test]
fn gzip_is_detected_by_magic_bytes_and_decoded_incrementally() {
    let xml = small_guide();
    let packed = gzip(xml.as_bytes());
    let (plain, plain_summary) = run(xml.as_bytes(), &Options::new(NOW), false);
    let (guide, summary) = run(Chunked::new(&packed, 1, 5), &Options::new(NOW), false);
    assert!(summary.gzip && !plain_summary.gzip);
    assert_eq!(guide, plain);
    assert_eq!(
        guide.channel("ch1").unwrap().names,
        ["Alpha \u{e9} \u{17c}"]
    );
    assert_eq!(
        guide.titles("ch1"),
        Some(vec!["Bravo \u{e9} \u{15b}\u{107}"])
    );
}

#[test]
fn concatenated_gzip_members_are_one_guide() {
    let xml = small_guide();
    let (head, tail) = xml.split_at(xml.len() / 2);
    let packed = [gzip(head.as_bytes()), gzip(tail.as_bytes())].concat();
    let (guide, _) = run(packed.as_slice(), &Options::new(NOW), false);
    assert_eq!(guide.titles("ch1").map(|t| t.len()), Some(1));
}

#[test]
fn multibyte_text_survives_every_chunk_boundary() {
    let xml = small_guide();
    let expected = run(xml.as_bytes(), &Options::new(NOW), false);
    for split in 1..xml.len() {
        let got = run(
            Chunked::new(xml.as_bytes(), split, 3),
            &Options::new(NOW),
            false,
        );
        assert_eq!(got.0, expected.0, "split at {split}");
    }
}

#[test]
fn invalid_utf8_is_replaced_and_parsing_goes_on() {
    let xml = format!(
        "<tv>{}{}</tv>",
        one_hour("c1", "Caf?"),
        one_hour("c1", "Next")
    );
    let mut bytes = xml.into_bytes();
    let at = bytes.iter().position(|&b| b == b'?').unwrap();
    bytes[at] = 0xE9; // Latin-1 e-acute, not UTF-8
    let (guide, _) = run(bytes.as_slice(), &Options::new(NOW), false);
    assert_eq!(guide.titles("c1"), Some(vec!["Caf\u{FFFD}", "Next"]));
}

#[test]
fn a_bom_and_an_empty_input_are_fine() {
    let xml = format!("\u{FEFF}{}", small_guide());
    assert_eq!(parse(&xml).0.channel_ids(), ["ch1"]);
    let (guide, summary) = run(&b""[..], &Options::new(NOW), false);
    assert_eq!(guide, Guide::default());
    assert_eq!(summary.stats, Stats::default());
}

#[test]
fn text_fields_are_capped() {
    let long = "x".repeat(10_000);
    let xml = format!(
        r#"<tv><channel id="c1"><display-name>{long}</display-name></channel>{}</tv>"#,
        one_hour("c1", &long)
    )
    .replace("</title>", &format!("</title><desc>{long}</desc>"));
    let (guide, _) = parse(&xml);
    let programme = &guide.programmes[0].1[0];
    assert_eq!(guide.channel("c1").unwrap().names[0].len(), 256);
    assert_eq!(programme.title.len(), 512);
    assert_eq!(programme.description.as_ref().unwrap().len(), 2048);
}

#[test]
fn an_oversized_event_aborts_the_parse() {
    let xml = format!("<tv><!-- {} --></tv>", "x".repeat(2 << 20));
    let mut sink = Collect::default();
    let result = xmltv::parse(xml.as_bytes(), &Options::new(NOW), &mut sink);
    assert!(matches!(result, Err(xmltv::Error::Read(_))));
}

#[test]
fn self_closing_and_attribute_less_elements_are_handled() {
    let xml = format!(
        r#"<tv><channel id="c1"/><channel><display-name>No id</display-name></channel>
        <programme channel="c1" start="{}" stop="{}"/><programme start="{}"><title>No channel</title></programme></tv>"#,
        date(NOW),
        date(NOW + HOUR),
        date(NOW)
    );
    let (guide, summary) = parse(&xml);
    assert_eq!(guide.channel("c1").unwrap().names, ["c1"]);
    assert_eq!(guide.titles("c1"), Some(vec![""]));
    assert_eq!(summary.stats.malformed, 2);
    assert_eq!(summary.stats.channels_seen, 2);
}

// --- review regressions -------------------------------------------------------------------

fn described(start: Timestamp, title: &str, desc: &str) -> String {
    format!(
        r#"<programme channel="c1" start="{}" stop="{}"><title>{title}</title><desc>{desc}</desc></programme>"#,
        date(start),
        date(start + HOUR)
    )
}

#[test]
fn an_unclosed_inner_tag_does_not_swallow_later_programmes() {
    let mut xml = String::from("<tv>");
    xml += &described(NOW, "One", "Line<br>two");
    for i in 0..5 {
        xml += &described(NOW + (i + 1) * HOUR, &format!("P{i}"), "ok");
    }
    xml += "</tv>";
    let (guide, summary) = parse(&xml);
    assert_eq!(
        guide.titles("c1"),
        Some(vec!["One", "P0", "P1", "P2", "P3", "P4"])
    );
    assert_eq!(
        guide.programmes[0].1[0].description.as_deref(),
        Some("Linetwo")
    );
    assert!(summary.complete);
}

#[test]
fn many_unclosed_inner_tags_stay_within_the_depth_cap() {
    let mut xml = String::from("<tv>");
    for i in 0..500 {
        xml += &described(NOW - DAY + i * 60, "Show", "a<br>b<p>c");
    }
    xml += "</tv>";
    let (_, summary) = parse(&xml);
    assert_eq!(summary.stats.programmes_kept, 500);
}

#[test]
fn a_channel_or_programme_start_ends_an_unfinished_element() {
    let xml = format!(
        r#"<tv><channel id="c1"><display-name>Alpha</display-name>{}{}</tv>"#,
        r#"<programme channel="c1" start="20000101000000" stop="20000101010000"><title>Old"#,
        one_hour("c1", "Good")
    );
    let (guide, summary) = parse(&xml);
    assert_eq!(guide.titles("c1"), Some(vec!["Good"]));
    assert_eq!(summary.stats.malformed, 2);
    assert!(summary.complete);
}

#[test]
fn deep_nesting_aborts_the_parse() {
    let xml = format!("<tv>{}</tv>", "<a>".repeat(10_000));
    let mut sink = Collect::default();
    let result = xmltv::parse(xml.as_bytes(), &Options::new(NOW), &mut sink);
    assert!(matches!(result, Err(xmltv::Error::Read(_))));
}

#[test]
fn a_syntax_error_aborts_and_a_cut_guide_is_incomplete() {
    let xml = format!(
        "<tv>{}{}</tv>",
        described(NOW, "One", "Wow <!amazing"),
        described(NOW + HOUR, "Two", "ok")
    );
    let mut sink = Collect::default();
    let result = xmltv::parse(xml.as_bytes(), &Options::new(NOW), &mut sink);
    assert!(matches!(result, Err(xmltv::Error::Read(_))));

    let mut full = String::from("<tv>");
    for i in 0..6 {
        full += &described(NOW + i * HOUR, &format!("P{i}"), "ok");
    }
    full += "</tv>";
    assert!(parse(&full).1.complete);
    // Cut inside a tag it is a syntax error, cut elsewhere an incomplete guide.
    for cut in 1..full.len() {
        let mut sink = Collect::default();
        let result = xmltv::parse(&full.as_bytes()[..cut], &Options::new(NOW), &mut sink);
        assert!(result.map_or(true, |s| !s.complete), "cut at {cut}");
    }
}

#[test]
fn programmes_that_do_not_last_are_dropped_and_twelve_digit_times_parse() {
    let xml = format!(
        r#"<tv><programme channel="c1" start="{0}" stop="{0}"><title>Zero</title></programme>
        <programme channel="c1" start="{1}" stop="{0}"><title>Backwards</title></programme>
        <programme channel="c1" start="202605282000 +0000" stop="202605282100 +0000"><title>Short form</title></programme></tv>"#,
        date(NOW),
        date(NOW + HOUR)
    );
    let (guide, summary) = parse(&xml);
    assert_eq!(guide.titles("c1"), Some(vec!["Short form"]));
    assert_eq!(summary.stats.skipped_date, 2);
    let programme = &guide.programmes[0].1[0];
    assert_eq!(programme.stop - programme.start, HOUR);
}
