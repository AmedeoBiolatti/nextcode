//! Rename Symbol (F2, JetBrains Shift+F6): an input over the symbol, then
//! the server's workspace edit renames it across the project.

use gpui::anchored;
use lsp::types::{
    OneOf, PrepareRenameResponse, RenameParams, TextDocumentPositionParams,
    request::{PrepareRenameRequest, Rename},
};

use super::*;
use crate::lsp_store;

actions!(editor, [RenameSymbol, ConfirmRename, CancelRename]);

pub(super) const RENAME_CONTEXT: &str = "RenameInput";

pub(super) struct RenameInput {
    input: Entity<Editor>,
    /// The symbol being renamed.
    range: Range<usize>,
    /// The name it had (renaming to it does nothing).
    old_name: String,
    position: TextDocumentPositionParams,
    server: Arc<RunningServer>,
}

impl Editor {
    /// The identifier at or just before `offset`.
    fn identifier_at(&self, offset: usize) -> Option<Range<usize>> {
        let line_end = self.buffer.line_end(self.buffer.line_of(offset));
        let after: usize = self
            .buffer
            .slice(offset..line_end)
            .chars()
            .take_while(|&c| is_word_char(c))
            .map(char::len_utf8)
            .sum();
        let end = offset + after;
        let start = language::completion::word_start(&self.buffer, end);
        (start < end).then_some(start..end)
    }

    pub(super) fn rename_symbol(
        &mut self,
        _: &RenameSymbol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.server() else {
            self.notice = Some("No language server for this file".into());
            return cx.notify();
        };
        let prepare = match &server.capabilities.rename_provider {
            Some(OneOf::Left(true)) => false,
            Some(OneOf::Right(options)) => options.prepare_provider == Some(true),
            _ => {
                self.notice = Some("The language server cannot rename symbols".into());
                return cx.notify();
            }
        };
        let offset = self.primary().head;
        let Some(position) = self.position_params(offset) else {
            return;
        };
        let identifier = self.identifier_at(offset);
        if !prepare {
            match identifier {
                Some(range) => self.open_rename(range, None, position, server, window, cx),
                None => {
                    self.notice = Some("Nothing to rename here".into());
                    cx.notify();
                }
            }
            return;
        }
        let version = self.version;
        let executor = cx.background_executor().clone();
        self._rename_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = lsp_store::request_retrying::<PrepareRenameRequest>(
                &server,
                position.clone(),
                &executor,
                2,
                Duration::from_millis(400),
            )
            .await;
            this.update_in(cx, |this, window, cx| {
                if this.version != version {
                    return;
                }
                let encoding = server.encoding;
                let (range, placeholder) = match result {
                    Ok(Some(PrepareRenameResponse::Range(range))) => {
                        (language::to_range(&this.buffer, range, encoding), None)
                    }
                    Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
                        range,
                        placeholder,
                    })) => (
                        language::to_range(&this.buffer, range, encoding),
                        Some(placeholder),
                    ),
                    Ok(Some(PrepareRenameResponse::DefaultBehavior { .. })) => match identifier {
                        Some(range) => (range, None),
                        None => {
                            this.notice = Some("Nothing to rename here".into());
                            return cx.notify();
                        }
                    },
                    Ok(None) => {
                        this.notice = Some("This element cannot be renamed".into());
                        return cx.notify();
                    }
                    Err(err) => {
                        this.notice = Some(match err {
                            lsp::Error::Response { message, .. } => message.into(),
                            err => format!("Rename failed: {err}").into(),
                        });
                        return cx.notify();
                    }
                };
                this.open_rename(range, placeholder, position, server, window, cx);
            })
            .ok();
        }));
    }

    fn open_rename(
        &mut self,
        range: Range<usize>,
        placeholder: Option<String>,
        position: TextDocumentPositionParams,
        server: Arc<RunningServer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let old_name = placeholder.unwrap_or_else(|| self.buffer.slice(range.clone()));
        let input = cx.new(|cx| {
            let mut input = Editor::single_line(cx);
            input.set_text(&old_name, cx);
            input.select_all_text(cx);
            input
        });
        window.focus(&input.focus_handle(cx));
        self.completion = None;
        self.hover = None;
        self.rename = Some(RenameInput {
            input,
            range,
            old_name,
            position,
            server,
        });
        cx.notify();
    }

    pub(super) fn confirm_rename(
        &mut self,
        _: &ConfirmRename,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        window.focus(&self.focus_handle);
        cx.notify();
        let new_name = rename.input.read(cx).text().trim().to_owned();
        if new_name.is_empty() || new_name == rename.old_name {
            return;
        }
        let params = RenameParams {
            text_document_position: rename.position,
            new_name,
            work_done_progress_params: Default::default(),
        };
        let server = rename.server;
        let executor = cx.background_executor().clone();
        self._rename_task = Some(cx.spawn(async move |this, cx| {
            let result = lsp_store::request_retrying::<Rename>(
                &server,
                params,
                &executor,
                2,
                Duration::from_millis(400),
            )
            .await;
            this.update(cx, |this, cx| match result {
                Ok(Some(edit)) => cx.emit(EditorEvent::ApplyEdit {
                    edit,
                    server,
                    command: None,
                    label: "Rename".into(),
                }),
                Ok(None) => {
                    this.notice = Some("Nothing to rename".into());
                    cx.notify();
                }
                Err(err) => {
                    this.notice = Some(match err {
                        lsp::Error::Response { message, .. } => {
                            format!("Rename failed: {message}").into()
                        }
                        err => format!("Rename failed: {err}").into(),
                    });
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    pub(super) fn cancel_rename(
        &mut self,
        _: &CancelRename,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rename.take().is_some() {
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    /// The rename input, over the symbol (at the top of the text when the
    /// symbol scrolled out of view).
    pub(super) fn render_rename(&self) -> Option<impl IntoElement + use<>> {
        let rename = self.rename.as_ref()?;
        let layout = self.layout.as_ref()?;
        let (x, top) = self
            .screen_position(rename.range.start)
            .unwrap_or((layout.text_bounds.left(), px(0.)));
        let padding = px(5.);
        let origin = point(
            x - padding - px(1.),
            layout.bounds.top() + top - padding - px(1.),
        );
        let columns = rename.old_name.chars().count().max(16) + 8;
        let width = layout.em_width * columns as f32 + padding * 2.;
        Some(
            anchored().position(origin).snap_to_window().child(
                div()
                    .key_context(RENAME_CONTEXT)
                    .w(width)
                    .p(padding)
                    .flex()
                    .flex_col()
                    .gap_1()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::accent())
                    .rounded_md()
                    .shadow_lg()
                    .child(
                        div()
                            .h(layout.line_height)
                            .font_family(theme::MONO_FONT)
                            .text_size(px(theme::EDITOR_FONT_SIZE))
                            .line_height(layout.line_height)
                            .child(rename.input.clone()),
                    )
                    .child(
                        div()
                            .font_family(theme::UI_FONT)
                            .text_xs()
                            .text_color(theme::muted())
                            .child("Enter to rename, Escape to cancel"),
                    ),
            ),
        )
    }
}
