//! Playlists, guide sources, guide import and the guide queries.

mod common;

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, BufRead, Read};
use std::time::{Duration, Instant};

use common::{date, gzip, TempDir};
use iptv::epg::{Match, Matcher, Override, PlaylistId, SourceId};
use iptv::model::{Catchup, CatchupMode, Channel, ChannelKey, Playlist, Programme, Timestamp};
use iptv::storage::Storage;
use iptv::xmltv::{self, Options};

const NOW: Timestamp = 1_780_000_000;
const HOUR: i64 = 3600;
const DAY: i64 = 86_400;
const GUIDE_URL: &str = "http://guide.example/epg.xml";

fn guide_xml(
    channels: &[(&str, &str)],
    programmes: &[(&str, Timestamp, Timestamp, &str)],
) -> String {
    let mut xml = String::from(r#"<tv source-info-name="Guide Alpha">"#);
    for (id, name) in channels {
        xml += &format!(r#"<channel id="{id}"><display-name>{name}</display-name></channel>"#);
    }
    for (channel, start, stop, title) in programmes {
        xml += &format!(
            r#"<programme channel="{channel}" start="{}" stop="{}"><title>{title}</title><desc>About {title}</desc></programme>"#,
            date(*start),
            date(*stop)
        );
    }
    xml + "</tv>"
}

fn channel(name: &str, tvg_id: &str) -> Channel {
    let url = format!("http://stream.example/{name}");
    Channel {
        key: ChannelKey::of_url(&url),
        name: name.into(),
        groups: vec!["News".into()],
        url,
        tvg_id: (!tvg_id.is_empty()).then(|| tvg_id.into()),
        ..Default::default()
    }
}

/// A playlist naming one guide, stored, and that guide's source id.
fn setup(storage: &Storage, channels: Vec<Channel>) -> (PlaylistId, SourceId) {
    let id = storage
        .add_playlist("Main", "http://playlist.example/list.m3u")
        .unwrap();
    let playlist = Playlist {
        name: None,
        channels,
        epg_urls: vec![GUIDE_URL.into()],
    };
    storage.save_playlist(id, &playlist, NOW).unwrap();
    (id, storage.guide_sources().unwrap()[0].id)
}

fn import(storage: &Storage, source: SourceId, xml: &str) -> iptv::storage::Imported {
    storage
        .import_guide(source, xml.as_bytes(), &Options::new(NOW))
        .unwrap()
}

fn find(storage: &Storage, playlist: PlaylistId, channel: &Channel) -> Option<Match> {
    let sources = storage.epg_sources().unwrap();
    Matcher::new(&sources).find(playlist, channel, None)
}

fn titles(programmes: &[Programme]) -> Vec<&str> {
    programmes.iter().map(|p| p.title.as_str()).collect()
}

// --- playlists and sources ---------------------------------------------------------------

#[test]
fn channels_round_trip_and_header_guides_are_linked() {
    let storage = Storage::open_in_memory().unwrap();
    let full = Channel {
        number: Some(101),
        logo: Some("http://logo.example/a.png".into()),
        groups: vec!["News".into(), "HD".into()],
        tvg_name: Some("Alpha Source".into()),
        tvg_shift_minutes: -90,
        catchup: Catchup {
            mode: Some(CatchupMode::Flussonic),
            days: 7,
            source: Some("{utc}".into()),
        },
        radio: true,
        http_headers: vec![("User-Agent".into(), "Agent/1".into())],
        extra: vec![("tvg-country".into(), "PL".into())],
        ..channel("Alpha", "alpha.id")
    };
    let plain = channel("Bravo", "");
    let (playlist, source) = setup(&storage, vec![full.clone(), plain.clone()]);
    assert_eq!(storage.channels(playlist).unwrap(), [full, plain]);
    let info = &storage.playlists().unwrap()[0];
    assert_eq!(
        (info.id, info.enabled, info.updated_at),
        (playlist, true, Some(NOW))
    );
    let guides = storage.guide_sources().unwrap();
    assert_eq!(guides.len(), 1);
    assert_eq!(
        (
            guides[0].id,
            guides[0].url.as_str(),
            guides[0].manual,
            &guides[0].playlists
        ),
        (source, GUIDE_URL, false, &vec![playlist])
    );
}

#[test]
fn the_same_playlist_again_is_not_rewritten() {
    let storage = Storage::open_in_memory().unwrap();
    let alpha = channel("Alpha", "a.id");
    let (playlist, _) = setup(&storage, vec![alpha.clone()]);
    let same = Playlist {
        name: None,
        channels: vec![alpha.clone()],
        epg_urls: vec![GUIDE_URL.into()],
    };
    assert!(!storage.save_playlist(playlist, &same, NOW + 1).unwrap());
    assert_eq!(storage.playlists().unwrap()[0].updated_at, Some(NOW + 1));
    let renamed = Playlist {
        channels: vec![Channel {
            name: "Alpha HD".into(),
            ..alpha
        }],
        ..same
    };
    assert!(storage.save_playlist(playlist, &renamed, NOW + 2).unwrap());
    assert_eq!(storage.channels(playlist).unwrap()[0].name, "Alpha HD");
    let no_guide = Playlist {
        epg_urls: vec![],
        ..renamed
    };
    assert!(storage.save_playlist(playlist, &no_guide, NOW + 3).unwrap());
    assert!(storage.guide_sources().unwrap().is_empty());
}

#[test]
fn every_catchup_mode_survives_storage() {
    let storage = Storage::open_in_memory().unwrap();
    let modes = [
        CatchupMode::Default,
        CatchupMode::Append,
        CatchupMode::Shift,
        CatchupMode::Flussonic,
        CatchupMode::Xc,
        CatchupMode::Vod,
    ];
    let channels: Vec<Channel> = modes
        .iter()
        .enumerate()
        .map(|(i, mode)| Channel {
            catchup: Catchup {
                mode: Some(*mode),
                days: 3,
                source: None,
            },
            ..channel(&format!("c{i}"), "")
        })
        .collect();
    let (playlist, _) = setup(&storage, channels.clone());
    assert_eq!(storage.channels(playlist).unwrap(), channels);
}

#[test]
fn removing_a_playlist_drops_only_the_guides_it_alone_named() {
    let storage = Storage::open_in_memory().unwrap();
    let (playlist, _) = setup(&storage, vec![channel("Alpha", "a")]);
    let manual = storage
        .add_manual_guide("http://manual.example/x.xml", &[])
        .unwrap();
    // A header naming a manual guide does not narrow the scope the user gave it.
    let other = storage
        .add_playlist("Other", "http://playlist.example/2.m3u")
        .unwrap();
    let names_manual = Playlist {
        epg_urls: vec!["http://manual.example/x.xml".into()],
        ..Playlist::default()
    };
    storage.save_playlist(other, &names_manual, NOW).unwrap();
    storage.remove_playlist(playlist).unwrap();
    let guides = storage.guide_sources().unwrap();
    assert_eq!(guides.len(), 1);
    assert_eq!((guides[0].id, guides[0].manual), (manual, true));
    assert!(guides[0].playlists.is_empty());
    assert_eq!(storage.playlists().unwrap().len(), 1);
}

// --- import and queries ------------------------------------------------------------------

fn sample() -> String {
    guide_xml(
        &[("a.id", "Alpha"), ("b.id", "Bravo"), ("quiet", "Quiet")],
        &[
            ("a.id", NOW - 2 * HOUR, NOW - HOUR, "Past"),
            ("a.id", NOW - HOUR, NOW + HOUR, "Now"),
            ("a.id", NOW + HOUR, NOW + 2 * HOUR, "Next"),
            ("a.id", NOW + 2 * HOUR, NOW + 3 * HOUR, "Later"),
            ("b.id", NOW - HOUR, NOW + HOUR, "Bravo Now"),
        ],
    )
}

#[test]
fn an_import_feeds_the_matcher_and_the_window_queries() {
    let storage = Storage::open_in_memory().unwrap();
    let alpha = channel("Alpha", "a.id");
    let (playlist, source) = setup(&storage, vec![alpha.clone()]);
    let imported = import(&storage, source, &sample());
    assert!(imported.replaced);
    assert_eq!(imported.summary.stats.programmes_kept, 5);
    assert_eq!(
        storage.guide_sources().unwrap()[0].name.as_deref(),
        Some("Guide Alpha")
    );

    let sources = storage.epg_sources().unwrap();
    let catalog: Vec<_> = sources[0]
        .channels
        .iter()
        .map(|c| (c.epg.id.as_str(), c.has_programmes))
        .collect();
    assert_eq!(catalog, [("a.id", true), ("b.id", true), ("quiet", false)]);

    let guide = find(&storage, playlist, &alpha).unwrap();
    let window = |from, to| storage.programmes(&guide, from, to).unwrap();
    assert_eq!(titles(&window(NOW, NOW + 2 * HOUR)), ["Now", "Next"]);
    // A programme ending exactly at the window start is out; one starting at its end is out.
    assert_eq!(titles(&window(NOW - HOUR, NOW + HOUR)), ["Now"]);
    assert_eq!(
        titles(&window(NOW - 3 * HOUR, NOW - 2 * HOUR)),
        Vec::<&str>::new()
    );
    assert_eq!(
        titles(&window(NOW - 3 * HOUR, NOW + 10 * HOUR)),
        ["Past", "Now", "Next", "Later"]
    );
    let programme = &window(NOW, NOW + 1)[0];
    assert_eq!(programme.description.as_deref(), Some("About Now"));
    assert_eq!((programme.start, programme.stop), (NOW - HOUR, NOW + HOUR));

    let (current, next) = storage.now_next(&guide, NOW).unwrap();
    assert_eq!(current.map(|p| p.title), Some("Now".into()));
    assert_eq!(next.map(|p| p.title), Some("Next".into()));
    let (current, next) = storage.now_next(&guide, NOW + 3 * HOUR).unwrap();
    assert_eq!((current, next), (None, None));
}

#[test]
fn shifts_apply_to_query_bounds_and_results() {
    let storage = Storage::open_in_memory().unwrap();
    let alpha = channel("Alpha", "a.id");
    let (playlist, source) = setup(&storage, vec![alpha.clone()]);
    import(&storage, source, &sample());
    storage.set_guide_offset(source, 60).unwrap();
    let sources = storage.epg_sources().unwrap();
    let user = Override {
        guide: None,
        shift_minutes: Some(60),
    };
    let guide = Matcher::new(&sources)
        .find(playlist, &alpha, Some(&user))
        .unwrap();
    assert_eq!((guide.shift_minutes, guide.shift_secs()), (120, 7200));
    let shown = storage
        .programmes(&guide, NOW + HOUR, NOW + HOUR + 1)
        .unwrap();
    assert_eq!(titles(&shown), ["Now"]);
    assert_eq!(
        (shown[0].start, shown[0].stop),
        (NOW + HOUR, NOW + 3 * HOUR)
    );
    let (current, _) = storage.now_next(&guide, NOW).unwrap();
    assert_eq!(current.map(|p| p.title), Some("Past".into()));
    let at = storage.programme_at(&guide, NOW + HOUR).unwrap();
    assert_eq!(
        at.map(|p| (p.title, p.stop)),
        Some(("Now".into(), NOW + 3 * HOUR))
    );
}

#[test]
fn the_guide_window_reads_many_channels_at_once() {
    let storage = Storage::open_in_memory().unwrap();
    let (alpha, bravo) = (channel("Alpha", "a.id"), channel("Bravo", "b.id"));
    let (playlist, source) = setup(&storage, vec![alpha.clone(), bravo.clone()]);
    import(&storage, source, &sample());
    let sources = storage.epg_sources().unwrap();
    let matches = Matcher::new(&sources).match_playlist(playlist, &[alpha, bravo], &HashMap::new());
    let rows: Vec<&Match> = matches.channels.iter().flatten().collect();
    let window = storage.guide_window(&rows, NOW, NOW + 90 * 60).unwrap();
    let got: Vec<Vec<&str>> = window.iter().map(|p| titles(p)).collect();
    assert_eq!(got, [vec!["Now", "Next"], vec!["Bravo Now"]]);
    // The grid reads what it paints; the focused programme's details come one at a time.
    let now = &window[0][0];
    assert_eq!((now.start, now.description.as_deref()), (NOW - HOUR, None));
    let focused = storage.programme_at(rows[0], now.start).unwrap().unwrap();
    assert_eq!(
        (focused.title.as_str(), focused.description.as_deref()),
        ("Now", Some("About Now"))
    );
    assert_eq!(storage.programme_at(rows[0], NOW).unwrap(), None);
}

#[test]
fn a_reimport_replaces_programmes_and_keeps_channel_rows() {
    let storage = Storage::open_in_memory().unwrap();
    let alpha = channel("Alpha", "a.id");
    let (playlist, source) = setup(&storage, vec![alpha.clone()]);
    import(&storage, source, &sample());
    let before = find(&storage, playlist, &alpha).unwrap();
    let fresh = guide_xml(
        &[("a.id", "Alpha")],
        &[("a.id", NOW - HOUR, NOW + HOUR, "Fresh")],
    );
    assert!(import(&storage, source, &fresh).replaced);
    let after = find(&storage, playlist, &alpha).unwrap();
    assert_eq!(after.guide_channel, before.guide_channel);
    let all = storage.programmes(&after, NOW - DAY, NOW + DAY).unwrap();
    assert_eq!(titles(&all), ["Fresh"]);
    let ids: Vec<String> = storage.epg_sources().unwrap()[0]
        .channels
        .iter()
        .map(|c| c.epg.id.clone())
        .collect();
    assert_eq!(ids, ["a.id"]);
}

#[test]
fn an_empty_download_keeps_the_previous_guide() {
    let storage = Storage::open_in_memory().unwrap();
    let alpha = channel("Alpha", "a.id");
    let (playlist, source) = setup(&storage, vec![alpha.clone()]);
    // With nothing stored yet an empty guide is simply the guide.
    assert!(import(&storage, source, "<tv></tv>").replaced);
    import(&storage, source, &sample());
    let imported = import(&storage, source, "<tv><channel id='a.id'/></tv>");
    assert!(!imported.replaced);
    let guide = find(&storage, playlist, &alpha).unwrap();
    let (current, _) = storage.now_next(&guide, NOW).unwrap();
    assert_eq!(current.map(|p| p.title), Some("Now".into()));
}

#[test]
fn a_failed_download_keeps_the_previous_guide() {
    let storage = Storage::open_in_memory().unwrap();
    let alpha = channel("Alpha", "a.id");
    let (playlist, source) = setup(&storage, vec![alpha.clone()]);
    import(&storage, source, &sample());
    let packed = gzip(sample().replace("Now", "Broken").as_bytes());
    let truncated = &packed[..packed.len() - 20];
    let result = storage.import_guide(source, truncated, &Options::new(NOW));
    assert!(matches!(result, Err(xmltv::Error::Read(_))));
    let guide = find(&storage, playlist, &alpha).unwrap();
    let all = storage.programmes(&guide, NOW - DAY, NOW + DAY).unwrap();
    assert_eq!(titles(&all), ["Past", "Now", "Next", "Later"]);
    // Nothing of the failed attempt lingers into the next one.
    import(&storage, source, &sample());
    let all = storage.programmes(&guide, NOW - DAY, NOW + DAY).unwrap();
    assert_eq!(all.len(), 4);
}

#[test]
fn programmes_before_their_channel_get_a_row_and_then_its_names() {
    let storage = Storage::open_in_memory().unwrap();
    let alpha = channel("Alpha", "");
    let (playlist, source) = setup(&storage, vec![alpha.clone()]);
    let xml = format!(
        r#"<tv><programme channel="late" start="{}" stop="{}"><title>Early</title></programme><channel id="late"><display-name>Alpha</display-name></channel></tv>"#,
        date(NOW),
        date(NOW + HOUR)
    );
    import(&storage, source, &xml);
    let sources = storage.epg_sources().unwrap();
    assert_eq!(sources[0].channels[0].epg.names, ["Alpha"]);
    let guide = Matcher::new(&sources).find(playlist, &alpha, None).unwrap();
    assert_eq!(
        titles(&storage.programmes(&guide, NOW, NOW + 1).unwrap()),
        ["Early"]
    );
}

#[test]
fn a_filtered_import_keeps_the_whole_catalog_but_only_wanted_programmes() {
    let storage = Storage::open_in_memory().unwrap();
    let (_, source) = setup(&storage, vec![channel("Alpha", "a.id")]);
    let options = Options {
        ids: ["a.id".to_string()].into_iter().collect(),
        ..Options::new(NOW)
    };
    let imported = storage
        .import_guide(source, sample().as_bytes(), &options)
        .unwrap();
    assert_eq!(imported.summary.stats.programmes_kept, 4);
    let catalog: Vec<_> = storage.epg_sources().unwrap()[0]
        .channels
        .iter()
        .map(|c| (c.epg.id.clone(), c.has_programmes))
        .collect();
    assert_eq!(
        catalog,
        [
            ("a.id".into(), true),
            ("b.id".into(), false),
            ("quiet".into(), false)
        ]
    );
}

/// Serves `data` and runs `hook` once, when the reader gets past `at`.
struct Hooked<'a> {
    data: &'a [u8],
    pos: usize,
    at: usize,
    hook: Option<Box<dyn FnOnce() + 'a>>,
}

impl Read for Hooked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Hooked<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos >= self.at {
            if let Some(hook) = self.hook.take() {
                hook();
            }
        }
        let end = (self.pos + 4096).min(self.data.len());
        Ok(&self.data[self.pos..end])
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
    }
}

#[test]
fn a_refresh_never_blocks_user_writes_and_readers_keep_the_old_guide() {
    let dir = TempDir::new("refresh");
    let storage = Storage::open(&dir.0).unwrap();
    let alpha = channel("Alpha", "a.id");
    let (playlist, source) = setup(&storage, vec![alpha.clone()]);
    import(&storage, source, &sample());
    let guide = find(&storage, playlist, &alpha).unwrap();

    // Enough programmes for several committed batches before the hook runs.
    let programmes: Vec<(&str, Timestamp, Timestamp, &str)> = (0..3000)
        .map(|i| ("a.id", NOW - DAY + i * 60, NOW - DAY + (i + 1) * 60, "New"))
        .collect();
    let xml = guide_xml(&[("a.id", "Alpha")], &programmes);
    let seen = RefCell::new(None);
    let mut input = Hooked {
        data: xml.as_bytes(),
        pos: 0,
        at: xml.len() * 9 / 10,
        hook: Some(Box::new(|| {
            let ui = Storage::open(&dir.0).unwrap();
            let started = Instant::now();
            ui.toggle_favourite(alpha.key).unwrap();
            let waited = started.elapsed();
            let (current, _) = ui.now_next(&guide, NOW).unwrap();
            *seen.borrow_mut() = Some((waited, current.map(|p| p.title)));
        })),
    };
    let imported = storage
        .import_guide(source, &mut input, &Options::new(NOW))
        .unwrap();
    assert!(imported.replaced);
    let (waited, title) = seen.take().expect("the hook ran");
    assert!(
        waited < Duration::from_secs(1),
        "user write waited {waited:?}"
    );
    assert_eq!(title.as_deref(), Some("Now"));
    let (current, _) = storage.now_next(&guide, NOW).unwrap();
    assert_eq!(current.map(|p| p.title), Some("New".into()));
    assert!(storage.is_favourite(alpha.key).unwrap());
}

#[test]
fn a_week_of_guide_for_191_channels_imports_and_answers_the_grid() {
    const CHANNELS: usize = 191;
    let ids: Vec<String> = (0..CHANNELS).map(|c| format!("ch{c}")).collect();
    let channels: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), id.as_str())).collect();
    let mut programmes = Vec::new();
    for id in &ids {
        for slot in 0..14 * 48 {
            let start = NOW - 7 * DAY + slot * 1800;
            programmes.push((id.as_str(), start, start + 1800, "Show"));
        }
    }
    let xml = guide_xml(&channels, &programmes);
    let dir = TempDir::new("week");
    let storage = Storage::open(&dir.0).unwrap();
    let playlist_channels: Vec<Channel> = ids.iter().map(|id| channel(id, id)).collect();
    let (playlist, source) = setup(&storage, playlist_channels.clone());

    let started = Instant::now();
    let imported = storage
        .import_guide(source, xml.as_bytes(), &Options::new(NOW))
        .unwrap();
    let import_time = started.elapsed();
    assert_eq!(imported.summary.stats.programmes_kept, CHANNELS * 14 * 48);

    let sources = storage.epg_sources().unwrap();
    let matches =
        Matcher::new(&sources).match_playlist(playlist, &playlist_channels, &HashMap::new());
    assert_eq!(matches.unmatched().count(), 0);
    let rows: Vec<&Match> = matches.channels.iter().flatten().collect();
    let started = Instant::now();
    let window = storage.guide_window(&rows, NOW, NOW + 3 * HOUR).unwrap();
    let grid_time = started.elapsed();
    assert!(window.iter().all(|row| row.len() == 6));
    println!(
        "{} MB, {} programmes: import {import_time:?}, grid of {CHANNELS} rows x 3 h {grid_time:?}",
        xml.len() / 1_000_000,
        CHANNELS * 14 * 48
    );
    assert!(
        grid_time < Duration::from_secs(2),
        "grid took {grid_time:?}"
    );
}

// --- review regressions -------------------------------------------------------------------

#[test]
fn a_wanted_channel_after_a_huge_catalog_keeps_its_programmes() {
    let storage = Storage::open_in_memory().unwrap();
    let alpha = channel("Alpha", "wanted");
    let (playlist, source) = setup(&storage, vec![alpha.clone()]);
    let mut xml = String::from("<tv>");
    for i in 0..10_000 {
        xml += &format!("<channel id='u{i}'><display-name>U{i}</display-name></channel>");
    }
    xml += "<channel id='wanted'><display-name>Alpha</display-name></channel>";
    xml += &guide_xml(&[], &[("wanted", NOW, NOW + HOUR, "Show")])
        [r#"<tv source-info-name="Guide Alpha">"#.len()..];
    let options = Options {
        ids: ["wanted".to_string()].into_iter().collect(),
        ..Options::new(NOW)
    };
    assert!(
        storage
            .import_guide(source, xml.as_bytes(), &options)
            .unwrap()
            .replaced
    );
    let guide = find(&storage, playlist, &alpha).expect("the wanted channel matches");
    let (current, _) = storage.now_next(&guide, NOW).unwrap();
    assert_eq!(current.map(|p| p.title), Some("Show".into()));
}

#[test]
fn a_cut_download_does_not_replace_the_guide() {
    let storage = Storage::open_in_memory().unwrap();
    let alpha = channel("Alpha", "a.id");
    let (playlist, source) = setup(&storage, vec![alpha.clone()]);
    import(&storage, source, &sample());
    let fresh = guide_xml(
        &[("a.id", "Alpha")],
        &[
            ("a.id", NOW - HOUR, NOW + HOUR, "Fresh"),
            ("a.id", NOW + HOUR, NOW + 2 * HOUR, "Fresh next"),
        ],
    );
    // Cut between two elements: well-formed so far, but no `</tv>`.
    let cut = &fresh[..fresh.rfind("<programme").unwrap()];
    let imported = import(&storage, source, cut);
    assert!(!imported.replaced && !imported.summary.complete);
    // Cut inside a tag: a syntax error.
    let result = storage.import_guide(
        source,
        &fresh.as_bytes()[..fresh.len() - 2],
        &Options::new(NOW),
    );
    assert!(matches!(result, Err(xmltv::Error::Read(_))));
    let guide = find(&storage, playlist, &alpha).unwrap();
    let all = storage.programmes(&guide, NOW - DAY, NOW + DAY).unwrap();
    assert_eq!(titles(&all), ["Past", "Now", "Next", "Later"]);
}

#[test]
fn overlapping_programmes_are_found_and_duplicate_starts_counted() {
    let storage = Storage::open_in_memory().unwrap();
    let alpha = channel("Alpha", "a.id");
    let (playlist, source) = setup(&storage, vec![alpha.clone()]);
    let xml = guide_xml(
        &[("a.id", "Alpha")],
        &[
            ("a.id", NOW - 2 * HOUR, NOW + 2 * HOUR, "Long"),
            ("a.id", NOW - HOUR, NOW - HOUR / 2, "Flash"),
            ("a.id", NOW + 3 * HOUR, NOW + 4 * HOUR, "First"),
            ("a.id", NOW + 3 * HOUR, NOW + 4 * HOUR, "Second"),
        ],
    );
    assert_eq!(import(&storage, source, &xml).duplicates, 1);
    let guide = find(&storage, playlist, &alpha).unwrap();
    let (current, next) = storage.now_next(&guide, NOW).unwrap();
    assert_eq!(current.map(|p| p.title), Some("Long".into()));
    assert_eq!(next.map(|p| p.title), Some("First".into()));
    let window = storage.programmes(&guide, NOW, NOW + HOUR).unwrap();
    assert_eq!(titles(&window), ["Long"]);
    let window = storage
        .programmes(&guide, NOW - 3 * HOUR, NOW + 4 * HOUR)
        .unwrap();
    assert_eq!(titles(&window), ["Long", "Flash", "First"]);
    // A window starts at the programme on air then, the latest-starting one as in now_next.
    let window = storage
        .programmes(&guide, NOW - HOUR, NOW + 4 * HOUR)
        .unwrap();
    assert_eq!(titles(&window), ["Flash", "First"]);
}

#[test]
fn source_and_playlist_debug_output_masks_urls() {
    let storage = Storage::open_in_memory().unwrap();
    let id = storage
        .add_playlist(
            "Main",
            "http://playlist.example/get.php?username=u1&password=SECRET1",
        )
        .unwrap();
    let playlist = Playlist {
        epg_urls: vec!["http://guide.example/xmltv.php?username=u1&password=SECRET2".into()],
        ..Playlist::default()
    };
    storage.save_playlist(id, &playlist, NOW).unwrap();
    let shown = format!(
        "{:?} {:?}",
        storage.playlists().unwrap(),
        storage.guide_sources().unwrap()
    );
    assert!(
        !shown.contains("SECRET1") && !shown.contains("SECRET2"),
        "{shown}"
    );
    assert!(
        shown.contains("playlist.example") && shown.contains("guide.example"),
        "{shown}"
    );
}

#[test]
fn the_switch_to_a_new_guide_keeps_the_wal_and_user_waits_small() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let ids: Vec<String> = (0..191).map(|c| format!("ch{c}")).collect();
    let channels: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), id.as_str())).collect();
    let mut programmes = Vec::new();
    for id in &ids {
        for slot in 0..14 * 48 {
            let start = NOW - 7 * DAY + slot * 1800;
            programmes.push((id.as_str(), start, start + 1800, "Show"));
        }
    }
    let xml = guide_xml(&channels, &programmes);
    let dir = TempDir::new("switch");
    let storage = Storage::open(&dir.0).unwrap();
    let (_, source) = setup(&storage, ids.iter().map(|id| channel(id, id)).collect());
    import(&storage, source, &xml);

    let stop = Arc::new(AtomicBool::new(false));
    let user = {
        let (dir, stop) = (dir.0.clone(), stop.clone());
        std::thread::spawn(move || {
            let ui = Storage::open(&dir).unwrap();
            let wal = dir.join(format!("{}-wal", iptv::storage::DB_FILE));
            let (mut worst, mut largest) = (Duration::ZERO, 0);
            while !stop.load(Ordering::Relaxed) {
                let started = Instant::now();
                ui.toggle_favourite(ChannelKey(1)).unwrap();
                worst = worst.max(started.elapsed());
                largest = largest.max(std::fs::metadata(&wal).map_or(0, |m| m.len()));
                std::thread::sleep(Duration::from_millis(2));
            }
            (worst, largest)
        })
    };
    assert!(import(&storage, source, &xml).replaced);
    stop.store(true, Ordering::Relaxed);
    let (worst, largest) = user.join().unwrap();
    println!(
        "worst user write {worst:?}, largest WAL {} KiB",
        largest / 1024
    );
    assert!(largest < 10 << 20, "WAL grew to {largest} bytes");
    assert!(
        worst < Duration::from_millis(500),
        "a user write waited {worst:?}"
    );
}
