//! Assembly inspection: compiler/target and optimization controls, a searchable
//! read-only listing, and the complete compiler diagnostics.

use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use gpui::{
    App, ClipboardItem, Context, Div, Entity, EventEmitter, FocusHandle, Focusable,
    ListHorizontalSizingBehavior, ScrollStrategy, SharedString, Subscription, Task,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use super::{Builtin, Entry, MenuItem};
use crate::{
    editor::{Editor, EditorEvent},
    pinpoint::Pin,
    project::{CancelToken, Project},
    selectable::selectable,
    theme,
    workspace::Workspace,
};

pub mod compiler;

gpui::actions!(assembly, [InspectAssembly]);

pub const BUILTIN: Builtin = Builtin {
    id: "assembly",
    name: "Assembly Inspection",
    description: "Compiler output for C, C++ and Rust sources: Run > Tools > Inspect Assembly.",
    default_enabled: false,
    labels: &[("assembly::InspectAssembly", "Inspect Assembly")],
    menu_items: &[MenuItem {
        menu: "Run/Tools",
        after: "ports::ForwardPort",
        items: &[Entry::Command("assembly::InspectAssembly")],
    }],
    workspace_actions,
    ..Builtin::NONE
};

fn workspace_actions(element: Div, cx: &mut Context<Workspace>) -> Div {
    element.on_action(cx.listener(inspect))
}

/// Opens another assembly tab for the active source file, or for the
/// source of the active assembly tab.
pub(crate) fn inspect(
    workspace: &mut Workspace,
    _: &InspectAssembly,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let path = workspace
        .active_file_editor()
        .map(|(path, _)| path)
        .or_else(|| {
            let view = workspace.active_builtin_view::<AssemblyView>()?;
            Some(view.read(cx).path().to_path_buf())
        });
    match path {
        Some(path) if compiler::supported(&path) => open_tab(workspace, path, window, cx),
        _ => workspace.report(
            "Open a C, C++ or Rust source file to inspect its assembly.".into(),
            cx,
        ),
    }
}

/// Assembly tabs keep their source path, so Refresh can save its open
/// sources before compiling, and Source can return to the normal editor.
fn open_tab(
    workspace: &mut Workspace,
    path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let view = cx.new(|cx| AssemblyView::new(workspace.project().clone(), path.clone(), cx));
    let subscription =
        cx.subscribe_in(
            &view,
            window,
            |workspace, view, event, window, cx| match event {
                AssemblyEvent::Generate => generate(workspace, view, cx),
                AssemblyEvent::OpenSource => {
                    let source = view.read(cx).path().to_path_buf();
                    workspace.open(&source, window, cx);
                }
                AssemblyEvent::NewAssembly => {
                    open_tab(workspace, view.read(cx).path().to_path_buf(), window, cx);
                }
            },
        );
    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    let mut number = 1;
    let title = loop {
        let title = if number == 1 {
            format!("Assembly · {filename}")
        } else {
            format!("Assembly {number} · {filename}")
        };
        if !workspace.has_tab(&path, &title) {
            break title.into();
        }
        number += 1;
    };
    workspace.open_builtin_tab(
        BUILTIN.tab(view.clone(), "assembly"),
        path,
        title,
        subscription,
        window,
        cx,
    );
    view.update(cx, |view, cx| view.request(cx));
}

/// Saves the open C, C++ and Rust sources of the listing's project folder,
/// then compiles.
fn generate(workspace: &mut Workspace, view: &Entity<AssemblyView>, cx: &mut Context<Workspace>) {
    let path = view.read(cx).path().to_path_buf();
    let root = workspace.project().root_for(&path).path.clone();
    let saved = workspace.save_files(
        |file| file.starts_with(&root) && compiler::supported(file),
        cx,
    );
    let view = view.downgrade();
    cx.spawn(async move |_, cx| {
        let saved = saved.await;
        view.update(cx, |view, cx| {
            if saved {
                view.generate(cx);
            } else {
                view.fail(
                    "Could not save source files. Resolve the save error or conflict, then refresh.",
                    cx,
                );
            }
        })
        .ok();
    })
    .detach();
}

pub enum AssemblyEvent {
    Generate,
    OpenSource,
    NewAssembly,
}

pub struct AssemblyView {
    project: Project,
    path: PathBuf,
    compilers: Vec<compiler::Compiler>,
    selected: usize,
    optimization: u8,
    flags: Entity<Editor>,
    find: Entity<Editor>,
    matches: Vec<usize>,
    current_match: usize,
    output: SharedString,
    lines: Vec<SharedString>,
    longest: Option<usize>,
    diagnostics: SharedString,
    show_diagnostics: bool,
    error: Option<SharedString>,
    busy: bool,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    cancel: CancelToken,
    _task: Task<()>,
    _find: Subscription,
}

impl EventEmitter<AssemblyEvent> for AssemblyView {}

impl AssemblyView {
    pub fn new(project: Project, path: PathBuf, cx: &mut Context<Self>) -> Self {
        let flags = cx.new(Editor::single_line);
        let find = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&find, |this, _, event, cx| {
            if matches!(event, EditorEvent::Edited) {
                this.find_matches(cx);
            }
        });
        Self {
            project,
            path,
            compilers: Vec::new(),
            selected: 0,
            optimization: 2,
            flags,
            find,
            matches: Vec::new(),
            current_match: 0,
            output: "".into(),
            lines: Vec::new(),
            longest: None,
            diagnostics: "".into(),
            show_diagnostics: false,
            error: None,
            busy: false,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            cancel: CancelToken::new(),
            _task: Task::ready(()),
            _find: subscription,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn request(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        self.output = "".into();
        self.lines.clear();
        self.matches.clear();
        self.diagnostics = "".into();
        cx.emit(AssemblyEvent::Generate);
        cx.notify();
    }

    pub fn fail(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.busy = false;
        self.error = Some(message.into());
        cx.notify();
    }

    pub fn generate(&mut self, cx: &mut Context<Self>) {
        self.busy = true;
        self.cancel.cancel();
        self.cancel = CancelToken::new();
        let cancel = self.cancel.clone();
        let project = self.project.clone();
        let path = self.path.clone();
        let chosen = self
            .compilers
            .get(self.selected)
            .map(|compiler| compiler.label.clone());
        let optimization = self.optimization;
        let flags = self.flags.read(cx).text();
        self._task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let compilers = compiler::discover(&project, &path, &cancel)?;
                    let selected = chosen
                        .and_then(|label| compilers.iter().position(|c| c.label == label))
                        .unwrap_or(0);
                    let result = compiler::compile(
                        &project,
                        &compilers[selected],
                        optimization,
                        &flags,
                        &cancel,
                    );
                    Ok::<_, std::io::Error>((compilers, selected, result))
                })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok((compilers, selected, result)) => {
                        this.compilers = compilers;
                        this.selected = selected;
                        match result {
                            Ok(output) => {
                                this.lines = output
                                    .assembly
                                    .lines()
                                    .map(|line| line.replace('\t', "    ").into())
                                    .collect();
                                this.longest = this
                                    .lines
                                    .iter()
                                    .enumerate()
                                    .max_by_key(|(_, line)| line.len())
                                    .map(|(i, _)| i);
                                this.output = output.assembly.into();
                                this.diagnostics =
                                    format!("{}\n\n{}", output.command, output.diagnostics).into();
                                this.error = None;
                                this.find_matches(cx);
                            }
                            Err(err) => this.error = Some(err.to_string().into()),
                        }
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    fn find_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.find.read(cx).text().to_lowercase();
        self.matches = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| !query.is_empty() && line.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect();
        self.current_match = 0;
        self.reveal_match();
        cx.notify();
    }

    fn reveal_match(&mut self) {
        if let Some(&row) = self.matches.get(self.current_match) {
            self.scroll.scroll_to_item(row, ScrollStrategy::Center);
        }
    }
}

impl Drop for AssemblyView {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl Focusable for AssemblyView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .bg(if selected {
            theme::active_row()
        } else {
            theme::panel()
        })
        .hover(|style| style.bg(theme::active_row()))
        .child(label.into())
}

impl Render for AssemblyView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.busy;
        let choices = self
            .compilers
            .iter()
            .enumerate()
            .map(|(i, compiler)| {
                button(("compiler", i), compiler.label.clone(), i == self.selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.busy {
                            this.selected = i;
                            this.request(cx);
                        }
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let optimization = (0..=3u8).map(|level| {
            button(
                ("optimization", level as usize),
                format!("O{level}"),
                level == self.optimization,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.busy {
                    this.optimization = level;
                    this.request(cx);
                }
            }))
            .into_any_element()
        });
        let body = if let Some(error) = &self.error {
            div()
                .id("assembly-error")
                .flex_1()
                .overflow_scroll()
                .p_3()
                .text_color(theme::error())
                .child(selectable("error", error.clone()))
                .into_any_element()
        } else if busy {
            div().p_3().child("Generating assembly…").into_any_element()
        } else {
            uniform_list(
                "assembly-lines",
                self.lines.len(),
                cx.processor(|this, range: Range<usize>, _, _| {
                    range
                        .map(|i| {
                            div()
                                .h(px(20.))
                                .flex()
                                .whitespace_nowrap()
                                .when(this.matches.get(this.current_match) == Some(&i), |line| {
                                    line.bg(theme::active_row())
                                })
                                .child(
                                    div()
                                        .w(px(60.))
                                        .flex_none()
                                        .px_2()
                                        .text_color(theme::muted())
                                        .child((i + 1).to_string()),
                                )
                                .child(selectable(("assembly-line", i), this.lines[i].clone()))
                        })
                        .collect()
                }),
            )
            .track_scroll(self.scroll.clone())
            .with_width_from_item(self.longest)
            .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
            .flex_1()
            .min_h_0()
            .w_full()
            .pin("listing")
            .into_any_element()
        };
        div().size_full().flex().flex_col().track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &crate::editor::Newline, window, cx| {
                if this.find.focus_handle(cx).contains_focused(window, cx) {
                    if !this.matches.is_empty() { this.current_match = (this.current_match + 1) % this.matches.len(); this.reveal_match(); cx.notify(); }
                } else { this.request(cx); }
            }))
            .bg(theme::background()).text_color(theme::text()).text_sm()
            .child(div().flex().flex_wrap().items_center().gap_1().p_2()
                .child(button("source", "Source", false).on_click(cx.listener(|_, _, _, cx| cx.emit(AssemblyEvent::OpenSource))))
                .child(button("new-assembly", "New Assembly Tab", false)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(AssemblyEvent::NewAssembly)))
                    .pin("new-assembly").label("Open another assembly tab for this source file"))
                .child(button("refresh", if busy { "Building…" } else { "Refresh" }, false)
                    .on_click(cx.listener(|this, _, _, cx| this.request(cx))).pin("refresh"))
                .when(busy, |bar| bar.child(button("cancel", "Cancel", false).on_click(cx.listener(|this, _, _, cx| { this.cancel.cancel(); cx.notify(); }))))
                .child(button("copy", "Copy Assembly", false).on_click(cx.listener(|this, _, _, cx| {
                    if !this.output.is_empty() { cx.write_to_clipboard(ClipboardItem::new_string(this.output.to_string())); }
                })))
                .children(optimization).children(choices))
            .child(div().flex().items_center().gap_2().px_2().pb_2()
                .child("Compiler flags")
                .child(div().h(px(28.)).flex_1().min_w_0().border_1().border_color(theme::border()).child(self.flags.clone())))
            .child(div().flex().items_center().gap_2().px_2().pb_2()
                .child("Find")
                .child(div().h(px(28.)).w(px(230.)).border_1().border_color(theme::border()).child(self.find.clone()))
                .child(button("next-match", "Next", false).on_click(cx.listener(|this, _, _, cx| {
                    if !this.matches.is_empty() { this.current_match = (this.current_match + 1) % this.matches.len(); this.reveal_match(); cx.notify(); }
                })))
                .child(format!("{} matching lines", self.matches.len()))
                .child(button("diagnostics", "Compiler Output", self.show_diagnostics).on_click(cx.listener(|this, _, _, cx| {
                    this.show_diagnostics = !this.show_diagnostics; cx.notify();
                }))))
            .child(div().px_3().pb_1().text_xs().text_color(theme::muted()).child(selectable("scope",
                if self.compilers.get(self.selected).is_some_and(|c| c.cargo_output.is_some()) {
                    "Assembly for the selected Cargo target. Refresh saves open source files first."
                } else { "Assembly for this source file. Refresh saves it first." })))
            .child(div().flex_1().min_h_0().flex().flex_col().font_family(theme::MONO_FONT).child(body))
            .when(self.show_diagnostics && !self.diagnostics.is_empty(), |view| view.child(
                div().id("compiler-output").max_h(px(200.)).overflow_scroll().p_2().font_family(theme::MONO_FONT)
                    .border_t_1().border_color(theme::border()).child(selectable("diagnostics", self.diagnostics.clone()))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn searches_assembly_without_changing_the_listing(cx: &mut gpui::TestAppContext) {
        let window = cx.add_window(|_, cx| {
            AssemblyView::new(
                Project::local(std::env::temp_dir()),
                PathBuf::from("/tmp/square.c"),
                cx,
            )
        });
        window
            .update(cx, |view, _, cx| {
                view.output = "square:\n  imul eax, eax\n  ret\n".into();
                view.lines = view
                    .output
                    .lines()
                    .map(|line| SharedString::from(line.to_owned()))
                    .collect();
                view.find.update(cx, |find, cx| find.set_text("EAX", cx));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, cx| {
                assert_eq!(view.matches, [1]);
                assert!(view.output.contains("imul eax, eax"));
                view.find.update(cx, |find, cx| find.set_text("", cx));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .read_with(cx, |view, _| assert!(view.matches.is_empty()))
            .unwrap();
    }
}
