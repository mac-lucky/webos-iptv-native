//! The accent colour in the running app: blue until set, every screen paints the picked one
//! from the next frame without a restart, and a restart starts on the stored one.

use iptv::storage::Storage;
use tv_screens::theme::{Accent, ACCENT, ACCENT_TEXT};

use super::*;

fn set(s: &mut Sim, value: &str) {
    let (key, value) = (keys::ACCENT_COLOR.to_owned(), value.to_owned());
    s.shell.run(Command::SetSetting { key, value }, s.t);
}

#[test]
pub(super) fn the_accent_colour_applies_at_once() {
    let mut s = Sim::new();
    assert_eq!(theme::accent(), Accent::Blue, "blue by default");
    assert_eq!(theme::ink(ACCENT), ACCENT);
    s.shell.dirty = false;
    set(&mut s, "red");
    assert_eq!(s.shell.settings.get(keys::ACCENT_COLOR), "red");
    assert_eq!(theme::accent(), Accent::Red);
    assert!(s.shell.needs_frame(), "the next frame paints it");
    let (fill, text) = Accent::Red.colors();
    assert_eq!((theme::ink(ACCENT), theme::ink(ACCENT_TEXT)), (fill, text));
    set(&mut s, "pink");
    assert_eq!(
        theme::accent(),
        Accent::Red,
        "a value no accent has is refused"
    );
    set(&mut s, "blue");
    assert_eq!(theme::accent(), Accent::Blue);
}

#[test]
fn a_restart_starts_on_the_stored_accent() {
    let dir = std::env::temp_dir().join(format!("webosiptv-accent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let storage = Storage::open(&dir).unwrap();
    storage.set_setting(keys::ACCENT_COLOR, "teal").unwrap();
    drop(storage);
    theme::set_accent(Accent::Blue);
    locale();
    let shell = Shell::new(
        Box::new(crate::store::open_at(&dir)),
        Box::new(Calls::default()),
        None,
        LogoCache::new(None),
    );
    assert_eq!(theme::accent(), Accent::Teal);
    drop(shell);
    theme::set_accent(Accent::Blue);
    let _ = std::fs::remove_dir_all(&dir);
}
