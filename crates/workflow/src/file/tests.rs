use super::*;

fn steps(text: &str) -> Result<Vec<Step>, String> {
    parse(text, "fallback").map(|workflow| workflow.steps)
}

fn error(text: &str) -> String {
    steps(text).unwrap_err()
}

/// Each problem with its 1-based line and column.
fn located(text: &str) -> Vec<(String, Option<(usize, usize)>)> {
    check(text, "f")
        .unwrap_err()
        .into_iter()
        .map(|problem| {
            let place = problem.line_column(text);
            (problem.message, place)
        })
        .collect()
}

fn ready(id: &str, condition: Condition) -> Dependency {
    Dependency {
        step: id.into(),
        condition,
    }
}

#[test]
fn parses_every_field_in_file_order() {
    let workflow = parse(
        r#"
# Comment
name: Env
steps:
  zeta:
    title: First
    project: {local: C:/src/a}
    run: serve
    terminal: Server
    check: "true"
    on_fail: fix it
    ready: {output: 'listening on \d+'}
    timeout: 30s
    open_urls: true
    env: {A: "1", N: 2, B: true, V: 1.10}
    cwd: sub
  b: {depends_on: [zeta], ready: {manual: Go}}
  c: {depends_on: b, ready: {delay: 500ms}}
  d: {check: x, ready: {port: 8080}}
  e: {run: make, ready: {exit: true}}
  f:
    run: |
      line one
      line two
"#,
        "fallback",
    )
    .unwrap();
    assert_eq!(workflow.name, "Env");
    let ids: Vec<&str> = workflow.steps.iter().map(|step| step.id.as_str()).collect();
    assert_eq!(
        ids,
        ["zeta", "b", "c", "d", "e", "f"],
        "file order, not sorted"
    );
    let a = &workflow.steps[0];
    assert_eq!(a.label(), "First");
    assert_eq!(a.terminal_name(), "Server");
    assert_eq!(a.project, Some(ProjectSpec::Local("C:/src/a".into())));
    assert_eq!(a.check.as_deref(), Some("true"));
    assert_eq!(a.ready, Ready::Output("listening on \\d+".into()));
    assert_eq!(a.timeout, Some(Duration::from_secs(30)));
    assert!(a.open_urls);
    assert_eq!(a.env["N"], "2");
    assert_eq!(a.env["B"], "true");
    assert_eq!(a.env["V"], "1.10", "as written");
    assert_eq!(a.cwd.as_deref(), Some("sub"));
    assert_eq!(workflow.steps[1].ready, Ready::Manual("Go".into()));
    assert_eq!(workflow.steps[2].depends_on, [ready("b", Condition::Ready)]);
    assert_eq!(
        workflow.steps[2].ready,
        Ready::Delay(Duration::from_millis(500))
    );
    assert_eq!(workflow.steps[3].ready, Ready::Port(8080));
    assert_eq!(workflow.steps[4].ready, Ready::Exit);
    assert_eq!(workflow.steps[4].terminal_name(), "e");
    assert_eq!(
        workflow.steps[5].run.as_deref(),
        Some("line one\nline two\n")
    );
    assert_eq!(parse("steps: {}", "file").unwrap().name, "file");
    assert!(parse("steps:", "file").unwrap().steps.is_empty());
    // JSON is YAML.
    let json = parse(r#"{"steps": {"a": {"run": "x"}}}"#, "j").unwrap();
    assert_eq!(json.steps[0].run.as_deref(), Some("x"));
}

#[test]
fn depends_on_takes_an_id_a_list_or_conditions() {
    let workflow = parse(
        r#"
steps:
  a: {run: a}
  b: {run: b}
  one: {depends_on: a, run: x}
  list: {depends_on: [a, b], run: x}
  short: {depends_on: {a: ready, b: completed}, run: x}
  compose:
    depends_on:
      a: {condition: completed}
      b:
    run: x
"#,
        "f",
    )
    .unwrap();
    let depends = |index: usize| workflow.steps[index].depends_on.clone();
    assert_eq!(depends(2), [ready("a", Condition::Ready)]);
    assert_eq!(
        depends(3),
        [ready("a", Condition::Ready), ready("b", Condition::Ready)]
    );
    assert_eq!(
        depends(4),
        [
            ready("a", Condition::Ready),
            ready("b", Condition::Completed)
        ]
    );
    assert_eq!(
        depends(5),
        [
            ready("a", Condition::Completed),
            ready("b", Condition::Ready)
        ]
    );
    let step = &workflow.steps[4];
    assert_eq!(step.depending(Condition::Ready).collect::<Vec<_>>(), ["a"]);
    assert_eq!(
        step.depending(Condition::Completed).collect::<Vec<_>>(),
        ["b"]
    );
    assert_eq!(step.upstream().collect::<Vec<_>>(), ["a", "b"]);

    let one = |extra: &str| {
        error(&format!(
            "steps:\n  a: {{run: a}}\n  b: {{run: b, {extra}}}\n"
        ))
    };
    assert_eq!(
        one("depends_on: [a, a]"),
        "`b`: `depends_on` lists `a` twice"
    );
    assert!(
        one("depends_on: {a: started}").contains("`a`: unknown condition `started`"),
        "{}",
        one("depends_on: {a: started}")
    );
    assert!(one("depends_on: {a: {when: x}}").contains("`{condition: ready}`"));
    assert!(one("depends_on: [[a]]").contains("`depends_on` is a step id"));
    assert_eq!(one("depends_on: zz"), "`b` depends on unknown step `zz`");
}

#[test]
fn durations_are_compose_style() {
    for (text, expected) in [
        ("90s", Duration::from_secs(90)),
        ("10m", Duration::from_secs(600)),
        ("1h30m", Duration::from_secs(5400)),
        ("500ms", Duration::from_millis(500)),
        ("1.5s", Duration::from_millis(1500)),
    ] {
        assert_eq!(parse_duration(text), Ok(expected), "{text}");
    }
    for bad in ["", "10", "s", "10 minutes", "2m 30s", "1x", "-5s"] {
        assert!(parse_duration(bad).is_err(), "{bad}");
    }
    for (duration, text) in [
        (Duration::from_secs(600), "10m"),
        (Duration::from_secs(5400), "1h30m"),
        (Duration::from_secs(90), "1m30s"),
        (Duration::from_millis(500), "500ms"),
        (Duration::from_millis(1500), "1.5s"),
        (Duration::from_secs(3), "3s"),
        (Duration::ZERO, "0s"),
    ] {
        assert_eq!(format_duration(duration), text);
    }
    // A plain number is seconds.
    let step = &steps("steps:\n  a: {run: x, timeout: 45, stop_grace_period: 2m}\n").unwrap()[0];
    assert_eq!(step.timeout, Some(Duration::from_secs(45)));
    assert_eq!(step.stop_grace(), Duration::from_secs(120));
    let one = |extra: &str| error(&format!("steps:\n  a: {{run: x, {extra}}}\n"));
    assert!(one("timeout: 0s").contains("longer than 0"));
    assert!(one("timeout: soon").contains("`timeout`: `soon` is not a duration"));
    assert!(one("stop_grace_period: 500ms").contains("from 1s to 1h"));
    assert!(one("stop_grace_period: 2h").contains("from 1s to 1h"));
    assert!(one("ready: {delay: [1]}").contains("must be a duration"));
}

#[test]
fn parses_every_project_shape() {
    let project = |text: &str| {
        let document = crate::yaml::load(text);
        project_spec(&document.root.unwrap())
    };
    assert_eq!(
        project("{local: C:/p}"),
        Ok(ProjectSpec::Local("C:/p".into()))
    );
    let wsl = ProjectSpec::Wsl {
        distro: "Ubuntu".into(),
        path: "~/x".into(),
    };
    assert_eq!(project("{wsl: Ubuntu, path: ~/x}"), Ok(wsl.clone()));
    assert_eq!(project("{path: ~/x, wsl: Ubuntu}"), Ok(wsl.clone()));
    assert_eq!(
        project("{devcontainer: C:/p}"),
        Ok(ProjectSpec::DevContainer {
            folder: Box::new(ProjectSpec::Typed("C:/p".into())),
            config: None,
        })
    );
    assert_eq!(
        project("{devcontainer: {wsl: Ubuntu, path: ~/x}, config: /c/devcontainer.json}"),
        Ok(ProjectSpec::DevContainer {
            folder: Box::new(wsl.clone()),
            config: Some("/c/devcontainer.json".into()),
        })
    );
    assert_eq!(
        project("'Ubuntu:~/x'"),
        Ok(ProjectSpec::Typed("Ubuntu:~/x".into()))
    );
    // The editor's own serde shape, as in recent.json.
    assert_eq!(
        project(r#"{"Local": "C:\\p"}"#),
        Ok(ProjectSpec::Local("C:\\p".into()))
    );
    assert_eq!(
        project(r#"{"Wsl": {"distro": "Ubuntu", "path": "~/x"}}"#),
        Ok(wsl.clone())
    );
    assert_eq!(
        project(
            r#"{"DevContainer": {"folder": {"Wsl": {"distro": "Ubuntu", "path": "~/x"}}, "config": null}}"#
        ),
        Ok(ProjectSpec::DevContainer {
            folder: Box::new(wsl),
            config: None,
        })
    );
    assert!(project("{devcontainer: {devcontainer: C:/p}}").is_err());
    assert!(project("{local: ''}").is_err());
    assert!(project("{local: C:/p, path: x}").is_err());
    assert!(project("[1]").is_err());
}

#[test]
fn rejects_invalid_steps() {
    let one = |step: &str| error(&format!("steps:\n  a: {step}\n"));
    assert_eq!(
        one("{}"),
        "`a`: needs `run`, `check` or `ready` (or `steps`: a group)"
    );
    assert_eq!(
        one(""),
        "`a`: needs `run`, `check` or `ready` (or `steps`: a group)"
    );
    assert!(one("{rum: x}").contains("unknown key `rum` (known: title, depends_on,"));
    assert!(one("[run, x]").contains("a step is a mapping"));
    assert!(one("{check: x, ready: {output: y}}").contains("need `run`"));
    assert!(one("{run: x, ready: {output: '('}}").contains("invalid regex"));
    assert!(one("{run: x, ready: {port: 0}}").contains("1-65535"));
    assert!(one("{run: x, ready: {exit: true, port: 1}}").contains("exactly one condition"));
    assert!(
        one("{ready: {manual: Log in, then Continue}}").contains("a comma inside `{…}` separates")
    );
    assert!(one("{run: x, ready: {exit: false}}").contains("must be true"));
    assert!(one("{run: x, ready: port}").contains("expected one of"));
    assert!(one("{run: x, on_fail: m}").contains("`on_fail` needs `check`"));
    assert!(one("{run: [x]}").contains("`run` must be a string, not a list"));
    assert!(one("{check: x, open_urls: true}").contains("`open_urls` needs `run`"));
    assert!(one("{run: x, open_urls: yes}").contains("true or false"));
    assert!(one("{check: x, terminal: t}").contains("`terminal` needs `run`"));
    assert!(one("{run: x, env: [A]}").contains("must be a mapping of names"));
    assert!(one("{run: x, env: {A: }}").contains("`A` needs a value"));
    assert!(error("steps: [a]").contains("not a list"));
    assert!(error("steps: 3").contains("must be a mapping"));
    assert!(error("[1]").contains("a workflow is a mapping"));
    assert_eq!(error(""), "missing `steps`");
    assert!(error("steps: {a: {run: x}").starts_with("invalid YAML"));
    assert!(error("name: ''\nsteps: {}").contains("`name` must be a non-empty string"));
    assert!(
        error("colour: red\nsteps: {}").contains("unknown key `colour` (known: name, description")
    );
    // `x-` keys hold shared parts; `description` and `$schema` are fine.
    assert!(steps("x-shared: 1\ndescription: d\n$schema: s\nsteps: {}").is_ok());
    assert!(steps("steps:\n  a: {run: x, x-note: n}").is_ok());
}

#[test]
fn keys_of_the_old_format_and_planned_ones_say_so() {
    let one = |extra: &str| error(&format!("steps:\n  a: {{run: x, {extra}}}\n"));
    for (key, now) in [
        ("needs: [b]", "now `depends_on`"),
        ("after: [b]", "`depends_on` with `condition: completed`"),
        ("onFail: m", "now `on_fail`"),
        ("openUrls: true", "now `open_urls`"),
        ("timeout_s: 5", "now `timeout` (a duration"),
        ("stop_grace_s: 5", "now `stop_grace_period`"),
        ("id: a", "the step's key in `steps`"),
    ] {
        let message = one(key);
        assert!(message.contains("is from the old JSON format"), "{message}");
        assert!(message.contains(now), "{message}");
    }
    assert!(one("ready: {delay_ms: 5}").contains("now `delay`"));
    for key in [
        "restart: on_failure",
        "healthcheck: {}",
        "if: x",
        "env_file: .env",
    ] {
        assert!(
            one(key).contains("is planned but not supported yet"),
            "{key}"
        );
    }
    assert!(one("ready: {http: 'http://x'}").contains("planned"));
}

#[test]
fn reports_unknown_dependencies_and_cycles() {
    assert_eq!(
        error("steps:\n  a: {run: x, depends_on: [b, c]}\n  b: {run: y}\n"),
        "`a` depends on unknown step `c`"
    );
    assert_eq!(
        error(
            "steps:\n  a: {run: x, depends_on: [c]}\n  b: {run: y, depends_on: [a]}\n  c: {run: z, depends_on: [b]}\n"
        ),
        "dependency cycle: a → c → b → a"
    );
    assert_eq!(
        error("steps:\n  a: {run: x, depends_on: [a]}\n"),
        "dependency cycle: a → a"
    );
    // Cycles through `completed` dependencies too.
    assert_eq!(
        error(
            "steps:\n  a: {run: x, depends_on: {b: completed}}\n  b: {run: y, depends_on: [a]}\n"
        ),
        "dependency cycle: a → b → a"
    );
    // A duplicate id is a duplicate key.
    assert_eq!(
        error("steps:\n  a: {run: x}\n  a: {run: y}\n"),
        "duplicate key `a`"
    );
    // Several problems at once, one per line.
    let both = error("name: [3]\nsteps:\n  a: {}\n  b: {run: [1]}\n");
    assert_eq!(both.lines().count(), 3, "{both}");
}

#[test]
fn the_template_is_a_valid_workflow() {
    let workflow = parse(TEMPLATE, "template").unwrap();
    assert_eq!(workflow.steps.len(), 7);
    assert_eq!(workflow.steps[6].steps.len(), 2, "a group");
    assert_eq!(
        workflow.steps[5].depends_on,
        [ready("tests", Condition::Completed)]
    );
    assert_eq!(workflow.steps[4].stop_grace(), Duration::from_secs(30));
    assert_eq!(workflow.env["LOG_LEVEL"], "info");
    assert_eq!(
        workflow.steps[1].project,
        Some(ProjectSpec::DevContainer {
            folder: Box::new(ProjectSpec::Typed("C:/src/api".into())),
            config: None
        }),
        "from the anchor"
    );
    assert_eq!(workflow.inputs.len(), 2);
}

#[test]
fn problems_point_at_what_they_are_about() {
    // YAML syntax: where the parser stopped.
    let problems = located("steps:\n  a:\n    run: [x\n  b: {run: y}\n");
    assert!(problems[0].0.starts_with("invalid YAML"), "{problems:?}");
    assert!(problems[0].1.is_some());
    // An unknown dependency: at that id.
    let text = "steps:\n  a: {run: x}\n  b:\n    run: y\n    depends_on: [a, zz]\n";
    assert_eq!(
        located(text),
        [("`b` depends on unknown step `zz`".to_owned(), Some((5, 21)))]
    );
    let text = "steps:\n  a: {run: x}\n  b:\n    run: y\n    depends_on:\n      zz: completed\n";
    assert_eq!(located(text)[0].1, Some((6, 7)));
    // A bad value: at the value; an unknown key: at the key.
    let text = "steps:\n  a:\n    run: x\n    timeout: soon\n";
    assert_eq!(located(text)[0].1, Some((4, 14)));
    let text = "steps:\n  a:\n    run: x\n    rum: y\n";
    assert_eq!(located(text)[0].1, Some((4, 5)));
    // A step that does nothing: at its key; a cycle: at the step's key.
    assert_eq!(located("steps:\n  a: {}\n")[0].1, Some((2, 3)));
    let text = "steps:\n  a: {run: x, depends_on: b}\n  b: {run: y, depends_on: a}\n";
    assert_eq!(located(text)[0].1, Some((2, 3)));
    // A duplicate key: at the second one.
    assert_eq!(
        located("steps:\n  a: {run: x}\n  a: {run: x}\n"),
        [("duplicate key `a`".to_owned(), Some((3, 3)))]
    );
    // File-level problems have no place.
    assert_eq!(located("name: x\n"), [("missing `steps`".to_owned(), None)]);
}

#[test]
fn parses_artifacts_and_stop_grace() {
    let steps = steps(
        r#"
steps:
  eval:
    run: python eval.py
    stop_grace_period: 60s
    cwd: runs
    artifacts:
      - out/metrics.json
      - https://dash.example/run/${env:RUN}
      - {label: report, path: out/report.html}
      - {label: board, url: 'http://localhost:6006'}
  report: {depends_on: {eval: completed}, run: 'r ${steps.eval.exit_code}'}
  both:
    depends_on: {report: ready, eval: completed}
    check: test -f ${steps.eval.artifacts.report}
    env: {S: '${steps.eval.status}', M: '${steps.eval.artifacts.0}'}
"#,
    )
    .unwrap();
    let eval = &steps[0];
    assert_eq!(eval.stop_grace(), Duration::from_secs(60));
    assert_eq!(steps[1].stop_grace(), DEFAULT_STOP_GRACE);
    assert_eq!(
        eval.artifacts,
        [
            Artifact {
                label: None,
                target: ArtifactTarget::Path("out/metrics.json".into())
            },
            Artifact {
                label: None,
                target: ArtifactTarget::Url("https://dash.example/run/${env:RUN}".into())
            },
            Artifact {
                label: Some("report".into()),
                target: ArtifactTarget::Path("out/report.html".into())
            },
            Artifact {
                label: Some("board".into()),
                target: ArtifactTarget::Url("http://localhost:6006".into())
            },
        ]
    );
    let names: Vec<&str> = eval.artifacts.iter().map(Artifact::name).collect();
    assert_eq!(
        names,
        [
            "metrics.json",
            "https://dash.example/run/${env:RUN}",
            "report",
            "board"
        ]
    );
    assert_eq!(eval.artifact(&ArtifactRef::Label("report".into())), Some(2));
    assert_eq!(eval.artifact(&ArtifactRef::Index(3)), Some(3));
    assert_eq!(eval.artifact(&ArtifactRef::Index(4)), None);
    let upstream: Vec<&str> = steps[2].upstream().collect();
    assert_eq!(upstream, ["report", "eval"]);
}

#[test]
fn rejects_bad_stop_grace_and_artifacts() {
    let one = |extra: &str| {
        error(&format!(
            "steps:\n  a: {{run: x}}\n  b:\n    run: y\n    {extra}\n"
        ))
    };
    assert!(one("stop_grace_period: '3'").contains("is not a duration"));
    assert!(
        error("steps:\n  a: {check: x, stop_grace_period: 5s}\n")
            .contains("`stop_grace_period` needs `run`")
    );
    assert!(one("artifacts: x").contains("`artifacts` must be a list"));
    assert!(one("artifacts: [[3]]").contains("each is"));
    assert!(one("artifacts: ['']").contains("must not be empty"));
    assert!(one("artifacts: [{label: l}]").contains("needs `path` or `url`"));
    assert!(one("artifacts: [{path: p, url: 'http://u'}]").contains("needs `path` or `url`"));
    assert!(one("artifacts: [{url: 'ftp://u'}]").contains("not an http(s) URL"));
    assert!(one("artifacts: [{path: p, name: n}]").contains("unknown key `name`"));
    assert!(one("artifacts: [{label: 7, path: p}]").contains("is a number"));
    assert!(
        one("artifacts: [{label: l, path: p}, {label: l, path: q}]")
            .contains("two are labelled `l`")
    );
}

#[test]
fn step_variables_must_name_dependencies_and_known_artifacts() {
    let with = |step: &str| {
        error(&format!(
            "steps:\n  eval: {{run: x, artifacts: [{{label: log, path: l}}]}}\n  other: {{run: y}}\n  report:\n    depends_on: {{eval: completed}}\n    {step}\n"
        ))
    };
    assert_eq!(
        with("run: r ${steps.other.status}"),
        "`report`: `${steps.other.status}` refers to `other`, which is not in this step's \
         `depends_on`"
    );
    assert_eq!(
        with("run: r ${steps.eval.artifacts.html}"),
        "`report`: `${steps.eval.artifacts.html}`: `eval` has no artifact labelled `html`"
    );
    assert_eq!(
        with("check: ${steps.eval.artifacts.1}"),
        "`report`: `${steps.eval.artifacts.1}`: `eval` has 1 artifact(s), numbered from 0"
    );
    assert!(with("run: ${steps.eval.code}").contains("unknown field `code`"));
    // At the variable in the text.
    let text = "steps:\n  a: {run: x}\n  b:\n    depends_on: [c]\n    run: echo ${steps.a.status}\n  c: {run: z}\n";
    assert_eq!(located(text)[0].1, Some((5, 15)));
}

#[test]
fn groups_hold_steps_and_pass_on_their_defaults() {
    let workflow = parse(
        r#"
steps:
  db: {run: d}
  api:
    title: API
    depends_on: db
    project: C:/api
    env: {A: "1"}
    cwd: srv
    steps:
      build: {run: b}
      db: {depends_on: build, run: migrate}
      inner:
        depends_on: {db: completed}
        steps:
          x: {check: x}
"#,
        "f",
    )
    .unwrap();
    let api = &workflow.steps[1];
    assert!(api.is_group() && !workflow.steps[0].is_group());
    assert_eq!(api.label(), "API");
    assert_eq!(api.steps.len(), 3);
    assert_eq!(api.steps[1].id, "db", "ids are unique among siblings");
    assert_eq!(api.steps[2].steps[0].id, "x");
    assert_eq!(api.cwd.as_deref(), Some("srv"));
    assert!(
        api.steps[0].env.is_empty(),
        "as written; flattening inherits"
    );
}

#[test]
fn rejects_bad_groups() {
    let group = |extra: &str| {
        error(&format!(
            "steps:\n  g:\n    {extra}\n    steps:\n      a: {{run: x}}\n"
        ))
    };
    for key in [
        "run: x",
        "check: x",
        "ready: {exit: true}",
        "timeout: 5s",
        "open_urls: true",
        "terminal: t",
        "stop_grace_period: 5s",
        "artifacts: []",
        "on_fail: x",
    ] {
        let message = group(key);
        assert!(message.contains("a group cannot have"), "{key}: {message}");
    }
    assert!(error("steps:\n  g:\n    steps: {}\n").contains("a group needs at least one step"));
    assert!(error("steps:\n  g:\n    steps: [a]\n").contains("`steps` must be a mapping"));
    assert_eq!(
        error("steps:\n  a/b: {run: x}\n"),
        "`a/b`: a step id must not contain `/` (it joins a group's id and its steps')"
    );
    // Problems inside a group are named by qualified id.
    assert_eq!(
        error("steps:\n  g:\n    steps:\n      a: {run: x}\n      b: {}\n"),
        "`g/b`: needs `run`, `check` or `ready` (or `steps`: a group)"
    );
    // Siblings only: `depends_on` cannot reach out of (or into) a group.
    assert_eq!(
        error(
            "steps:\n  top: {run: t}\n  g:\n    steps:\n      a: {depends_on: top, run: x}\n  after: {depends_on: a, run: y}\n"
        ),
        "`after` depends on unknown step `a`\n`a` depends on unknown step `top` in group `g`"
    );
    assert_eq!(
        error(
            "steps:\n  g:\n    steps:\n      a: {depends_on: b, run: x}\n      b: {depends_on: {a: completed}, run: y}\n"
        ),
        "dependency cycle: a → b → a in group `g`"
    );
}

#[test]
fn nested_problems_point_at_where_they_are() {
    let text = "steps:\n  top: {run: t}\n  g:\n    steps:\n      a: {run: x}\n      b: {depends_on: [nope], run: y}\n";
    let problems = located(text);
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].1, Some((6, 24)), "at `nope`");
    let text = "steps:\n  g:\n    steps:\n      a: {timeout: 0s, run: x}\n";
    assert_eq!(located(text)[0].1, Some((4, 20)), "the nested value");
    // An inherited variable is found in the group that holds it.
    let text = "steps:\n  g:\n    env: {V: '${steps.zz.status}'}\n    steps:\n      a: {run: x}\n";
    let problems = located(text);
    assert!(
        problems[0]
            .0
            .starts_with("`g/a`: `${steps.zz.status}` refers to `zz`")
    );
    assert_eq!(problems[0].1, Some((3, 15)));
}

#[test]
fn variables_reach_siblings_and_what_enclosing_groups_wait_for() {
    let ok = steps(
        r#"
steps:
  eval: {run: e, artifacts: [{label: m, path: m.json}]}
  g:
    depends_on: {eval: completed}
    steps:
      build: {run: b}
      use:
        depends_on: build
        run: x ${steps.eval.artifacts.m} ${steps.eval.exit_code} ${steps.build.status}
  report: {depends_on: {g: completed}, run: 'r ${steps.g.status}'}
"#,
    );
    assert!(ok.is_ok(), "{ok:?}");
    assert_eq!(
        error(
            "steps:\n  g:\n    steps: {a: {run: x}}\n  r: {depends_on: {g: completed}, run: '${steps.g.exit_code}'}\n"
        ),
        "`r`: `${steps.g.exit_code}`: `g` is a group (a group only has `status`)"
    );
    assert_eq!(
        error(
            "steps:\n  top: {run: t}\n  g:\n    steps:\n      a: {run: x}\n      b: {run: '${steps.a.status} ${steps.top.status}'}\n"
        ),
        "`g/b`: `${steps.a.status}` refers to `a`, which is not in the `depends_on` of this \
         step or of its groups\n`g/b`: `${steps.top.status}` refers to `top`, which is not in \
         the `depends_on` of this step or of its groups"
    );
}

#[test]
fn the_workflow_env_reaches_every_step() {
    let workflow = parse(
        "inputs: {env: {default: dev}}\nenv: {A: top, E: '${inputs.env}'}\nsteps:\n  a: {run: x, env: {A: own}}\n",
        "f",
    )
    .unwrap();
    assert_eq!(workflow.env["A"], "top");
    let nodes = tree::flatten(&workflow);
    assert_eq!(nodes[0].step.env["A"], "own");
    assert_eq!(nodes[0].step.env["E"], "${inputs.env}");
    let resolved = workflow.with_inputs(&inputs::defaults(&workflow.inputs));
    assert_eq!(resolved.env["E"], "dev");
    assert!(error("env: [A]\nsteps: {}\n").contains("`env` must be a mapping"));
    assert_eq!(
        error("env: {E: '${inputs.nope}'}\nsteps: {}\n"),
        "`env`: `${inputs.nope}` names no input `nope` (the workflow has no `inputs`)"
    );
    // A step variable in the workflow's env is checked in each step.
    assert!(
        error("env: {S: '${steps.x.status}'}\nsteps:\n  a: {run: x}\n")
            .starts_with("`a`: `${steps.x.status}` refers to `x`")
    );
}

#[test]
fn anchors_share_settings_between_steps() {
    let workflow = parse(
        r#"
x-api: &api
  project: {devcontainer: C:/src/api}
  env: {PROFILE: dev}
steps:
  build:
    <<: *api
    run: uv sync
  serve:
    <<: *api
    depends_on: [build]
    run: uv run serve
    env: {PROFILE: prod}
"#,
        "f",
    )
    .unwrap();
    assert_eq!(workflow.steps[0].project, workflow.steps[1].project);
    assert!(workflow.steps[0].project.is_some());
    assert_eq!(
        workflow.steps[1].env["PROFILE"], "prod",
        "explicit keys win"
    );
}

#[test]
fn inputs_are_declared_referenced_and_substituted_at_any_depth() {
    let workflow = parse(
        r#"
inputs:
  env: {type: choice, options: [dev, prod], default: dev}
  workers: {type: number, default: 4}
  clean: {type: boolean}
steps:
  a:
    run: deploy ${inputs.env} -j ${inputs.workers}
    check: test ${inputs.clean}
    cwd: out/${inputs.env}
    env: {ENV: '${inputs.env}'}
    artifacts: ['logs/${inputs.env}.log']
    project: {wsl: Ubuntu, path: '~/${inputs.env}'}
  g:
    project: {devcontainer: {local: 'C:/${inputs.env}'}, config: '${inputs.env}.json'}
    steps:
      inner:
        steps:
          deep: {run: 'echo ${inputs.env}-${inputs.workers} ${env:HOME}'}
"#,
        "f",
    )
    .unwrap();
    assert_eq!(workflow.inputs.len(), 3);
    let mut values = inputs::defaults(&workflow.inputs);
    assert_eq!(values.summary(), "env=dev · workers=4 · clean=false");
    values = inputs::resolve(
        &workflow.inputs,
        serde_json::json!({ "env": "prod", "workers": 8 })
            .as_object()
            .unwrap(),
    )
    .unwrap();
    let resolved = workflow.with_inputs(&values);
    let a = &resolved.steps[0];
    assert_eq!(a.run.as_deref(), Some("deploy prod -j 8"));
    assert_eq!(a.check.as_deref(), Some("test false"));
    assert_eq!(a.cwd.as_deref(), Some("out/prod"));
    assert_eq!(a.env["ENV"], "prod");
    assert_eq!(a.artifacts[0].target.text(), "logs/prod.log");
    assert_eq!(
        a.project,
        Some(ProjectSpec::Wsl {
            distro: "Ubuntu".into(),
            path: "~/prod".into()
        })
    );
    let g = &resolved.steps[1];
    assert_eq!(
        g.project,
        Some(ProjectSpec::DevContainer {
            folder: Box::new(ProjectSpec::Local("C:/prod".into())),
            config: Some("prod.json".into())
        })
    );
    assert_eq!(
        g.steps[0].steps[0].run.as_deref(),
        Some("echo prod-8 ${env:HOME}"),
        "other variables stay"
    );
    assert_eq!(
        workflow.steps[0].run.as_deref(),
        Some("deploy ${inputs.env} -j ${inputs.workers}"),
        "the file's own steps are unchanged"
    );
    assert!(parse("steps: {}", "f").unwrap().inputs.is_empty());
    // Unquoted numbers keep their text for strings; a bare key is a
    // string input.
    let workflow = parse(
        "inputs:\n  version: {default: 1.10}\n  pick: {type: choice, options: [1, 2.0], default: 2.0}\n  note:\nsteps: {}\n",
        "f",
    )
    .unwrap();
    let defaults = inputs::defaults(&workflow.inputs);
    assert_eq!(defaults.summary(), "version=1.10 · pick=2.0 · note=");
}

#[test]
fn input_problems_point_at_where_they_are() {
    let text = "inputs:
  env: {type: choice, options: [dev], default: qa}
  n:
    type: number
    max: 64
    default: 100
  x: {colour: 1}
  l: [1]
steps: {a: {run: x}}
";
    assert_eq!(
        located(text),
        [
            (
                "input `env`: `default` `qa` is not one of dev".to_owned(),
                Some((2, 39))
            ),
            ("input `n`: `default` must be ≤ 64".to_owned(), Some((6, 5))),
            (
                format!(
                    "input `x`: unknown key `colour` (known: {})",
                    inputs::INPUT_KEYS.join(", ")
                ),
                Some((7, 3))
            ),
            (
                "input `l`: an input is a mapping of its settings".to_owned(),
                Some((8, 3))
            ),
        ]
    );
    // Unknown inputs in steps, at the variable (in a group's project and
    // a nested step too).
    let text = "inputs: {env: {}}
steps:
  a: {run: 'echo ${inputs.env} ${inputs.nope}'}
  g:
    project: {local: 'C:/${inputs.where}'}
    steps:
      b: {run: y, env: {X: '${inputs.}'}}
";
    assert_eq!(
        located(text),
        [
            (
                "`a`: `${inputs.nope}` names no input `nope` (inputs: env)".to_owned(),
                Some((3, 32))
            ),
            (
                "`g`: `${inputs.where}` names no input `where` (inputs: env)".to_owned(),
                Some((5, 26))
            ),
            (
                "`g/b`: `${inputs.}` names no input".to_owned(),
                Some((7, 29))
            ),
        ]
    );
    assert_eq!(
        error("steps: {a: {run: '${inputs.env}'}}"),
        "`a`: `${inputs.env}` names no input `env` (the workflow has no `inputs`)"
    );
    assert!(error("inputs: [a]\nsteps: {}").contains("`inputs` is a mapping of input ids"));
    // Step problems are reported with input problems.
    let both = error("inputs: {a: {type: x}}\nsteps: {s: {}}");
    assert!(both.contains("unknown type `x`"), "{both}");
    assert!(both.contains("needs `run`, `check` or `ready`"), "{both}");
}

/// The schema, compiled.
fn schema() -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(SCHEMA).expect("the schema is JSON");
    jsonschema::draft7::new(&schema).expect("the schema is a draft-07 schema")
}

/// A workflow file's text as JSON, as a YAML editor sees it.
fn as_json(text: &str) -> Value {
    crate::yaml::load(text)
        .root
        .map_or(Value::Null, |root| root.to_json())
}

/// The keys the schema lists under `properties` at `pointer`.
fn schema_keys(pointer: &str) -> Vec<String> {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let mut keys: Vec<String> = schema
        .pointer(pointer)
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("no {pointer}"))
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

fn sorted(keys: impl IntoIterator<Item = impl ToString>) -> Vec<String> {
    let mut keys: Vec<String> = keys.into_iter().map(|key| key.to_string()).collect();
    keys.sort();
    keys
}

#[test]
fn the_schema_lists_the_keys_the_parser_knows() {
    assert_eq!(
        schema_keys("/properties"),
        sorted(TOP_KEYS),
        "top-level keys"
    );
    let old = RENAMED_KEYS.iter().map(|(old, _)| *old);
    assert_eq!(
        schema_keys("/definitions/step/properties"),
        sorted(
            STEP_KEYS
                .iter()
                .copied()
                .chain(PLANNED_KEYS.iter().copied())
                .chain(old)
        ),
        "step keys, planned keys and the old format's"
    );
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let group = schema
        .pointer("/definitions/step/allOf/1/then/propertyNames/anyOf/0/enum")
        .unwrap();
    assert_eq!(
        sorted(
            group
                .as_array()
                .unwrap()
                .iter()
                .map(|key| key.as_str().unwrap())
        ),
        sorted(GROUP_KEYS)
    );
    assert_eq!(
        schema_keys("/definitions/input/properties"),
        sorted(inputs::INPUT_KEYS.iter().copied().chain(["id"])),
        "input keys (and the old `id`)"
    );
    assert_eq!(
        schema_keys("/definitions/ready/properties"),
        sorted([
            "exit", "output", "port", "manual", "delay", "http", "delay_ms"
        ])
    );
    assert!(TEMPLATE.starts_with(SCHEMA_MODELINE));
    assert!(SCHEMA_MODELINE.ends_with(SCHEMA_FILE));
}

#[test]
fn the_schema_and_the_parser_agree() {
    let validator = schema();
    let cases: &[(&str, bool)] = &[
        (TEMPLATE, true),
        ("steps: {}", true),
        ("steps:", true),
        (
            "name: x\nx-shared: &s {run: y}\nsteps:\n  a: {<<: *s}\n",
            true,
        ),
        (
            "inputs:\n  env: {type: choice, options: [dev, {value: prod, label: P}], default: dev}\n  n: {type: number, min: 1, max: 9, integer: true, required: true}\n  b: {type: boolean}\n  note:\nsteps: {a: {run: x}}\n",
            true,
        ),
        (
            "env: {A: 1, B: true}\nsteps:\n  a: {run: x, timeout: 1h30m, stop_grace_period: 2.5s, ready: {delay: 500ms}}\n",
            true,
        ),
        (
            "steps:\n  a: {check: x, on_fail: m, timeout: 90}\n  b: {depends_on: a, run: y, open_urls: true, terminal: T}\n  c: {depends_on: [a, b], ready: {manual: go}}\n  d: {depends_on: {a: completed, b: ready, c: {condition: ready}}, run: z}\n",
            true,
        ),
        (
            "steps:\n  a:\n    run: x\n    project: {devcontainer: {wsl: Ubuntu, path: ~/p}, config: c.json}\n    artifacts: [out.txt, {label: r, path: r.html}, {url: 'https://x'}]\n  b: {run: y, project: 'Ubuntu:~/p'}\n  c: {run: y, project: {Wsl: {distro: U, path: /p}}}\n",
            true,
        ),
        (
            "steps:\n  g:\n    depends_on: [a]\n    project: {local: C:/p}\n    env: {A: x}\n    cwd: s\n    steps:\n      x: {run: x}\n  a: {run: a}\n",
            true,
        ),
        // Problems the schema sees too.
        ("name: x", false),
        ("steps: [a]", false),
        ("colour: red\nsteps: {}", false),
        ("steps:\n  a: {}\n", false),
        ("steps:\n  a: {rum: x}\n", false),
        ("steps:\n  a: {run: x, needs: [b]}\n", false),
        ("steps:\n  a: {run: x, restart: always}\n", false),
        ("steps:\n  a: {run: x, timeout: soon}\n", false),
        (
            "steps:\n  a: {run: x, ready: {exit: true, port: 1}}\n",
            false,
        ),
        ("steps:\n  a: {run: x, ready: {port: 0}}\n", false),
        ("steps:\n  a: {run: x, ready: {delay_ms: 5}}\n", false),
        ("steps:\n  a: {check: x, ready: {output: y}}\n", false),
        ("steps:\n  a: {run: x, on_fail: m}\n", false),
        ("steps:\n  a: {check: x, open_urls: true}\n", false),
        ("steps:\n  a: {run: x, depends_on: {b: started}}\n", false),
        ("steps:\n  a: {run: x, depends_on: [b, b]}\n", false),
        ("steps:\n  a/b: {run: x}\n", false),
        (
            "steps:\n  a: {run: x, artifacts: [{label: '7', path: p}]}\n",
            false,
        ),
        (
            "steps:\n  a: {run: x, artifacts: [{url: 'ftp://x'}]}\n",
            false,
        ),
        (
            "steps:\n  a: {run: x, project: {local: C:/p, path: x}}\n",
            false,
        ),
        ("steps:\n  g: {run: x, steps: {a: {run: y}}}\n", false),
        ("steps:\n  g: {steps: {}}\n", false),
        ("inputs: [a]\nsteps: {}", false),
        ("inputs: {a: {type: date}}\nsteps: {}", false),
        ("inputs: {a: {type: choice}}\nsteps: {}", false),
        ("inputs: {a: {min: 1}}\nsteps: {}", false),
        (
            "inputs: {a: {type: boolean, required: true}}\nsteps: {}",
            false,
        ),
        ("inputs: {a.b: {}}\nsteps: {}", false),
        ("inputs: {a: {id: a}}\nsteps: {}", false),
    ];
    for (text, valid) in cases {
        assert_eq!(parse(text, "f").is_ok(), *valid, "parser: {text}");
        let instance = as_json(text);
        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|err| format!("{err} at {}", err.instance_path()))
            .collect();
        assert_eq!(errors.is_empty(), *valid, "schema: {text}\n{errors:#?}");
    }
}

#[test]
fn files_the_converter_writes_fit_the_schema() {
    let old = r#"// Header
{
  "name": "Demo",
  "inputs": [{ "id": "env", "type": "choice", "options": ["dev", "prod"] }],
  "steps": [
    { "id": "vpn", "check": "ping host", "onFail": "Connect" },
    { "id": "api", "needs": ["vpn"], "project": { "devcontainer": "C:/src/api" },
      "run": "serve", "ready": { "port": 8000 }, "timeout_s": 600, "stop_grace_s": 30,
      "openUrls": true, "env": { "A": "1", "N": 2 } },
    { "id": "wait", "needs": "api", "ready": { "delay_ms": 1500 } },
    { "id": "report", "after": ["api"], "run": "report", "artifacts": ["out/log.txt"] },
    { "id": "svc", "steps": [{ "id": "db", "run": "db", "ready": { "port": 5432 } }] }
  ]
}"#;
    let converted = crate::legacy::convert(old).unwrap();
    assert!(parse(&converted, "f").is_ok());
    let instance = as_json(&converted);
    let validator = schema();
    let errors: Vec<String> = validator
        .iter_errors(&instance)
        .map(|err| err.to_string())
        .collect();
    assert!(errors.is_empty(), "{converted}\n{errors:#?}");
}
