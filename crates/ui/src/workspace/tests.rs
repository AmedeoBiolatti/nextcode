//! Editor panes and session restore, driven through key bindings like a user.

use gpui::{TestAppContext, WindowHandle};

use super::*;
use crate::builtins::assembly::{self, AssemblyView, InspectAssembly};

/// A project folder with three small files.
fn project_dir(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("nc-panes-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for (file, text) in [
        ("a.txt", "alpha\n"),
        ("b.txt", "beta\n"),
        ("c.txt", "gamma\n"),
    ] {
        std::fs::write(root.join(file), text).unwrap();
    }
    root
}

/// Removes the project and this test's state (session, layout, recents).
fn cleanup(root: &Path) {
    std::fs::remove_dir_all(root).ok();
    if let Some(state) = crate::layout::state_root() {
        std::fs::remove_dir_all(state).ok();
    }
    if let Some(recent) = recent::path() {
        std::fs::remove_file(recent).ok();
    }
}

fn open_workspace(root: &Path, cx: &mut TestAppContext) -> WindowHandle<Workspace> {
    cx.update(|cx| {
        crate::editor::bind_keys(cx);
        bind_keys(cx);
    });
    let project = Project::local(root.to_path_buf());
    let window = cx.add_window(|window, cx| Workspace::new(project, None, window, cx));
    cx.run_until_parked();
    window
}

fn open(window: WindowHandle<Workspace>, path: &Path, cx: &mut TestAppContext) {
    window
        .update(cx, |workspace, window, cx| workspace.open(path, window, cx))
        .unwrap();
    cx.run_until_parked();
}

fn keys(window: WindowHandle<Workspace>, keystrokes: &str, cx: &mut TestAppContext) {
    cx.simulate_keystrokes(window.into(), keystrokes);
    cx.run_until_parked();
}

/// Tab titles per pane, and the active pane's index.
fn panes(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> (Vec<Vec<String>>, usize) {
    window
        .read_with(cx, |workspace, _| {
            let titles = workspace
                .panes
                .iter()
                .map(|pane| pane.tabs.iter().map(|tab| tab.title.to_string()).collect())
                .collect();
            let active = workspace
                .panes
                .iter()
                .position(|pane| pane.id == workspace.active_pane)
                .unwrap();
            (titles, active)
        })
        .unwrap()
}

fn titles(panes: &[&[&str]]) -> Vec<Vec<String>> {
    panes
        .iter()
        .map(|tabs| tabs.iter().map(|title| (*title).to_owned()).collect())
        .collect()
}

/// The text of every open editor, in pane order.
fn texts(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> Vec<String> {
    window
        .read_with(cx, |workspace, cx| {
            workspace
                .all_tabs()
                .filter_map(Tab::editor)
                .map(|editor| editor.read(cx).text())
                .collect()
        })
        .unwrap()
}

fn dirty(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> usize {
    window
        .read_with(cx, |workspace, cx| workspace.dirty_count(cx))
        .unwrap()
}

#[gpui::test]
fn assembly_inspection_opens_independent_tabs_that_can_move_between_panes(cx: &mut TestAppContext) {
    let root = project_dir("assembly");
    let path = root.join("square.c");
    std::fs::write(&path, "int square(int x) { return x*x; }\n").unwrap();
    let window = open_workspace(&root, cx);
    open(window, &path, cx);
    // Off (not saved: other tests share the extension state file), the
    // built-in extension has no command.
    let store = cx.update(crate::extensions::shared);
    let available = |cx: &mut TestAppContext| {
        window
            .update(cx, |_, window, cx| {
                window.is_action_available(&InspectAssembly, cx)
            })
            .unwrap()
    };
    store.update(cx, |store, cx| {
        store.set_builtin_unsaved("assembly", false, cx)
    });
    cx.run_until_parked();
    assert!(!available(cx));
    store.update(cx, |store, cx| {
        store.set_builtin_unsaved("assembly", true, cx)
    });
    cx.run_until_parked();
    assert!(available(cx));
    cx.simulate_input(window.into(), " ");
    cx.run_until_parked();
    assert_eq!(dirty(window, cx), 1);
    // As the menu and the palette run it: the built-in extension's handler.
    cx.dispatch_action(window.into(), InspectAssembly);
    cx.run_until_parked();
    assert_eq!(dirty(window, cx), 0);
    assert!(std::fs::read_to_string(&path).unwrap().starts_with(' '));
    window
        .update(cx, |workspace, window, cx| {
            assert!(workspace.active_builtin_view::<AssemblyView>().is_some());
            assert_eq!(workspace.all_tabs().count(), 2);
            assert!(!workspace.active_tab().unwrap().is_dirty(cx));
            let session::Node::Pane { tabs, .. } = workspace.session(cx).root else {
                panic!("expected one pane");
            };
            assert_eq!(tabs.len(), 1);
            assert_eq!(tabs[0].path, path);
            let first = workspace.active_tab().unwrap().entity_id();
            // Invoke from the source, then from an existing assembly listing.
            workspace.open(&path, window, cx);
            assembly::inspect(workspace, &InspectAssembly, window, cx);
            let second = workspace.active_tab().unwrap().entity_id();
            assert_ne!(first, second);
            assert_eq!(
                workspace.active_tab().unwrap().title.as_ref(),
                "Assembly 2 · square.c"
            );
            assembly::inspect(workspace, &InspectAssembly, window, cx);
            assert_eq!(workspace.all_tabs().count(), 4);
            assert_eq!(
                workspace.active_tab().unwrap().title.as_ref(),
                "Assembly 3 · square.c"
            );
            let pins: HashSet<_> = workspace
                .all_tabs()
                .map(|tab| tab.pin_key(&workspace.project, cx))
                .collect();
            assert_eq!(
                pins.len(),
                4,
                "each assembly page has a separate Pinpoint identity"
            );
        })
        .unwrap();
    cx.run_until_parked();
    keys(window, "ctrl-w", cx);
    assert!(!cx.has_pending_prompt());
    assert_eq!(
        panes(window, cx).0,
        titles(&[&["square.c", "Assembly · square.c", "Assembly 2 · square.c"]])
    );
    open(window, &path, cx);
    keys(window, "ctrl-\\", cx);
    window
        .update(cx, |workspace, window, cx| {
            let (pane, index) = workspace
                .find_tab(|tab| tab.title.as_ref() == "Assembly 2 · square.c")
                .unwrap();
            workspace.activate(pane, index, window, cx);
        })
        .unwrap();
    keys(window, "ctrl-alt-right", cx);
    assert_eq!(
        panes(window, cx).0,
        titles(&[
            &["square.c", "Assembly · square.c"],
            &["square.c", "Assembly 2 · square.c"],
        ])
    );
    keys(window, "ctrl-w", cx);
    assert!(!cx.has_pending_prompt());
    assert_eq!(
        panes(window, cx).0,
        titles(&[&["square.c", "Assembly · square.c"], &["square.c"],])
    );
    // Turned off, its tabs close and its command goes.
    store.update(cx, |store, cx| {
        store.set_builtin_unsaved("assembly", false, cx)
    });
    cx.run_until_parked();
    assert_eq!(panes(window, cx).0, titles(&[&["square.c"], &["square.c"]]));
    assert!(!available(cx));
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cleanup(&root);
}

#[gpui::test]
fn turning_python_off_hides_its_panel_offer_and_commands_but_keeps_its_place(
    cx: &mut TestAppContext,
) {
    use crate::builtins::python::{TogglePanel, panel::PythonPanel};
    let root = project_dir("python-builtin");
    let window = open_workspace(&root, cx);
    let store = cx.update(crate::extensions::shared);
    let python = PanelId::Builtin("python");
    let set = |on: bool, cx: &mut TestAppContext| {
        store.update(cx, |store, cx| store.set_builtin_unsaved("python", on, cx));
        cx.run_until_parked();
    };
    let available = |cx: &mut TestAppContext| {
        window
            .update(cx, |_, window, cx| {
                window.is_action_available(&TogglePanel, cx)
            })
            .unwrap()
    };
    let sync = OfferKind::Builtin {
        extension: "python",
        key: "python-sync".into(),
        action: "Sync",
    };
    set(true, cx);
    assert!(available(cx));
    window
        .update(cx, |workspace, _, cx| {
            assert!(workspace.builtin_panel::<PythonPanel>("python").is_some());
            workspace.offer(sync.clone(), "out of date", cx);
            workspace.show_panel(python, cx);
            assert_eq!(
                workspace.shown_dock(DockPosition::Left).active,
                Some(python)
            );
        })
        .unwrap();

    set(false, cx);
    assert!(!available(cx));
    window
        .update(cx, |workspace, _, cx| {
            assert!(workspace.builtin_panel::<PythonPanel>("python").is_none());
            assert!(!workspace.offers.iter().any(|offer| offer.kind == sync));
            let left = workspace.shown_dock(DockPosition::Left);
            assert_eq!(
                left.panels,
                [
                    PanelId::Files,
                    PanelId::Search,
                    PanelId::Commit,
                    PanelId::Tests
                ]
            );
            assert_eq!(left.active, Some(PanelId::Files));
            // Dropped on Search (shown second), Tests lands before it.
            workspace.move_panel(PanelId::Tests, DockPosition::Left, Some(1), cx);
            assert_eq!(
                workspace.layout.left.panels,
                [
                    PanelId::Files,
                    PanelId::Tests,
                    PanelId::Search,
                    PanelId::Commit,
                    python
                ]
            );
        })
        .unwrap();

    set(true, cx);
    window
        .update(cx, |workspace, _, _| {
            assert!(
                workspace
                    .shown_dock(DockPosition::Left)
                    .panels
                    .contains(&python)
            );
        })
        .unwrap();
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cleanup(&root);
}

#[gpui::test]
fn turning_notes_off_takes_them_from_open_editors_and_on_gives_them_back(cx: &mut TestAppContext) {
    use crate::{builtins::notes::panel::NotesPanel, editor::AddNote};
    let root = project_dir("notes-builtin");
    let window = open_workspace(&root, cx);
    let store = cx.update(crate::extensions::shared);
    let set = |on: bool, cx: &mut TestAppContext| {
        store.update(cx, |store, cx| store.set_builtin_unsaved("notes", on, cx));
        cx.run_until_parked();
    };
    // Add Note is the open editor's, there while it has the notes.
    let add_note = |cx: &mut TestAppContext| {
        window
            .update(cx, |_, window, cx| window.is_action_available(&AddNote, cx))
            .unwrap()
    };
    let notes = PanelId::Builtin("notes");
    let message = OfferKind::Builtin {
        extension: "notes",
        key: "note-0-n1a2b".into(),
        action: "Show Note",
    };
    set(true, cx);
    open(window, &root.join("a.txt"), cx);
    assert!(add_note(cx));
    window
        .update(cx, |workspace, _, cx| {
            assert!(workspace.builtin_panel::<NotesPanel>("notes").is_some());
            workspace.offer(message.clone(), "An agent left you a note", cx);
        })
        .unwrap();

    set(false, cx);
    assert!(!add_note(cx));
    window
        .update(cx, |workspace, _, _| {
            assert!(workspace.builtin_panel::<NotesPanel>("notes").is_none());
            assert!(!workspace.offers.iter().any(|offer| offer.kind == message));
            assert!(
                !workspace
                    .shown_dock(DockPosition::Bottom)
                    .panels
                    .contains(&notes)
            );
            assert!(workspace.layout.bottom.panels.contains(&notes));
        })
        .unwrap();

    set(true, cx);
    assert!(add_note(cx), "editors already open get the notes again");
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cleanup(&root);
}

#[gpui::test]
fn a_split_shows_the_file_twice_over_one_buffer(cx: &mut TestAppContext) {
    let root = project_dir("share");
    let window = open_workspace(&root, cx);
    open(window, &root.join("a.txt"), cx);
    keys(window, "ctrl-\\", cx);
    assert_eq!(panes(window, cx), (titles(&[&["a.txt"], &["a.txt"]]), 1));

    // Typing in the new pane shows in both; the file counts as one.
    cx.simulate_input(window.into(), "x");
    cx.run_until_parked();
    assert_eq!(texts(window, cx), ["xalpha\n", "xalpha\n"]);
    assert_eq!(dirty(window, cx), 1);

    // Undo in the first pane reverts the shared edit.
    keys(window, "ctrl-1", cx);
    assert_eq!(panes(window, cx).1, 0);
    keys(window, "ctrl-z", cx);
    assert_eq!(texts(window, cx), ["alpha\n", "alpha\n"]);
    assert_eq!(dirty(window, cx), 0);

    // Closing one view of an edited file does not ask; its pane goes.
    cx.simulate_input(window.into(), "y");
    cx.run_until_parked();
    keys(window, "ctrl-w", cx);
    assert!(!cx.has_pending_prompt());
    assert_eq!(panes(window, cx), (titles(&[&["a.txt"]]), 0));
    assert_eq!(texts(window, cx), ["yalpha\n"]);

    // Closing the last view asks once.
    keys(window, "ctrl-w", cx);
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Don't Save");
    cx.run_until_parked();
    assert_eq!(panes(window, cx), (vec![Vec::new()], 0));
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "alpha\n"
    );
    cleanup(&root);
}

#[gpui::test]
fn tabs_move_between_panes_and_files_open_in_the_active_one(cx: &mut TestAppContext) {
    let root = project_dir("move");
    let window = open_workspace(&root, cx);
    open(window, &root.join("a.txt"), cx);
    open(window, &root.join("b.txt"), cx);

    // No pane to the right yet: moving the tab makes one.
    keys(window, "ctrl-alt-right", cx);
    assert_eq!(panes(window, cx), (titles(&[&["a.txt"], &["b.txt"]]), 1));
    open(window, &root.join("c.txt"), cx);
    assert_eq!(
        panes(window, cx),
        (titles(&[&["a.txt"], &["b.txt", "c.txt"]]), 1)
    );
    keys(window, "ctrl-1", cx);
    open(window, &root.join("b.txt"), cx);
    assert_eq!(
        panes(window, cx),
        (titles(&[&["a.txt", "b.txt"], &["b.txt", "c.txt"]]), 0)
    );

    // Moving onto a pane that has the file activates it there.
    keys(window, "ctrl-alt-right", cx);
    assert_eq!(
        panes(window, cx),
        (titles(&[&["a.txt"], &["b.txt", "c.txt"]]), 1)
    );
    keys(window, "ctrl-alt-left", cx);
    assert_eq!(
        panes(window, cx),
        (titles(&[&["a.txt", "b.txt"], &["c.txt"]]), 0)
    );

    // Closing a pane's last tab removes the pane.
    keys(window, "ctrl-2", cx);
    keys(window, "ctrl-w", cx);
    assert_eq!(panes(window, cx), (titles(&[&["a.txt", "b.txt"]]), 0));

    // Split down, then close that pane.
    keys(window, "ctrl-k ctrl-\\", cx);
    assert_eq!(
        panes(window, cx),
        (titles(&[&["a.txt", "b.txt"], &["b.txt"]]), 1)
    );
    let vertical = window
        .read_with(cx, |workspace, _| {
            matches!(&workspace.pane_tree, PaneNode::Split(split) if split.axis == panes::SplitAxis::Vertical)
        })
        .unwrap();
    assert!(vertical);
    keys(window, "ctrl-k w", cx);
    assert_eq!(panes(window, cx), (titles(&[&["a.txt", "b.txt"]]), 0));
    cleanup(&root);
}

#[gpui::test]
fn the_session_comes_back_when_the_project_reopens(cx: &mut TestAppContext) {
    let root = project_dir("session");
    let window = open_workspace(&root, cx);
    for file in ["a.txt", "b.txt", "c.txt", "b.txt"] {
        open(window, &root.join(file), cx);
    }
    keys(window, "ctrl-\\", cx);
    window
        .update(cx, |workspace, _, cx| {
            if let PaneNode::Split(split) = &mut workspace.pane_tree {
                split.ratio = 0.3;
            }
            let editor = workspace.panes[0].tabs[0].editor().unwrap().clone();
            editor.update(cx, |editor, cx| editor.set_cursor(3, cx));
        })
        .unwrap();
    // Closing the window releases the workspace, which saves the session.
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    std::fs::remove_file(root.join("c.txt")).unwrap();

    let window = open_workspace(&root, cx);
    assert_eq!(
        panes(window, cx),
        (titles(&[&["a.txt", "b.txt"], &["b.txt"]]), 1),
        "c.txt no longer exists"
    );
    let (active, ratio, cursor) = window
        .read_with(cx, |workspace, cx| {
            let ratio = match &workspace.pane_tree {
                PaneNode::Split(split) => split.ratio,
                PaneNode::Leaf(_) => 1.,
            };
            let cursor = workspace.panes[0].tabs[0]
                .editor()
                .unwrap()
                .read(cx)
                .cursor();
            (workspace.panes[0].active, ratio, cursor)
        })
        .unwrap();
    assert_eq!(active, 1, "b.txt was active in the first pane");
    assert!((ratio - 0.3).abs() < 1e-6);
    assert_eq!(cursor, 3);

    // The restored views of b.txt share one buffer again.
    cx.simulate_input(window.into(), "z");
    cx.run_until_parked();
    assert_eq!(texts(window, cx), ["alpha\n", "zbeta\n", "zbeta\n"]);
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cleanup(&root);
}

fn panel_contribution(id: &str, placement: extension_api::PanelPlacement) -> PanelContribution {
    PanelContribution {
        id: id.into(),
        title: format!("Panel {id}"),
        placement,
    }
}

fn panel_update(id: &str, count: u64) -> PanelUpdate {
    PanelUpdate {
        refresh_ms: None,
        id: id.into(),
        root: extension_api::Widget::Label {
            id: "result".into(),
            text: count.to_string(),
        },
        state: serde_json::json!({ "count": count }),
    }
}

#[gpui::test]
fn extension_placement_preserves_view_and_state_but_close_rejects_late_replies(
    cx: &mut TestAppContext,
) {
    let root = project_dir("extension-placement");
    let window = open_workspace(&root, cx);
    window.update(cx, |workspace, window, cx| {
        let contributions = vec![
            panel_contribution("dock", extension_api::PanelPlacement::Dock),
            panel_contribution("editor", extension_api::PanelPlacement::Editor),
        ];
        let view = workspace.prepare_extension_panel("test.extension", &contributions[0], window, cx);
        workspace.apply_extension_panels("test.extension", &contributions, vec![panel_update("dock", 7)], None, window, cx).unwrap();
        let revision = view.read(cx).revision();
        workspace.move_extension_to_editor(view.entity_id(), window, cx);
        assert!(matches!(&workspace.active_tab().unwrap().content, TabContent::Extension(tab) if *tab == view));
        assert!(!workspace.active_tab().unwrap().is_dirty(cx));
        assert!(workspace.active_tab().unwrap().file_path().is_none());
        workspace.move_extension_to_dock(view.entity_id(), window, cx);
        assert_eq!(workspace.active_docked_extension(), Some(view.clone()));
        assert_eq!(view.read(cx).revision(), revision);
        assert_eq!(view.read(cx).snapshot(cx).unwrap().1.state, serde_json::json!({ "count": 7 }));
        assert_eq!(workspace.prepare_extension_panel("test.extension", &contributions[0], window, cx), view);
        assert_eq!(workspace.extension_panels.len(), 1);

        // Stale origins cannot mutate the current model or open another panel.
        workspace.apply_extension_panels("test.extension", &contributions, vec![panel_update("dock", 8)], None, window, cx).unwrap();
        assert!(workspace.apply_extension_panels("test.extension", &contributions, vec![panel_update("editor", 9)],
            Some(ExtensionPanelOrigin { process_result: false, view: view.clone(), revision, submitted_inputs: Some(Default::default()) }), window, cx).is_err());
        assert!(workspace.extension_panel("test.extension", "editor").is_none());
        let revision = view.read(cx).revision();
        workspace.move_extension_to_editor(view.entity_id(), window, cx);
        let (pane, index) = workspace.find_view(view.entity_id()).unwrap();
        workspace.close_tab(pane, index, window, cx);
        assert!(!workspace.contains_extension_view(&view));
        assert!(workspace.apply_extension_panels("test.extension", &contributions, vec![panel_update("dock", 10)],
            Some(ExtensionPanelOrigin { process_result: false, view, revision, submitted_inputs: Some(Default::default()) }), window, cx).is_err());
    }).unwrap();
    assert!(!cx.has_pending_prompt());
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cleanup(&root);
}

#[gpui::test]
fn extension_tabs_are_absent_from_sessions_and_reload_invalidates_live_entities(
    cx: &mut TestAppContext,
) {
    let root = project_dir("extension-session");
    let window = open_workspace(&root, cx);
    open(window, &root.join("a.txt"), cx);
    window
        .update(cx, |workspace, window, cx| {
            let contribution = panel_contribution("editor", extension_api::PanelPlacement::Editor);
            let view =
                workspace.prepare_extension_panel("test.extension", &contribution, window, cx);
            assert_eq!(workspace.dirty_count(cx), 0);
            assert_eq!(workspace.tab_paths(), vec![root.join("a.txt")]);
            match workspace.session(cx).root {
                session::Node::Pane { tabs, .. } => {
                    assert_eq!(tabs.len(), 1);
                    assert_eq!(tabs[0].path, root.join("a.txt"));
                }
                _ => panic!("Unexpected split"),
            }
            let model = crate::extensions::shared(cx);
            model.update(cx, |model, _| model.generation += 1);
            workspace.sync_extension_panels(&model, window, cx);
            assert!(!workspace.contains_extension_view(&view));
            assert!(workspace.find_view(view.entity_id()).is_none());
            assert_eq!(workspace.tab_paths(), vec![root.join("a.txt")]);
        })
        .unwrap();
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let window = open_workspace(&root, cx);
    assert_eq!(panes(window, cx).0, titles(&[&["a.txt"]]));
    window
        .read_with(cx, |workspace, _| {
            assert!(workspace.extension_panels.is_empty())
        })
        .unwrap();
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cleanup(&root);
}

#[gpui::test]
fn extension_open_accepts_multiple_declared_panels_atomically(cx: &mut TestAppContext) {
    let root = project_dir("extension-multi-open");
    let window = open_workspace(&root, cx);
    window
        .update(cx, |workspace, window, cx| {
            let contributions = vec![
                panel_contribution("main", extension_api::PanelPlacement::Dock),
                panel_contribution("other", extension_api::PanelPlacement::Dock),
            ];
            let view =
                workspace.prepare_extension_panel("test.extension", &contributions[0], window, cx);
            let revision = view.read(cx).revision();
            workspace
                .apply_extension_panels(
                    "test.extension",
                    &contributions,
                    vec![panel_update("main", 7), panel_update("other", 9)],
                    Some(ExtensionPanelOrigin {
                        process_result: false,
                        view: view.clone(),
                        revision,
                        submitted_inputs: None,
                    }),
                    window,
                    cx,
                )
                .unwrap();
            assert_eq!(
                view.read(cx).model().unwrap().state,
                serde_json::json!({"count": 7})
            );
            let other = workspace
                .extension_panel("test.extension", "other")
                .unwrap();
            assert_eq!(
                other.read(cx).model().unwrap().state,
                serde_json::json!({"count": 9})
            );
            let revision = view.read(cx).revision();
            assert!(
                workspace
                    .apply_extension_panels(
                        "test.extension",
                        &contributions,
                        vec![panel_update("main", 99), panel_update("other", 99)],
                        Some(ExtensionPanelOrigin {
                            process_result: false,
                            view: view.clone(),
                            revision,
                            submitted_inputs: Some(Default::default())
                        }),
                        window,
                        cx
                    )
                    .is_err()
            );
            assert_eq!(
                view.read(cx).model().unwrap().state,
                serde_json::json!({"count": 7})
            );
            assert_eq!(
                other.read(cx).model().unwrap().state,
                serde_json::json!({"count": 9})
            );
        })
        .unwrap();
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cleanup(&root);
}

#[gpui::test]
fn closing_extension_panel_cancels_its_lifetime_even_when_a_reply_retains_the_view(
    cx: &mut TestAppContext,
) {
    let root = project_dir("extension-lifetime");
    let window = open_workspace(&root, cx);
    window
        .update(cx, |workspace, window, cx| {
            let contribution = panel_contribution("overview", extension_api::PanelPlacement::Dock);
            let view =
                workspace.prepare_extension_panel("test.extension", &contribution, window, cx);
            let lifetime = view.read(cx).lifetime.clone();
            workspace.close_extension_panel(view.entity_id(), window, cx);
            assert!(lifetime.is_cancelled());
            assert!(!workspace.contains_extension_view(&view));
        })
        .unwrap();
}
