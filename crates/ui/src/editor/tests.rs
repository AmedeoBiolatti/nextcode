//! Editing behavior through the editor's commands, on real buffers.

use std::path::PathBuf;

use gpui::{Modifiers, TestAppContext, WindowHandle};

use super::{selections::SelectionState, *};

/// An editor for `marked` (see `parse`) as file `name`.
fn editor(name: &str, marked: &str, cx: &mut TestAppContext) -> WindowHandle<Editor> {
    let (text, selections) = parse(marked);
    let window = cx.add_window(|_, cx| {
        let mut editor = Editor::new(Buffer::with_path(&text, PathBuf::from(name)), cx);
        editor.selections = selections;
        editor
    });
    cx.run_until_parked();
    window
}

/// Text with `|` for cursors and `«…»` for selections (anchor at `«`);
/// the last one is primary.
fn parse(marked: &str) -> (String, SelectionSet) {
    let mut text = String::new();
    let mut items = Vec::new();
    let mut anchor = None;
    for c in marked.chars() {
        match c {
            '|' => items.push(Selection::cursor(text.len())),
            '«' => anchor = Some(text.len()),
            '»' => items.push(Selection::new(anchor.take().unwrap(), text.len())),
            c => text.push(c),
        }
    }
    let primary = items.len() - 1;
    (text, SelectionSet::new(items, primary))
}

/// The editor's text with its selections marked as in `parse`.
fn marked(editor: &Editor) -> String {
    let text = editor.text();
    let mut out = String::new();
    let mut last = 0;
    for s in editor.selections.all() {
        if s.is_empty() {
            out.push_str(&text[last..s.head]);
            out.push('|');
        } else {
            out.push_str(&text[last..s.start()]);
            out.push('«');
            out.push_str(&text[s.range()]);
            out.push('»');
        }
        last = s.end();
    }
    out.push_str(&text[last..]);
    out
}

fn update<R>(
    window: WindowHandle<Editor>,
    cx: &mut TestAppContext,
    f: impl FnOnce(&mut Editor, &mut Window, &mut Context<Editor>) -> R,
) -> R {
    window.update(cx, f).unwrap()
}

fn state(window: WindowHandle<Editor>, cx: &mut TestAppContext) -> String {
    window.read_with(cx, |editor, _| marked(editor)).unwrap()
}

fn type_text(window: WindowHandle<Editor>, text: &str, cx: &mut TestAppContext) {
    for c in text.chars() {
        update(window, cx, |editor, window, cx| {
            editor.replace_text_in_range(None, &c.to_string(), window, cx)
        });
    }
}

#[gpui::test]
fn typing_and_deleting_at_every_cursor_undo_as_one_step(cx: &mut TestAppContext) {
    let window = editor("a.txt", "a|\nb|\nc|", cx);
    type_text(window, "xy", cx);
    assert_eq!(state(window, cx), "axy|\nbxy|\ncxy|");
    update(window, cx, |e, w, cx| e.backspace(&Backspace, w, cx));
    assert_eq!(state(window, cx), "ax|\nbx|\ncx|");
    update(window, cx, |e, w, cx| e.undo(&Undo, w, cx));
    assert_eq!(state(window, cx), "axy|\nbxy|\ncxy|");
    update(window, cx, |e, w, cx| e.undo(&Undo, w, cx));
    assert_eq!(state(window, cx), "a|\nb|\nc|", "typing was one step");
    update(window, cx, |e, w, cx| e.redo(&Redo, w, cx));
    assert_eq!(state(window, cx), "axy|\nbxy|\ncxy|");
}

#[gpui::test]
fn undo_restores_the_selections_of_the_edit(cx: &mut TestAppContext) {
    let window = editor("a.txt", "«one» two «three»", cx);
    update(window, cx, |e, w, cx| e.cut(&Cut, w, cx));
    assert_eq!(state(window, cx), "| two |");
    update(window, cx, |e, w, cx| e.undo(&Undo, w, cx));
    assert_eq!(state(window, cx), "«one» two «three»");
}

#[gpui::test]
fn paste_spreads_one_line_per_cursor(cx: &mut TestAppContext) {
    let window = editor("a.txt", "a|\nb|\nc|", cx);
    cx.write_to_clipboard(ClipboardItem::new_string("1\n2\n3\n".into()));
    update(window, cx, |e, w, cx| e.paste(&Paste, w, cx));
    assert_eq!(state(window, cx), "a1|\nb2|\nc3|");
    cx.write_to_clipboard(ClipboardItem::new_string("z".into()));
    update(window, cx, |e, w, cx| e.paste(&Paste, w, cx));
    assert_eq!(state(window, cx), "a1z|\nb2z|\nc3z|");
    // Copying several selections joins them by line, which pastes back.
    update(window, cx, |e, w, cx| e.select_left(&SelectLeft, w, cx));
    update(window, cx, |e, w, cx| e.copy(&Copy, w, cx));
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("z\nz\nz".into())
    );
}

#[gpui::test]
fn overlapping_selections_merge(cx: &mut TestAppContext) {
    let window = editor("a.txt", "x foo|bar| y", cx);
    // Both deletions reach the word start: one cursor remains.
    update(window, cx, |e, w, cx| {
        e.delete_word_left(&DeleteWordLeft, w, cx)
    });
    assert_eq!(state(window, cx), "x | y");
    let window = editor("a.txt", "ab|c|d", cx);
    update(window, cx, |e, w, cx| e.select_left(&SelectLeft, w, cx));
    update(window, cx, |e, w, cx| e.select_left(&SelectLeft, w, cx));
    assert_eq!(state(window, cx), "«abc»d", "selections that overlap merge");
}

#[gpui::test]
fn ctrl_d_adds_whole_word_occurrences_and_select_all_finds_every_one(cx: &mut TestAppContext) {
    let window = editor("a.py", "fo|o = food + foo(foo)", cx);
    update(window, cx, |e, w, cx| {
        e.select_next_occurrence(&SelectNextOccurrence, w, cx)
    });
    assert_eq!(state(window, cx), "«foo» = food + foo(foo)");
    update(window, cx, |e, w, cx| {
        e.select_next_occurrence(&SelectNextOccurrence, w, cx)
    });
    assert_eq!(state(window, cx), "«foo» = food + «foo»(foo)", "skips food");
    update(window, cx, |e, w, cx| {
        e.select_next_occurrence(&SelectNextOccurrence, w, cx)
    });
    update(window, cx, |e, w, cx| {
        e.select_next_occurrence(&SelectNextOccurrence, w, cx)
    });
    assert_eq!(state(window, cx), "«foo» = food + «foo»(«foo»)");
    type_text(window, "bar", cx);
    assert_eq!(state(window, cx), "bar| = food + bar|(bar|)");

    let window = editor("a.py", "«oo» foo food", cx);
    update(window, cx, |e, w, cx| {
        e.select_all_occurrences(&SelectAllOccurrences, w, cx)
    });
    assert_eq!(
        state(window, cx),
        "«oo» f«oo» f«oo»d",
        "a selection matches inside words"
    );
    update(window, cx, |e, w, cx| {
        e.collapse_selections(&CollapseSelections, w, cx)
    });
    assert_eq!(state(window, cx).matches('«').count(), 1);
}

#[gpui::test]
fn cursors_above_and_below_keep_the_column(cx: &mut TestAppContext) {
    let window = editor("a.txt", "abcd\nab\nabc|d", cx);
    update(window, cx, |e, w, cx| {
        e.add_cursor_above(&AddCursorAbove, w, cx)
    });
    update(window, cx, |e, w, cx| {
        e.add_cursor_above(&AddCursorAbove, w, cx)
    });
    assert_eq!(state(window, cx), "abc|d\nab|\nabc|d");
}

#[gpui::test]
fn line_moves_copies_and_deletes_stop_at_buffer_ends(cx: &mut TestAppContext) {
    let window = editor("a.txt", "one\ntw|o\nthree", cx);
    update(window, cx, |e, w, cx| e.move_line_up(&MoveLineUp, w, cx));
    assert_eq!(state(window, cx), "tw|o\none\nthree");
    update(window, cx, |e, w, cx| e.move_line_up(&MoveLineUp, w, cx));
    assert_eq!(state(window, cx), "tw|o\none\nthree", "already first");
    update(window, cx, |e, w, cx| {
        e.move_line_down(&MoveLineDown, w, cx)
    });
    update(window, cx, |e, w, cx| {
        e.move_line_down(&MoveLineDown, w, cx)
    });
    assert_eq!(state(window, cx), "one\nthree\ntw|o");
    update(window, cx, |e, w, cx| {
        e.move_line_down(&MoveLineDown, w, cx)
    });
    assert_eq!(state(window, cx), "one\nthree\ntw|o", "already last");

    let window = editor("a.txt", "a\n«b\nc»\nd", cx);
    update(window, cx, |e, w, cx| {
        e.copy_line_down(&CopyLineDown, w, cx)
    });
    assert_eq!(state(window, cx), "a\nb\nc\n«b\nc»\nd");
    update(window, cx, |e, w, cx| e.copy_line_up(&CopyLineUp, w, cx));
    assert_eq!(state(window, cx), "a\nb\nc\n«b\nc»\nb\nc\nd");

    let window = editor("a.txt", "a|a\nb|\nlong\nla|st", cx);
    update(window, cx, |e, w, cx| e.delete_line(&DeleteLine, w, cx));
    assert_eq!(
        state(window, cx),
        "l|o|ng",
        "two blocks; the last line goes up"
    );
    let window = editor("a.txt", "on|ly", cx);
    update(window, cx, |e, w, cx| e.delete_line(&DeleteLine, w, cx));
    assert_eq!(state(window, cx), "|");
}

#[gpui::test]
fn insert_line_below_and_above_keep_indentation(cx: &mut TestAppContext) {
    let window = editor("a.py", "def f():|\n    x = 1|", cx);
    update(window, cx, |e, w, cx| {
        e.insert_line_below(&InsertLineBelow, w, cx)
    });
    assert_eq!(state(window, cx), "def f():\n    |\n    x = 1\n    |");
    let window = editor("a.py", "    x| = 1", cx);
    update(window, cx, |e, w, cx| {
        e.insert_line_above(&InsertLineAbove, w, cx)
    });
    assert_eq!(state(window, cx), "    |\n    x = 1");
}

#[gpui::test]
fn line_comments_toggle_at_the_smallest_indent(cx: &mut TestAppContext) {
    let window = editor("a.py", "«def f():\n    x = 1\n\n    return x»\n", cx);
    update(window, cx, |e, w, cx| {
        e.toggle_line_comment(&ToggleLineComment, w, cx)
    });
    assert_eq!(
        state(window, cx),
        "«# def f():\n#     x = 1\n\n#     return x»\n"
    );
    update(window, cx, |e, w, cx| {
        e.toggle_line_comment(&ToggleLineComment, w, cx)
    });
    assert_eq!(state(window, cx), "«def f():\n    x = 1\n\n    return x»\n");

    // Mixed: some lines commented, so all get commented.
    let window = editor("a.rs", "    «// a\n    b»", cx);
    update(window, cx, |e, w, cx| {
        e.toggle_line_comment(&ToggleLineComment, w, cx)
    });
    assert_eq!(state(window, cx), "    «// // a\n    // b»");

    let window = editor("q.sql", "select| 1;", cx);
    update(window, cx, |e, w, cx| {
        e.toggle_line_comment(&ToggleLineComment, w, cx)
    });
    assert_eq!(state(window, cx), "-- select| 1;");

    let window = editor("p.html", "  <p>|hi</p>", cx);
    update(window, cx, |e, w, cx| {
        e.toggle_line_comment(&ToggleLineComment, w, cx)
    });
    assert_eq!(state(window, cx), "  <!-- <p>|hi</p> -->");
    update(window, cx, |e, w, cx| {
        e.toggle_line_comment(&ToggleLineComment, w, cx)
    });
    assert_eq!(state(window, cx), "  <p>|hi</p>");
}

#[gpui::test]
fn block_comments_wrap_and_unwrap_selections(cx: &mut TestAppContext) {
    let window = editor("a.rs", "let «x» = 1;|", cx);
    update(window, cx, |e, w, cx| {
        e.toggle_block_comment(&ToggleBlockComment, w, cx)
    });
    assert_eq!(state(window, cx), "let /* «x» */ = 1;/* | */");
    let window = editor("a.rs", "let /* «x» */ = 1;", cx);
    update(window, cx, |e, w, cx| {
        e.toggle_block_comment(&ToggleBlockComment, w, cx)
    });
    assert_eq!(state(window, cx), "let «x» = 1;");
}

#[gpui::test]
fn tab_indents_selected_lines_and_shift_tab_outdents(cx: &mut TestAppContext) {
    let window = editor("a.py", "«if x:\n    y»\nz", cx);
    update(window, cx, |e, w, cx| e.tab(&Tab, w, cx));
    assert_eq!(state(window, cx), "«    if x:\n        y»\nz");
    update(window, cx, |e, w, cx| e.outdent(&Outdent, w, cx));
    update(window, cx, |e, w, cx| e.outdent(&Outdent, w, cx));
    assert_eq!(state(window, cx), "«if x:\ny»\nz");
    // A single cursor inserts spaces to the next tab stop.
    let window = editor("a.py", "ab|", cx);
    update(window, cx, |e, w, cx| e.tab(&Tab, w, cx));
    assert_eq!(state(window, cx), "ab  |");
    // A tab-indented file indents with tabs.
    let window = editor("a.go", "func f() {\n\tx|\n}", cx);
    update(window, cx, |e, w, cx| e.indent(&Indent, w, cx));
    assert_eq!(state(window, cx), "func f() {\n\t\tx|\n}");
}

#[gpui::test]
fn brackets_and_quotes_auto_close(cx: &mut TestAppContext) {
    let window = editor("a.py", "x = |", cx);
    type_text(window, "f(", cx);
    assert_eq!(state(window, cx), "x = f(|)");
    type_text(window, "\"a", cx);
    assert_eq!(state(window, cx), "x = f(\"a|\")");
    type_text(window, "\")", cx);
    assert_eq!(state(window, cx), "x = f(\"a\")|", "steps over the closers");
    // Backspace inside an empty pair removes both.
    type_text(window, "[", cx);
    update(window, cx, |e, w, cx| e.backspace(&Backspace, w, cx));
    assert_eq!(state(window, cx), "x = f(\"a\")|");
    // No quote pair after a word character; no bracket pair before one.
    let window = editor("a.py", "it|s |x", cx);
    type_text(window, "'(", cx);
    assert_eq!(state(window, cx), "it'(|s '(|x");
    // A selection gets wrapped.
    let window = editor("a.py", "«abc»", cx);
    type_text(window, "(", cx);
    assert_eq!(state(window, cx), "(«abc»)");
    // A typed closer that was not auto-inserted is just typed.
    let window = editor("a.py", "|)", cx);
    type_text(window, ")", cx);
    assert_eq!(state(window, cx), ")|)");
}

#[gpui::test]
fn enter_indents_after_openers_and_splits_pairs(cx: &mut TestAppContext) {
    let window = editor("a.rs", "fn f() {|}", cx);
    update(window, cx, |e, w, cx| e.newline(&Newline, w, cx));
    assert_eq!(state(window, cx), "fn f() {\n    |\n}");
    let window = editor("a.py", "    def f(x):|", cx);
    update(window, cx, |e, w, cx| e.newline(&Newline, w, cx));
    assert_eq!(state(window, cx), "    def f(x):\n        |");
    let window = editor("a.py", "    x = 1|", cx);
    update(window, cx, |e, w, cx| e.newline(&Newline, w, cx));
    assert_eq!(state(window, cx), "    x = 1\n    |");
    let window = editor("a.ts", "f(|)", cx);
    update(window, cx, |e, w, cx| e.newline(&Newline, w, cx));
    assert_eq!(state(window, cx), "f(\n  |\n)", "2 spaces for TypeScript");
}

#[gpui::test]
fn bracket_pairs_skip_strings(cx: &mut TestAppContext) {
    let window = editor("a.py", "f|(\"(\", [x])", cx);
    let pair = window.read_with(cx, |e, _| e.bracket_pair()).unwrap();
    assert_eq!(pair, Some((1, 10)));
    let window = editor("a.py", "f(\"(\", [x])|", cx);
    let pair = window.read_with(cx, |e, _| e.bracket_pair()).unwrap();
    assert_eq!(pair, Some((10, 1)), "the bracket before the cursor");
    // Markdown has no bracket tokens: the text scan finds the pair.
    let window = editor("a.md", "see |(a (b) c)", cx);
    let pair = window.read_with(cx, |e, _| e.bracket_pair()).unwrap();
    assert_eq!(pair, Some((4, 12)));
    update(window, cx, |e, w, cx| {
        e.jump_to_bracket(&JumpToBracket, w, cx)
    });
    assert_eq!(state(window, cx), "see (a (b) c|)");
}

/// Opens the find bar with `query` and returns the match count.
fn find(window: WindowHandle<Editor>, query: &str, cx: &mut TestAppContext) -> usize {
    update(window, cx, |e, w, cx| {
        e.find(&Find, w, cx);
        let input = e.find.as_ref().unwrap().query.clone();
        input.update(cx, |input, cx| input.set_text(query, cx));
    });
    cx.run_until_parked();
    update(window, cx, |e, _, cx| {
        e.refresh_find(cx);
        e.find.as_ref().unwrap().matches.len()
    })
}

#[gpui::test]
fn find_counts_follow_options_and_edits(cx: &mut TestAppContext) {
    let window = editor("a.py", "|Foo foo food\nfoo", cx);
    assert_eq!(find(window, "foo", cx), 4);
    assert_eq!(
        state(window, cx),
        "«Foo» foo food\nfoo",
        "selects the first match"
    );
    update(window, cx, |e, w, cx| {
        e.toggle_match_case(&ToggleMatchCase, w, cx)
    });
    assert_eq!(find(window, "foo", cx), 3);
    update(window, cx, |e, w, cx| {
        e.toggle_whole_word(&ToggleWholeWord, w, cx)
    });
    assert_eq!(find(window, "foo", cx), 2);
    assert_eq!(state(window, cx), "Foo «foo» food\nfoo");
    update(window, cx, |e, w, cx| e.find_next(&FindNext, w, cx));
    assert_eq!(state(window, cx), "Foo foo food\n«foo»");
    update(window, cx, |e, w, cx| e.find_next(&FindNext, w, cx));
    assert_eq!(state(window, cx), "Foo «foo» food\nfoo", "wraps around");
    update(window, cx, |e, w, cx| e.find_previous(&FindPrevious, w, cx));
    assert_eq!(state(window, cx), "Foo foo food\n«foo»");
    // Editing the text updates the matches.
    update(window, cx, |e, w, cx| e.select_all(&SelectAll, w, cx));
    type_text(window, "foo foo foo", cx);
    let count = update(window, cx, |e, _, cx| {
        e.refresh_find(cx);
        e.find.as_ref().unwrap().matches.len()
    });
    assert_eq!(count, 3);
    update(window, cx, |e, w, cx| {
        e.select_all_matches(&SelectAllMatches, w, cx)
    });
    assert_eq!(state(window, cx), "«foo» «foo» «foo»");
}

#[gpui::test]
fn replace_all_expands_regex_groups_as_one_step(cx: &mut TestAppContext) {
    let window = editor("a.py", "|a = 1\nb = 2\n", cx);
    update(window, cx, |e, _, cx| {
        e.ensure_find_bar(cx);
        e.find.as_mut().unwrap().options.regex = true;
    });
    assert_eq!(find(window, r"(\w) = (\d)", cx), 2);
    update(window, cx, |e, w, cx| {
        let replacement = e.find.as_ref().unwrap().replacement.clone();
        replacement.update(cx, |input, cx| input.set_text("$2 == $1", cx));
        e.replace_all(&ReplaceAll, w, cx);
    });
    assert_eq!(
        window.read_with(cx, |e, _| e.text()).unwrap(),
        "1 == a\n2 == b\n"
    );
    update(window, cx, |e, w, cx| e.undo(&Undo, w, cx));
    assert_eq!(
        window.read_with(cx, |e, _| e.text()).unwrap(),
        "a = 1\nb = 2\n"
    );
}

#[gpui::test]
fn state_round_trips_selections_and_scroll(cx: &mut TestAppContext) {
    let window = editor("a.txt", "one\n«two»\nth|ree", cx);
    let saved = window.read_with(cx, |e, _| e.state()).unwrap();
    assert_eq!(
        saved.selections,
        [
            SelectionState {
                anchor: (1, 0),
                head: (1, 3)
            },
            SelectionState {
                anchor: (2, 2),
                head: (2, 2)
            }
        ]
    );
    let json = serde_json::to_string(&saved).unwrap();
    let other = editor("a.txt", "one\ntwo\nthree|", cx);
    update(other, cx, |e, _, cx| {
        e.restore_state(&serde_json::from_str(&json).unwrap(), cx)
    });
    assert_eq!(state(other, cx), "one\n«two»\nth|ree");
}

/// Wraps the editor at `columns` from its next layout on.
fn wrap_at(window: WindowHandle<Editor>, columns: usize, cx: &mut TestAppContext) {
    update(window, cx, |e, _, cx| {
        e.word_wrap = Some(true);
        e.wrap_columns = Some(columns);
        cx.notify();
    });
    cx.run_until_parked();
}

fn row_count(window: WindowHandle<Editor>, cx: &mut TestAppContext) -> usize {
    window.read_with(cx, |e, _| e.display.row_count()).unwrap()
}

#[gpui::test]
fn alt_z_wraps_every_editor_also_new_ones(cx: &mut TestAppContext) {
    let text = "    al|pha beta gamma delta\nxy";
    let first = editor("a.txt", text, cx);
    let second = editor("b.txt", text, cx);
    for window in [first, second] {
        update(window, cx, |e, _, cx| {
            e.wrap_columns = Some(16);
            cx.notify();
        });
    }
    cx.run_until_parked();
    assert_eq!(row_count(second, cx), 2, "no wrap by default");
    update(first, cx, |e, w, cx| {
        e.toggle_word_wrap(&ToggleWordWrap, w, cx)
    });
    cx.run_until_parked();
    assert_eq!(row_count(second, cx), 3, "the other editor wraps too");
    let later = editor("c.txt", text, cx);
    update(later, cx, |e, _, cx| {
        e.wrap_columns = Some(16);
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(row_count(later, cx), 3, "and so does one opened later");
    update(later, cx, |e, w, cx| {
        e.toggle_word_wrap(&ToggleWordWrap, w, cx)
    });
    cx.run_until_parked();
    assert_eq!(row_count(first, cx), 2);
}

#[gpui::test]
fn vertical_motion_and_home_end_follow_wrapped_rows(cx: &mut TestAppContext) {
    // Rows: "    alpha beta ", "    gamma delta" (continuation indented), "xy".
    let window = editor("a.txt", "    al|pha beta gamma delta\nxy", cx);
    wrap_at(window, 16, cx);
    assert_eq!(row_count(window, cx), 3);
    update(window, cx, |e, w, cx| e.down(&Down, w, cx));
    assert_eq!(state(window, cx), "    alpha beta ga|mma delta\nxy");
    update(window, cx, |e, w, cx| e.down(&Down, w, cx));
    assert_eq!(state(window, cx), "    alpha beta gamma delta\nxy|");
    update(window, cx, |e, w, cx| e.up(&Up, w, cx));
    assert_eq!(
        state(window, cx),
        "    alpha beta ga|mma delta\nxy",
        "the goal column survives the short line"
    );
    update(window, cx, |e, w, cx| e.up(&Up, w, cx));
    assert_eq!(state(window, cx), "    al|pha beta gamma delta\nxy");
    // End: the row's end first (drawn there), then the line's.
    update(window, cx, |e, w, cx| e.end(&End, w, cx));
    assert_eq!(state(window, cx), "    alpha beta |gamma delta\nxy");
    assert_eq!(cursor_point(window, cx), (0, 15));
    update(window, cx, |e, w, cx| e.end(&End, w, cx));
    assert_eq!(state(window, cx), "    alpha beta gamma delta|\nxy");
    // Home: the row's start first, then the line's indentation.
    update(window, cx, |e, w, cx| e.home(&Home, w, cx));
    assert_eq!(state(window, cx), "    alpha beta |gamma delta\nxy");
    assert_eq!(cursor_point(window, cx), (1, 4));
    update(window, cx, |e, w, cx| e.home(&Home, w, cx));
    assert_eq!(state(window, cx), "    |alpha beta gamma delta\nxy");
    // Toggling wrap off brings back one row per line.
    update(window, cx, |e, w, cx| {
        e.toggle_word_wrap(&ToggleWordWrap, w, cx)
    });
    cx.run_until_parked();
    assert_eq!(row_count(window, cx), 2);
}

/// Screen row and column of the primary cursor.
fn cursor_point(window: WindowHandle<Editor>, cx: &mut TestAppContext) -> (usize, usize) {
    window
        .read_with(cx, |e, _| {
            let s = e.primary();
            e.display.point(&e.buffer, s.head, s.at_row_end)
        })
        .unwrap()
}

#[gpui::test]
fn end_of_a_wrapped_row_stays_on_that_row(cx: &mut TestAppContext) {
    // Rows: "    alpha beta ", "    gamma delta", "xy".
    let window = editor("a.txt", "    al|pha beta gamma delta\nxy", cx);
    wrap_at(window, 16, cx);
    update(window, cx, |e, w, cx| e.end(&End, w, cx));
    assert_eq!(
        cursor_point(window, cx),
        (0, 15),
        "after the row's last char"
    );
    // Home from there goes to the first row's indentation.
    update(window, cx, |e, w, cx| e.home(&Home, w, cx));
    assert_eq!(state(window, cx), "    |alpha beta gamma delta\nxy");
    // Shift+End selects the whole row.
    update(window, cx, |e, w, cx| e.select_end(&SelectEnd, w, cx));
    assert_eq!(state(window, cx), "    «alpha beta »gamma delta\nxy");
    assert_eq!(cursor_point(window, cx), (0, 15));
    // Up/Down keep the row end's screen column.
    update(window, cx, |e, _, cx| e.set_cursor(6, cx));
    update(window, cx, |e, w, cx| e.end(&End, w, cx));
    update(window, cx, |e, w, cx| e.down(&Down, w, cx));
    assert_eq!(state(window, cx), "    alpha beta gamma delta|\nxy");
    update(window, cx, |e, w, cx| e.up(&Up, w, cx));
    assert_eq!(state(window, cx), "    alpha beta |gamma delta\nxy");
    assert_eq!(cursor_point(window, cx), (0, 15));
    // Typing inserts at the wrap position; the cursor follows the char.
    type_text(window, "X", cx);
    assert_eq!(state(window, cx), "    alpha beta X|gamma delta\nxy");
    assert_eq!(cursor_point(window, cx), (1, 5), "after the X");
}

#[gpui::test]
fn clicking_past_a_wrapped_row_end_stays_on_that_row(cx: &mut TestAppContext) {
    let window = editor("a.txt", "    al|pha beta gamma delta\nxy", cx);
    wrap_at(window, 16, cx);
    let click = |e: &mut Editor, row: usize, w: &mut Window, cx: &mut Context<Editor>| {
        let layout = e.layout.as_ref().expect("laid out");
        let y = layout.text_bounds.top() + layout.line_height * (row as f32 + 0.5);
        let position = point(layout.text_bounds.right() - px(4.), y);
        let event = MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        e.on_mouse_down(&event, w, cx);
        e.on_mouse_up(
            &MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
            },
            w,
            cx,
        );
    };
    update(window, cx, |e, w, cx| click(e, 0, w, cx));
    assert_eq!(state(window, cx), "    alpha beta |gamma delta\nxy");
    assert_eq!(cursor_point(window, cx), (0, 15), "drawn at the row's end");
    update(window, cx, |e, w, cx| click(e, 1, w, cx));
    assert_eq!(state(window, cx), "    alpha beta gamma delta|\nxy");
    assert_eq!(cursor_point(window, cx), (1, 15));
    // End on the clicked row end goes on to the line end.
    update(window, cx, |e, w, cx| click(e, 0, w, cx));
    update(window, cx, |e, w, cx| e.end(&End, w, cx));
    assert_eq!(state(window, cx), "    alpha beta gamma delta|\nxy");
}

#[gpui::test]
fn motion_steps_over_folds(cx: &mut TestAppContext) {
    let window = editor("a.py", "de|f f():\n    x = 1\n    y = 2\nz = 3", cx);
    update(window, cx, |e, w, cx| e.fold(&Fold, w, cx));
    assert_eq!(row_count(window, cx), 2);
    update(window, cx, |e, w, cx| e.down(&Down, w, cx));
    assert_eq!(state(window, cx), "def f():\n    x = 1\n    y = 2\nz |= 3");
    update(window, cx, |e, w, cx| e.up(&Up, w, cx));
    assert_eq!(state(window, cx), "de|f f():\n    x = 1\n    y = 2\nz = 3");
    update(window, cx, |e, w, cx| e.end(&End, w, cx));
    update(window, cx, |e, w, cx| e.right(&Right, w, cx));
    assert_eq!(state(window, cx), "def f():\n    x = 1\n    y = 2\n|z = 3");
    update(window, cx, |e, w, cx| e.left(&Left, w, cx));
    assert_eq!(state(window, cx), "def f():|\n    x = 1\n    y = 2\nz = 3");
    assert_eq!(row_count(window, cx), 2, "still folded");
    // A cursor placed inside (a jump, a search…) opens the fold.
    update(window, cx, |e, _, cx| e.set_cursor(12, cx));
    assert_eq!(row_count(window, cx), 4);
}

#[gpui::test]
fn fold_all_unfold_and_recursive_folding(cx: &mut TestAppContext) {
    let text =
        "|impl A {\n    fn f() {\n        g();\n    }\n    fn h() {\n        i();\n    }\n}\n";
    let window = editor("a.rs", text, cx);
    assert_eq!(row_count(window, cx), 9);
    update(window, cx, |e, w, cx| e.fold_all(&FoldAll, w, cx));
    assert_eq!(
        row_count(window, cx),
        3,
        "the impl header, its closer, the last line"
    );
    // Unfolding the outer fold leaves the inner ones folded.
    update(window, cx, |e, w, cx| e.unfold(&Unfold, w, cx));
    assert_eq!(row_count(window, cx), 7);
    update(window, cx, |e, w, cx| e.unfold_all(&UnfoldAll, w, cx));
    assert_eq!(row_count(window, cx), 9);
    update(window, cx, |e, w, cx| {
        e.fold_recursively(&FoldRecursively, w, cx)
    });
    let folds = window.read_with(cx, |e, _| e.folded_lines()).unwrap();
    assert_eq!(folds, [(0, 6), (1, 2), (4, 5)]);
    update(window, cx, |e, w, cx| {
        e.unfold_recursively(&UnfoldRecursively, w, cx)
    });
    assert_eq!(row_count(window, cx), 9);
    // Fold at a cursor inside picks the innermost region.
    update(window, cx, |e, _, cx| {
        e.set_cursor(text.find("g()").unwrap() - 1, cx)
    });
    update(window, cx, |e, w, cx| e.fold(&Fold, w, cx));
    let folds = window.read_with(cx, |e, _| e.folded_lines()).unwrap();
    assert_eq!(folds, [(1, 2)]);
    assert_eq!(
        state(window, cx),
        "impl A {\n    fn f() {|\n        g();\n    }\n    fn h() {\n        i();\n    }\n}\n",
        "the cursor left the folded text"
    );
}

#[gpui::test]
fn folds_follow_edits_and_open_when_edited_inside(cx: &mut TestAppContext) {
    let window = editor("a.py", "x = 0\nde|f f():\n    return 1\n", cx);
    update(window, cx, |e, w, cx| e.fold(&Fold, w, cx));
    update(window, cx, |e, w, cx| e.doc_start(&DocStart, w, cx));
    update(window, cx, |e, w, cx| e.newline(&Newline, w, cx));
    let folds = window.read_with(cx, |e, _| e.folded_lines()).unwrap();
    assert_eq!(folds, [(2, 3)], "moved down with the text");
    update(window, cx, |e, w, cx| e.undo(&Undo, w, cx));
    let folds = window.read_with(cx, |e, _| e.folded_lines()).unwrap();
    assert_eq!(folds, [(1, 2)]);
    // An edit of the hidden text (e.g. from another pane) opens the fold.
    update(window, cx, |e, _, cx| {
        let at = e.text().find('1').unwrap();
        e.commit(
            vec![(at..at + 1, "2".into())],
            |_| SelectionSet::single(Selection::cursor(0)),
            cx,
        );
    });
    let folds = window.read_with(cx, |e, _| e.folded_lines()).unwrap();
    assert!(folds.is_empty());
}

#[gpui::test]
fn find_opens_folds_around_matches(cx: &mut TestAppContext) {
    let window = editor("a.py", "de|f f():\n    secret = 1\nz = 2\n", cx);
    update(window, cx, |e, w, cx| e.fold(&Fold, w, cx));
    assert_eq!(row_count(window, cx), 3);
    assert_eq!(find(window, "secret", cx), 1);
    assert_eq!(row_count(window, cx), 4);
    assert_eq!(state(window, cx), "def f():\n    «secret» = 1\nz = 2\n");
}

#[gpui::test]
fn state_round_trips_folds(cx: &mut TestAppContext) {
    let window = editor("a.py", "de|f f():\n    x = 1\nz = 2\n", cx);
    update(window, cx, |e, w, cx| e.fold(&Fold, w, cx));
    let saved = window.read_with(cx, |e, _| e.state()).unwrap();
    assert_eq!(saved.folds, [(0, 1)]);
    let other = editor("a.py", "def f():\n    x = 1\nz = 2\n|", cx);
    update(other, cx, |e, _, cx| e.restore_state(&saved, cx));
    assert_eq!(row_count(other, cx), 3);
}
