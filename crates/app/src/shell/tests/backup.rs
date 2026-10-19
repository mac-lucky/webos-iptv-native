//! Backup in the running app: an export from one TV imported on another brings its settings and
//! favourites along and keeps them after a restart; a file that does not parse whole changes
//! nothing; no line or toast carries a value from the file.

use iptv::model::Playlist;
use iptv::storage::{GroupRef, Storage};
use tv_screens::data::BackupOp;
use tv_screens::panels::settings::SPECS;

use super::*;
use crate::backup::{self, FILE};

/// A data dir of its own holding the 12 synthetic channels as its playlist: one TV. Removed when
/// dropped.
struct Tv(std::path::PathBuf);

impl Tv {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("webosiptv-backup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let storage = Storage::open(&dir).unwrap();
        let id = storage
            .add_playlist("p", "http://playlist.test/list.m3u")
            .unwrap();
        let playlist = Playlist {
            channels: FakeData::new(NOW, 12).channels,
            ..Playlist::default()
        };
        assert!(storage.save_playlist(id, &playlist, NOW).unwrap());
        Tv(dir)
    }

    /// A started shell over what is on disk now, without a network.
    fn sim(&self) -> Sim {
        locale();
        let calls = Calls::default();
        let mut shell = Shell::new(
            Box::new(crate::store::open_at(&self.0)),
            Box::new(calls.clone()),
            None,
            LogoCache::new(None),
        );
        shell.start(0);
        Sim {
            shell,
            calls,
            seen: Rc::default(),
            t: 1_000,
        }
    }

    fn file(&self) -> std::path::PathBuf {
        self.0.join(FILE)
    }

    fn storage(&self) -> Storage {
        Storage::open(&self.0).unwrap()
    }
}

impl Drop for Tv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What the Settings panel sends for a change.
fn set(s: &mut Sim, key: &str, value: &str) {
    let (key, value) = (key.to_owned(), value.to_owned());
    s.shell.run(Command::SetSetting { key, value }, s.t);
}

/// What the Backup rows send; the toast it left.
fn backup(s: &mut Sim, op: BackupOp) -> Option<String> {
    s.shell.run(Command::Backup(op), s.t);
    s.toast()
}

fn favorites(s: &Sim) -> Vec<ChannelKey> {
    let list = s.shell.store.channels(&Group::Favorites);
    list.iter().map(|c| c.key).collect()
}

/// Every setting as the shell holds it and as the store has it on disk.
fn settings(s: &Sim) -> (Settings, Settings) {
    (s.shell.settings.clone(), s.shell.store.settings())
}

/// A backup file with `body` after its format and version.
fn file(body: &str) -> String {
    format!(r#"{{"format":"webosiptv-backup","version":1{body}}}"#)
}

/// A playlist URL whose path and query carry plain words that are masked only once registered.
fn token_url() -> (String, &'static str) {
    let secret = "quinceowner";
    (
        format!("http://pl.example.test/{secret}/list.m3u?k={secret}"),
        secret,
    )
}

#[test]
fn an_export_from_one_tv_restores_on_another_and_survives_a_restart() {
    let (one, two) = (Tv::new("one"), Tv::new("two"));
    let mut a = one.sim();
    let (url, secret) = token_url();
    set(&mut a, keys::PLAYLIST_URL, &url);
    set(&mut a, keys::LONG_PRESS_MS, "800");
    set(&mut a, keys::GROUPS_HIDDEN, "News");
    let (k3, k1, k7) = (a.key(3), a.key(1), a.key(7));
    for key in [k3, k1, k7] {
        a.shell.run(Command::ToggleFavorite(key), a.t);
    }
    // A custom group with a moved channel, and an order: what the library pages make.
    let store = one.storage();
    assert!(store.create_group("Kids").unwrap());
    let from = GroupRef::Playlist("News".into());
    assert!(store.add_to_group("Kids", &[k7, k1], Some(&from)).unwrap());
    let order = [GroupRef::Custom("Kids".into()), from];
    store.set_group_order(&order).unwrap();
    let layout = store.user_layout().unwrap();
    assert_eq!(layout.custom_groups.len(), 1);
    assert_eq!(
        backup(&mut a, BackupOp::Export).as_deref(),
        Some("Settings exported")
    );
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(one.file()).unwrap().permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
    let text = std::fs::read_to_string(one.file()).unwrap();
    assert!(text.contains(&url), "the file is the whole backup");
    // What `cargo xtask backup` does between the two TVs.
    std::fs::copy(one.file(), two.file()).unwrap();

    let mut b = two.sim();
    assert_eq!(favorites(&b), []);
    let (toast, line) = b.shell.import_backup();
    assert_eq!(toast, "Settings imported");
    let carried = SPECS.iter().filter(|s| backup::carried(s.key)).count();
    assert_eq!(
        line,
        format!("backup: imported {carried} settings, 3 favourites and the layout")
    );
    assert_eq!(two.storage().user_layout().unwrap(), layout);
    assert!(!line.contains(secret));
    assert_eq!(settings(&b).0, a.shell.settings);
    assert_eq!(settings(&b).1, a.shell.settings, "stored as well");
    assert_eq!(favorites(&b), [k3, k1, k7], "in their order");
    assert_eq!(b.shell.remote.long_ms, 800, "each setting took effect");
    drop(b);
    let b = two.sim();
    assert_eq!(
        settings(&b),
        (a.shell.settings.clone(), a.shell.settings.clone())
    );
    assert_eq!(favorites(&b), [k3, k1, k7]);
    // An import over changed settings puts every exported value back, defaults too.
    let mut b = b;
    set(&mut b, keys::SKIP_BACK_SECS, "30");
    b.shell.run(Command::ToggleFavorite(k1), b.t);
    two.storage().delete_group("Kids").unwrap();
    assert_eq!(
        backup(&mut b, BackupOp::Import).as_deref(),
        Some("Settings imported")
    );
    assert_eq!(settings(&b).0.get(keys::SKIP_BACK_SECS), "10");
    assert_eq!(favorites(&b), [k3, k1, k7]);
    assert_eq!(two.storage().user_layout().unwrap(), layout);
}

#[test]
fn a_file_that_does_not_parse_whole_changes_nothing() {
    let tv = Tv::new("refused");
    let mut s = tv.sim();
    set(&mut s, keys::LONG_PRESS_MS, "600");
    s.shell.run(Command::ToggleFavorite(s.key(2)), s.t);
    let before = (settings(&s), favorites(&s));
    assert_eq!(
        backup(&mut s, BackupOp::Import).as_deref(),
        Some("No backup on this TV")
    );
    let (url, secret) = token_url();
    let key = format!("{:016x}", s.key(5).0);
    let favorite = format!(r#"{{"key":"{key}"}}"#);
    let twice = r#"[{"name":"A","channels":[]},{"name":"A","channels":[]}]"#;
    let files = [
        // A good setting and a good favourite before a refused value.
        file(&format!(
            r#","settings":{{"playlist.url":"{url}","remote.long_press_ms":"250"}},"favorites":[{favorite}]"#
        )),
        file(&format!(
            r#","settings":{{"remote.long_press_ms":"300"}},"favorites":[{favorite},{{"key":"{secret}"}}]"#
        )),
        // Cut short inside the URL.
        file(&format!(r#","settings":{{"playlist.url":"{url}"#)),
        file(&format!(r#","settings":{{"playlist.url":"{url}"}}"#))
            .replace("\"version\":1", "\"version\":3"),
        format!("{{\"pad\":\"{}\"}}", " ".repeat(crate::backup::MAX_BYTES)),
        // A layout that parses but storage refuses (two groups of one name), with good settings.
        file(&format!(
            r#","settings":{{"remote.long_press_ms":"300"}},"favorites":[{favorite}],"layout":{{"custom_groups":{twice},"group_order":[],"channel_order":[]}}"#
        )),
    ];
    let toasts = [
        "The backup file is not valid",
        "The backup file is not valid",
        "The backup file is not valid",
        "The backup is from a newer version of the app",
        "The backup file is not valid",
        "The backup file is not valid",
    ];
    for (text, toast) in files.iter().zip(toasts) {
        std::fs::write(tv.file(), text).unwrap();
        let (shown, line) = s.shell.import_backup();
        assert_eq!(shown, toast);
        assert!(line.starts_with("backup: import refused: "), "{line}");
        assert!(
            !line.contains(secret) && !line.contains("pl.example"),
            "{line}"
        );
        assert_eq!((settings(&s), favorites(&s)), before, "{line}");
        assert!(tv.storage().custom_groups().unwrap().is_empty());
    }
}

/// Import asks for no PIN, so it never touches the parental settings: a file from before the
/// PIN was set, or one written by hand, neither puts back an older PIN nor unblocks anything.
#[test]
fn an_import_leaves_the_parental_pin_and_locks_as_they_are() {
    let tv = Tv::new("parental");
    let mut s = tv.sim();
    let pin = tv_screens::parental::hash_pin("1234");
    let channel = format!("{:016x}", s.key(4).0);
    set(&mut s, keys::PARENTAL_PIN, &pin);
    set(&mut s, keys::PARENTAL_CHANNELS, &channel);
    set(&mut s, keys::PARENTAL_GROUPS, "News");
    let before = settings(&s);
    let text = file(&format!(
        r#","settings":{{"{}":"","{}":"","{}":"","remote.long_press_ms":"300"}}"#,
        keys::PARENTAL_PIN,
        keys::PARENTAL_CHANNELS,
        keys::PARENTAL_GROUPS
    ));
    std::fs::write(tv.file(), text).unwrap();
    assert_eq!(
        backup(&mut s, BackupOp::Import).as_deref(),
        Some("Settings imported")
    );
    let after = settings(&s);
    for key in [
        keys::PARENTAL_PIN,
        keys::PARENTAL_CHANNELS,
        keys::PARENTAL_GROUPS,
    ] {
        assert_eq!(after.0.get(key), before.0.get(key), "{key}");
        assert_eq!(after.1.get(key), before.1.get(key), "{key} stored");
    }
    assert_eq!(
        after.0.get(keys::LONG_PRESS_MS),
        "300",
        "the rest is imported"
    );
    assert!(s.shell.store.group_locked(&Group::Playlist("News".into())));
}

#[test]
fn without_a_data_dir_nothing_is_written_or_read() {
    let mut s = Sim::new();
    assert_eq!(
        backup(&mut s, BackupOp::Export).as_deref(),
        Some("Could not export the settings")
    );
    assert_eq!(
        backup(&mut s, BackupOp::Import).as_deref(),
        Some("No backup on this TV")
    );
    let (_, line) = s.shell.export_backup();
    assert_eq!(line, "backup: export failed: no data dir");
}

#[test]
fn import_from_the_settings_panel_closes_it_and_shows_the_toast() {
    let tv = Tv::new("panel");
    let mut s = tv.sim();
    set(&mut s, keys::LONG_PRESS_MS, "800");
    assert_eq!(
        backup(&mut s, BackupOp::Export).as_deref(),
        Some("Settings exported")
    );
    set(&mut s, keys::LONG_PRESS_MS, "300");
    // What the panel's "Import and replace" row answers (tv_screens' settings backup tests).
    let panel = s.push("settings");
    panel.0.slides.set(true);
    *panel.0.reply.borrow_mut() =
        Output::command(Command::Backup(BackupOp::Import)).and(Output::close());
    let open = s.open();
    s.press("OK");
    assert_eq!(s.toast().as_deref(), Some("Settings imported"));
    assert_eq!(s.shell.settings.get(keys::LONG_PRESS_MS), "800");
    assert_eq!(s.shell.remote.long_ms, 800);
    assert_eq!(s.open(), open - 1, "Settings closed");
}
