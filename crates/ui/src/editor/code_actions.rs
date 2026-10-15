//! Code actions and quick fixes (Ctrl+., JetBrains Alt+Enter): a menu at
//! the cursor with the server's actions for the primary selection and the
//! diagnostics under it, and a lightbulb in the gutter when the cursor
//! line has any (requested a moment after the cursor or text settles).

use gpui::{FontWeight, ScrollStrategy, UniformListScrollHandle, uniform_list};
use lsp::types::{
    CodeAction, CodeActionContext, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CodeActionTriggerKind, Command, ExecuteCommandParams,
    request::{CodeActionRequest, CodeActionResolveRequest, ExecuteCommand},
};

use super::*;
use crate::lsp_store;

actions!(
    editor,
    [
        QuickFix,
        SelectPrevCodeAction,
        SelectNextCodeAction,
        ConfirmCodeAction,
        CloseCodeActions,
    ]
);

/// Quiet time after a cursor move or edit before asking for the lightbulb.
const LIGHTBULB_DELAY: Duration = Duration::from_millis(250);
const MAX_ROWS: usize = 12;
const ROW_HEIGHT: f32 = 24.;

#[derive(Default)]
pub(super) struct CodeActionState {
    /// The line showing a lightbulb, and whether a quick fix is among its
    /// actions.
    pub lightbulb: Option<(usize, bool)>,
    /// The last answer, for (buffer version, primary selection).
    cached: Option<(u64, Range<usize>, Vec<CodeActionOrCommand>)>,
    /// The (buffer version, primary selection) the lightbulb and the menu
    /// belong to; a change (any edit or cursor move) asks again.
    tracked: Option<(u64, Range<usize>)>,
    pub menu: Option<CodeActionMenu>,
    _request: Option<Task<()>>,
    _apply: Option<Task<()>>,
}

pub(super) struct CodeActionMenu {
    /// Where the menu opens (the cursor).
    anchor: usize,
    /// Quick fixes first, preferred first among them.
    actions: Vec<CodeActionOrCommand>,
    selected: usize,
    scroll: UniformListScrollHandle,
    server: Arc<RunningServer>,
}

fn supports_code_actions(server: &RunningServer) -> bool {
    matches!(
        server.capabilities.code_action_provider,
        Some(CodeActionProviderCapability::Simple(true) | CodeActionProviderCapability::Options(_))
    )
}

fn resolves_code_actions(server: &RunningServer) -> bool {
    matches!(
        &server.capabilities.code_action_provider,
        Some(CodeActionProviderCapability::Options(options)) if options.resolve_provider == Some(true)
    )
}

pub(crate) fn is_quick_fix(action: &CodeActionOrCommand) -> bool {
    matches!(action, CodeActionOrCommand::CodeAction(CodeAction { kind: Some(kind), .. })
        if kind.as_str() == CodeActionKind::QUICKFIX.as_str()
            || kind.as_str().starts_with("quickfix."))
}

fn is_preferred(action: &CodeActionOrCommand) -> bool {
    matches!(
        action,
        CodeActionOrCommand::CodeAction(CodeAction {
            is_preferred: Some(true),
            ..
        })
    )
}

pub(crate) fn title(action: &CodeActionOrCommand) -> &str {
    match action {
        CodeActionOrCommand::Command(command) => &command.title,
        CodeActionOrCommand::CodeAction(action) => &action.title,
    }
}

pub(crate) fn kind(action: &CodeActionOrCommand) -> Option<&str> {
    match action {
        CodeActionOrCommand::Command(_) => None,
        CodeActionOrCommand::CodeAction(action) => action.kind.as_ref().map(|kind| kind.as_str()),
    }
}

/// Title, kind and whether the server prefers it, for listings.
pub(crate) fn info(action: &CodeActionOrCommand) -> (&str, Option<&str>, bool) {
    (title(action), kind(action), is_preferred(action))
}

/// The server's actions without disabled ones, quick fixes first and
/// preferred ones first among those (otherwise in the server's order).
pub(crate) fn usable(mut actions: Vec<CodeActionOrCommand>) -> Vec<CodeActionOrCommand> {
    actions.retain(|action| {
        !matches!(
            action,
            CodeActionOrCommand::CodeAction(CodeAction {
                disabled: Some(_),
                ..
            })
        )
    });
    actions.sort_by_key(|action| (!is_quick_fix(action), !is_preferred(action)));
    actions
}

/// Runs a server command (`workspace/executeCommand`); edits it makes come
/// back as `workspace/applyEdit` requests.
pub(crate) async fn execute_command(
    server: &RunningServer,
    command: Command,
) -> Result<(), String> {
    server
        .connection
        .request::<ExecuteCommand>(ExecuteCommandParams {
            command: command.command,
            arguments: command.arguments.unwrap_or_default(),
            work_done_progress_params: Default::default(),
        })
        .await
        .map(drop)
        .map_err(|err| match err {
            lsp::Error::Response { message, .. } => message,
            err => err.to_string(),
        })
}

/// Fills in a code action's edit if the server defers it.
pub(crate) async fn resolve(
    server: &RunningServer,
    action: CodeAction,
) -> Result<CodeAction, String> {
    if action.edit.is_some() || !resolves_code_actions(server) {
        return Ok(action);
    }
    server
        .connection
        .request::<CodeActionResolveRequest>(action)
        .await
        .map_err(|err| match err {
            lsp::Error::Response { message, .. } => message,
            err => err.to_string(),
        })
}

impl Editor {
    /// Parameters for the actions at `range`, with the diagnostics touching
    /// it (at their current positions).
    fn code_action_params(
        &self,
        range: &Range<usize>,
        trigger: CodeActionTriggerKind,
    ) -> Option<CodeActionParams> {
        let doc = self.lsp.as_ref()?;
        let encoding = doc.server.encoding;
        let to_lsp = |range: &Range<usize>| {
            lsp::types::Range::new(
                language::to_position(&self.buffer, range.start, encoding),
                language::to_position(&self.buffer, range.end, encoding),
            )
        };
        let diagnostics = self
            .diagnostics
            .iter()
            .filter(|d| d.range.start <= range.end && range.start <= d.range.end)
            .map(|d| lsp::types::Diagnostic {
                range: to_lsp(&d.range),
                ..d.lsp.clone()
            })
            .collect();
        Some(CodeActionParams {
            text_document: lsp::types::TextDocumentIdentifier::new(doc.uri.clone()),
            range: to_lsp(range),
            context: CodeActionContext {
                diagnostics,
                only: None,
                trigger_kind: Some(trigger),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
    }

    /// Before each frame: when the text or the primary selection changed
    /// (by any path), closes the menu and asks again for the lightbulb.
    pub(super) fn follow_cursor_for_code_actions(&mut self, cx: &mut Context<Self>) {
        if self.single_line {
            return;
        }
        let current = (self.version, self.primary().range());
        if self.code_actions.tracked.as_ref() != Some(&current) {
            self.code_actions.tracked = Some(current);
            self.schedule_code_actions(cx);
        }
    }

    /// Closes the menu and asks (after a pause) whether the cursor line gets
    /// a lightbulb.
    fn schedule_code_actions(&mut self, cx: &mut Context<Self>) {
        let state = &mut self.code_actions;
        state.menu = None;
        let primary = self.selections.primary();
        let line = self.buffer.line_of(primary.head);
        if state.lightbulb.is_some_and(|(shown, _)| shown != line) {
            state.lightbulb = None;
        }
        let Some(server) = self
            .lsp
            .as_ref()
            .map(|doc| doc.server.clone())
            .filter(|server| supports_code_actions(server))
        else {
            state.lightbulb = None;
            state._request = None;
            return;
        };
        let range = primary.range();
        let version = self.version;
        if state
            .cached
            .as_ref()
            .is_some_and(|(v, r, _)| *v == version && *r == range)
        {
            return;
        }
        let Some(params) = self.code_action_params(&range, CodeActionTriggerKind::AUTOMATIC) else {
            return;
        };
        self.code_actions._request = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIGHTBULB_DELAY).await;
            let result = server.connection.request::<CodeActionRequest>(params).await;
            this.update(cx, |this, cx| {
                if this.version != version || this.primary().range() != range {
                    return;
                }
                let actions = usable(result.ok().flatten().unwrap_or_default());
                let state = &mut this.code_actions;
                state.lightbulb =
                    (!actions.is_empty()).then(|| (line, actions.iter().any(is_quick_fix)));
                state.cached = Some((version, range, actions));
                cx.notify();
            })
            .ok();
        }));
    }

    /// New diagnostics may bring quick fixes: asks again (unless the menu
    /// is open).
    pub(super) fn code_action_diagnostics_changed(&mut self) {
        if self.code_actions.menu.is_none() {
            self.code_actions.cached = None;
            self.code_actions.tracked = None;
        }
    }

    /// Ctrl+. / Alt+Enter: the actions at the primary selection.
    pub(super) fn quick_fix(&mut self, _: &QuickFix, _: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = self.server() else {
            self.notice = Some("No language server for this file".into());
            return cx.notify();
        };
        if !supports_code_actions(&server) {
            self.notice = Some("The language server has no code actions".into());
            return cx.notify();
        }
        let range = self.primary().range();
        let version = self.version;
        if let Some((v, r, actions)) = &self.code_actions.cached
            && *v == version
            && *r == range
        {
            let actions = actions.clone();
            return self.open_code_actions(actions, server, cx);
        }
        let Some(params) = self.code_action_params(&range, CodeActionTriggerKind::INVOKED) else {
            return;
        };
        let executor = cx.background_executor().clone();
        self.code_actions._request = Some(cx.spawn(async move |this, cx| {
            let result = lsp_store::request_retrying::<CodeActionRequest>(
                &server,
                params,
                &executor,
                2,
                Duration::from_millis(400),
            )
            .await;
            this.update(cx, |this, cx| {
                if this.version != version || this.primary().range() != range {
                    return;
                }
                match result {
                    Ok(actions) => {
                        let actions = usable(actions.unwrap_or_default());
                        this.code_actions.cached = Some((version, range, actions.clone()));
                        this.open_code_actions(actions, server, cx);
                    }
                    Err(err) => {
                        this.notice = Some(format!("Code actions failed: {err}").into());
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    fn open_code_actions(
        &mut self,
        actions: Vec<CodeActionOrCommand>,
        server: Arc<RunningServer>,
        cx: &mut Context<Self>,
    ) {
        if actions.is_empty() {
            self.notice = Some("No code actions available here".into());
            return cx.notify();
        }
        self.completion = None;
        self.completion_generation += 1;
        self.hover = None;
        let primary = self.primary();
        self.code_actions.lightbulb = Some((
            self.buffer.line_of(primary.head),
            actions.iter().any(is_quick_fix),
        ));
        self.code_actions.menu = Some(CodeActionMenu {
            anchor: primary.head,
            actions,
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            server,
        });
        cx.notify();
    }

    fn move_code_action(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.code_actions.menu else {
            return;
        };
        let len = menu.actions.len() as isize;
        menu.selected = (menu.selected as isize + delta).rem_euclid(len) as usize;
        menu.scroll
            .scroll_to_item(menu.selected, ScrollStrategy::Center);
        cx.notify();
    }

    pub(super) fn select_prev_code_action(
        &mut self,
        _: &SelectPrevCodeAction,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_code_action(-1, cx);
    }

    pub(super) fn select_next_code_action(
        &mut self,
        _: &SelectNextCodeAction,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_code_action(1, cx);
    }

    pub(super) fn confirm_code_action(
        &mut self,
        _: &ConfirmCodeAction,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self.code_actions.menu.as_ref().map(|menu| menu.selected);
        if let Some(index) = selected {
            self.apply_code_action(index, cx);
        }
    }

    pub(super) fn close_code_actions(
        &mut self,
        _: &CloseCodeActions,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.code_actions.menu = None;
        cx.notify();
    }

    /// Resolves the action if needed, then applies its edit across the
    /// project (through the workspace) and runs its command.
    fn apply_code_action(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(menu) = self.code_actions.menu.take() else {
            return;
        };
        cx.notify();
        let Some(action) = menu.actions.get(index).cloned() else {
            return;
        };
        // The text is about to change; ask again afterwards.
        self.code_actions.cached = None;
        let server = menu.server;
        self.code_actions._apply = Some(cx.spawn(async move |this, cx| {
            let result = match action {
                CodeActionOrCommand::Command(command) => {
                    execute_command(&server, command).await.map(|()| None)
                }
                CodeActionOrCommand::CodeAction(action) => resolve(&server, action).await.map(Some),
            };
            this.update(cx, |this, cx| match result {
                Ok(Some(action)) => match action.edit {
                    Some(edit) => cx.emit(EditorEvent::ApplyEdit {
                        edit,
                        server,
                        command: action.command,
                        label: action.title.into(),
                    }),
                    None => match action.command {
                        Some(command) => this.run_command_later(server, command, cx),
                        None => {
                            this.notice =
                                Some(format!("\"{}\" changed nothing", action.title).into());
                            cx.notify();
                        }
                    },
                },
                Ok(None) => {}
                Err(err) => {
                    this.notice = Some(format!("Code action failed: {err}").into());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn run_command_later(
        &mut self,
        server: Arc<RunningServer>,
        command: Command,
        cx: &mut Context<Self>,
    ) {
        self.code_actions._apply = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = execute_command(&server, command).await {
                this.update(cx, |this, cx| {
                    this.notice = Some(format!("Code action failed: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// Left edge and width of the lightbulb column (before the numbers).
    pub(super) fn lightbulb_column(layout: &EditorLayout) -> (Pixels, Pixels) {
        (
            layout.bounds.left() + GUTTER_PADDING / 2.,
            layout.em_width * LIGHTBULB_COLUMNS,
        )
    }

    /// A click on the lightbulb opens the menu.
    pub(super) fn lightbulb_click(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let (Some(layout), Some((line, _))) = (&self.layout, self.code_actions.lightbulb) else {
            return false;
        };
        let (left, width) = Self::lightbulb_column(layout);
        let on_bulb = (left..left + width).contains(&position.x)
            && layout
                .row_layout_at(position.y, self.scroll.y)
                .is_some_and(|(_, row)| row.row.first && row.row.line == line);
        if !on_bulb {
            return false;
        }
        let Some(server) = self.server() else {
            return false;
        };
        let actions = self
            .code_actions
            .cached
            .as_ref()
            .map(|(_, _, actions)| actions.clone())
            .unwrap_or_default();
        self.open_code_actions(actions, server, cx);
        true
    }

    /// Paints the lightbulb on the row at `top` when it shows `line`.
    pub(super) fn paint_lightbulb(
        &self,
        layout: &EditorLayout,
        line: usize,
        top: Pixels,
        window: &mut Window,
    ) {
        let Some((bulb_line, quick_fix)) = self.code_actions.lightbulb else {
            return;
        };
        if bulb_line != line {
            return;
        }
        let color = if quick_fix {
            theme::warning()
        } else {
            theme::lightbulb()
        };
        let (left, width) = Self::lightbulb_column(layout);
        let height = layout.line_height;
        let diameter = (height * 0.5).min(width);
        let center_x = left + width / 2.;
        let bulb_top = top + height * 0.16;
        window.paint_quad(
            fill(
                Bounds::new(
                    point(center_x - diameter / 2., bulb_top),
                    size(diameter, diameter),
                ),
                color,
            )
            .corner_radii(diameter / 2.),
        );
        let base = diameter * 0.5;
        window.paint_quad(
            fill(
                Bounds::new(
                    point(center_x - base / 2., bulb_top + diameter + px(1.)),
                    size(base, (height * 0.14).max(px(2.))),
                ),
                color,
            )
            .corner_radii(px(1.)),
        );
    }

    pub(super) fn render_code_actions(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let menu = self.code_actions.menu.as_ref()?;
        let layout = self.layout.as_ref()?;
        let (x, line_top) = self.screen_position(menu.anchor)?;
        let bounds = layout.bounds;
        let below = line_top < bounds.size.height * 0.6;
        let rows = menu.actions.len().min(MAX_ROWS);
        let list = uniform_list(
            "code-actions",
            menu.actions.len(),
            cx.processor(|this, range: Range<usize>, _, cx| {
                let Some(menu) = &this.code_actions.menu else {
                    return Vec::new();
                };
                range
                    .map(|row| code_action_row(menu, row, cx).into_any_element())
                    .collect()
            }),
        )
        .track_scroll(menu.scroll.clone())
        .size_full();
        Some(
            div()
                .absolute()
                .left((x - bounds.left()).max(px(0.)))
                .when(below, |menu| menu.top(line_top + layout.line_height))
                .when(!below, |menu| menu.bottom(bounds.size.height - line_top))
                .w(px(440.))
                .h(px(ROW_HEIGHT * rows as f32 + 2.))
                .bg(theme::panel())
                .border_1()
                .border_color(theme::border())
                .rounded_md()
                .shadow_md()
                .font_family(theme::UI_FONT)
                .text_sm()
                .cursor_default()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                .child(list),
        )
    }
}

/// Columns (in em widths) of the lightbulb, at the gutter's left edge.
pub(super) const LIGHTBULB_COLUMNS: f32 = 1.5;

fn code_action_row(
    menu: &CodeActionMenu,
    row: usize,
    cx: &mut Context<Editor>,
) -> impl IntoElement + use<> {
    let action = &menu.actions[row];
    let (badge, color) = if is_quick_fix(action) {
        ("fix", theme::warning())
    } else {
        match kind(action).and_then(|kind| kind.split('.').next()) {
            Some("refactor") => ("refactor", theme::accent_text()),
            Some("source") => ("source", theme::muted()),
            _ => ("", theme::muted()),
        }
    };
    let preferred = is_preferred(action);
    div()
        .id(row)
        .h(px(ROW_HEIGHT))
        .w_full()
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .overflow_hidden()
        .when(row == menu.selected, |row| row.bg(theme::active_row()))
        .when(row != menu.selected, |row| {
            row.hover(|style| style.bg(theme::hover_row()))
        })
        .on_click(cx.listener(move |this, _, _, cx| this.apply_code_action(row, cx)))
        .child(
            div()
                .w(px(52.))
                .flex_none()
                .text_xs()
                .text_color(color)
                .child(badge),
        )
        .child(
            div()
                .flex_1()
                .truncate()
                .text_color(if preferred {
                    theme::accent_text()
                } else {
                    theme::text()
                })
                .when(preferred, |title| title.font_weight(FontWeight::BOLD))
                .child(SharedString::from(title(action).to_owned())),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(title: &str, kind: Option<&str>, preferred: bool) -> CodeActionOrCommand {
        CodeActionOrCommand::CodeAction(CodeAction {
            title: title.into(),
            kind: kind.map(|kind| CodeActionKind::from(kind.to_owned())),
            is_preferred: preferred.then_some(true),
            ..CodeAction::default()
        })
    }

    #[test]
    fn quick_fixes_come_first_and_preferred_ones_lead() {
        let disabled = CodeActionOrCommand::CodeAction(CodeAction {
            title: "disabled".into(),
            disabled: Some(lsp::types::CodeActionDisabled {
                reason: "no".into(),
            }),
            ..CodeAction::default()
        });
        let command = CodeActionOrCommand::Command(Command::new("run".into(), "x".into(), None));
        let actions = usable(vec![
            action("extract", Some("refactor.extract"), false),
            command,
            action("fix a", Some("quickfix"), false),
            disabled,
            action("fix b", Some("quickfix"), true),
        ]);
        let titles: Vec<&str> = actions.iter().map(title).collect();
        assert_eq!(titles, ["fix b", "fix a", "extract", "run"]);
    }
}
