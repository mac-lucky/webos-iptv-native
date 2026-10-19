//! What the database may be opened in, how much flash it may take and which tokenised URLs it
//! keeps.

use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rusqlite::ErrorCode;

use super::tests::stamp;
use super::*;
use crate::refresh::Store;

const NOW: Timestamp = 1_780_000_000;

/// A fresh 0700 directory, as the app's data dir is, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("iptv-unit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        set_mode(&dir, 0o700);
        Dir(dir)
    }

    fn file(&self, suffix: &str) -> PathBuf {
        self.0.join(format!("{DB_FILE}{suffix}"))
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().mode() & 0o777
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// `channels` channels with `each` half-hour programmes from [`NOW`] on, each described in
/// `desc` bytes (none for 0).
fn guide(channels: usize, each: usize, desc: usize) -> String {
    let mut xml = String::from("<tv>");
    for c in 0..channels {
        xml += &format!(r#"<channel id="c{c}"><display-name>C{c}</display-name></channel>"#);
    }
    let desc = match desc {
        0 => String::new(),
        n => format!("<desc>{}</desc>", "d".repeat(n)),
    };
    for c in 0..channels {
        for i in 0..each {
            let start = NOW + i as i64 * 1800;
            xml += &format!(
                r#"<programme channel="c{c}" start="{}" stop="{}"><title>T{i}</title>{desc}</programme>"#,
                stamp(start),
                stamp(start + 1800)
            );
        }
    }
    xml + "</tv>"
}

fn import(
    s: &Storage,
    source: SourceId,
    xml: &str,
) -> std::result::Result<Imported, xmltv::Error<rusqlite::Error>> {
    s.import_guide(source, xml.as_bytes(), &xmltv::Options::new(NOW))
}

/// Programmes in every guide's current generation.
fn stored(s: &Storage) -> usize {
    s.programmes_besides(-1).unwrap()
}

// --- the data dir and the files ----------------------------------------------------------

#[test]
fn a_link_at_any_database_name_is_refused_and_nothing_is_made_through_it() {
    let dir = Dir::new("links");
    let target = dir.0.join("elsewhere");
    for suffix in ["", "-wal", "-shm"] {
        let link = dir.file(suffix);
        symlink(&target, &link).unwrap();
        assert!(Storage::open(&dir.0).is_err(), "{DB_FILE}{suffix}");
        assert!(
            !target.exists(),
            "{DB_FILE}{suffix}: created through the link"
        );
        std::fs::remove_file(&link).unwrap();
    }
    Storage::open(&dir.0).unwrap();
}

#[test]
fn a_loose_database_of_ours_is_made_0600_with_its_wal_and_shm() {
    let dir = Dir::new("loose");
    Storage::open(&dir.0).unwrap();
    set_mode(&dir.file(""), 0o666);
    for suffix in ["-wal", "-shm"] {
        std::fs::write(dir.file(suffix), "").unwrap();
        set_mode(&dir.file(suffix), 0o666);
    }
    let s = Storage::open(&dir.0).unwrap();
    s.set_setting("k", "v").unwrap();
    for suffix in ["", "-wal", "-shm"] {
        assert_eq!(mode(&dir.file(suffix)), 0o600, "{DB_FILE}{suffix}");
    }
}

#[test]
fn a_dir_others_can_write_or_a_linked_or_foreign_one_is_refused() {
    let dir = Dir::new("shared");
    for loose in [0o777, 0o1777, 0o770] {
        set_mode(&dir.0, loose);
        assert!(Storage::open(&dir.0).is_err(), "{loose:o}");
        assert!(
            !dir.file("").exists(),
            "{loose:o}: nothing is created there"
        );
    }
    set_mode(&dir.0, 0o755);
    Storage::open(&dir.0).unwrap();
    let link = dir.0.with_extension("link");
    let _ = std::fs::remove_file(&link);
    symlink(&dir.0, &link).unwrap();
    assert!(Storage::open(&link).is_err(), "a linked data dir");
    std::fs::remove_file(&link).unwrap();
    let other = tv_base::paths::euid().wrapping_add(1);
    assert!(
        private_files(&dir.0, &dir.file(""), other).is_err(),
        "another uid's"
    );
}

/// The `fchmod` is what makes a loose `-wal` private: when it is refused, so is the database.
/// An immutable file is one `fchmod` cannot change.
#[cfg(target_os = "macos")]
#[test]
fn a_refused_chmod_refuses_the_database() {
    let dir = Dir::new("chmod");
    Storage::open(&dir.0).unwrap();
    let wal = dir.file("-wal");
    std::fs::write(&wal, "").unwrap();
    set_mode(&wal, 0o644);
    let flags = |flag: &str| {
        let ok = std::process::Command::new("chflags")
            .arg(flag)
            .arg(&wal)
            .status()
            .unwrap();
        assert!(ok.success());
    };
    flags("uchg");
    let opened = Storage::open(&dir.0);
    let left = mode(&wal);
    flags("nouchg");
    assert!(opened.is_err(), "a -wal still 0644 must not be used");
    assert_eq!(left, 0o644);
}

// --- flash budgets -----------------------------------------------------------------------

#[test]
fn the_file_is_capped_at_its_budget() {
    let s = Storage::open_in_memory().unwrap();
    let pages: i64 = s
        .conn
        .query_row("PRAGMA max_page_count", [], |row| row.get(0))
        .unwrap();
    let size: i64 = s
        .conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    assert_eq!(pages * size, MAX_DB_BYTES);
}

#[test]
fn a_guide_past_the_file_budget_fails_and_the_old_one_stays() {
    let s = Storage::open_in_memory().unwrap();
    let source = s
        .add_manual_guide("http://guide.example/a.xml", &[])
        .unwrap();
    assert!(import(&s, source, &guide(2, 10, 100)).unwrap().replaced);
    let pages: i64 = s
        .conn
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    s.conn
        .execute_batch(&format!("PRAGMA max_page_count = {}", pages + 16))
        .unwrap();
    let e = import(&s, source, &guide(20, 100, 2000)).unwrap_err();
    assert!(
        matches!(&e, xmltv::Error::Sink(e) if e.sqlite_error_code() == Some(ErrorCode::DiskFull)),
        "{e}"
    );
    assert_eq!(stored(&s), 20, "the old guide stays");
    let pending: i64 = s
        .conn
        .query_row("SELECT COUNT(*) FROM programme", [], |row| row.get(0))
        .unwrap();
    assert_eq!(pending, 20, "and the cut import left nothing behind");
    s.toggle_favourite(ChannelKey(1)).unwrap();
    assert!(import(&s, source, &guide(2, 10, 100)).unwrap().replaced);
}

#[test]
fn every_guide_together_keeps_at_most_the_programme_cap() {
    let mut s = Storage::open_in_memory().unwrap();
    s.max_programmes = 30;
    let a = s
        .add_manual_guide("http://guide.example/a.xml", &[])
        .unwrap();
    let b = s
        .add_manual_guide("http://guide.example/b.xml", &[])
        .unwrap();
    let first = import(&s, a, &guide(2, 10, 0)).unwrap();
    assert_eq!(first.summary.stats.programmes_kept, 20);
    let second = import(&s, b, &guide(2, 10, 0)).unwrap();
    let kept = |i: &Imported| {
        (
            i.summary.stats.programmes_kept,
            i.summary.stats.dropped_budget,
        )
    };
    assert_eq!(kept(&second), (10, 10), "b gets the room a leaves");
    assert!(second.replaced);
    assert_eq!(stored(&s), 30);
    // A guide's own previous programmes are not counted against it.
    assert_eq!(kept(&import(&s, a, &guide(2, 10, 0)).unwrap()), (20, 0));
    // With no room at all, a guide keeps what it has.
    s.max_programmes = 20;
    let full = import(&s, b, &guide(3, 10, 0)).unwrap();
    assert_eq!(kept(&full), (0, 30));
    assert!(!full.replaced);
    assert_eq!(stored(&s), 30);
}

// --- tokenised URLs -----------------------------------------------------------------------

#[test]
fn set_sources_forgets_the_urls_the_settings_no_longer_name() {
    let s = Storage::open_in_memory().unwrap();
    let old = "http://old.example/list.m3u?token=old-secret";
    let header = "http://header.example/epg.xml";
    let old_guide = "http://old.example/epg.xml?token=old-secret";
    let id = s.add_playlist("Main", old).unwrap();
    let playlist = Playlist {
        name: None,
        channels: vec![Channel {
            name: "One".into(),
            url: "http://stream.example/1".into(),
            ..Channel::default()
        }],
        epg_urls: vec![header.into()],
    };
    s.save_playlist(id, &playlist, NOW).unwrap();
    let manual = s.add_manual_guide(old_guide, &[]).unwrap();
    import(&s, manual, &guide(1, 4, 0)).unwrap();
    for url in [old, header, old_guide, "http://gone.example/x.xml"] {
        s.conn
            .execute(
                "INSERT INTO http_validator (url, etag) VALUES (?1, 'e')",
                [url],
            )
            .unwrap();
    }
    let new = "http://new.example/list.m3u?token=new";
    s.set_sources(id, new, &["http://new.example/epg.xml".into()])
        .unwrap();
    assert_eq!(s.playlists().unwrap()[0].url, new);
    let urls: Vec<String> = s
        .conn
        .prepare("SELECT url FROM http_validator")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_>>()
        .unwrap();
    assert_eq!(
        urls,
        [header],
        "only a URL still fetched keeps its validators"
    );
    let left: i64 = s
        .conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM playlist WHERE url LIKE '%old-secret%')
             + (SELECT COUNT(*) FROM guide_source WHERE url LIKE '%old-secret%')
             + (SELECT COUNT(*) FROM http_validator WHERE url LIKE '%old-secret%')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(left, 0, "no old token left, at once");
    // The detached guide's programmes go later, on the refresher's thread.
    assert_eq!(stored(&s), 4);
    let sources = s.epg_sources().unwrap();
    let detached = sources.iter().find(|g| g.id == manual).unwrap();
    assert!(!detached.serves(id), "but serve nothing");
    let mut store = RefreshStore::new(s);
    assert_eq!(store.guide_urls(), [header]);
    let s = store.storage();
    let sources: Vec<String> = s
        .guide_sources()
        .unwrap()
        .into_iter()
        .map(|g| g.url)
        .collect();
    assert_eq!(sources, [header], "the refresh deleted the detached guide");
    assert_eq!(stored(s), 0, "with its programmes");
}

#[test]
fn a_header_guide_made_manual_and_dropped_comes_back_with_its_playlist() {
    let s = Storage::open_in_memory().unwrap();
    let url = "http://playlist.example/list.m3u";
    let header = "http://header.example/epg.xml";
    let id = s.add_playlist("Main", url).unwrap();
    let playlist = Playlist {
        name: None,
        channels: vec![Channel {
            name: "One".into(),
            url: "http://stream.example/1".into(),
            ..Channel::default()
        }],
        epg_urls: vec![header.into()],
    };
    s.save_playlist(id, &playlist, NOW).unwrap();
    s.add_manual_guide(header, &[]).unwrap();
    s.set_sources(id, url, &[]).unwrap();
    let serving = s.epg_sources().unwrap();
    assert!(serving.iter().all(|g| !g.serves(id)));
    assert!(
        s.save_playlist(id, &playlist, NOW).unwrap(),
        "the same download links it again"
    );
    let sources = s.guide_sources().unwrap();
    let linked = sources.iter().find(|g| g.url == header).unwrap();
    assert_eq!(linked.playlists, [id]);
}
