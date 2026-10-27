use super::*;
use futures::StreamExt;
use futures::executor::block_on;
use std::fs;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("next_code-search-{}-{name}", std::process::id()));
        fs::remove_dir_all(&path).ok();
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn write(&self, rel: &str, contents: impl AsRef<[u8]>) {
        let path = self.0.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write file");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn query(pattern: &str) -> SearchQuery {
    SearchQuery {
        pattern: pattern.into(),
        case_sensitive: true,
        ..Default::default()
    }
}

struct Results {
    files: Vec<FileMatches>,
    files_searched: usize,
    matches: usize,
    limit_hit: bool,
}

impl Results {
    fn rel_paths(&self, root: &Path) -> Vec<String> {
        let mut paths: Vec<String> = self
            .files
            .iter()
            .map(|file| {
                file.path
                    .strip_prefix(root)
                    .expect("absolute path under root")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        paths.sort();
        paths
    }

    fn file(&self, name: &str) -> &FileMatches {
        self.files
            .iter()
            .find(|file| file.path.ends_with(name))
            .unwrap_or_else(|| panic!("no matches for {name}"))
    }
}

fn collect(events: Vec<SearchEvent>) -> Results {
    let (last, files) = events.split_last().expect("at least a Done event");
    let SearchEvent::Done {
        files_searched,
        matches,
        limit_hit,
    } = *last
    else {
        panic!("last event must be Done, got {last:?}");
    };
    let files = files
        .iter()
        .map(|event| match event {
            SearchEvent::File(file) => file.clone(),
            SearchEvent::Done { .. } => panic!("Done sent more than once"),
        })
        .collect();
    Results {
        files,
        files_searched,
        matches,
        limit_hit,
    }
}

fn run(root: &Path, query: &SearchQuery, limits: SearchLimits) -> Results {
    let (_handle, rx) = search(root, query, limits).expect("valid query");
    collect(block_on(rx.collect()))
}

fn run_default(root: &Path, query: &SearchQuery) -> Results {
    run(root, query, SearchLimits::default())
}

#[test]
fn literal_pattern_escapes_regex_metacharacters() {
    let dir = TempDir::new("literal");
    dir.write("a.txt", "a.b(\naxb(\n");

    let literal = run_default(&dir.0, &query("a.b("));
    let lines = &literal.file("a.txt").lines;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].line, 0);
    assert_eq!(lines[0].ranges, vec![0..4]);

    let regex = run_default(
        &dir.0,
        &SearchQuery {
            regex: true,
            ..query(r"a.b\(")
        },
    );
    let lines: Vec<usize> = regex.file("a.txt").lines.iter().map(|l| l.line).collect();
    assert_eq!(lines, vec![0, 1]);
    assert_eq!(regex.matches, 2);
}

#[test]
fn case_sensitivity_and_whole_word() {
    let dir = TempDir::new("case_word");
    dir.write("a.txt", "Foo foo foobar\n");
    let ranges = |query: SearchQuery| {
        let results = run_default(&dir.0, &query);
        results.file("a.txt").lines[0].ranges.clone()
    };

    assert_eq!(ranges(query("foo")), vec![4..7, 8..11]);
    assert_eq!(
        ranges(SearchQuery {
            case_sensitive: false,
            ..query("foo")
        }),
        vec![0..3, 4..7, 8..11]
    );
    assert_eq!(
        ranges(SearchQuery {
            whole_word: true,
            ..query("foo")
        }),
        vec![4..7]
    );
    assert_eq!(
        ranges(SearchQuery {
            whole_word: true,
            case_sensitive: false,
            ..query("foo")
        }),
        vec![0..3, 4..7]
    );
}

#[test]
fn honors_gitignore_and_globs() {
    let dir = TempDir::new("globs");
    dir.write(".gitignore", "ignored/\n*.log\n");
    dir.write("src/a.py", "needle\n");
    dir.write("src/b.rs", "needle\n");
    dir.write("src/deep/c.py", "needle\n");
    dir.write("ignored/d.py", "needle\n");
    dir.write("x.log", "needle\n");
    dir.write(".hidden.py", "needle\n");
    dir.write(".git/config", "needle\n");

    let all = run_default(&dir.0, &query("needle"));
    assert_eq!(
        all.rel_paths(&dir.0),
        vec![".hidden.py", "src/a.py", "src/b.rs", "src/deep/c.py"]
    );
    assert!(all.files.iter().all(|file| file.path.is_absolute()));

    let py = run_default(
        &dir.0,
        &SearchQuery {
            include: vec!["*.py".into()],
            ..query("needle")
        },
    );
    assert_eq!(
        py.rel_paths(&dir.0),
        vec![".hidden.py", "src/a.py", "src/deep/c.py"]
    );

    let src_no_rs = run_default(
        &dir.0,
        &SearchQuery {
            include: vec!["src/**".into()],
            exclude: vec!["*.rs".into(), "deep/".into()],
            ..query("needle")
        },
    );
    assert_eq!(src_no_rs.rel_paths(&dir.0), vec!["src/a.py"]);
    assert_eq!(src_no_rs.files_searched, 1);
}

#[test]
fn skips_binary_files() {
    let dir = TempDir::new("binary");
    dir.write("bin.dat", b"needle\n\x00\x01\x02needle\n");
    dir.write("text.txt", "needle\n");

    let results = run_default(&dir.0, &query("needle"));
    assert_eq!(results.rel_paths(&dir.0), vec!["text.txt"]);
    assert_eq!(results.matches, 1);
}

#[test]
fn crlf_lines_exclude_terminator() {
    let dir = TempDir::new("crlf");
    dir.write("a.txt", "first needle\r\nsecond\r\nneedle third\r\n");

    let results = run_default(
        &dir.0,
        &SearchQuery {
            regex: true,
            ..query("needle$|^needle")
        },
    );
    let lines = &results.file("a.txt").lines;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].line, 0);
    assert_eq!(lines[0].text, "first needle");
    assert_eq!(lines[0].ranges, vec![6..12]);
    assert_eq!(lines[0].column, 6);
    assert_eq!(lines[1].line, 2);
    assert_eq!(lines[1].text, "needle third");
    assert_eq!(lines[1].ranges, vec![0..6]);
}

#[test]
fn long_lines_are_truncated_with_valid_ranges() {
    let dir = TempDir::new("long");
    // 3-byte chars so the window start lands mid-character.
    let prefix = "€".repeat(200);
    let line = format!(
        "{prefix}needle{}needle{}",
        "x".repeat(150),
        "y".repeat(1000)
    );
    dir.write("a.txt", format!("{line}\n"));

    let results = run_default(&dir.0, &query("needle"));
    let found = &results.file("a.txt").lines[0];
    assert_eq!(found.column, prefix.len());
    assert!(found.text.len() < line.len());
    assert!(found.text.len() <= MAX_LINE_LEN);
    assert_eq!(found.ranges.len(), 2);
    for range in &found.ranges {
        assert_eq!(&found.text[range.clone()], "needle");
    }
    assert!(found.ranges.windows(2).all(|w| w[0].end <= w[1].start));
}

#[test]
fn match_limit_sets_limit_hit() {
    let dir = TempDir::new("limit");
    for i in 0..5 {
        dir.write(&format!("f{i}.txt"), "needle needle\n".repeat(5));
    }

    let limited = run(
        &dir.0,
        &query("needle"),
        SearchLimits {
            max_matches: 7,
            ..Default::default()
        },
    );
    assert!(limited.limit_hit);
    assert_eq!(limited.matches, 7);
    let reported: usize = limited
        .files
        .iter()
        .flat_map(|file| &file.lines)
        .map(|line| line.ranges.len())
        .sum();
    assert_eq!(reported, 7);

    let unlimited = run_default(&dir.0, &query("needle"));
    assert!(!unlimited.limit_hit);
    assert_eq!(unlimited.matches, 50);
    assert_eq!(unlimited.files_searched, 5);
}

#[test]
fn skips_files_over_size_limit() {
    let dir = TempDir::new("size");
    dir.write("big.txt", format!("needle\n{}", "x".repeat(4096)));
    dir.write("small.txt", "needle\n");

    let results = run(
        &dir.0,
        &query("needle"),
        SearchLimits {
            max_file_size: 1024,
            ..Default::default()
        },
    );
    assert_eq!(results.rel_paths(&dir.0), vec!["small.txt"]);
}

#[test]
fn cancellation_stops_events() {
    let dir = TempDir::new("cancel");
    const FILES: usize = 500;
    for i in 0..FILES {
        dir.write(&format!("f{i}.txt"), "needle\n");
    }

    let (handle, rx) = search(&dir.0, &query("needle"), SearchLimits::default()).expect("valid");
    handle.cancel();
    let results = collect(block_on(rx.collect()));
    assert!(results.files.len() < FILES);

    let (handle, rx) = search(&dir.0, &query("needle"), SearchLimits::default()).expect("valid");
    drop(handle);
    let results = collect(block_on(rx.collect()));
    assert!(results.files.len() < FILES);
}

#[test]
fn invalid_pattern_and_glob_are_rejected() {
    let dir = TempDir::new("invalid");
    let regex = SearchQuery {
        regex: true,
        ..query("(")
    };
    assert!(matches!(
        search(&dir.0, &regex, SearchLimits::default()),
        Err(QueryError::InvalidPattern(_))
    ));
    assert!(search(&dir.0, &query("("), SearchLimits::default()).is_ok());

    let glob = SearchQuery {
        include: vec!["src/[a".into()],
        ..query("x")
    };
    assert!(matches!(
        search(&dir.0, &glob, SearchLimits::default()),
        Err(QueryError::InvalidGlob(_))
    ));
}

#[test]
fn empty_pattern_sends_only_done() {
    let dir = TempDir::new("empty");
    dir.write("a.txt", "anything\n");
    let results = run_default(&dir.0, &query(""));
    assert!(results.files.is_empty());
    assert_eq!(results.files_searched, 0);
    assert_eq!(results.matches, 0);
}

/// Searches `text` as file `a.txt` and replaces every reported match.
fn replace_all(text: &str, query: &SearchQuery, replacement: &str) -> String {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let dir = TempDir::new(&format!("replace-{}", RUNS.fetch_add(1, Ordering::Relaxed)));
    dir.write("a.txt", text);
    let results = run_default(&dir.0, query);
    let targets: Vec<Target> = results
        .files
        .iter()
        .flat_map(|file| &file.lines)
        .flat_map(Target::of_line)
        .map(|(_, target)| target)
        .collect();
    let replacer = Replacer::new(query, replacement).unwrap();
    let planned = replacer.plan(text, &targets).expect("nothing changed");
    apply_replacements(text, &planned)
}

#[test]
fn replacements_expand_numbered_and_named_groups_only_in_regex_mode() {
    let regex = SearchQuery {
        regex: true,
        ..query(r"(\w+)@(?P<host>\w+)")
    };
    assert_eq!(
        replace_all("mail ann@home and bob@work\n", &regex, "${host}:$1 $$"),
        "mail home:ann $ and work:bob $\n"
    );
    // An unknown group expands to nothing; `$1x` is the group named `1x`.
    assert_eq!(replace_all("a@b\n", &regex, "[$9|${1}x]"), "[|ax]\n");
    // Literal mode: `$1` stays as typed, metacharacters match themselves.
    assert_eq!(
        replace_all("f(x) f(y)\n", &query("f(x)"), "$1"),
        "$1 f(y)\n"
    );
}

#[test]
fn replacements_honour_case_and_whole_word() {
    let insensitive = SearchQuery {
        case_sensitive: false,
        ..query("foo")
    };
    assert_eq!(
        replace_all("Foo FOO foo food\n", &insensitive, "bar"),
        "bar bar bar bard\n"
    );
    assert_eq!(
        replace_all("Foo FOO foo food\n", &query("foo"), "bar"),
        "Foo FOO bar bard\n"
    );
    let word = SearchQuery {
        whole_word: true,
        ..insensitive
    };
    assert_eq!(
        replace_all("Foo food foo_x (foo)\n", &word, "bar"),
        "bar food foo_x (bar)\n"
    );
    // Regex anchors see the line, CRLF endings survive.
    let anchored = SearchQuery {
        regex: true,
        ..query(r"^(\w+) = (\w+)$")
    };
    assert_eq!(
        replace_all("a = 1\r\nb = 2\r\n", &anchored, "$2 = $1"),
        "1 = a\r\n2 = b\r\n"
    );
}

#[test]
fn replacements_use_byte_columns_across_multibyte_text() {
    assert_eq!(
        replace_all("héllo wörld 😀 wörld\n", &query("wörld"), "мир"),
        "héllo мир 😀 мир\n"
    );
    let replacer = Replacer::new(&query("wörld"), "x").unwrap();
    let planned = replacer
        .plan(
            "😀\nwörld wörld\n",
            &[Target {
                line: 1,
                column: 7,
                text: "wörld".into(),
                partial: false,
            }],
        )
        .unwrap();
    assert_eq!(planned[0].columns, 7..13);
    assert_eq!(planned[0].offsets, 12..18);
    // A line truncated by the search still yields the original columns.
    let long = format!("{}é needle {} needle\n", "x".repeat(600), "y".repeat(50));
    assert_eq!(
        replace_all(&long, &query("needle"), "pin"),
        format!("{}é pin {} pin\n", "x".repeat(600), "y".repeat(50))
    );
}

#[test]
fn plans_refuse_text_that_changed_since_the_search() {
    let replacer = Replacer::new(&query("old"), "new").unwrap();
    let target = |line, column| Target {
        line,
        column,
        text: "old".into(),
        partial: false,
    };
    let targets = [target(0, 4), target(1, 0)];
    let planned = replacer.plan("the old\nold\n", &targets).unwrap();
    assert_eq!(
        apply_replacements("the old\nold\n", &planned),
        "the new\nnew\n"
    );
    // A line inserted above, an edit moving the match, a removed match,
    // and a file that got shorter are all stale.
    for changed in [
        "\nthe old\nold\n",
        "the  old\nold\n",
        "the old\nolD\n",
        "the old",
    ] {
        assert!(replacer.plan(changed, &targets).is_err(), "{changed:?}");
    }
    // Only the listed targets change: an excluded match stays.
    let planned = replacer.plan("the old\nold\n", &targets[1..]).unwrap();
    assert_eq!(
        apply_replacements("the old\nold\n", &planned),
        "the old\nnew\n"
    );
}

#[test]
fn text_search_matches_the_file_search_and_filters_paths() {
    let dir = TempDir::new("text");
    let text = "one two\r\ntwo\nthree two two\n";
    dir.write("src/a.txt", text);
    let results = run_default(&dir.0, &query("two"));
    assert_eq!(
        search_text(&query("two"), text).unwrap(),
        results.file("a.txt").lines
    );
    assert!(search_text(&query(""), text).unwrap().is_empty());

    let filter = PathFilter::new(
        &dir.0,
        &SearchQuery {
            include: vec!["src/**".into()],
            exclude: vec!["gen/".into(), "*.log".into()],
            ..query("x")
        },
    )
    .unwrap();
    assert!(filter.matches(&dir.0.join("src/a.txt")));
    assert!(!filter.matches(&dir.0.join("src/b.log")));
    assert!(!filter.matches(&dir.0.join("src/gen/c.txt")));
    assert!(!filter.matches(&dir.0.join("other/a.txt")));
    assert!(!filter.matches(Path::new("/elsewhere/src/a.txt")));
}
