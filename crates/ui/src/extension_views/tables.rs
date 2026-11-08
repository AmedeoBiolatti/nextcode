//! Tables: sized columns, one-line cells, numbers aligned right, rows to select and to tick.
use super::{ExtensionView, charts};
use crate::{debugger::panel::Tooltip, pinpoint::Pin, theme};
use extension_api::{Icon, Mark, UiEvent, UiEventKind, Widget};
use gpui::{AnyElement, Context, SharedString, Window, div, prelude::*, px, svg};
use std::collections::BTreeSet;

const ROW: f32 = 26.;

/// What the person changed in a table before the extension answered, and the keyboard's row.
#[derive(Default)]
pub(super) struct TableState {
    checked: Option<BTreeSet<u32>>,
    selected: Option<u32>,
    cursor: Option<usize>,
}
impl TableState {
    pub(super) fn reset(&mut self) {
        self.checked = None;
        self.selected = None;
    }
}

/// About 7 px a character at the table's text size, plus a cell's padding.
fn estimate(chars: usize) -> f32 {
    chars as f32 * 7. + 12.
}
/// Columns whose content is at most this wide keep it; wider ones share what is left.
const SHORT: f32 = 110.;

/// How a column is sized: fixed px, or a share of what is left starting from an estimate of its content.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Width {
    Fixed(f32),
    Share(f32),
}

pub(super) fn widths(widget: &Widget) -> Vec<Width> {
    let Widget::Table {
        columns,
        rows,
        widths,
        ..
    } = widget
    else {
        return Vec::new();
    };
    (0..columns.len())
        .map(|column| match widths.get(column) {
            Some(0) => Width::Share(80.),
            Some(width) => Width::Fixed(f32::from(*width)),
            // Without widths, short columns (ids, numbers, statuses) keep their content's width and long text
            // columns share the rest, so the text is what gets cut.
            None => {
                let longest = std::iter::once(&columns[column])
                    .chain(rows.iter().take(200).map(|row| &row[column]))
                    .map(|cell| cell.chars().count())
                    .max()
                    .unwrap_or(0)
                    .clamp(1, 48);
                let width = estimate(longest);
                if width <= SHORT {
                    Width::Fixed(width)
                } else {
                    Width::Share(width)
                }
            }
        })
        .collect()
}

/// Columns whose every nonempty cell is a number (an optional sign, `%`, `±` or a unit suffix aside) align right.
pub(super) fn numeric(rows: &[Vec<String>], column: usize) -> bool {
    let mut any = false;
    for row in rows.iter().take(200) {
        let cell = row[column].trim();
        if cell.is_empty() || cell == "-" || cell == "—" {
            continue;
        }
        let head = cell.split([' ', '±']).next().unwrap_or("");
        let head = head
            .trim_end_matches(['%', 'k', 'M', 'B', 's', 'x'])
            .replace(',', "");
        if head.parse::<f64>().is_err() {
            return false;
        }
        any = true;
    }
    any
}

impl ExtensionView {
    fn table_checked(&self, id: &str, model: &[u32]) -> BTreeSet<u32> {
        self.tables
            .get(id)
            .and_then(|table| table.checked.clone())
            .unwrap_or_else(|| model.iter().copied().collect())
    }

    fn select_row(&mut self, id: &str, index: u32, cx: &mut Context<Self>) {
        let table = self.tables.entry(id.to_owned()).or_default();
        table.selected = Some(index);
        self.emit(
            UiEvent {
                widget: id.to_owned(),
                kind: UiEventKind::SelectRow { index },
            },
            cx,
        );
        cx.notify();
    }

    fn check_row(&mut self, id: &str, index: u32, cx: &mut Context<Self>) {
        let Some(model) = self.model.clone() else {
            return;
        };
        let Some(Widget::Table {
            checked, disabled, ..
        }) = super::find(&model.root, id)
        else {
            return;
        };
        if disabled.contains(&index) {
            return;
        }
        let mut ticked = self.table_checked(id, checked);
        let value = ticked.insert(index);
        if !value {
            ticked.remove(&index);
        }
        let table = self.tables.entry(id.to_owned()).or_default();
        table.checked = Some(ticked);
        self.emit(
            UiEvent {
                widget: id.to_owned(),
                kind: UiEventKind::CheckRow { index, value },
            },
            cx,
        );
        cx.notify();
    }

    /// Up/Down move a focused table's row, Enter selects it and Space ticks it.
    pub(super) fn table_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self
            .controls
            .iter()
            .find(|(_, handle)| handle.is_focused(window))
            .map(|(id, _)| id.clone())
        else {
            return false;
        };
        let Some(model) = self.model.clone() else {
            return false;
        };
        let Some(Widget::Table {
            rows,
            selectable,
            check,
            selected,
            ..
        }) = super::find(&model.root, &id)
        else {
            return false;
        };
        if rows.is_empty() {
            return false;
        }
        let state = self.tables.entry(id.clone()).or_default();
        let cursor = state
            .cursor
            .or(state.selected.map(|row| row as usize))
            .or(selected.map(|row| row as usize));
        match key {
            "up" | "down" => {
                let last = rows.len() - 1;
                state.cursor = Some(match (cursor, key) {
                    (None, _) => 0,
                    (Some(row), "up") => row.saturating_sub(1),
                    (Some(row), _) => (row + 1).min(last),
                });
                cx.notify();
                true
            }
            "enter" if *selectable => {
                let row = cursor.unwrap_or(0) as u32;
                self.select_row(&id, row, cx);
                true
            }
            "space" if *check => {
                let row = cursor.unwrap_or(0) as u32;
                self.check_row(&id, row, cx);
                true
            }
            _ => false,
        }
    }

    pub(super) fn render_table(&self, widget: &Widget, cx: &Context<Self>) -> AnyElement {
        let Widget::Table {
            id,
            columns,
            rows,
            selectable,
            selected,
            max_height,
            check,
            checked,
            disabled,
            swatches,
            marks,
            ..
        } = widget
        else {
            return div().into_any_element();
        };
        let no_marks: &[Mark] = &[];
        let marks_of = |row: usize, column: usize| {
            marks
                .get(row)
                .and_then(|cells| cells.get(column))
                .map_or(no_marks, Vec::as_slice)
        };
        let widths = widths(widget);
        // A column with icons shows states, not quantities: its text stays on the left.
        let numeric: Vec<bool> = (0..columns.len())
            .map(|c| {
                numeric(rows, c) && marks.iter().all(|row| row.get(c).is_none_or(Vec::is_empty))
            })
            .collect();
        let state = self.tables.get(id);
        let selected = state.and_then(|table| table.selected).or(*selected);
        // The keyboard's row, set only by Up/Down.
        let cursor = state.and_then(|table| table.cursor);
        let ticked = self.table_checked(id, checked);
        let has_swatches = swatches.iter().any(Option::is_some);
        let lead = if *check { 28. } else { 0. } + if has_swatches { 14. } else { 0. };
        let cell = |column: usize, text: &str, header: bool, marks: &[Mark], key: String| {
            let base = div()
                .h_full()
                .flex()
                .items_center()
                .gap(px(4.))
                .px(px(6.))
                .min_w_0()
                .overflow_hidden()
                .when(numeric[column], |cell| cell.justify_end())
                .when(!marks.is_empty(), |cell| {
                    cell.child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(1.))
                            .children(
                                marks
                                    .iter()
                                    .enumerate()
                                    .map(|(i, mark)| render_mark(mark, format!("{key}-{i}"))),
                            ),
                    )
                })
                .when(!text.is_empty(), |cell| {
                    cell.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .when(numeric[column] && !header, |cell| {
                                cell.font_family(theme::MONO_FONT).text_xs()
                            })
                            .child(SharedString::from(text.to_owned())),
                    )
                });
            match widths[column] {
                Width::Fixed(width) => base.w(px(width)).flex_none(),
                Width::Share(basis) => base
                    .flex_basis(px(basis))
                    .flex_grow()
                    .flex_shrink()
                    .min_w(px(48.)),
            }
        };
        let weak = cx.entity().downgrade();
        let header = div()
            .flex()
            .h(px(ROW))
            .flex_none()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .text_xs()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(theme::muted())
            .when(lead > 0., |row| row.child(div().w(px(lead)).flex_none()))
            .children(
                columns
                    .iter()
                    .enumerate()
                    .map(|(c, name)| cell(c, name, true, no_marks, String::new())),
            );
        let body = rows.iter().enumerate().map(|(index, cells)| {
            let row_index = index as u32;
            let is_selected = selected == Some(row_index);
            let is_checked = ticked.contains(&row_index);
            let locked = disabled.contains(&row_index);
            // What the row says, its marks' labels included (hover, pinpoint).
            let full: String = cells
                .iter()
                .enumerate()
                .map(|(c, text)| {
                    let labels = marks_of(index, c)
                        .iter()
                        .map(|m| m.label.as_str())
                        .filter(|l| !l.is_empty());
                    let mut parts: Vec<&str> = labels.collect();
                    if !text.is_empty() {
                        parts.push(text);
                    }
                    parts.join(", ")
                })
                .collect::<Vec<_>>()
                .join("  ·  ");
            let long = cells.iter().any(|cell| cell.chars().count() > 24);
            let (select_weak, check_weak) = (weak.clone(), weak.clone());
            let (select_id, check_id) = (id.clone(), id.clone());
            div()
                .id(SharedString::from(format!("{id}-row-{index}")))
                .flex()
                .h(px(ROW))
                .flex_none()
                .when(index + 1 < rows.len(), |row| {
                    row.border_b_1().border_color(theme::border().opacity(0.6))
                })
                .map(|row| {
                    if is_selected {
                        row.bg(theme::active_row())
                    } else if cursor == Some(index) {
                        row.bg(theme::hover_row())
                    } else {
                        row.hover(|style| style.bg(theme::hover_row()))
                    }
                })
                .when(*selectable, |row| {
                    row.cursor_pointer().on_click(move |_, _, cx| {
                        select_weak
                            .update(cx, |view, cx| view.select_row(&select_id, row_index, cx))
                            .ok();
                    })
                })
                .when(long, |row| {
                    let full = full.clone();
                    row.tooltip(move |window, cx| Tooltip::view(full.clone(), window, cx))
                })
                .when(*check, |row| {
                    row.child(
                        div()
                            .id(SharedString::from(format!("{id}-check-{index}")))
                            .w(px(28.))
                            .h_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(!locked, |cell| {
                                cell.cursor_pointer().on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    check_weak
                                        .update(cx, |view, cx| {
                                            view.check_row(&check_id, row_index, cx)
                                        })
                                        .ok();
                                })
                            })
                            .child(
                                div()
                                    .size(px(14.))
                                    .rounded_sm()
                                    .border_1()
                                    .when(locked, |boxed| boxed.opacity(0.4))
                                    .map(|boxed| {
                                        if is_checked {
                                            boxed
                                                .border_color(theme::accent())
                                                .bg(theme::accent())
                                                .text_color(theme::on_accent())
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .text_size(px(10.))
                                                .child("✓")
                                        } else {
                                            boxed.border_color(theme::muted().opacity(0.7))
                                        }
                                    }),
                            )
                            .pin(format!("check:{}", index + 1))
                            .label(if is_checked { "Ticked" } else { "Not ticked" }),
                    )
                })
                .when(has_swatches, |row| {
                    let swatch = swatches.get(index).copied().flatten();
                    row.child(
                        div()
                            .w(px(14.))
                            .h_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .children(swatch.map(|color| {
                                div().size(px(8.)).rounded_sm().bg(charts::color(color))
                            })),
                    )
                })
                .children(cells.iter().enumerate().map(|(c, text)| {
                    cell(
                        c,
                        text,
                        false,
                        marks_of(index, c),
                        format!("{id}-mark-{index}-{c}"),
                    )
                }))
                .pin(format!("row:{}", index + 1))
                .label(full)
        });
        let height = f32::from(max_height.unwrap_or(320));
        let content = div()
            .flex()
            .flex_col()
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(theme::border())
            .overflow_hidden()
            .child(header)
            .child(
                div()
                    .id(SharedString::from(format!("{id}-rows")))
                    .flex()
                    .flex_col()
                    .max_h(px(height))
                    .overflow_y_scroll()
                    .children(body),
            );
        if *selectable || *check {
            self.focusable_control(id, false, false, content.into_any_element())
        } else {
            content.into_any_element()
        }
    }
}

/// A mark: its icon in its color (muted without one), its label on hover.
fn render_mark(mark: &Mark, key: String) -> AnyElement {
    let path = match mark.icon {
        Icon::Check => "icons/check.svg",
        Icon::Cross => "icons/close.svg",
        Icon::Clock => "icons/clock.svg",
        Icon::Circle => "icons/circle.svg",
        Icon::Dot => "icons/dot.svg",
        Icon::Minus => "icons/minus.svg",
        Icon::Warning => "icons/warning.svg",
        Icon::Comment => "icons/comment.svg",
        Icon::Person => "icons/person.svg",
    };
    let color = mark.color.map_or_else(theme::muted, charts::color);
    let label = mark.label.clone();
    div()
        .id(SharedString::from(key))
        .flex_none()
        .size(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .child(svg().path(path).size(px(13.)).text_color(color))
        .when(!label.is_empty(), |mark| {
            mark.tooltip(move |window, cx| Tooltip::view(label.clone(), window, cx))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_are_fixed_shared_or_estimated_and_numbers_align_right() {
        let rows = vec![
            vec![
                "r0012".into(),
                "0.8123 ± 0.002".into(),
                "a long hypothesis".into(),
            ],
            vec!["r0013".into(), "—".into(), "x".into()],
        ];
        let table = Widget::table(
            "t",
            vec!["run".into(), "score".into(), "hypothesis".into()],
            rows.clone(),
        );
        assert_eq!(
            widths(&table),
            [
                Width::Fixed(estimate(5)),
                Width::Fixed(estimate(14)),
                Width::Share(estimate(17))
            ]
        );
        assert_eq!(widths(&table.widths([64, 0, 0]))[0], Width::Fixed(64.));
        assert!(!numeric(&rows, 0));
        assert!(numeric(&rows, 1));
        assert!(numeric(
            &[
                vec!["1,024".into()],
                vec!["12%".into()],
                vec!["3.5k".into()]
            ],
            0
        ));
    }
}
