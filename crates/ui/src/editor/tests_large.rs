//! Large files and long lines: reduced features over the size limits, and
//! rows laid out only in and around the view.

use gpui::TestAppContext;

use super::{
    tests::{editor, update},
    *,
};

#[gpui::test]
fn files_over_the_limits_open_with_reduced_features(cx: &mut TestAppContext) {
    let small = editor("a.rs", "|fn main() {}\n", cx);
    small
        .read_with(cx, |e, _| {
            assert!(!e.large_file);
            assert!(e.syntax.is_some(), "{:?}", syntax::problems());
        })
        .unwrap();
    let long_line = format!("|fn main() {{}}\n// {}\n", "x".repeat(LARGE_LINE_BYTES + 1));
    let big = editor("b.rs", &long_line, cx);
    big.read_with(cx, |e, _| {
        assert!(e.large_file);
        assert!(e.syntax.is_none(), "no parse of a large file");
        assert_eq!(e.language(), None);
    })
    .unwrap();
    // Both limits come from the buffer alone.
    assert!(!is_large_file(&Buffer::new("short")));
    assert!(is_large_file(&Buffer::new(
        &"y".repeat(LARGE_LINE_BYTES + 1)
    )));
    let many_short = "z\n".repeat(LARGE_LINE_BYTES);
    assert!(!is_large_file(&Buffer::new(&many_short)));
}

#[gpui::test]
fn rows_longer_than_the_view_are_laid_out_around_it_only(cx: &mut TestAppContext) {
    let length = 4000;
    let line = "a".repeat(length);
    let window = editor("a.txt", &format!("|{line}\nshort"), cx);
    let em = window
        .read_with(cx, |e, _| {
            let layout = e.layout.as_ref().expect("laid out");
            let row = &layout.rows[0];
            assert_eq!(row.clip.start, 0);
            assert!(row.clip.end < length, "cut after the view: {:?}", row.clip);
            assert_eq!(row.clip_x, px(0.));
            assert_eq!(row.shaped.len(), row.clip.len());
            let short = &layout.rows[1];
            assert_eq!(short.clip, short.row.range, "rows that fit are whole");
            // Offsets past the cut are placed a column per byte.
            assert_eq!(
                row.x_for(length),
                row.shaped.width + layout.em_width * (length - row.clip.end) as f32
            );
            layout.em_width
        })
        .unwrap();

    // Scrolled into the line: the part before the view is skipped too.
    update(window, cx, |e, _, cx| {
        e.scroll.x = em * 2000.;
        cx.notify();
    });
    cx.run_until_parked();
    window
        .read_with(cx, |e, _| {
            let layout = e.layout.as_ref().unwrap();
            let row = &layout.rows[0];
            assert_eq!(row.clip.start, 2000 - CLIP_MARGIN_COLUMNS);
            assert!(
                row.clip.end > 2000 && row.clip.end < length,
                "{:?}",
                row.clip
            );
            assert_eq!(row.clip_x, em * row.clip.start as f32);
            assert_eq!(row.x_for(row.clip.start), px(0.));
            assert_eq!(row.x_for(0), -em * row.clip.start as f32);
            assert_eq!(row.offset_closest_to(px(-5000.)), row.clip.start);
            assert_eq!(row.offset_closest_to(px(1e6)), row.clip.end);
            // The cursor drawn at a point is the one a click there gives.
            let offset = 2010;
            let x = layout.text_left(row, e.scroll.x) + row.x_for(offset);
            let y = layout.row_top(0, e.scroll.y) + layout.line_height / 2.;
            assert_eq!(e.cursor_for_position(point(x, y)).0, offset);
        })
        .unwrap();

    // End: the cursor lands past the laid-out part; the view follows it
    // and the next frame lays that part out.
    update(window, cx, |e, w, cx| e.end(&End, w, cx));
    cx.run_until_parked();
    window
        .read_with(cx, |e, _| {
            assert_eq!(e.primary().head, length);
            let layout = e.layout.as_ref().unwrap();
            let row = &layout.rows[0];
            assert_eq!(row.clip.end, length, "{:?}", row.clip);
            assert!(row.clip.start > 0);
            let x = layout.text_left(row, e.scroll.x) + row.x_for(length);
            assert!(
                x >= layout.text_bounds.left() && x <= layout.text_bounds.right(),
                "cursor at {x:?} in {:?}",
                layout.text_bounds
            );
        })
        .unwrap();
}

#[test]
fn row_clips_snap_to_chars_and_count_columns() {
    let buffer = Buffer::new("ab漢字cd\nxy");
    let row = DisplayRow {
        line: 0,
        range: 0..buffer.line_end(0),
        indent: 0,
        first: true,
        last: true,
    };
    let clip = |window: Range<usize>| {
        let clip = row_clip(&buffer, &row, Some(&window));
        (clip.range, clip.columns_before)
    };
    assert_eq!(clip(0..100), (0..10, 0));
    // The wide chars take two columns each and are never split.
    assert_eq!(
        clip(0..3),
        (0..5, 0),
        "cut after the char covering column 3"
    );
    assert_eq!(clip(3..5), (5..8, 4));
    assert_eq!(clip(6..7), (8..9, 6));
    assert_eq!(clip(20..30), (10..10, 8), "a row ending before the window");
    let short = DisplayRow {
        line: 1,
        range: 11..13,
        indent: 0,
        first: true,
        last: true,
    };
    let whole = row_clip(&buffer, &short, Some(&(0..2)));
    assert_eq!((whole.range, whole.columns_before), (11..13, 0));
    let unclipped = row_clip(&buffer, &row, None);
    assert_eq!(unclipped.range, 0..10);
}
