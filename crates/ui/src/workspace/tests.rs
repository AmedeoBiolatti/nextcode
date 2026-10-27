//! Editor panes and session restore, driven through key bindings like a user.

use gpui::{TestAppContext, WindowHandle};

use super::*;

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
