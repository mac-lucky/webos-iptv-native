use super::*;
use crate::data::fake::FakeData;
use crate::data::Navigate;
use crate::panels::testing::{draw, press};
use iptv::model::Timestamp;

const NOW: Timestamp = 1_700_000_000;
/// A playlist URL whose path and query carry a token.
const SECRET: &str = "abcdefghijk123456";

/// Six channels: News holds Channel 1 and 4, Sport 2 and 5, Movies 3 and 6; the custom group
/// Kids holds Channel 1, 2 and 3.
fn data() -> FakeData {
    let mut d = FakeData::new(NOW, 6);
    let kids = d.channels[..3].iter().map(|c| c.key).collect();
    d.custom_groups.push(("Kids".into(), kids));
    d
}

fn open(page: LibraryPage, data: &FakeData, settings: &Settings) -> LibraryPanel {
    let cx = PanelCtx { data, settings };
    LibraryPanel::open(&page, &cx).expect("the page opens")
}

fn rows(panel: &LibraryPanel) -> Vec<(String, Option<String>, Option<bool>)> {
    panel
        .panel()
        .page()
        .rows()
        .iter()
        .map(|r| {
            (
                r.label().to_owned(),
                r.value_text().map(str::to_owned),
                r.checked(),
            )
        })
        .collect()
}

fn labels(panel: &LibraryPanel) -> Vec<String> {
    rows(panel).into_iter().map(|(l, _, _)| l).collect()
}

fn value(panel: &LibraryPanel, label: &str) -> Option<String> {
    rows(panel)
        .into_iter()
        .find(|(l, _, _)| l == label)
        .and_then(|(_, v, _)| v)
}

/// Focus the row labelled `label` and press OK.
fn choose(panel: &mut LibraryPanel, data: &FakeData, label: &str) -> Output {
    let i = labels(panel)
        .iter()
        .position(|l| l == label)
        .unwrap_or_else(|| panic!("{label:?} in {:?}", labels(panel)));
    let focus = panel.panel().page().focus();
    let moves = if focus > i {
        vec![Key::Up; focus - i]
    } else {
        vec![Key::Down; i - focus]
    };
    press(panel, data, &moves);
    press(panel, data, &[Key::Ok])
}

fn edit(edit: LibraryEdit) -> Command {
    Command::Library(edit)
}

fn set(key: &str, value: &str) -> Command {
    Command::SetSetting {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

fn custom(name: &str) -> Group {
    Group::Custom(name.into())
}

fn playlist(name: &str) -> Group {
    Group::Playlist(name.into())
}

#[test]
fn playlists_are_listed_and_one_is_added_by_its_url_never_shown_whole() {
    let d = data();
    let mut panel = open(LibraryPage::Playlists, &d, &Settings::new());
    assert_eq!(panel.panel().page().title(), "Playlists");
    assert_eq!(
        rows(&panel),
        [
            ("Playlist".into(), Some("6 channels".into()), None),
            ("backup.host".into(), Some("Off".into()), None),
            ("Add playlist".into(), None, None),
        ]
    );
    choose(&mut panel, &d, "Add playlist");
    assert!(panel.editing(), "the keyboard is up");
    panel.text("ftp://new.host/list.m3u", &d);
    let out = press(&mut panel, &d, &[Key::Ok]);
    assert_eq!(out.toast.as_deref(), Some("Enter an http or https URL"));
    assert!(out.commands.is_empty());
    press(&mut panel, &d, &[Key::Clear]);
    let url = format!("http://new.host/pl/{SECRET}/list.m3u?token={SECRET}");
    panel.text(&url, &d);
    let shown = panel.panel().page().field().unwrap().to_owned();
    assert!(
        shown.starts_with("http://new.host/") && !shown.contains(SECRET),
        "{shown}"
    );
    let out = press(&mut panel, &d, &[Key::Ok]);
    assert_eq!(
        out.commands,
        [edit(LibraryEdit::AddPlaylist(SecretUrl(url.clone())))]
    );
    assert_eq!(out.toast.as_deref(), Some("Playlist added"));
    assert!(!panel.editing() && panel.panel().depth() == 1);
    let logged = format!("{:?}", out.commands);
    assert!(!logged.contains(SECRET), "{logged}");
    assert!(
        !redact::text(&url).contains(SECRET),
        "registered for masking"
    );
    draw(&mut panel);
}

#[test]
fn with_no_playlist_one_an_added_playlist_becomes_the_settings_url() {
    let mut d = data();
    d.playlists.clear();
    let mut panel = open(LibraryPage::Playlists, &d, &Settings::new());
    assert_eq!(labels(&panel), ["Add playlist"]);
    choose(&mut panel, &d, "Add playlist");
    panel.text("http://new.host/list.m3u", &d);
    let out = press(&mut panel, &d, &[Key::Ok]);
    assert_eq!(
        out.commands,
        [set(keys::PLAYLIST_URL, "http://new.host/list.m3u")]
    );
    assert_eq!(
        panel.settings().get(keys::PLAYLIST_URL),
        "http://new.host/list.m3u"
    );
}

#[test]
fn a_playlist_is_enabled_sorted_and_removed_from_its_page() {
    let d = data();
    let mut panel = open(LibraryPage::Playlists, &d, &Settings::new());
    choose(&mut panel, &d, "backup.host");
    assert_eq!(panel.panel().page().title(), "backup.host");
    assert_eq!(
        labels(&panel),
        ["Enabled", "Playlist URL", "Move up", "Remove playlist"]
    );
    assert_eq!(value(&panel, "Enabled").as_deref(), Some("Off"));
    let out = choose(&mut panel, &d, "Enabled");
    assert_eq!(
        out.commands,
        [edit(LibraryEdit::SetPlaylistEnabled {
            playlist: 2,
            enabled: true
        })]
    );
    assert_eq!(value(&panel, "Enabled").as_deref(), Some("On"));
    let out = choose(&mut panel, &d, "Move up");
    assert_eq!(
        out.commands,
        [edit(LibraryEdit::MovePlaylist {
            playlist: 2,
            up: true
        })]
    );
    assert_eq!(
        labels(&panel),
        ["Enabled", "Playlist URL", "Move down", "Remove playlist"],
        "first now"
    );
    // Its URL is its own; the field starts with it, masked.
    choose(&mut panel, &d, "Playlist URL");
    assert_eq!(
        panel.panel().page().field(),
        Some(redact::url("http://backup.host/list.m3u").as_str())
    );
    press(&mut panel, &d, &[Key::Clear]);
    panel.text("https://backup.host/new.m3u", &d);
    let out = press(&mut panel, &d, &[Key::Ok]);
    assert_eq!(
        out.commands,
        [edit(LibraryEdit::SetPlaylistUrl {
            playlist: 2,
            url: SecretUrl("https://backup.host/new.m3u".into()),
        })]
    );
    choose(&mut panel, &d, "Remove playlist");
    assert_eq!(panel.panel().page().title(), "Remove this playlist?");
    let out = choose(&mut panel, &d, "Cancel");
    assert!(out.commands.is_empty());
    assert_eq!(panel.panel().page().title(), "backup.host");
    choose(&mut panel, &d, "Remove playlist");
    let out = choose(&mut panel, &d, "Remove playlist");
    assert_eq!(out.commands, [edit(LibraryEdit::RemovePlaylist(2))]);
    assert_eq!(out.toast.as_deref(), Some("Playlist removed"));
    assert_eq!(panel.panel().depth(), 1);
    assert_eq!(labels(&panel), ["Playlist", "Add playlist"]);
}

#[test]
fn playlist_one_is_the_settings_url() {
    let d = data();
    let mut settings = Settings::new();
    settings.set(keys::PLAYLIST_URL, "http://host/list.m3u".into());
    let mut panel = open(LibraryPage::Playlists, &d, &settings);
    choose(&mut panel, &d, "Playlist");
    choose(&mut panel, &d, "Playlist URL");
    press(&mut panel, &d, &[Key::Clear]);
    panel.text("http://host/other.m3u", &d);
    let out = press(&mut panel, &d, &[Key::Ok]);
    assert_eq!(
        out.commands,
        [set(keys::PLAYLIST_URL, "http://host/other.m3u")]
    );
    choose(&mut panel, &d, "Remove playlist");
    let out = choose(&mut panel, &d, "Remove playlist");
    assert_eq!(
        out.commands,
        [
            set(keys::PLAYLIST_URL, ""),
            edit(LibraryEdit::RemovePlaylist(MAIN_PLAYLIST))
        ],
        "the Settings URL is cleared with it"
    );
}

#[test]
fn manage_groups_shows_and_hides_every_group_the_built_in_ones_too() {
    let d = data();
    let mut panel = open(LibraryPage::Groups, &d, &Settings::new());
    assert_eq!(panel.panel().page().title(), "Manage groups");
    assert_eq!(
        rows(&panel),
        [
            ("New group".into(), None, None),
            ("Reorder groups".into(), None, None),
            ("Favorites".into(), None, Some(true)),
            ("All channels".into(), None, Some(true)),
            ("History".into(), None, Some(true)),
            ("Kids".into(), None, None),
            ("News".into(), None, Some(true)),
            ("Sport".into(), None, Some(true)),
            ("Movies".into(), None, Some(true)),
        ]
    );
    let out = choose(&mut panel, &d, "News");
    assert_eq!(out.commands, [set(keys::GROUPS_HIDDEN, "News")]);
    let out = choose(&mut panel, &d, "Favorites");
    assert_eq!(out.commands, [set(keys::GROUPS_SHOW_FAVORITES, "false")]);
    let checks: Vec<Option<bool>> = rows(&panel).into_iter().map(|(_, _, c)| c).collect();
    assert_eq!(checks[2], Some(false));
    assert_eq!(checks[6], Some(false));
    // A hidden group is still listed, so it can be shown again.
    let out = choose(&mut panel, &d, "News");
    assert_eq!(out.commands, [set(keys::GROUPS_HIDDEN, "")]);
    // A custom group opens its page.
    choose(&mut panel, &d, "Kids");
    assert_eq!(
        labels(&panel),
        ["Manage channels", "Rename group", "Delete group"]
    );
}

#[test]
fn groups_are_reordered_by_carrying_one_and_back_keeps_the_order() {
    let d = data();
    let mut panel = open(LibraryPage::Groups, &d, &Settings::new());
    choose(&mut panel, &d, "Reorder groups");
    assert_eq!(panel.panel().page().title(), "Reorder groups");
    assert_eq!(
        rows(&panel),
        [
            ("Kids".into(), Some("OK: move".into()), None),
            ("News".into(), None, None),
            ("Sport".into(), None, None),
            ("Movies".into(), None, None),
        ],
        "the built-in groups always lead"
    );
    // Nothing moved: Back saves nothing.
    let out = press(&mut panel, &d, &[Key::Down, Key::Back]);
    assert!(out.commands.is_empty());
    choose(&mut panel, &d, "Reorder groups");
    press(&mut panel, &d, &[Key::Ok]);
    assert_eq!(value(&panel, "Kids").as_deref(), Some("OK: drop here"));
    press(&mut panel, &d, &[Key::Down, Key::Down, Key::Ok, Key::Down]);
    assert_eq!(labels(&panel), ["News", "Sport", "Kids", "Movies"]);
    assert_eq!(
        panel.panel().page().focus(),
        3,
        "dropped, the focus moves on alone"
    );
    let out = press(&mut panel, &d, &[Key::Back]);
    assert_eq!(
        out.commands,
        [edit(LibraryEdit::SetGroupOrder(vec![
            playlist("News"),
            playlist("Sport"),
            custom("Kids"),
            playlist("Movies"),
        ]))]
    );
    assert_eq!(
        labels(&panel)[5..],
        ["News", "Sport", "Kids", "Movies"],
        "Manage groups shows the new order"
    );
}

#[test]
fn a_new_group_is_named_with_the_keyboard() {
    let d = data();
    let mut panel = open(LibraryPage::Groups, &d, &Settings::new());
    choose(&mut panel, &d, "New group");
    assert_eq!(panel.panel().page().title(), "Group name");
    assert!(panel.editing());
    let out = press(&mut panel, &d, &[Key::Ok]);
    assert!(
        out == Output::default() && panel.editing(),
        "an empty name waits"
    );
    panel.text("Sport", &d);
    let out = press(&mut panel, &d, &[Key::Ok]);
    assert_eq!(out.toast.as_deref(), Some("A group has this name"));
    press(&mut panel, &d, &[Key::Clear]);
    panel.text("  Late night ", &d);
    let out = press(&mut panel, &d, &[Key::Ok]);
    assert_eq!(
        out.commands,
        [edit(LibraryEdit::CreateGroup("Late night".into()))]
    );
    assert_eq!(out.toast.as_deref(), Some("Group created"));
    assert!(labels(&panel).contains(&"Late night".to_owned()));
    // Back without saving makes nothing.
    choose(&mut panel, &d, "New group");
    panel.text("Other", &d);
    let out = press(&mut panel, &d, &[Key::Back]);
    assert!(out.commands.is_empty() && !panel.editing());
}

#[test]
fn a_custom_group_is_renamed_and_deleted_from_its_page() {
    let d = data();
    let mut panel = open(
        LibraryPage::CustomGroup("Kids".into()),
        &d,
        &Settings::new(),
    );
    assert_eq!(panel.panel().page().title(), "Kids");
    choose(&mut panel, &d, "Rename group");
    assert_eq!(panel.panel().page().field(), Some("Kids"));
    press(&mut panel, &d, &[Key::Clear]);
    panel.text("Family", &d);
    let out = press(&mut panel, &d, &[Key::Ok]);
    assert_eq!(
        out.commands,
        [edit(LibraryEdit::RenameGroup {
            from: "Kids".into(),
            to: "Family".into()
        })]
    );
    assert_eq!(panel.panel().page().title(), "Family");
    choose(&mut panel, &d, "Delete group");
    assert_eq!(panel.panel().page().title(), "Delete this group?");
    let out = choose(&mut panel, &d, "Delete group");
    assert_eq!(
        out.commands,
        [edit(LibraryEdit::DeleteGroup("Family".into()))]
    );
    assert_eq!(
        out.navigate,
        [Navigate::Close],
        "the panel was opened on it"
    );
    assert!(panel.closing());
}

#[test]
fn selected_channels_are_copied_or_moved_into_a_custom_group() {
    let d = data();
    let (one, four) = (d.channels[0].key, d.channels[3].key);
    let mut panel = open(
        LibraryPage::Channels(playlist("News")),
        &d,
        &Settings::new(),
    );
    assert_eq!(panel.panel().page().title(), "News");
    assert_eq!(
        rows(&panel),
        [
            ("Copy to group".into(), None, None),
            ("Move to group".into(), None, None),
            ("Reorder channels".into(), None, None),
            ("Channel 1".into(), None, Some(false)),
            ("Channel 4".into(), None, Some(false)),
        ]
    );
    let out = choose(&mut panel, &d, "Copy to group");
    assert_eq!(out.toast.as_deref(), Some("Select channels first"));
    choose(&mut panel, &d, "Channel 1");
    assert_eq!(
        value(&panel, "Copy to group").as_deref(),
        Some("1 selected")
    );
    choose(&mut panel, &d, "Copy to group");
    assert_eq!(labels(&panel), ["Kids", "New group"]);
    let out = choose(&mut panel, &d, "Kids");
    assert_eq!(
        out.commands,
        [edit(LibraryEdit::AddToGroup {
            to: "Kids".into(),
            channels: vec![one],
            from: None,
        })]
    );
    assert_eq!(out.toast.as_deref(), Some("1 channel copied to Kids"));
    assert_eq!(panel.panel().page().title(), "News");
    assert_eq!(
        value(&panel, "Copy to group"),
        None,
        "the selection is done"
    );
    // Moved into a new group: out of this one.
    choose(&mut panel, &d, "Channel 4");
    choose(&mut panel, &d, "Move to group");
    choose(&mut panel, &d, "New group");
    panel.text("Late", &d);
    let out = press(&mut panel, &d, &[Key::Ok]);
    assert_eq!(
        out.commands,
        [
            edit(LibraryEdit::CreateGroup("Late".into())),
            edit(LibraryEdit::AddToGroup {
                to: "Late".into(),
                channels: vec![four],
                from: Some(playlist("News")),
            })
        ]
    );
    assert_eq!(out.toast.as_deref(), Some("1 channel moved to Late"));
    assert_eq!(panel.panel().page().title(), "News");
    assert!(!labels(&panel).contains(&"Channel 4".to_owned()));
}

#[test]
fn a_custom_groups_channels_are_removed_and_reordered() {
    let d = data();
    let keys_of =
        |i: &[usize]| -> Vec<ChannelKey> { i.iter().map(|&i| d.channels[i].key).collect() };
    let mut settings = Settings::new();
    settings.set(keys::GROUPS_CHANNEL_SORT, "name".into());
    let mut panel = open(LibraryPage::Channels(custom("Kids")), &d, &settings);
    assert_eq!(
        labels(&panel),
        [
            "Copy to group",
            "Move to group",
            "Remove from group",
            "Reorder channels",
            "Channel 1",
            "Channel 2",
            "Channel 3"
        ]
    );
    choose(&mut panel, &d, "Copy to group");
    choose(&mut panel, &d, "Channel 3");
    let out = choose(&mut panel, &d, "Remove from group");
    assert_eq!(
        out.commands,
        [edit(LibraryEdit::RemoveFromGroup {
            group: "Kids".into(),
            channels: keys_of(&[2]),
        })]
    );
    assert_eq!(
        out.toast.as_deref(),
        Some("1 channel removed from the group")
    );
    choose(&mut panel, &d, "Reorder channels");
    let out = press(&mut panel, &d, &[Key::Ok, Key::Down, Key::Back]);
    assert_eq!(
        out.commands,
        [
            edit(LibraryEdit::SetChannelOrder {
                group: custom("Kids"),
                channels: keys_of(&[1, 0]),
            }),
            set(keys::GROUPS_CHANNEL_SORT, "playlist"),
        ],
        "the manual order shows under playlist sorting"
    );
    assert_eq!(labels(&panel)[4..], ["Channel 2", "Channel 1"]);
}

#[test]
fn a_refresh_rebuilds_the_page_and_closes_on_a_group_that_is_gone() {
    let mut d = data();
    let mut panel = open(
        LibraryPage::CustomGroup("Kids".into()),
        &d,
        &Settings::new(),
    );
    d.custom_groups.clear();
    panel.refresh(&d);
    assert!(panel.closing());
    let cx = PanelCtx {
        data: &d,
        settings: &Settings::new(),
    };
    assert!(LibraryPanel::open(&LibraryPage::CustomGroup("Kids".into()), &cx).is_none());
    let mut panel = open(LibraryPage::Playlists, &d, &Settings::new());
    d.playlists.remove(1);
    d.playlists[0].channels = 9;
    panel.refresh(&d);
    assert_eq!(
        rows(&panel),
        [
            ("Playlist".into(), Some("9 channels".into()), None),
            ("Add playlist".into(), None, None),
        ]
    );
}

#[test]
fn polish_plurals_read_right() {
    let _pl =
        tv_platform::i18n::language_on_this_thread_for_test(tv_platform::i18n::Preference::Pl);
    assert_eq!(msg::library_channel_count(1), "1 kana\u{142}");
    assert_eq!(msg::library_channel_count(3), "3 kana\u{142}y");
    assert_eq!(msg::library_channel_count(5), "5 kana\u{142}\u{f3}w");
}
