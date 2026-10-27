//! Saving a file: the write runs off the UI thread, never overwrites a
//! file that changed on disk since the buffer last matched it, and never
//! leaves the file truncated (see `worktree::write_file`).

use std::{fs, io, path::Path};

use futures::channel::oneshot;
use gpui::{AppContext as _, Context, Task};
use lsp::types::{DidSaveTextDocumentParams, TextDocumentIdentifier, notification};
use text::DiskStamp;

use super::{Editor, EditorEvent};
use crate::{project::Project, theme};

/// The save under way and the ones asked for meanwhile.
#[derive(Default)]
pub(super) struct SaveState {
    /// A write is under way.
    in_flight: bool,
    /// Asked to save again during the write (`true`: also over a file that
    /// changed on disk); it starts, with the text of that moment, when
    /// the write finishes.
    queued: Option<bool>,
    /// Told whether the save (the queued one, if there is one) succeeded.
    waiters: Vec<oneshot::Sender<bool>>,
}

/// Why nothing was written.
enum SaveError {
    /// The file on disk is not the one the buffer was read from or saved to.
    Changed,
    Io(io::Error),
}

/// The file as it is now, to compare with the buffer's stamp; `None`
/// where the editor cannot stat it (a remote project, a missing file).
pub(crate) fn disk_stamp(project: &Project, path: &Path) -> Option<DiskStamp> {
    if project.is_remote() {
        return None;
    }
    let meta = fs::metadata(path).ok()?;
    Some(DiskStamp {
        modified: meta.modified().ok(),
        len: meta.len(),
    })
}

impl Editor {
    /// Writes the buffer to its file off the UI thread. The task says
    /// whether the save succeeded (the status bar shows why not); dropping
    /// it does not cancel the save. Edits made while the write is under
    /// way keep the buffer dirty. A file that changed on disk since it was
    /// read or saved is left alone: [`EditorEvent::SaveConflict`] asks
    /// what to do (see [`Editor::save_overwriting`]).
    pub fn save(&mut self, cx: &mut Context<Self>) -> Task<bool> {
        self.save_with(false, cx)
    }

    /// Like [`Editor::save`], also over a file that changed on disk.
    pub fn save_overwriting(&mut self, cx: &mut Context<Self>) -> Task<bool> {
        self.save_with(true, cx)
    }

    fn save_with(&mut self, overwrite: bool, cx: &mut Context<Self>) -> Task<bool> {
        let (tx, rx) = oneshot::channel();
        self.save.waiters.push(tx);
        if self.save.in_flight {
            self.save.queued = Some(self.save.queued.unwrap_or(false) || overwrite);
        } else {
            self.start_write(overwrite, cx);
        }
        cx.spawn(async move |_, _| rx.await.unwrap_or(false))
    }

    fn start_write(&mut self, overwrite: bool, cx: &mut Context<Self>) {
        let (Some(project), Some(path)) = (
            self.project.clone(),
            self.buffer.path().map(Path::to_path_buf),
        ) else {
            let err = io::Error::new(io::ErrorKind::NotFound, "buffer has no file");
            let version = self.version;
            self.finish_save(Err(SaveError::Io(err)), String::new(), version, cx);
            return;
        };
        let text = self.text();
        let version = self.version;
        let expected = if overwrite {
            None
        } else {
            self.buffer.disk_stamp()
        };
        let data = text.clone().into_bytes();
        self.save.in_flight = true;
        let write = cx.background_spawn(async move {
            if let Some(expected) = expected
                && let Some(actual) = disk_stamp(&project, &path)
                && actual != expected
            {
                return Err(SaveError::Changed);
            }
            project.write(&path, data).map_err(SaveError::Io)?;
            Ok(disk_stamp(&project, &path))
        });
        // Detached: closing the editor does not cancel a write under way.
        cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |this, cx| {
                this.finish_save(result, text, version, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Takes in the outcome of a write of `written` (the text at
    /// `version`), then starts the queued save or answers the waiters.
    fn finish_save(
        &mut self,
        result: Result<Option<DiskStamp>, SaveError>,
        written: String,
        version: u64,
        cx: &mut Context<Self>,
    ) {
        self.save.in_flight = false;
        let was_dirty = self.buffer.is_dirty();
        let saved = match result {
            Ok(stamp) => {
                self.buffer.set_disk_stamp(stamp);
                // Edits made during the write stay unsaved.
                if self.version == version || self.buffer.equals(&written) {
                    self.buffer.mark_saved();
                    self.write_note_anchors(cx);
                }
                self.error = None;
                if let Some(doc) = &self.lsp {
                    doc.server
                        .connection
                        .notify::<notification::DidSaveTextDocument>(DidSaveTextDocumentParams {
                            text_document: TextDocumentIdentifier::new(doc.uri.clone()),
                            text: doc.server.save_includes_text.then_some(written),
                        });
                }
                cx.emit(EditorEvent::Saved);
                // A saved `settings.json` or theme file applies at once.
                theme::refresh(cx);
                true
            }
            Err(SaveError::Changed) => {
                let name = self
                    .buffer
                    .path()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.error = Some(format!("Not saved: {name} changed on disk").into());
                cx.emit(EditorEvent::SaveConflict);
                false
            }
            Err(SaveError::Io(err)) => {
                self.error = Some(format!("Save failed: {err}").into());
                false
            }
        };
        self.finish_change(was_dirty, cx);
        match self.save.queued.take() {
            Some(overwrite) => self.start_write(overwrite, cx),
            None => {
                for waiter in self.save.waiters.drain(..) {
                    waiter.send(saved).ok();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gpui::{TestAppContext, WindowHandle};
    use text::Buffer;

    use super::*;

    /// An editor for `text` saved as `name` in a temporary project.
    fn project_editor(
        name: &str,
        text: &str,
        cx: &mut TestAppContext,
    ) -> (PathBuf, WindowHandle<Editor>) {
        let root = std::env::temp_dir().join(format!(
            "next_code_save_{}_{}",
            name.replace('.', "_"),
            std::process::id()
        ));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        let path = root.join(name);
        fs::write(&path, text).unwrap();
        let project = Project::local(root);
        let window = cx.add_window(|_, cx| {
            Editor::for_file(Buffer::with_path(text, path.clone()), project, cx)
        });
        (path, window)
    }

    fn dirty(window: WindowHandle<Editor>, cx: &mut TestAppContext) -> bool {
        window.read_with(cx, |e, _| e.buffer.is_dirty()).unwrap()
    }

    #[gpui::test]
    fn saves_off_the_ui_thread_and_marks_the_buffer_clean(cx: &mut TestAppContext) {
        let (path, window) = project_editor("a.rs", "one\n", cx);
        let task = window
            .update(cx, |e, _, cx| {
                e.type_text("x", cx);
                e.save(cx)
            })
            .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "one\n",
            "not written yet"
        );
        assert!(dirty(window, cx));
        cx.run_until_parked();
        assert!(cx.executor().block_test(task));
        assert_eq!(fs::read_to_string(&path).unwrap(), "xone\n");
        assert!(!dirty(window, cx));
        let stamp = window.read_with(cx, |e, _| e.buffer.disk_stamp()).unwrap();
        assert_eq!(stamp.map(|s| s.len), Some(5), "stamped after the write");
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[gpui::test]
    fn edits_during_the_write_stay_unsaved_and_a_second_save_follows(cx: &mut TestAppContext) {
        let (path, window) = project_editor("b.rs", "one\n", cx);
        let first = window
            .update(cx, |e, _, cx| {
                e.type_text("x", cx);
                e.save(cx)
            })
            .unwrap();
        // Typed while the first write is in flight; the second save queues.
        let second = window
            .update(cx, |e, _, cx| {
                e.type_text("y", cx);
                e.save(cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert!(cx.executor().block_test(first));
        assert!(cx.executor().block_test(second));
        assert_eq!(fs::read_to_string(&path).unwrap(), "xyone\n");
        assert!(!dirty(window, cx));

        // Without a second save the buffer stays dirty with the newer text.
        let task = window
            .update(cx, |e, _, cx| {
                e.type_text("1", cx);
                let task = e.save(cx);
                e.type_text("2", cx);
                task
            })
            .unwrap();
        cx.run_until_parked();
        assert!(cx.executor().block_test(task));
        assert_eq!(fs::read_to_string(&path).unwrap(), "xy1one\n");
        assert!(dirty(window, cx), "the '2' is not on disk");
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[gpui::test]
    fn a_file_changed_on_disk_is_only_overwritten_on_request(cx: &mut TestAppContext) {
        let (path, window) = project_editor("c.rs", "one\n", cx);
        fs::write(&path, "changed elsewhere\n").unwrap();
        let task = window
            .update(cx, |e, _, cx| {
                e.type_text("x", cx);
                e.save(cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert!(!cx.executor().block_test(task));
        assert_eq!(fs::read_to_string(&path).unwrap(), "changed elsewhere\n");
        let (is_dirty, error) = window
            .read_with(cx, |e, _| (e.buffer.is_dirty(), e.error.clone()))
            .unwrap();
        assert!(is_dirty);
        assert_eq!(
            error.as_ref().map(|error| error.as_ref()),
            Some("Not saved: c.rs changed on disk")
        );

        let task = window
            .update(cx, |e, _, cx| e.save_overwriting(cx))
            .unwrap();
        cx.run_until_parked();
        assert!(cx.executor().block_test(task));
        assert_eq!(fs::read_to_string(&path).unwrap(), "xone\n");
        assert!(!dirty(window, cx));
        // Stamped by the write: the next save goes through.
        let task = window
            .update(cx, |e, _, cx| {
                e.type_text("y", cx);
                e.save(cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert!(cx.executor().block_test(task));
        assert_eq!(fs::read_to_string(&path).unwrap(), "xyone\n");
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[gpui::test]
    fn a_buffer_without_a_file_reports_the_failure(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, cx| Editor::new(Buffer::new("text"), cx));
        let task = window.update(cx, |e, _, cx| e.save(cx)).unwrap();
        cx.run_until_parked();
        assert!(!cx.executor().block_test(task));
        let error = window.read_with(cx, |e, _| e.error.clone()).unwrap();
        assert_eq!(
            error.as_ref().map(|error| error.as_ref()),
            Some("Save failed: buffer has no file")
        );
    }
}
