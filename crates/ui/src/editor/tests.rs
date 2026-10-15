//! Editing behavior through the editor's commands, on real buffers.

use std::path::PathBuf;

use gpui::{TestAppContext, WindowHandle};

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
