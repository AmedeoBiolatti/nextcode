//! Go to line (Ctrl+G): a small input over the text; `line` or
//! `line:column` previews as you type, Enter keeps it, Escape goes back.

use gpui::Subscription;

use super::*;

actions!(editor, [GoToLine, ConfirmGoToLine, CancelGoToLine]);

pub(super) const GO_TO_LINE_CONTEXT: &str = "GoToLine";

/// 0-based line and optional char column from `line[:column]` (1-based).
/// Out-of-range numbers are clamped by the caller.
pub(super) fn parse_line_column(input: &str) -> Option<(usize, Option<usize>)> {
    let input = input.trim();
    let (line, column) = match input.split_once([':', ',']) {
        Some((line, column)) => (line.trim(), Some(column.trim())),
        None => (input, None),
    };
    let line: usize = line.parse().ok()?;
    let column = match column {
        Some("") | None => None,
        Some(column) => Some(column.parse::<usize>().ok()?.saturating_sub(1)),
    };
    Some((line.saturating_sub(1), column))
}

pub(super) struct GoToLineInput {
    input: Entity<Editor>,
    /// Selections and scroll to return to on Escape.
    original: (SelectionSet, Point<Pixels>),
    _subscription: Subscription,
}

impl Editor {
    #[cfg(test)]
    pub(crate) fn go_to_line_input(&self) -> Option<Entity<Editor>> {
        Some(self.go_to_line.as_ref()?.input.clone())
    }

    pub(super) fn go_to_line_action(
        &mut self,
        _: &GoToLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.single_line {
            return;
        }
        let input = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&input, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.preview_go_to_line(cx);
            }
        });
        window.focus(&input.focus_handle(cx));
        self.go_to_line = Some(GoToLineInput {
            input,
            original: (self.selections.clone(), self.scroll),
            _subscription: subscription,
        });
        cx.notify();
    }

    fn preview_go_to_line(&mut self, cx: &mut Context<Self>) {
        let Some(go_to) = &self.go_to_line else {
            return;
        };
        let Some((line, column)) = parse_line_column(&go_to.input.read(cx).text()) else {
            return;
        };
        let line = line.min(self.buffer.line_count() - 1);
        let offset = match column {
            Some(column) => self.buffer.offset_at(line, column),
            None => {
                let text = self.buffer.line_text(line);
                self.buffer.line_start(line) + text.len() - text.trim_start().len()
            }
        };
        self.selections = SelectionSet::single(Selection::cursor(offset));
        self.reveal_selections();
        self.completion = None;
        self.center_cursor = true;
        cx.notify();
    }

    pub(super) fn confirm_go_to_line(
        &mut self,
        _: &ConfirmGoToLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preview_go_to_line(cx);
        self.go_to_line = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    pub(super) fn cancel_go_to_line(
        &mut self,
        _: &CancelGoToLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(go_to) = self.go_to_line.take() {
            let (selections, scroll) = go_to.original;
            self.selections = selections.clamped(&self.buffer);
            self.scroll = scroll;
            self.center_cursor = false;
            self.autoscroll = false;
        }
        window.focus(&self.focus_handle);
        cx.notify();
    }

    pub(super) fn render_go_to_line(&self, cx: &App) -> Option<impl IntoElement + use<>> {
        let go_to = self.go_to_line.as_ref()?;
        let line_count = self.buffer.line_count();
        let empty = go_to.input.read(cx).buffer().is_empty();
        let valid = parse_line_column(&go_to.input.read(cx).text()).is_some();
        let hint = if empty || valid {
            format!("Line[:column], 1 to {line_count}")
        } else {
            "Type a line number, e.g. 12 or 12:5".into()
        };
        Some(
            div()
                .absolute()
                .top(px(8.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .key_context(GO_TO_LINE_CONTEXT)
                        .w(px(320.))
                        .p_2()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .bg(theme::panel())
                        .border_1()
                        .border_color(theme::border())
                        .rounded_md()
                        .shadow_lg()
                        .font_family(theme::UI_FONT)
                        .child(
                            div()
                                .h(px(28.))
                                .px_2()
                                .py(px(3.))
                                .bg(theme::background())
                                .border_1()
                                .border_color(theme::accent())
                                .rounded_sm()
                                .text_sm()
                                .child(go_to.input.clone()),
                        )
                        .child(div().text_xs().text_color(theme::muted()).child(hint)),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_line_and_optional_column() {
        assert_eq!(parse_line_column("12"), Some((11, None)));
        assert_eq!(parse_line_column(" 12:5 "), Some((11, Some(4))));
        assert_eq!(parse_line_column("3,1"), Some((2, Some(0))));
        assert_eq!(parse_line_column("7:"), Some((6, None)));
        assert_eq!(
            parse_line_column("0"),
            Some((0, None)),
            "clamped to the first line"
        );
        assert_eq!(parse_line_column(""), None);
        assert_eq!(parse_line_column("x"), None);
        assert_eq!(parse_line_column("4:y"), None);
    }
}
