//! Backup and restore: the settings, the favourites and the user's group layout (custom groups,
//! group and channel order) in one versioned JSON file, [`FILE`] in the data dir, which
//! `cargo xtask backup` moves between the Mac and a TV over ssh. The shell writes it (Settings >
//! General > Backup > Export) and restores from it (Import).
//!
//! The file holds the playlist and guide URLs with their tokens, so it is kept like the database:
//! owner-only (0600) in the private data dir, written through an exclusive temp file and a rename,
//! read only as a regular file this uid owns, never through a link. Nothing in it is logged or
//! shown: an [`Error`] says what is wrong with a file, never what is in it.
//!
//! The parser is strict where a file could do harm and lenient where a newer build would differ:
//! a file is at most [`MAX_BYTES`], carries [`FORMAT`] and a version this build reads, and every
//! setting in it is validated by [`Settings::set`] as the Settings panel's are (which registers a
//! URL's secrets for masking first, as provisioning does). Fields and settings this build does
//! not know are ignored; a known setting with a value it does not accept, a favourite that is not
//! a channel key, or a layout of the wrong shape refuses the whole file. Nothing is applied before
//! the whole file parsed, and the layout, which storage checks once more as it takes it, is
//! applied first ([`iptv::storage::Storage::set_user_layout`]): a layout it refuses changes
//! nothing either.
//!
//! The parental settings never travel ([`carried`]): the PIN and what it blocks change only
//! through the PIN dialog, and an import asks for no PIN.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;

use iptv::model::ChannelKey;
use iptv::storage::UserLayout;
use serde_json::{Map, Value};
use tv_platform::i18n::msg;
use tv_screens::panels::settings::{spec, SPECS};
use tv_screens::panels::Settings;

/// The backup file in the data dir.
pub const FILE: &str = "backup.json";
/// An export is written here, then renamed over [`FILE`].
const TEMP: &str = ".backup.json.tmp";
/// What the `format` field holds.
pub const FORMAT: &str = "webosiptv-backup";
/// The format this build writes, and the newest it reads.
pub const VERSION: u64 = 1;
/// The largest backup read or written.
pub const MAX_BYTES: usize = 2 << 20;
/// The most favourites a backup holds.
const MAX_FAVORITES: usize = 10_000;
/// The most entries the settings object may have, known keys or not.
const MAX_SETTINGS: usize = 1024;
/// A favourite's name is written for whoever reads the file, at most this many characters.
const MAX_NAME_CHARS: usize = 128;

/// A favourite as an export writes it: the channel's stable key ([`ChannelKey::of_url`]), and its
/// name while the playlist has the channel. Import goes by the key alone.
pub struct Favorite {
    pub key: ChannelKey,
    pub name: Option<String>,
}

/// A parsed backup, every value validated. No `Debug`: the settings carry tokens.
pub struct Backup {
    /// Settings this build knows, each accepted by its spec, in key order.
    pub settings: Vec<(&'static str, String)>,
    /// The favourites in order; `None` when the file has no list (they are left as they are).
    pub favorites: Option<Vec<ChannelKey>>,
    /// `None` when the file has none (the layout is left as it is).
    pub layout: Option<UserLayout>,
}

/// Why no backup was written or read. Never quotes the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The store is in memory: there is no data dir to keep a file in.
    NoDataDir,
    /// There is no backup file in the data dir.
    Missing,
    Io(io::ErrorKind),
    /// Larger than [`MAX_BYTES`], or more favourites or settings than a backup holds.
    TooLarge,
    /// Not JSON: where the parser stopped.
    Syntax {
        line: usize,
        column: usize,
    },
    /// JSON, but not a backup: this field is missing or of the wrong shape.
    NotBackup(&'static str),
    /// Written by a newer build, in this format version.
    Newer(u64),
    /// A setting this build knows, with a value its spec does not accept.
    Setting(&'static str),
    /// The favourite at this index is not a channel key.
    Favorite(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoDataDir => write!(f, "no data dir"),
            Error::Missing => write!(f, "no {FILE} in the data dir"),
            Error::Io(kind) => write!(f, "{FILE}: {kind:?}"),
            Error::TooLarge => write!(f, "larger than a backup may be"),
            Error::Syntax { line, column } => {
                write!(f, "not JSON (line {line}, column {column})")
            }
            Error::NotBackup(field) => write!(f, "not a backup: `{field}`"),
            Error::Newer(v) => write!(f, "format version {v}, newer than {VERSION}"),
            Error::Setting(key) => write!(f, "setting `{key}` has a value it does not accept"),
            Error::Favorite(i) => write!(f, "favourite {i} is not a channel key"),
        }
    }
}

impl Error {
    /// The toast an import that failed this way shows.
    pub fn toast(self) -> &'static str {
        match self {
            Error::NoDataDir | Error::Missing => msg::backup_none(),
            Error::Newer(_) => msg::backup_newer(),
            _ => msg::backup_invalid(),
        }
    }
}

/// Whether a backup carries setting `key`: every one but the parental PIN and what it blocks.
/// Those change only through the PIN dialog; an import would put back an older PIN, or unblock
/// what was blocked after the export, without one.
pub fn carried(key: &str) -> bool {
    !key.starts_with("parental.")
}

/// The backup of `settings` (every [`carried`] key, defaults included, so an import puts back
/// exactly these), `favorites` and `layout`, as [`FILE`] holds it. [`Error::TooLarge`] rather
/// than a file the import would refuse.
pub fn to_json(
    settings: &Settings,
    favorites: &[Favorite],
    layout: Option<&UserLayout>,
) -> Result<String, Error> {
    if favorites.len() > MAX_FAVORITES {
        return Err(Error::TooLarge);
    }
    let settings: Map<String, Value> = SPECS
        .iter()
        .filter(|s| carried(s.key))
        .map(|s| (s.key.to_owned(), Value::from(settings.get(s.key))))
        .collect();
    let favorites: Vec<Value> = favorites
        .iter()
        .map(|f| {
            let mut entry = Map::new();
            entry.insert("key".into(), f.key.hex().into());
            if let Some(name) = &f.name {
                let name: String = name.chars().take(MAX_NAME_CHARS).collect();
                entry.insert("name".into(), name.into());
            }
            Value::Object(entry)
        })
        .collect();
    let mut backup = serde_json::json!({
        "format": FORMAT,
        "version": VERSION,
        "app": env!("CARGO_PKG_VERSION"),
        "settings": settings,
        "favorites": favorites,
    });
    if let (Some(layout), Some(root)) = (layout, backup.as_object_mut()) {
        let layout = serde_json::to_value(layout).map_err(|_| Error::TooLarge)?;
        root.insert("layout".into(), layout);
    }
    let mut text = serde_json::to_string_pretty(&backup).map_err(|_| Error::TooLarge)?;
    text.push('\n');
    if text.len() > MAX_BYTES {
        return Err(Error::TooLarge);
    }
    Ok(text)
}

/// The backup in `bytes`, validated whole: nothing comes back unless every part of it parsed.
pub fn parse(bytes: &[u8]) -> Result<Backup, Error> {
    if bytes.len() > MAX_BYTES {
        return Err(Error::TooLarge);
    }
    let root: Value = serde_json::from_slice(bytes).map_err(|e| Error::Syntax {
        line: e.line(),
        column: e.column(),
    })?;
    let root = root.as_object().ok_or(Error::NotBackup("{}"))?;
    if root.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(Error::NotBackup("format"));
    }
    let version = root
        .get("version")
        .and_then(Value::as_u64)
        .filter(|&v| v > 0)
        .ok_or(Error::NotBackup("version"))?;
    if version > VERSION {
        return Err(Error::Newer(version));
    }
    let settings = match root.get("settings") {
        Some(v) => settings(v)?,
        None => Vec::new(),
    };
    let favorites = root.get("favorites").map(favorites).transpose()?;
    // Never serde's message: it can quote the value it refused.
    let layout = root
        .get("layout")
        .map(|v| serde_json::from_value(v.clone()).map_err(|_| Error::NotBackup("layout")))
        .transpose()?;
    Ok(Backup {
        settings,
        favorites,
        layout,
    })
}

/// The known settings in `v` a backup [`carried`], each through [`Settings::set`] as the panel's
/// are; the others are ignored as unknown ones are.
fn settings(v: &Value) -> Result<Vec<(&'static str, String)>, Error> {
    let map = v.as_object().ok_or(Error::NotBackup("settings"))?;
    if map.len() > MAX_SETTINGS {
        return Err(Error::TooLarge);
    }
    // Each key appears once, so each is set on its default here: set or equal means accepted.
    let mut probe = Settings::new();
    let mut out = Vec::new();
    for (key, value) in map {
        let Some(spec) = spec(key).filter(|s| carried(s.key)) else {
            continue;
        };
        let value = value.as_str().ok_or(Error::Setting(spec.key))?;
        if !(probe.set(spec.key, value.to_owned()) || probe.get(spec.key) == value) {
            return Err(Error::Setting(spec.key));
        }
        out.push((spec.key, value.to_owned()));
    }
    Ok(out)
}

fn favorites(v: &Value) -> Result<Vec<ChannelKey>, Error> {
    let list = v.as_array().ok_or(Error::NotBackup("favorites"))?;
    if list.len() > MAX_FAVORITES {
        return Err(Error::TooLarge);
    }
    list.iter()
        .enumerate()
        .map(|(i, f)| {
            f.get("key")
                .and_then(Value::as_str)
                .and_then(channel_key)
                .ok_or(Error::Favorite(i))
        })
        .collect()
}

/// [`ChannelKey::hex`] read back: exactly sixteen hex digits.
fn channel_key(hex: &str) -> Option<ChannelKey> {
    if hex.len() != 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let bits = u64::from_str_radix(hex, 16).ok()?;
    Some(ChannelKey(bits as i64))
}

/// Write `json` as [`FILE`] in `dir`, owner-only, replacing the one there: through an exclusive
/// temp file and a rename, so a cut write never leaves half a backup and a link at either name is
/// never written through.
pub fn write(dir: &Path, json: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let temp = dir.join(TEMP);
    // One an export cut short left; removing a link removes the link, never its target.
    match std::fs::remove_file(&temp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temp)?;
    let written = file
        .write_all(json.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    match written.and_then(|()| std::fs::rename(&temp, dir.join(FILE))) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

/// The bytes of [`FILE`] in `dir`: a regular file this uid owns (a looser mode is tightened), at
/// most [`MAX_BYTES`].
pub fn read(dir: &Path) -> Result<Vec<u8>, Error> {
    let path = dir.join(FILE);
    let file = tv_base::paths::private_file(&path, tv_base::paths::euid(), false)
        .map_err(|e| Error::Io(e.kind()))?
        .ok_or(Error::Missing)?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::Io(e.kind()))?;
    if bytes.len() > MAX_BYTES {
        return Err(Error::TooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iptv::storage::{CustomGroup, GroupRef};
    use std::os::unix::fs::PermissionsExt;
    use tv_screens::panels::keys;

    /// A playlist URL whose path and query carry a token-shaped run, built at run time.
    fn secret_url() -> (String, String) {
        let token: String = "Zq8w".repeat(5).chars().take(17).collect();
        let url = format!("https://pl.example.test/get/{token}/list.m3u?pass={token}");
        (url, token)
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("webosiptv-backup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The toasts read the locale, fixed to English here as boot sets the TV's.
    fn locale() {
        tv_platform::i18n::initialize(tv_platform::i18n::Preference::En, true);
    }

    fn favorite(n: i64, name: &str) -> Favorite {
        Favorite {
            key: ChannelKey(n),
            name: Some(name.into()),
        }
    }

    /// A minimal file around `body` (more top-level fields).
    fn file(body: &str) -> Vec<u8> {
        format!(r#"{{"format":"{FORMAT}","version":1{body}}}"#).into_bytes()
    }

    #[test]
    fn an_export_parses_back_to_the_same_settings_favourites_and_layout() {
        let (url, _) = secret_url();
        let mut s = Settings::new();
        assert!(s.set(keys::PLAYLIST_URL, url.clone()));
        assert!(s.set(keys::LONG_PRESS_MS, "800".into()));
        assert!(s.set(keys::GROUPS_HIDDEN, "News\nSport".into()));
        let favorites = [
            favorite(-3, "Jeden"),
            favorite(i64::MAX, "Dwa"),
            favorite(7, ""),
        ];
        let layout = UserLayout {
            custom_groups: vec![CustomGroup {
                name: "Sport+".into(),
                channels: vec![(-3, true), (7, false)],
            }],
            group_order: vec![
                GroupRef::Custom("Sport+".into()),
                GroupRef::Playlist("News".into()),
            ],
            channel_order: vec![("News".into(), vec![7, -3])],
        };
        let json = to_json(&s, &favorites, Some(&layout)).unwrap();
        let back = parse(json.as_bytes()).unwrap();
        assert_eq!(back.layout, Some(layout));
        let carried_specs = SPECS.iter().filter(|s| carried(s.key)).count();
        assert_eq!(
            back.settings.len(),
            carried_specs,
            "every key, defaults too"
        );
        let restored = Settings::from_stored(back.settings.iter().map(|(k, v)| (*k, v.as_str())));
        assert_eq!(restored, s);
        let keys = back.favorites.unwrap();
        assert_eq!(keys, [ChannelKey(-3), ChannelKey(i64::MAX), ChannelKey(7)]);
        assert!(json.contains("\"name\": \"Jeden\""), "names for a reader");
        let bare = to_json(&s, &[], None).unwrap();
        let back = parse(bare.as_bytes()).unwrap();
        assert!(
            back.layout.is_none(),
            "none stored: the layout is left as it is"
        );
        assert_eq!(back.favorites.as_deref(), Some(&[][..]));
    }

    #[test]
    fn unknown_fields_and_settings_are_ignored() {
        let body = concat!(
            r#","from":"a newer build","extra":[1,2],"#,
            r#""settings":{"no.such.key":"x","remote.long_press_ms":"300","nested":{"a":[1]}}"#
        );
        let b = parse(&file(body)).unwrap();
        assert_eq!(b.settings, [(keys::LONG_PRESS_MS, "300".to_owned())]);
        assert!(b.favorites.is_none(), "no list: the favourites stay");
        assert!(b.layout.is_none());
        let extra = r#","layout":{"custom_groups":[],"group_order":[],"channel_order":[],"x":1}"#;
        let b = parse(&file(extra)).unwrap();
        assert_eq!(b.layout, Some(UserLayout::default()));
        let b = parse(&file(r#","favorites":[]"#)).unwrap();
        assert_eq!(
            b.favorites.as_deref(),
            Some(&[][..]),
            "an empty list empties them"
        );
        assert!(b.settings.is_empty());
    }

    /// The PIN and what it blocks change only through the PIN dialog: an export leaves them out
    /// and an import passes over them, whatever the file says.
    #[test]
    fn the_parental_settings_never_travel() {
        let parental = [
            keys::PARENTAL_PIN,
            keys::PARENTAL_CHANNELS,
            keys::PARENTAL_GROUPS,
        ];
        assert!(parental.iter().all(|k| !carried(k)));
        let mut s = Settings::new();
        assert!(s.set(keys::PARENTAL_GROUPS, "Adult".into()));
        assert!(s.set(keys::LONG_PRESS_MS, "800".into()));
        let json = to_json(&s, &[], None).unwrap();
        assert!(
            !json.contains("parental.") && !json.contains("Adult"),
            "{json}"
        );
        let file_with = file(&format!(
            r#","settings":{{"{}":"","{}":"","{}":"","remote.long_press_ms":"300"}}"#,
            keys::PARENTAL_PIN,
            keys::PARENTAL_CHANNELS,
            keys::PARENTAL_GROUPS
        ));
        let b = parse(&file_with).unwrap();
        assert_eq!(b.settings, [(keys::LONG_PRESS_MS, "300".to_owned())]);
    }

    #[test]
    fn the_version_is_checked() {
        locale();
        let newer = format!(r#"{{"format":"{FORMAT}","version":2,"settings":{{}}}}"#);
        assert_eq!(parse(newer.as_bytes()).err(), Some(Error::Newer(2)));
        assert_eq!(Error::Newer(2).toast(), msg::backup_newer());
        for version in ["0", "\"1\"", "1.5", "-1", "null"] {
            let text = format!(r#"{{"format":"{FORMAT}","version":{version}}}"#);
            assert_eq!(
                parse(text.as_bytes()).err(),
                Some(Error::NotBackup("version")),
                "{version}"
            );
        }
        let none = format!(r#"{{"format":"{FORMAT}"}}"#);
        assert_eq!(
            parse(none.as_bytes()).err(),
            Some(Error::NotBackup("version"))
        );
    }

    #[test]
    fn malformed_files_are_refused_whole() {
        let cases: &[(&[u8], Error)] = &[
            (b"", Error::Syntax { line: 1, column: 0 }),
            (
                b"{\"format\":",
                Error::Syntax {
                    line: 1,
                    column: 10,
                },
            ),
            (b"[1]", Error::NotBackup("{}")),
            (
                b"{\"format\":\"other\",\"version\":1}",
                Error::NotBackup("format"),
            ),
            (b"{\"version\":1}", Error::NotBackup("format")),
            (&file(r#","settings":[]"#), Error::NotBackup("settings")),
            (&file(r#","favorites":{}"#), Error::NotBackup("favorites")),
            (&file(r#","layout":[1]"#), Error::NotBackup("layout")),
            (
                &file(r#","layout":{"custom_groups":[{"name":"a","channels":[[1]]}]}"#),
                Error::NotBackup("layout"),
            ),
            (
                &file(r#","settings":{"remote.long_press_ms":"250"}"#),
                Error::Setting(keys::LONG_PRESS_MS),
            ),
            (
                &file(r#","settings":{"general.last_channel_on_start":true}"#),
                Error::Setting(keys::LAST_CHANNEL_ON_START),
            ),
            (
                &file(r#","favorites":[{"key":"00000000000000ff"},{"key":"ff"}]"#),
                Error::Favorite(1),
            ),
            (
                &file(r#","favorites":["00000000000000ff"]"#),
                Error::Favorite(0),
            ),
            (
                &file(r#","favorites":[{"key":"+0000000000000ff"}]"#),
                Error::Favorite(0),
            ),
            (
                b"{\"format\":\"webosiptv-backup\",\"version\":1} x",
                Error::Syntax {
                    line: 1,
                    column: 46,
                },
            ),
            (b"\xff\xfe{}", Error::Syntax { line: 1, column: 1 }),
        ];
        for (bytes, want) in cases {
            let got = parse(bytes).err();
            match (got, want) {
                // Where a syntax error is reported is serde_json's; that it is one is ours.
                (Some(Error::Syntax { .. }), Error::Syntax { .. }) => {}
                (got, want) => assert_eq!(got, Some(*want), "{}", String::from_utf8_lossy(bytes)),
            }
        }
        // A good setting before the bad one is not applied: nothing comes back at all.
        let mixed = file(r#","settings":{"epg.past_days":"3","player.info_secs":"4"}"#);
        assert_eq!(
            parse(&mixed).err(),
            Some(Error::Setting(keys::PLAYER_INFO_SECS))
        );
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert!(matches!(parse(deep.as_bytes()), Err(Error::Syntax { .. })));
    }

    #[test]
    fn oversized_files_are_refused_before_parsing_and_never_read_whole() {
        let pad = " ".repeat(MAX_BYTES);
        let big = file(&format!(",\"pad\":\"{pad}\""));
        assert_eq!(parse(&big).err(), Some(Error::TooLarge));
        let many: Vec<String> = (0..=MAX_FAVORITES)
            .map(|i| format!(r#"{{"key":"{:016x}"}}"#, i))
            .collect();
        let list = file(&format!(",\"favorites\":[{}]", many.join(",")));
        assert_eq!(parse(&list).err(), Some(Error::TooLarge));
        let favorites: Vec<Favorite> = (0..=MAX_FAVORITES as i64)
            .map(|n| favorite(n, "x"))
            .collect();
        assert_eq!(
            to_json(&Settings::new(), &favorites, None).err(),
            Some(Error::TooLarge)
        );
        let dir = scratch("big");
        std::fs::write(dir.join(FILE), &big).unwrap();
        assert_eq!(read(&dir).err(), Some(Error::TooLarge));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn errors_about_a_token_bearing_file_never_carry_the_token() {
        locale();
        let (url, token) = secret_url();
        let bad_scheme = url.replace("https://", "ftp://");
        let files = [
            file(&format!(r#","settings":{{"playlist.url":"{bad_scheme}"}}"#)),
            file(&format!(r#","settings":{{"epg.url":"{url} x"}}"#)),
            file(&format!(
                r#","settings":{{"playlist.url":"{url}"}},"favorites":[{{"key":"{token}"}}]"#
            )),
            format!(r#"{{"format":"{FORMAT}","version":1,"settings":{{"playlist.url":"{url}"#)
                .into_bytes(),
            format!(r#"{{"format":"{url}","version":1}}"#).into_bytes(),
            format!(r#"{{"format":"{FORMAT}","version":1,"{token}":{{"{token}":"#).into_bytes(),
            file(&format!(
                r#","layout":{{"custom_groups":[{{"name":"{token}","channels":"{url}"}}]}}"#
            )),
            file(&format!(
                r#","layout":{{"group_order":[{{"{token}":"{url}"}}]}}"#
            )),
        ];
        for bytes in &files {
            let e = parse(bytes).err().expect("refused");
            for shown in [e.to_string(), format!("{e:?}"), e.toast().to_owned()] {
                assert!(
                    !shown.contains(&token) && !shown.contains("pl.example"),
                    "{shown}"
                );
            }
        }
    }

    #[test]
    fn an_imported_url_is_registered_for_masking_even_in_a_refused_file() {
        // Plain words: in free text only a registered secret is masked.
        let (account, key) = ("apricotowner", "medlarsecret");
        let line = format!("refused for {account} with {key}");
        assert_eq!(iptv::redact::text(&line), line);
        let url = format!("http://pl.example.test/{account}/list.m3u?k={key}");
        let refused = file(&format!(
            r#","settings":{{"playlist.url":"{url}","remote.long_press_ms":"9"}}"#
        ));
        assert_eq!(
            parse(&refused).err(),
            Some(Error::Setting(keys::LONG_PRESS_MS))
        );
        let masked = iptv::redact::text(&line);
        assert!(
            !masked.contains(account) && !masked.contains(key),
            "{masked}"
        );
    }

    #[test]
    fn the_file_is_written_owner_only_and_read_back() {
        let dir = scratch("io");
        assert_eq!(read(&dir).err(), Some(Error::Missing));
        write(&dir, "{\"a\":1}").unwrap();
        let path = dir.join(FILE);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(read(&dir).unwrap(), b"{\"a\":1}");
        // A stale temp file is replaced; a newer export replaces the file.
        std::fs::write(dir.join(TEMP), "stale").unwrap();
        write(&dir, "{}").unwrap();
        assert_eq!(read(&dir).unwrap(), b"{}");
        assert!(!dir.join(TEMP).exists());
        // A loose mode is tightened on read.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read(&dir).unwrap(), b"{}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn links_are_never_followed() {
        let dir = scratch("links");
        let target = dir.join("elsewhere");
        std::fs::write(&target, file("")).unwrap();
        std::os::unix::fs::symlink(&target, dir.join(FILE)).unwrap();
        assert!(
            matches!(read(&dir), Err(Error::Io(_))),
            "a linked backup is not read"
        );
        // The temp name as a link: removed, and the export lands in a file of its own.
        std::os::unix::fs::symlink(&target, dir.join(TEMP)).unwrap();
        write(&dir, "{}").unwrap();
        assert_eq!(
            std::fs::read(&target).unwrap(),
            file(""),
            "the target is untouched"
        );
        let meta = std::fs::symlink_metadata(dir.join(FILE)).unwrap();
        assert!(meta.is_file(), "the link at the name was replaced");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
