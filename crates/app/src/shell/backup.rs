//! [`Command::Backup`](tv_screens::data::Command::Backup), from Settings > General > Backup:
//! export the settings, the favourites and the group layout to the data dir's backup file, or
//! restore them from it ([`crate::backup`]). A toast says how it went; the log line says why,
//! never with a value.

use tv_platform::i18n::msg;
use tv_screens::data::BackupOp;

use super::Shell;
use crate::backup::{self, Error};
use crate::logs::log;

impl Shell {
    pub(super) fn backup(&mut self, op: BackupOp, now: u64) {
        let (toast, line) = match op {
            BackupOp::Export => self.export_backup(),
            BackupOp::Import => self.import_backup(),
        };
        log(&line);
        self.toast(toast.to_owned(), now);
    }

    /// The toast and the log line of an export.
    pub(super) fn export_backup(&self) -> (&'static str, String) {
        let favorites = self.store.favorite_list();
        let layout = self.store.backup_layout();
        let written = match self.store.data_dir() {
            Some(dir) => backup::to_json(&self.settings, &favorites, layout.as_ref())
                .and_then(|json| backup::write(dir, &json).map_err(|e| Error::Io(e.kind()))),
            None => Err(Error::NoDataDir),
        };
        match written {
            Ok(()) => (
                msg::backup_exported(),
                format!(
                    "backup: exported the settings and {} favourites",
                    favorites.len()
                ),
            ),
            Err(e) => (
                msg::backup_export_failed(),
                format!("backup: export failed: {e}"),
            ),
        }
    }

    /// The toast and the log line of an import. A file that does not parse whole changes nothing.
    /// The layout goes first, as storage checks it once more and may refuse it (then nothing is
    /// changed either); then the favourites; then every setting through the same path as the
    /// Settings panel's, so each change has its usual effect (a new playlist URL refreshes); then
    /// every screen reads the data again.
    pub(super) fn import_backup(&mut self) -> (&'static str, String) {
        let read = match self.store.data_dir() {
            Some(dir) => backup::read(dir),
            None => Err(Error::NoDataDir),
        };
        let backup = match read.and_then(|bytes| backup::parse(&bytes)) {
            Ok(backup) => backup,
            Err(e) => return (e.toast(), format!("backup: import refused: {e}")),
        };
        if let Some(layout) = &backup.layout {
            if !self.store.restore_layout(layout) {
                let line = "backup: import refused: storage refused the layout".to_owned();
                return (msg::backup_invalid(), line);
            }
        }
        let favorites = match &backup.favorites {
            Some(keys) => {
                self.store.set_favorites(keys);
                keys.len().to_string()
            }
            None => "no".to_owned(),
        };
        let settings = backup.settings.len();
        for (key, value) in backup.settings {
            self.set_setting(key, value);
        }
        self.refresh_screens();
        (
            msg::backup_imported(),
            format!(
                "backup: imported {settings} settings, {favorites} favourites and {}",
                if backup.layout.is_some() {
                    "the layout"
                } else {
                    "no layout"
                }
            ),
        )
    }
}
