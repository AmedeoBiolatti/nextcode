//! Containers and dashboard pieces: rows, columns (sidebars), grids, sections, tabs and stat tiles.
use super::{ExtensionView, charts};
use crate::{pinpoint::Pin, theme};
use extension_api::{Align, InputValue, Widget};
use gpui::{AnyElement, Context, SharedString, canvas, div, prelude::*, px};

impl ExtensionView {
    /// A sidebar root (see [`super::panes`]): its columns side by side, the full height, each scrolling.
    pub(super) fn render_panes(&self, root: &Widget, cx: &Context<Self>) -> AnyElement {
        let (Some((id, columns)), Widget::Row { gap, .. }) = (super::panes(root), root) else {
            return self.render_widget(root, cx);
        };
        div()
            .id(SharedString::from(id.to_owned()))
            .flex_1()
            .min_h_0()
            .flex()
            .flex_row()
            .gap(px(f32::from(*gap)))
            .children(columns.iter().map(|column| {
                let grow = matches!(column, Widget::Column { grow: true, .. });
                div()
                    .id(SharedString::from(format!("{}-pane", column.id())))
                    .h_full()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when_some(self.panes.get(column.id()), |pane, scroll| {
                        pane.track_scroll(scroll)
                    })
                    .map(|pane| {
                        if grow {
                            pane.flex_1().min_w_0()
                        } else {
                            pane.flex_none()
                        }
                    })
                    .child(self.render_widget(column, cx))
            }))
            .into_any_element()
    }

    pub(super) fn render_layout(&self, widget: &Widget, cx: &Context<Self>) -> AnyElement {
        match widget {
            Widget::Row {
                id,
                children,
                gap,
                align,
            } => {
                let grows = children
                    .iter()
                    .any(|child| matches!(child, Widget::Column { grow: true, .. }));
                // A row with a growing column is a sidebar layout: it never wraps, and its columns start at the top.
                let align = align.unwrap_or(if grows { Align::Start } else { Align::End });
                div()
                    .id(SharedString::from(id.clone()))
                    .flex()
                    .flex_row()
                    .when(!grows, |row| row.flex_wrap())
                    .map(|row| match align {
                        Align::Start => row.items_start(),
                        Align::Center => row.items_center(),
                        Align::End => row.items_end(),
                    })
                    .gap(px(f32::from(*gap)))
                    .min_w_0()
                    .children(children.iter().map(|child| {
                        let element = self.render_widget(child, cx);
                        if matches!(child, Widget::Column { grow: true, .. }) {
                            div().flex_1().min_w_0().child(element).into_any_element()
                        } else {
                            element
                        }
                    }))
                    .into_any_element()
            }
            Widget::Column {
                id,
                children,
                gap,
                width,
                ..
            } => div()
                .id(SharedString::from(id.clone()))
                .flex()
                .flex_col()
                .gap(px(f32::from(*gap)))
                .min_w_0()
                .when_some(*width, |column, width| {
                    column.w(px(f32::from(width))).flex_none()
                })
                .children(children.iter().map(|child| self.render_widget(child, cx)))
                .into_any_element(),
            Widget::Grid {
                id,
                children,
                min_width,
                gap,
                columns: fixed,
            } => {
                let measured = self.grids.get(id).cloned();
                let gap = f32::from(*gap);
                let min_width = f32::from(*min_width);
                let count = children.len().max(1);
                let fixed = *fixed;
                let columns = move |width: f32| {
                    (((width + gap) / (min_width + gap)).floor() as usize).clamp(1, count)
                };
                // Laid out at the width it had last frame (or the panel's, the first time).
                let width = measured
                    .as_ref()
                    .map(|cell| cell.get())
                    .filter(|width| *width > 0.)
                    .unwrap_or_else(|| self.content_width.get());
                let shown = fixed.map_or_else(|| columns(width), usize::from);
                div()
                    .id(SharedString::from(id.clone()))
                    .relative()
                    .w_full()
                    .min_w_0()
                    .grid()
                    .grid_cols(shown as u16)
                    .gap(px(gap))
                    .children(children.iter().map(|child| {
                        div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(self.render_widget(child, cx))
                    }))
                    .when_some(measured, |grid, cell| {
                        grid.child(
                            canvas(
                                move |bounds, window, _| {
                                    let width = f32::from(bounds.size.width);
                                    if (cell.get() - width).abs() > 0.5 {
                                        cell.set(width);
                                        // Laid out with a guess: draw again with the measured width
                                        // (a refresh during this frame's drawing would be lost).
                                        if fixed.is_none() && columns(width) != shown {
                                            window.on_next_frame(|window, _| window.refresh());
                                        }
                                    }
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                        )
                    })
                    .into_any_element()
            }
            Widget::Section {
                id,
                title,
                children,
                ..
            } => {
                let open = !matches!(self.inputs.get(id), Some(InputValue::Bool(false)));
                let weak = cx.entity().downgrade();
                let toggle_id = id.clone();
                let header = div()
                    .id(SharedString::from(format!("{id}-header")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(26.))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        weak.update(cx, |view, cx| {
                            view.change(&toggle_id, InputValue::Bool(!open), cx)
                        })
                        .ok();
                    })
                    .child(
                        div()
                            .w(px(10.))
                            .text_xs()
                            .text_color(theme::muted())
                            .child(if open { "▾" } else { "▸" }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(SharedString::from(title.clone())),
                    )
                    .child(div().flex_1().h(px(1.)).bg(theme::border()));
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .min_w_0()
                    .child(self.focusable_control(id, false, false, header.into_any_element()))
                    .when(open, |section| {
                        section.child(
                            div().flex().flex_col().gap_2().min_w_0().children(
                                children.iter().map(|child| self.render_widget(child, cx)),
                            ),
                        )
                    })
                    .into_any_element()
            }
            Widget::Stat {
                label,
                value,
                detail,
                color,
                ..
            } => div()
                .flex()
                .gap_3()
                .min_w_0()
                // Tiles side by side in a grid row are as tall as the tallest.
                .flex_grow()
                .px_3()
                .py(px(10.))
                .rounded_md()
                .border_1()
                .border_color(theme::border())
                .bg(theme::panel())
                .when_some(*color, |tile, color| {
                    tile.child(
                        div()
                            .flex_none()
                            .w(px(3.))
                            .rounded_full()
                            .bg(charts::color(color)),
                    )
                })
                .child(
                    // Takes the tile's whole width, so its text is laid out at that width (not at its own).
                    div()
                        .flex_1()
                        .flex_basis(px(0.))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .min_w_0()
                        .overflow_hidden()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::muted())
                                .truncate()
                                .child(SharedString::from(label.clone())),
                        )
                        .child(
                            div()
                                .text_xl()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .truncate()
                                .child(SharedString::from(value.clone())),
                        )
                        .when(!detail.is_empty(), |tile| {
                            tile.child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(SharedString::from(detail.clone())),
                            )
                        }),
                )
                .into_any_element(),
            Widget::Tabs {
                id,
                options,
                disabled,
                ..
            } => {
                let current = match self.inputs.get(id) {
                    Some(InputValue::Text(value)) => Some(value.clone()),
                    _ => None,
                };
                let strip = div()
                    .flex()
                    .items_end()
                    .gap_1()
                    .children(options.iter().map(|option| {
                        let selected = current.as_deref() == Some(option.value.as_str());
                        let weak = cx.entity().downgrade();
                        let (tab_id, value) = (id.clone(), option.value.clone());
                        div()
                            .id(SharedString::from(format!("{id}-tab-{}", option.value)))
                            .flex_none()
                            .px_3()
                            .h(px(30.))
                            .flex()
                            .items_center()
                            .border_b_2()
                            .text_sm()
                            .map(|tab| {
                                if selected {
                                    tab.border_color(theme::accent())
                                        .text_color(theme::text())
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                } else {
                                    tab.border_color(gpui::transparent_black())
                                        .text_color(theme::muted())
                                }
                            })
                            .when(!disabled && !selected, |tab| {
                                tab.cursor_pointer()
                                    .hover(|style| style.text_color(theme::text()))
                                    .on_click(move |_, _, cx| {
                                        weak.update(cx, |view, cx| {
                                            view.change(
                                                &tab_id,
                                                InputValue::Text(value.clone()),
                                                cx,
                                            )
                                        })
                                        .ok();
                                    })
                            })
                            .child(SharedString::from(option.label.clone()))
                            .pin(format!("tab:{}", option.value))
                            .label(option.label.clone())
                    }));
                self.focusable_control(id, *disabled, false, strip.into_any_element())
            }
            _ => div().into_any_element(),
        }
    }
}
