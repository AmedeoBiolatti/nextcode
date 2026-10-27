//! Settings that follow the editor's file: the user's, under the
//! workspace file's and the file's folder's `.next_code/settings.json`
//! (see `Project::settings_for`).

use std::sync::Arc;

use super::Editor;
use crate::settings::{self, Settings};

impl Editor {
    /// The settings for this editor's file (the user's for editors without
    /// a project file). Cheap: the project caches them per folder.
    pub(crate) fn file_settings(&self) -> Arc<Settings> {
        match (&self.project, self.buffer.path()) {
            (Some(project), Some(path)) => project.settings_for(Some(path)),
            _ => settings::current(),
        }
    }

    /// `"word_wrap"` for this file's folder and language.
    pub(super) fn word_wrap_setting(&self) -> bool {
        self.file_settings().word_wrap(self.language())
    }

    /// `"inlay_hints"` for this file's folder and language.
    pub(super) fn inlay_hints_setting(&self) -> bool {
        self.file_settings().inlay_hints(self.language())
    }
}
