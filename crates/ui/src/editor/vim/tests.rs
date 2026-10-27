//! Vim through its key handler on real editors: keys in, text, cursor
//! and mode out (`|` cursor, `«…»` selection, as in `editor::tests`).

use gpui::{TestAppContext, WindowHandle};

use super::{super::tests::editor, *};

/// Keys in Vim notation: characters, `<esc>`, `<cr>`, `<bs>`, `<tab>`,
/// `<up>`/`<down>`/`<left>`/`<right>`, `<c-x>`.
fn keys(notation: &str) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut rest = notation;
    while let Some(c) = rest.chars().next() {
        if c == '<'
            && let Some(end) = rest.find('>')
            && end > 1
        {
            let name = &rest[1..end];
            let key = match name {
                "esc" => Some(Key::Esc),
                "cr" => Some(Key::Enter),
                "bs" => Some(Key::Backspace),
                "tab" => Some(Key::Tab),
                "up" => Some(Key::Up),
                "down" => Some(Key::Down),
                "left" => Some(Key::Left),
                "right" => Some(Key::Right),
                "lt" => Some(Key::Char('<')),
                _ => name
                    .strip_prefix("c-")
                    .and_then(|c| c.chars().next())
                    .map(Key::Ctrl),
            };
            if let Some(key) = key {
                keys.push(key);
                rest = &rest[end + 1..];
                continue;
            }
        }
        keys.push(Key::Char(c));
        rest = &rest[c.len_utf8()..];
    }
    keys
}

fn vim_editor(text: &str, cx: &mut TestAppContext) -> WindowHandle<Editor> {
    cx.update(|cx| cx.set_global(VimModeOverride(true)));
    let window = editor("a.txt", text, cx);
    window
        .update(cx, |editor, _, cx| editor.sync_vim(cx))
        .unwrap();
    window
}

fn feed(window: WindowHandle<Editor>, notation: &str, cx: &mut TestAppContext) {
    for key in keys(notation) {
        window
            .update(cx, |editor, window, cx| editor.feed_key(key, window, cx))
            .unwrap();
    }
    cx.run_until_parked();
}

fn mode(window: WindowHandle<Editor>, cx: &mut TestAppContext) -> Mode {
    window.read_with(cx, |editor, _| editor.vim.mode).unwrap()
}

fn state(window: WindowHandle<Editor>, cx: &mut TestAppContext) -> String {
    window
        .read_with(cx, |editor, _| super::super::tests::marked(editor))
        .unwrap()
}

/// Runs `(text, keys, expected)` cases from normal mode, expecting normal
/// mode at the end.
fn check(cases: &[(&str, &str, &str)], cx: &mut TestAppContext) {
    for &(text, notation, expected) in cases {
        let window = vim_editor(text, cx);
        feed(window, notation, cx);
        assert_eq!(state(window, cx), expected, "{text:?} + {notation}");
        assert_eq!(mode(window, cx), Mode::Normal, "{text:?} + {notation}");
    }
}

#[gpui::test]
fn motions_with_counts(cx: &mut TestAppContext) {
    check(
        &[
            ("|foo bar baz", "w", "foo |bar baz"),
            ("|foo bar baz", "2w", "foo bar |baz"),
            ("|foo.bar baz", "w", "foo|.bar baz"),
            ("|foo.bar baz", "W", "foo.bar |baz"),
            ("foo bar |baz", "b", "foo |bar baz"),
            ("foo bar |baz", "3b", "|foo bar baz"),
            ("|foo bar", "e", "fo|o bar"),
            ("foo b|ar", "ge", "fo|o bar"),
            ("foo\n\n|bar", "b", "foo\n|\nbar"),
            ("|foo", "w", "fo|o"),
            ("  x = |1", "0", "|  x = 1"),
            ("|  x = 1", "^", "  |x = 1"),
            ("|abc\nde", "$", "ab|c\nde"),
            ("|abc\nde", "2$", "abc\nd|e"),
            ("a|b\nabcd\nx", "$j", "ab\nabc|d\nx"),
            ("ab|cd\nx\nabcd", "jj", "abcd\nx\nab|cd"),
            ("abc\n|d\nabc", "kl", "a|bc\nd\nabc"),
            ("a\nb\n|c", "gg", "|a\nb\nc"),
            ("|a\nb\n  c", "G", "a\nb\n  |c"),
            ("|a\nb\nc", "2G", "a\n|b\nc"),
            ("|a,b,c", "f,", "a|,b,c"),
            ("|a,b,c", "2f,", "a,b|,c"),
            ("|ab,c", "t,", "a|b,c"),
            ("a,b,|c", "F,", "a,b|,c"),
            ("a,b,|c", "T,", "a,b,|c"),
            ("|a,b,c", "f,;", "a,b|,c"),
            ("|a,b,c", "f,;,", "a|,b,c"),
            ("|x(a[1])", "%", "x(a[1]|)"),
            ("x(a[1]|)", "%", "x|(a[1])"),
            ("|a\nb\n\nc", "}", "a\nb\n|\nc"),
            ("a\n\nb\n|c", "{", "a\n|\nb\nc"),
            ("a\n|b", "<bs>", "|a\nb"),
            ("|ab", "<right>", "a|b"),
            ("|a\n  b", "<cr>", "a\n  |b"),
            ("a\n|b", "-", "|a\nb"),
            ("|abcdef", "4|", "abc|def"),
            ("|ab  \nc", "g_", "a|b  \nc"),
        ],
        cx,
    );
}

#[gpui::test]
fn searches_marks_and_jumps(cx: &mut TestAppContext) {
    check(
        &[
            ("|one two one", "/one<cr>", "one two |one"),
            ("|one two one", "/one<cr>n", "|one two one"),
            ("one two |one", "?one<cr>", "|one two one"),
            ("|one two one", "/one<cr>N", "|one two one"),
            ("|Foo foo", "/foo<cr>", "Foo |foo"),
            ("|foo Foo", "/Foo<cr>", "foo |Foo"),
            ("|foo x foo", "*", "foo x |foo"),
            ("|foo foobar foo", "*", "foo foobar |foo"),
            ("foo x |foo", "#", "|foo x foo"),
            ("|a b a", "/a<cr>/<cr>", "|a b a"),
            ("|a\nb\nc", "majj'a", "|a\nb\nc"),
            ("a|b\nc", "mbj`b", "a|b\nc"),
            ("|a\nb\nc", "G''", "|a\nb\nc"),
        ],
        cx,
    );
}

#[gpui::test]
fn operators_with_motions_objects_and_counts(cx: &mut TestAppContext) {
    check(
        &[
            ("foo b|ar baz", "diw", "foo | baz"),
            ("foo b|ar baz", "daw", "foo |baz"),
            ("foo b|ar", "daw", "fo|o"),
            ("|one two three four", "d2w", "|three four"),
            ("|one two three four", "2dw", "|three four"),
            ("|one two three four", "2d2w", "|"),
            ("foo |bar\nbaz", "dw", "foo| \nbaz"),
            ("|abc,def", "dt,", "|,def"),
            ("|abc,def", "df,", "|def"),
            ("ab|c", "d0", "|c"),
            ("a|bc", "d$", "|a"),
            ("|one two three", "d/thr<cr>", "|three"),
            ("f(a, |b)", "di(", "f(|)"),
            ("f(a, |b)", "dib", "f(|)"),
            ("f(a, |b) x", "da(", "f| x"),
            ("a [x, [|y]]", "d2i[", "a [|]"),
            ("x {|a}", "diB", "x {|}"),
            ("x = \"he|llo\" + 1", "di\"", "x = \"|\" + 1"),
            ("x = \"he|llo\" + 1", "da\"", "x = |+ 1"),
            ("x = 'a|b'", "di'", "x = '|'"),
            ("x = `a|b`", "di`", "x = `|`"),
            ("<a><b>t|x</b></a>", "dit", "<a><b>|</b></a>"),
            ("<a><b>t|x</b></a>", "d2it", "<a>|</a>"),
            ("<a><b>t|x</b></a>", "dat", "<a>|</a>"),
            ("a <|x> b", "di<lt>", "a <|> b"),
            ("a\n|b\nc\n\nd", "dap", "|d"),
            ("a\n|b\nc\n\nd", "dip", "|\nd"),
            ("{\n    |x;\n    y;\n}", "di{", "{\n|}"),
            ("a\n|b\nc\nd", "2dd", "a\n|d"),
            ("a\n|b", "dd", "|a"),
            ("|a\nb\nc", "dj", "|c"),
            ("a\nb\n|c", "dk", "|a"),
            ("|a\nb\n\nc", "d}", "|\nc"),
            ("a\n  |b\nc", "dG", "|a"),
            ("|a\nb\nc", "dgg", "|b\nc"),
            ("a\n|b\nc", "dgg", "|c"),
            ("|foo Bar", "gUiw", "|FOO Bar"),
            ("|Foo", "g~~", "|fOO"),
            ("|FOO bar", "guu", "|foo bar"),
            ("|FOO BAR", "gu2w", "|foo bar"),
            ("|a\nb", ">j", "    |a\n    b"),
            ("    |a", "<lt><lt>", "|a"),
            ("|a\nb\nc", "2>>", "    |a\n    b\nc"),
        ],
        cx,
    );
}

#[gpui::test]
fn changes_enter_insert_and_escape_steps_back(cx: &mut TestAppContext) {
    check(
        &[
            ("foo b|ar baz", "ciwxy<esc>", "foo x|y baz"),
            ("|foo bar", "cwx<esc>", "|x bar"),
            ("fo|o bar", "cwx<esc>", "fo|x bar"),
            ("|one two three", "c2wx<esc>", "|x three"),
            ("  |foo\nbar", "ccx<esc>", "  |x\nbar"),
            ("x = \"he|llo\"", "ci\"bye<esc>", "x = \"by|e\""),
            ("{\n    |x;\n}", "ci{y<esc>", "{\n    |y\n}"),
            ("a|bcd", "Cx<esc>", "a|x"),
            ("a|bcd", "D", "|a"),
            ("|abc", "sx<esc>", "|xbc"),
            ("|abc", "2sx<esc>", "|xc"),
            ("  a|bc\nd", "Sx<esc>", "  |x\nd"),
            ("|abc", "ix<esc>", "|xabc"),
            ("|abc", "ax<esc>", "a|xbc"),
            ("a|bc", "Ix<esc>", "|xabc"),
            ("  a|bc", "Ix<esc>", "  |xabc"),
            ("  a|bc", "gIx<esc>", "|x  abc"),
            ("a|bc", "Ax<esc>", "abc|x"),
            ("  |a", "ob<esc>", "  a\n  |b"),
            ("  |a", "Ob<esc>", "  |b\n  a"),
            ("|a", "3ix<esc>", "xx|xa"),
            ("|x", "2oy<esc>", "x\ny\n|y"),
            ("a|b", "ix<esc>jgiy<esc>", "ax|yb"),
            ("|abc", "Rxy<esc>", "x|yc"),
            ("|abc", "Rxy<bs><esc>", "|xbc"),
            ("|ab", "Rxyz<esc>", "xy|z"),
        ],
        cx,
    );
}

#[gpui::test]
fn simple_commands(cx: &mut TestAppContext) {
    check(
        &[
            ("|abcd", "x", "|bcd"),
            ("|abcd", "3x", "|d"),
            ("ab|c", "x", "a|b"),
            ("ab|cd", "X", "a|cd"),
            ("|a\n  b\nc", "J", "a| b\nc"),
            ("|a\nb\nc", "3J", "a b| c"),
            ("|a\n  b", "gJ", "a|  b"),
            ("|a \nb", "J", "a |b"),
            ("|f(\n)", "J", "f(|)"),
            ("|abc", "2rx", "x|xc"),
            ("|abc", "4rx", "|abc"),
            ("|ab", "r<cr>", "\n|b"),
            ("|abc", "2~", "AB|c"),
            ("x = |9;", "<c-a>", "x = 1|0;"),
            ("|x 5", "<c-x>", "x |4"),
            ("|-1", "3<c-x>", "-|4"),
            ("|x 0x0f", "<c-a>", "x 0x1|0"),
        ],
        cx,
    );
}

#[gpui::test]
fn registers_and_put(cx: &mut TestAppContext) {
    check(
        &[
            ("|abc", "ylp", "a|abc"),
            ("|abc", "ylP", "|aabc"),
            ("|abc", "yl3p", "aaa|abc"),
            ("|a\nb", "yyp", "a\n|a\nb"),
            ("|a\nb", "yyjp", "a\nb\n|a"),
            ("a\n|b", "yyP", "a\n|b\nb"),
            ("|a\n  b", "jyykP", "  |b\na\n  b"),
            ("|one two", "dwwp", "twoone| "),
            ("|a b", "\"ayiwwdiw\"aP", "a|a "),
            ("|a b", "\"ayiw\"Ayiw\"ap", "aa|a b"),
            ("|a\nb", "yyjdd\"0p", "a\n|a"),
            ("|a\nb\nc", "dddd\"2p", "c\n|a"),
            ("|x y", "dl\"_dl\"-p", "y|x"),
            ("|ab\ncd", "<c-v>jly$p", "ab|ab\ncdcd"),
        ],
        cx,
    );
}

#[gpui::test]
fn system_clipboard_register(cx: &mut TestAppContext) {
    let window = vim_editor("|foo bar", cx);
    feed(window, "\"+yiw", cx);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("foo".into())
    );
    cx.write_to_clipboard(ClipboardItem::new_string("line\n".into()));
    feed(window, "\"+p", cx);
    assert_eq!(state(window, cx), "foo bar\n|line");
}

#[gpui::test]
fn dot_repeats_changes_with_their_text(cx: &mut TestAppContext) {
    check(
        &[
            ("|a b c", "dw.", "|c"),
            ("|a b c d", "dw2.", "|d"),
            ("|foo foo", "ciwbar<esc>w.", "bar ba|r"),
            ("|x\ny", "Ahi<esc>j.", "xhi\nyh|i"),
            ("|a\nb", "ix<esc>j.", "xa\n|xb"),
            ("|abcd", "x.", "|cd"),
            ("|a\nb\nc\nd", "dd.", "|c\nd"),
            ("|aa\nbb", "~j0.", "Aa\nB|b"),
            ("|ab cd", "Rxy<esc>w.", "xy x|y"),
            ("|a\nb", "oz<esc>j.", "a\nz\nb\n|z"),
            ("|abcd\nefgh", "vld.", "|\nefgh"),
            ("|a\nb\nc", "Vd.", "|c"),
            ("x = |1, 2", "<c-a>w.", "x = 2, |3"),
            ("|a.b.c", "f.r-;.", "a-b|-c"),
        ],
        cx,
    );
}

#[gpui::test]
fn undo_groups_a_command_and_its_insert(cx: &mut TestAppContext) {
    check(
        &[
            ("|foo bar", "ciwxyz<esc>u", "|foo bar"),
            ("|foo bar", "ciwxyz<esc>u<c-r>", "xyz| bar"),
            ("|a", "ox<cr>y<esc>u", "|a"),
            ("|a b c", "dwdwuu", "|a b c"),
            ("|a b c", "dwdwu", "|b c"),
            ("|a", "ix<esc>ay<esc>u", "x|a"),
            ("|aa\naa", ":%s/a/b/g<cr>u", "|aa\naa"),
            ("|abc", "3xu", "|abc"),
            ("|x", "3ia<esc>u", "|x"),
        ],
        cx,
    );
}

#[gpui::test]
fn visual_modes(cx: &mut TestAppContext) {
    let visual = [
        ("|abc", "vl", "«ab»c", VisualKind::Char),
        ("a|bc", "vh", "«ab»c", VisualKind::Char),
        ("foo b|ar baz", "vaw", "foo «bar »baz", VisualKind::Char),
        ("foo b|ar baz", "viw", "foo «bar» baz", VisualKind::Char),
        ("|abc", "vl<esc>gv", "«ab»c", VisualKind::Char),
        ("|abc", "v$", "«abc»", VisualKind::Char),
        ("a\n|b\nc", "Vj", "a\n«b\nc»", VisualKind::Line),
        ("a\n|b\nc", "vV", "a\n«b»\nc", VisualKind::Line),
        ("|abc", "vlo", "«ab»c", VisualKind::Char),
        ("|ab\ncd", "<c-v>jl", "«ab»\n«cd»", VisualKind::Block),
    ];
    for (text, notation, expected, kind) in visual {
        let window = vim_editor(text, cx);
        feed(window, notation, cx);
        assert_eq!(state(window, cx), expected, "{text:?} + {notation}");
        assert_eq!(
            mode(window, cx),
            Mode::Visual(kind),
            "{text:?} + {notation}"
        );
    }
    check(
        &[
            ("|abc def", "vlld", "| def"),
            ("|abc def", "vecx<esc>", "|x def"),
            ("a\n|b\nc", "Vjd", "|a"),
            ("a\n|b\nc", "Vy", "a\n|b\nc"),
            ("|ab\ncd\nef", "<c-v>jjld", "|\n\n"),
            ("|ab\ncd", "<c-v>jIx<esc>", "|xab\nxcd"),
            ("|ab\ncd", "<c-v>jAx<esc>", "a|xb\ncxd"),
            ("|ab\ncd", "<c-v>j$Ax<esc>", "ab|x\ncdx"),
            ("|ab\ncd", "<c-v>jcx<esc>", "|xb\nxd"),
            ("|abc xyz", "yiwwviwp", "abc ab|c"),
            ("|ab", "vlrx", "|xx"),
            ("|ab", "vU", "|Ab"),
            ("|aB", "v$~", "|Ab"),
            ("|a\nb", "VJ", "a| b"),
            ("|a\nb", "Vj>", "    |a\n    b"),
            ("|a b", "vx", "| b"),
            ("|a\nb", "vX", "|b"),
            ("|abc", "vl<esc>", "a|bc"),
            ("|abc", "vlu", "|abc"),
            ("|abc", "vlyp", "aa|bbc"),
        ],
        cx,
    );
}

#[gpui::test]
fn ex_commands_and_history(cx: &mut TestAppContext) {
    check(
        &[
            ("|aa\naa", ":s/a/b/<cr>", "|ba\naa"),
            ("|aa\naa", ":%s/a/b/g<cr>", "bb\n|bb"),
            ("|aa\naa\naa", ":2,3s/a/b/<cr>", "aa\nba\n|ba"),
            ("|a1 b22", ":s/\\d+/<&>/g<cr>", "|a<1> b<22>"),
            ("|ab", ":s/(a)(b)/\\2\\1/<cr>", "|ba"),
            ("|a\na\na", "Vj:s/a/b/<cr>", "b\n|b\na"),
            ("|a\nb\nc", ":3<cr>", "a\nb\n|c"),
            ("|a\nb\nc", ":$<cr>", "a\nb\n|c"),
            ("a\nb\n|c", ":.-1<cr>", "a\n|b\nc"),
            ("|a\nb\nc", ":2d<cr>", "a\n|c"),
            ("|a\nb\nc", ":1,2j<cr>", "a| b\nc"),
            ("|aa\naa", ":%s/a/b/<cr>u:<up><cr>", "ba\n|ba"),
            ("|xy", "/y<cr>:s//z/<cr>", "|xz"),
            ("|ab", ":s/a<esc>", "|ab"),
            ("|ab", ":x<bs><bs>", "|ab"),
        ],
        cx,
    );
    let window = vim_editor("|a", cx);
    feed(window, ":nonsense<cr>", cx);
    let notice = window.read_with(cx, |e, _| e.notice.clone()).unwrap();
    assert_eq!(
        notice.as_deref().map(|s| &**s),
        Some("E492: Not an editor command: nonsense")
    );
    feed(window, ":s/zz/y/<cr>", cx);
    let notice = window.read_with(cx, |e, _| e.notice.clone()).unwrap();
    assert_eq!(
        notice.as_deref().map(|s| &**s),
        Some("E486: Pattern not found: zz")
    );
    // The command line shows in the status bar while typed.
    feed(window, ":%s/a", cx);
    let line = window.read_with(cx, |e, _| e.vim_command_line()).unwrap();
    assert_eq!(line.as_deref().map(|s| &**s), Some(":%s/a▏"));
    feed(window, "<esc>", cx);
    assert_eq!(
        window.read_with(cx, |e, _| e.vim_command_line()).unwrap(),
        None
    );
}

#[gpui::test]
fn search_highlights_until_noh(cx: &mut TestAppContext) {
    let window = vim_editor("|ab ab ab", cx);
    feed(window, "/ab<cr>", cx);
    let highlights = window
        .update(cx, |e, _, cx| {
            e.update_vim_highlights(cx);
            e.vim_highlights().to_vec()
        })
        .unwrap();
    assert_eq!(highlights, [0..2, 3..5, 6..8]);
    feed(window, ":noh<cr>", cx);
    let highlights = window
        .update(cx, |e, _, cx| {
            e.update_vim_highlights(cx);
            e.vim_highlights().to_vec()
        })
        .unwrap();
    assert!(highlights.is_empty());
}

#[gpui::test]
fn macros_record_and_replay(cx: &mut TestAppContext) {
    check(
        &[
            ("|1\n2\n3", "qaA!<esc>jq2@a", "1!\n2!\n3|!"),
            ("|a b c d", "qqdwq@q@@", "|d"),
        ],
        cx,
    );
}

#[gpui::test]
fn escape_and_ide_keys_pass_through_when_vim_has_no_use_for_them(cx: &mut TestAppContext) {
    let window = vim_editor("|abc", cx);
    let consumed = |key: Key, cx: &mut TestAppContext| {
        window.update(cx, |e, w, cx| e.vim_key(key, w, cx)).unwrap()
    };
    assert!(!consumed(Key::Esc, cx), "plain Escape goes to the IDE");
    assert!(!consumed(Key::Ctrl('s'), cx), "Ctrl+S saves");
    assert!(!consumed(Key::Ctrl('p'), cx), "Ctrl+P is Go to File");
    assert!(consumed(Key::Char('q'), cx), "an unknown key is swallowed");
    assert!(consumed(Key::Esc, cx), "Escape cancels pending keys");
    feed(window, "i", cx);
    assert!(!consumed(Key::Char('x'), cx), "typing goes to the editor");
    assert!(
        !consumed(Key::Ctrl('d'), cx),
        "insert-mode Ctrl keys stay the IDE's"
    );
    assert!(consumed(Key::Esc, cx));
    assert_eq!(mode(window, cx), Mode::Normal);
}

#[gpui::test]
fn several_cursors_insert_together_and_collapse_on_escape(cx: &mut TestAppContext) {
    let window = vim_editor("|a\n|b", cx);
    feed(window, "ix<esc>", cx);
    assert_eq!(state(window, cx), "xa\n|xb");
    // A selection made outside Vim (Ctrl+D, the mouse) is visual mode.
    let window = vim_editor("|a a", cx);
    window
        .update(cx, |e, w, cx| {
            e.select_next_occurrence(&SelectNextOccurrence, w, cx);
            e.select_next_occurrence(&SelectNextOccurrence, w, cx);
            e.sync_vim(cx);
        })
        .unwrap();
    assert_eq!(mode(window, cx), Mode::Visual(VisualKind::Char));
    feed(window, "cb<esc>", cx);
    assert_eq!(state(window, cx), "b |b");
}

#[gpui::test]
fn scrolling_motions_follow_the_view(cx: &mut TestAppContext) {
    let text: String = (0..200).map(|i| format!("line {i}\n")).collect();
    let window = vim_editor(&format!("|{text}"), cx);
    let page = window.read_with(cx, |e, _| e.page_rows()).unwrap() as usize;
    feed(window, "<c-d>", cx);
    let line = |cx: &mut TestAppContext| {
        window
            .read_with(cx, |e, _| e.buffer.line_of(e.primary().head))
            .unwrap()
    };
    assert_eq!(line(cx), (page / 2).max(1));
    feed(window, "<c-u>", cx);
    assert_eq!(line(cx), 0);
    feed(window, "L", cx);
    let bottom = line(cx);
    assert!(
        bottom + 1 >= page.saturating_sub(1) && bottom < page + 1,
        "L at {bottom}, page {page}"
    );
    feed(window, "H", cx);
    assert_eq!(line(cx), 0);
}

#[gpui::test]
fn keystrokes_reach_vim_before_key_bindings(cx: &mut TestAppContext) {
    cx.update(crate::editor::bind_keys);
    let window = vim_editor("|foo foo", cx);
    window
        .update(cx, |e, w, _| w.focus(&e.focus_handle))
        .unwrap();
    cx.run_until_parked();
    let any = window.into();
    // `x` deletes instead of typing; Ctrl+D (Select Next Occurrence) is
    // half a page down for Vim; `i` then text types.
    cx.simulate_keystrokes(any, "x ctrl-d d i w");
    assert_eq!(state(window, cx), "| foo");
    cx.simulate_keystrokes(any, "i");
    cx.simulate_input(any, "hi");
    cx.simulate_keystrokes(any, "escape");
    assert_eq!(state(window, cx), "h|i foo");
    assert_eq!(mode(window, cx), Mode::Normal);
}

#[gpui::test]
fn vim_stays_out_of_inputs_and_follows_the_toggle(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(VimModeOverride(true)));
    let input = cx.add_window(|_, cx| Editor::single_line(cx));
    input.update(cx, |e, _, cx| e.sync_vim(cx)).unwrap();
    assert_eq!(input.read_with(cx, |e, _| e.vim_mode()).unwrap(), None);
    let area = cx.add_window(|_, cx| Editor::text_area(cx));
    area.update(cx, |e, _, cx| e.sync_vim(cx)).unwrap();
    assert_eq!(area.read_with(cx, |e, _| e.vim_mode()).unwrap(), None);

    let window = vim_editor("|ab", cx);
    assert_eq!(
        window.read_with(cx, |e, _| e.vim_mode()).unwrap(),
        Some(Mode::Normal)
    );
    window
        .update(cx, |e, w, cx| {
            e.toggle_vim_mode(&ToggleVimMode, w, cx);
            e.sync_vim(cx);
        })
        .unwrap();
    assert_eq!(window.read_with(cx, |e, _| e.vim_mode()).unwrap(), None);
}

/// On 100k lines a Vim key costs about what the editor's own commands
/// cost (each key redraws; the frame dominates in debug builds), and
/// search highlights cover the lines on screen only.
#[gpui::test]
fn large_files_stay_cheap(cx: &mut TestAppContext) {
    use std::time::{Duration, Instant};

    let text: String = (0..100_000)
        .map(|i| format!("let value_{i} = {i};\n"))
        .collect();
    let window = vim_editor(&format!("|{text}"), cx);
    // The same work through the editor's own commands: moves and an edit.
    let started = Instant::now();
    for _ in 0..3 {
        window.update(cx, |e, w, cx| e.down(&Down, w, cx)).unwrap();
        cx.run_until_parked();
    }
    window
        .update(cx, |e, w, cx| e.doc_end(&DocEnd, w, cx))
        .unwrap();
    window
        .update(cx, |e, w, cx| e.doc_start(&DocStart, w, cx))
        .unwrap();
    window
        .update(cx, |e, w, cx| e.delete_line(&DeleteLine, w, cx))
        .unwrap();
    window.update(cx, |e, w, cx| e.undo(&Undo, w, cx)).unwrap();
    cx.run_until_parked();
    let per_command = started.elapsed() / 7;

    let steps = [
        "G",
        "gg",
        "50%",
        "/value_99999<cr>",
        "dd",
        "u",
        "}",
        "j",
        "x",
        "u",
    ];
    let key_count: usize = steps.iter().map(|step| keys(step).len()).sum();
    let started = Instant::now();
    for step in steps {
        feed(window, step, cx);
    }
    let elapsed = started.elapsed();
    let line = window
        .read_with(cx, |e, _| e.buffer.line_of(e.primary().head))
        .unwrap();
    assert_eq!(line, 100_000);
    assert_eq!(
        window.read_with(cx, |e, _| e.buffer.line_count()).unwrap(),
        100_001
    );
    let highlights = window
        .update(cx, |e, _, cx| {
            e.update_vim_highlights(cx);
            e.vim_highlights().len()
        })
        .unwrap();
    assert!(highlights <= 1, "{highlights}");
    let budget = per_command * 3 * key_count as u32 + Duration::from_millis(500);
    assert!(
        elapsed < budget,
        "{key_count} keys took {elapsed:?}; editor commands {per_command:?} each"
    );
}
