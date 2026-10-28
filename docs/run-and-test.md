# Run configurations and tests

**Run configurations** say what Run and Debug start. They come from:

- `.next_code/run.json` (next_code's own; **Run > Edit Configurations…**
  creates it with a commented example and the automatic configurations as
  templates),
- `.vscode/launch.json` (read for compatibility; comments and trailing
  commas allowed),
- automatic ones: *Python: Current File*, *pytest: All Tests* and `uv run
  <script>` per `[project.scripts]` entry for `pyproject.toml` projects;
  `cargo run: <bin>` per binary and *cargo test: All Tests* from `cargo
  metadata` for Rust projects.

Both files have `launch.json`'s shape:

```jsonc
{
  "configurations": [
    // Python: "program" or "module", "args" (array or one string), "python"
    { "name": "Serve", "type": "python", "module": "app.server", "args": ["--port", "8000"],
      "cwd": "${workspaceFolder}/src", "env": { "LOG_LEVEL": "debug" }, "justMyCode": false },
    { "name": "Unit tests", "type": "pytest", "targets": ["tests/unit"], "args": ["-x"] },
    { "name": "CLI", "type": "uv", "script": "demo-cli", "args": ["--help"] },
    // cargo: "command" run | test, "package", "bin" | "test" | "example" | "bench" | "lib": true
    { "name": "Tool", "type": "cargo", "bin": "tool", "features": ["fast"], "cargoArgs": ["--release"],
      "args": ["--verbose"] },
    { "name": "Lint", "type": "shell", "command": "make lint" }
  ]
}
```

`launch.json` types: `python`/`debugpy` (launch with `program` or `module`;
attach, which can only be debugged), `lldb` (CodeLLDB: a `cargo` section
such as `{"args": ["build", "--bin=tool"], "filter": {...}}`, or `program`;
attach), `cppdbg`/`cppvsdbg` launches (run directly, debugged with LLDB).
Other types, `${command:…}`/`${input:…}` variables and invalid entries are
listed but disabled, with the reason. `preLaunchTask` is not run and
`envFile` is ignored (the picker says so). Keys a type does not use go to
the debug adapter unchanged (`justMyCode`, `stopOnEntry`, `pathMappings`…).
Variables: `${workspaceFolder}`, `${workspaceFolderBasename}`, `${file}`,
`${relativeFile}`, `${relativeFileDirname}`, `${fileBasename}`,
`${fileBasenameNoExtension}`, `${fileExtname}`, `${fileDirname}`,
`${lineNumber}`, `${pathSeparator}`, `${env:NAME}` (local projects only;
empty in WSL/containers). A relative `cwd` is relative to the project folder.

The selected configuration shows at the right of the title bar (click it to
pick another, ▶ runs it, *Debug* debugs it) and is remembered per project.

| Command | Keys |
|---|---|
| Run (selected configuration, in a terminal tab) | `Ctrl+F5`, `Shift+F10` |
| Debug (selected configuration) | `Shift+F9`; `F5` when no session is paused |
| Select Configuration… / Edit Configurations… | Run menu, palette |
| Run All Tests / Run Failed Tests / Show Tests | Run menu, palette |

Run types the command into a terminal tab named after the configuration,
where the project lives (PowerShell syntax for local Windows projects, POSIX
shells otherwise), changing directory and exporting `env` first; running it
again replaces that tab. Python runs with `uv run python …` in uv projects
(else the project's interpreter); pytest with `uv run pytest`; Rust with
`cargo run`/`cargo test`. Debug builds cargo configurations first (`cargo
build` / `cargo test --no-run` with `--message-format=json`, taking the one
executable) and hands the request to the debugger: debugpy for Python and
pytest (`module: pytest`), LLDB for Rust and native programs.

The **Tests** panel (left dock; View > Panels > Tests) finds tests when first shown
and after a test file is saved (`test_*.py`, `*_test.py`, `conftest.py`,
`.rs`; debounced):

- pytest: `pytest --collect-only -q --continue-on-collection-errors`
  (through `uv run` or the project's interpreter) for projects with
  `pyproject.toml`, `pytest.ini`, `setup.cfg`, `tox.ini` or `conftest.py`;
  folders → files → classes → tests, parametrized cases grouped under their
  function. Files that fail to import show as errors with pytest's report.
- cargo: `cargo test --no-run --workspace --tests` then each test binary's
  `--list --format terse`; packages → targets (`lib`, `bin x`, `test x`) →
  modules → tests. Doc tests are not listed.

Icons: ○ not run, ◌ running, ✓ passed, ✗ failed, ! error, ⊘ skipped; folders
show `failed ✗ passed/total`. The filter box matches names and ids. A click
opens the test's definition; a double click or the row's ▶ runs it (any
node: folder, file, class, module, target), *Debug* debugs it (pytest under
debugpy; the cargo test binary under LLDB with `--exact <name>`).
**Run All**, **Run Failed** and ⟳ (rediscover) are at the top. The details
area shows the selected test's message, traceback/panic and captured output,
or the last run's full output. Results come from pytest's JUnit XML report
(written to a temp file where the project lives and read back) and libtest's
`test … ok|FAILED|ignored` lines (one `cargo test --no-fail-fast` per
target). Once tests are discovered (the Tests panel was shown or a test run
started), editors show a ▶ in the gutter at tests, test classes and
parametrized functions; clicking it runs that test.

Settings (`settings.json`): `"tests": { "discover_on_save": true,
"pytest_args": [], "cargo_test_args": [] }` (extra arguments for discovery
and runs).
