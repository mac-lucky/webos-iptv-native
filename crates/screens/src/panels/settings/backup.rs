//! The Backup page under General: Export writes the backup file; Import, after a confirmation
//! page, restores from it and closes Settings, whose values it replaced. The shell does the work
//! ([`Command::Backup`]) and says in a toast how it went.

use tv_platform::i18n::msg;

use super::Item;
use crate::data::{BackupOp, Command, Output};
use crate::panels::side::{Page, Row, SidePanel};

/// A row of the Backup pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupRow {
    Export,
    /// Opens the confirmation page.
    Import,
    /// The confirmation page's one row.
    ConfirmImport,
}

pub(super) fn row(row: BackupRow) -> Row<Item> {
    let item = Item::Backup(row);
    match row {
        BackupRow::Export => Row::action(item, msg::backup_export()),
        BackupRow::Import => Row::submenu(item, msg::backup_import()),
        BackupRow::ConfirmImport => Row::action(item, msg::backup_import_confirm()),
    }
}

pub(super) fn activate(panel: &mut SidePanel<Item>, row: BackupRow) -> Output {
    match row {
        BackupRow::Export => Output::command(Command::Backup(BackupOp::Export)),
        BackupRow::Import => {
            let rows = vec![self::row(BackupRow::ConfirmImport)];
            panel.push(Page::list(msg::backup_import_question(), rows));
            Output::default()
        }
        BackupRow::ConfirmImport => {
            Output::command(Command::Backup(BackupOp::Import)).and(Output::close())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Settings, SettingsPanel};
    use super::*;
    use crate::data::fake::FakeData;
    use crate::data::Screen;
    use crate::input::Key;
    use crate::panels::testing::{draw, press};

    fn keys(panel: &mut SettingsPanel, keys: &[Key]) -> Output {
        press(panel, &FakeData::new(0, 0), keys)
    }

    fn labels(panel: &SettingsPanel) -> Vec<String> {
        let rows = panel.panel().page().rows();
        rows.iter().map(|r| r.label().to_owned()).collect()
    }

    /// Settings > General > Backup.
    fn open_backup() -> SettingsPanel {
        let mut panel = SettingsPanel::new(Settings::new(), "1");
        assert_eq!(keys(&mut panel, &[Key::Ok]), Output::default());
        assert_eq!(panel.panel().page().title(), "General");
        let at = labels(&panel).iter().position(|l| l == "Backup").unwrap();
        keys(&mut panel, &vec![Key::Down; at]);
        assert_eq!(keys(&mut panel, &[Key::Ok]), Output::default());
        assert_eq!(panel.panel().page().title(), "Backup");
        panel
    }

    #[test]
    fn general_opens_the_backup_page_with_export_and_import() {
        let mut panel = open_backup();
        assert_eq!(labels(&panel), ["Export settings", "Import settings"]);
        draw(&mut panel);
        assert_eq!(
            keys(&mut panel, &[Key::Ok]),
            Output::command(Command::Backup(BackupOp::Export))
        );
        assert!(!panel.closing(), "an export leaves Settings open");
    }

    #[test]
    fn import_asks_first_then_imports_and_closes_settings() {
        let mut panel = open_backup();
        assert_eq!(keys(&mut panel, &[Key::Down, Key::Ok]), Output::default());
        assert_eq!(
            panel.panel().page().title(),
            "Replace settings and favorites?"
        );
        assert_eq!(labels(&panel), ["Import and replace"]);
        draw(&mut panel);
        // Back leaves without importing.
        assert_eq!(keys(&mut panel, &[Key::Back]), Output::default());
        assert_eq!(panel.panel().page().title(), "Backup");
        keys(&mut panel, &[Key::Ok]);
        assert_eq!(
            keys(&mut panel, &[Key::Ok]),
            Output::command(Command::Backup(BackupOp::Import)).and(Output::close())
        );
        assert!(panel.closing());
    }

    #[test]
    fn the_rows_read_in_polish() {
        let _pl =
            tv_platform::i18n::language_on_this_thread_for_test(tv_platform::i18n::Preference::Pl);
        assert_eq!(row(BackupRow::Export).label(), "Eksportuj ustawienia");
        assert_eq!(msg::backup_title(), "Kopia zapasowa");
    }
}
