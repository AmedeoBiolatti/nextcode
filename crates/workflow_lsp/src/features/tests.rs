use super::*;

/// `text` without its `|` cursor, and the cursor's offset.
fn cursor(text: &str) -> (String, usize) {
    let offset = text.find('|').expect("a cursor");
    (text.replacen('|', "", 1), offset)
}

fn labels(text: &str) -> Vec<String> {
    let (text, offset) = cursor(text);
    complete(&text, offset)
        .into_iter()
        .map(|item| item.label)
        .collect()
}

const FILE: &str = "inputs:
  env: {type: choice, options: [dev, prod], default: dev}
  n: {type: number}
steps:
  db: {run: d, ready: {port: 5432}}
  api:
    title: API service
    depends_on: [db]
    steps:
      build: {run: b, artifacts: [{label: wheel, path: dist/x.whl}, out.txt]}
      serve: {depends_on: [build], run: s}
  report: {depends_on: {api: completed}, run: 'r ${inputs.env}'}
";

#[test]
fn keys_for_where_the_cursor_is() {
    let top = labels("name: x\n|");
    assert_eq!(
        top,
        ["description", "inputs", "env", "steps"],
        "`name` is there"
    );
    let step = labels("steps:\n  a:\n    run: x\n    |");
    assert!(step.contains(&"depends_on".to_owned()) && step.contains(&"ready".to_owned()));
    assert!(!step.contains(&"run".to_owned()), "present already");
    assert!(
        !step.iter().any(|key| key == "needs" || key == "restart"),
        "old and planned keys"
    );
    // A group takes only group keys.
    let group = labels("steps:\n  g:\n    steps:\n      a: {run: x}\n    |");
    assert_eq!(group, ["title", "depends_on", "project", "env", "cwd"]);
    assert_eq!(
        labels("steps:\n  a: {run: x, ready: {|"),
        ["exit", "output", "port", "manual", "delay"]
    );
    assert!(
        labels("steps:\n  a: {run: x, ready: {port: 1, |").is_empty(),
        "one condition"
    );
    assert_eq!(
        labels("steps:\n  a:\n    project: {devcontainer: {|"),
        ["local", "wsl", "path"]
    );
    assert_eq!(
        labels("steps:\n  a:\n    artifacts:\n      - label: x\n        |"),
        ["path", "url"]
    );
    // Input keys follow the type.
    let number = labels("inputs:\n  n:\n    type: number\n    |");
    assert!(number.contains(&"min".to_owned()) && !number.contains(&"options".to_owned()));
    let choice = labels("inputs:\n  c:\n    type: choice\n    |");
    assert!(choice.contains(&"options".to_owned()) && !choice.contains(&"min".to_owned()));
    // Ids are the user's: nothing to offer.
    assert!(labels("steps:\n  |").is_empty());
}

#[test]
fn keys_insert_their_colon_and_replace_what_is_typed() {
    let (text, offset) = cursor("steps:\n  a:\n    run: x\n    rea|");
    let items = complete(&text, offset);
    let ready = items.iter().find(|item| item.label == "ready").unwrap();
    assert_eq!(ready.insert, "ready: ");
    assert_eq!(&text[ready.replace.clone()], "rea");
    assert!(
        ready
            .documentation
            .as_deref()
            .unwrap()
            .contains("exactly one condition")
    );
}

#[test]
fn values_and_step_ids() {
    let file = |line: &str| {
        FILE.replace(
            "  report: {depends_on: {api: completed}, run: 'r ${inputs.env}'}\n",
            line,
        )
    };
    assert_eq!(
        labels(&file("  report:\n    depends_on: |\n")),
        ["db", "api"]
    );
    assert_eq!(
        labels(&file("  report: {depends_on: [db, |\n")),
        ["api"],
        "not listed twice"
    );
    assert_eq!(
        labels(&file("  report:\n    depends_on:\n      - |\n")),
        ["db", "api"]
    );
    assert_eq!(
        labels(&file("  report:\n    depends_on:\n      db: |\n")),
        ["ready", "completed"]
    );
    assert_eq!(
        labels(&file("  report:\n    depends_on: {|\n")),
        ["db", "api"]
    );
    // Inside a group: its siblings, not the top level's.
    let inside = FILE.replace(
        "serve: {depends_on: [build], run: s}",
        "serve: {depends_on: [|], run: s}",
    );
    assert_eq!(labels(&inside), ["build"]);
    assert_eq!(
        labels("inputs:\n  e: {type: |}"),
        ["string", "number", "boolean", "choice"]
    );
    assert_eq!(
        labels("inputs:\n  e:\n    type: choice\n    options: [a, {value: b}]\n    default: |"),
        ["a", "b"]
    );
    assert_eq!(
        labels("steps:\n  a: {run: x, open_urls: |}"),
        ["true", "false"]
    );
    let ready = labels("steps:\n  a:\n    run: x\n    ready: |");
    assert_eq!(ready[0], "{port: …}");
    let (text, offset) = cursor("steps:\n  a:\n    run: x\n    ready: |");
    assert!(complete(&text, offset)[0].snippet);
    assert!(
        labels("steps:\n  a:\n    run: >\n      ech|").is_empty(),
        "inside text"
    );
}

#[test]
fn variables_complete_what_the_step_may_read() {
    let file = |run: &str| FILE.replace("run: 'r ${inputs.env}'", run);
    assert_eq!(
        labels(&file("run: 'r ${|'")),
        ["steps", "inputs", "env", "workspaceFolder", "userHome"]
    );
    assert_eq!(
        labels(&file("run: 'r ${steps.|'")),
        ["api"],
        "its dependencies only"
    );
    assert_eq!(
        labels(&file("run: 'r ${steps.api.|'")),
        ["status"],
        "a group has a status only"
    );
    let serve = FILE.replace("run: s}", "run: 's ${steps.build.artifacts.|'}");
    assert_eq!(labels(&serve), ["wheel", "1"]);
    let serve = FILE.replace("run: s}", "run: 's ${steps.|'}");
    assert_eq!(
        labels(&serve),
        ["build", "db"],
        "and what its group depends on"
    );
    assert_eq!(labels(&file("run: 'r ${inputs.|'")), ["env", "n"]);
    // `}` is added unless it is there.
    let (text, offset) = cursor(&file("run: 'r ${inputs.e|'"));
    let env = complete(&text, offset)
        .into_iter()
        .find(|item| item.label == "env")
        .unwrap();
    assert_eq!(env.insert, "env}");
    assert_eq!(&text[env.replace.clone()], "e");
    let (text, offset) = cursor(&file("run: 'r ${inputs.|}'"));
    assert_eq!(complete(&text, offset)[0].insert, "env");
}

#[test]
fn hovers_describe_keys_steps_inputs_and_variables() {
    let hover_at = |marked: &str| {
        let (text, offset) = cursor(marked);
        hover(&text, offset).map(|(markdown, range)| (markdown, text[range].to_owned()))
    };
    let (markdown, word) = hover_at("steps:\n  a:\n    ti|meout: 5m\n    run: x").unwrap();
    assert_eq!(word, "timeout");
    assert!(markdown.contains("not ready this long"), "{markdown}");
    let (markdown, word) =
        hover_at(&FILE.replace("depends_on: [db]", "depends_on: [d|b]")).unwrap();
    assert_eq!(word, "db");
    assert!(
        markdown.contains("**db**")
            && markdown.contains("`run`: `d`")
            && markdown.contains("port 5432"),
        "{markdown}"
    );
    let (markdown, _) = hover_at(&FILE.replace("  api:\n", "  a|pi:\n")).unwrap();
    assert!(
        markdown.contains("**api**: API service")
            && markdown.contains("A group of 2: build, serve"),
        "{markdown}"
    );
    let (markdown, word) = hover_at(&FILE.replace("${inputs.env}", "${inputs.e|nv}")).unwrap();
    assert_eq!(word, "${inputs.env}");
    assert!(
        markdown.contains("type: choice") && markdown.contains("default: `dev`"),
        "{markdown}"
    );
    let (markdown, _) = hover_at(&FILE.replace("{api: completed}", "{api: comp|leted}")).unwrap();
    assert!(markdown.contains("ready or failed"), "{markdown}");
    let (markdown, _) = hover_at("steps:\n  a: {run: 'echo ${env:HO|ME}'}").unwrap();
    assert!(
        markdown.contains("`HOME` where the step runs"),
        "{markdown}"
    );
}

#[test]
fn definitions_and_references_follow_scopes() {
    let at = |needle: &str, nth: usize| -> usize {
        FILE.match_indices(needle).nth(nth).expect(needle).0 + 1
    };
    let line_of = |range: &Range<usize>| FILE[..range.start].matches('\n').count() + 1;
    // From a dependency to the step's key.
    let target = definition(FILE, at("[db]", 0)).unwrap();
    assert_eq!((&FILE[target.clone()], line_of(&target)), ("db", 5));
    let target = definition(FILE, at("[build]", 0)).unwrap();
    assert_eq!((&FILE[target.clone()], line_of(&target)), ("build", 10));
    // From a variable to the input.
    let target = definition(FILE, FILE.find("${inputs.env}").unwrap() + 10).unwrap();
    assert_eq!((&FILE[target.clone()], line_of(&target)), ("env", 2));
    // All of a step's mentions, its key first.
    let found: Vec<usize> = references(FILE, at("db:", 0), true)
        .iter()
        .map(line_of)
        .collect();
    assert_eq!(found, [5, 8]);
    assert_eq!(references(FILE, at("[db]", 0), false).len(), 1);
    assert!(definition(FILE, FILE.find("title").unwrap()).is_none());
}

#[test]
fn rename_changes_the_key_dependencies_and_variables() {
    let text = "inputs:
  env: {}
steps:
  db: {run: d}
  api:
    depends_on: [db]
    steps:
      a: {run: 'x ${steps.db.status} ${inputs.env}'}
  report: {depends_on: {db: completed}, run: 'r ${steps.db.exit_code}'}
  other: {run: 'not ${steps.db.status}'}
";
    let apply = |edits: Vec<(Range<usize>, String)>| {
        let mut out = text.to_owned();
        for (range, new) in edits.into_iter().rev() {
            out.replace_range(range, &new);
        }
        out
    };
    let offset = text.find("db: {run").unwrap() + 1;
    assert_eq!(
        prepare_rename(text, offset),
        Some((offset - 1..offset + 1, "db".to_owned()))
    );
    let renamed = apply(rename(text, offset, "postgres").unwrap());
    assert_eq!(
        renamed,
        text.replacen("db: {run", "postgres: {run", 1)
            .replace("[db]", "[postgres]")
            .replace(
                "${steps.db.status} ${inputs",
                "${steps.postgres.status} ${inputs"
            )
            .replace("{db: completed}", "{postgres: completed}")
            .replace("${steps.db.exit_code}", "${steps.postgres.exit_code}"),
        "`other` does not depend on it: its variable is not about this step"
    );
    assert!(
        workflow::parse(&renamed, "f").is_err(),
        "`other`'s variable stays wrong, as it was"
    );
    let renamed = apply(rename(text, text.find("${inputs.env}").unwrap() + 10, "target").unwrap());
    assert!(renamed.contains("  target: {}") && renamed.contains("${inputs.target}"));
    assert!(
        rename(text, offset, "report")
            .unwrap_err()
            .contains("exists already")
    );
    assert!(rename(text, offset, "a/b").is_err());
}

#[test]
fn symbols_outline_inputs_and_nested_steps() {
    let symbols = symbols(FILE);
    let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
    assert_eq!(names, ["inputs", "db", "api", "report"]);
    assert_eq!(symbols[0].children.len(), 2);
    assert_eq!(symbols[0].children[0].detail.as_deref(), Some("choice"));
    assert_eq!(symbols[2].kind, SymbolKind::Group);
    assert_eq!(symbols[2].detail.as_deref(), Some("API service"));
    let children: Vec<&str> = symbols[2]
        .children
        .iter()
        .map(|symbol| symbol.name.as_str())
        .collect();
    assert_eq!(children, ["build", "serve"]);
    assert_eq!(&FILE[symbols[1].selection.clone()], "db");
}

#[test]
fn diagnostics_cover_the_token_they_are_about() {
    let text = "steps:\n  a: {run: x, depends_on: [zz]}\n  b: {run: y, timeout: soon}\n";
    let found: Vec<(&str, String)> = diagnostics(text)
        .into_iter()
        .map(|(range, message)| (&text[range], message))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "graph problems wait for step problems: {found:?}"
    );
    assert_eq!(found[0].0, "soon");
    let text = "steps:\n  a: {run: x, depends_on: [zz]}\n";
    let found = diagnostics(text);
    assert_eq!(&text[found[0].0.clone()], "zz");
    assert!(diagnostics("steps:\n  a: {run: x}\n").is_empty());
}
