//! `next_code --headless`: the editor's project services without a window,
//! driven by JSON lines on stdin and reporting JSON lines on stdout, for
//! automation and end-to-end tests. It uses the same `Project` (local or WSL
//! agent) and `LspStore` as the GUI.
//!
//! Each request is one line: `{"id": 1, "cmd": "read", "path": "src/main.rs"}`.
//! Each gets one reply line, `{"id": 1, "ok": true, "result": ...}` or
//! `{"id": 1, "ok": false, "error": "..."}`; replies of slow commands may
//! overtake earlier ones. Unsolicited lines carry an `"event"` field.
//! Paths are absolute or relative to the project root.
//!
//! | `cmd` | parameters | result |
//! |---|---|---|
//! | `info` | | `root`, `remote`, `location` |
//! | `list_dir` | `path`? | `[{name, is_dir}]` |
//! | `list_files` | | root-relative paths |
//! | `read` | `path` | `{text}` |
//! | `write` | `path`, `text` | `null`; open documents are synced |
//! | `stat` | `path` | `{kind: "file" \| "dir" \| null}` |
//! | `search` | `pattern`, `regex`?, `case_sensitive`?, `whole_word`?, `include`?, `exclude`? | `{files, matches, files_searched, limit_hit}` |
//! | `watch` / `unwatch` | | starts/stops `files_changed` events |
//! | `open` | `path`, `timeout_ms`? | starts the language server, opens the document: `{language, server, encoding}` |
//! | `close` | `path` | `null` |
//! | `diagnostics` | `path`?, `wait_ms`? | `[{path, diagnostics}]` (raw LSP); `wait_ms` waits until the server published for `path` and stayed idle 1 s |
//! | `definition` / `declaration` / `type_definition` / `implementation` | `path` (open), `line`, `character` (LSP position) | `[{path, range}]` |
//! | `incoming_calls` / `outgoing_calls` | `path` (open), `line`, `character` | `[{name, kind, detail, path, range, selection_range, from_ranges}]`: the callers / callees of the first call hierarchy item there (`kind`: LSP `SymbolKind`; `from_ranges`: call sites, in the caller's file for incoming, in `path` for outgoing); `[]` when there is none |
//! | `document_symbols` | `path` (open) | `[{name, detail, kind, kind_name, depth, parent, range, selection_range}]`: the symbol tree in pre-order (siblings by position; flat server answers nested by range containment); `kind` is the LSP `SymbolKind` number, `parent` an index into the list |
//! | `workspace_symbols` | `query` | `[{name, kind, container, path, range}]` from every running server that lists workspace symbols (`range` `null` when the server gave only the file) |
//! | `semantic_tokens` | `path` (open) | `[{line, start, end, text, scope, modifiers}]`: the server's tokens as the editor decodes them; `start`/`end` are UTF-8 byte columns, `scope` is the highlight scope (`null`: modifiers only) |
//! | `signature_help` | `path` (open), `line`, `character` | `null` outside a call, else `{active_signature, active_parameter, signatures: [{label, parameters, active}]}`: `parameters` are the parameters' label texts, `active` the highlighted one's byte range in `label` (as the popover shows it) or `null` |
//! | `inlay_hints` | `path` (open), `start_line`? (0), `end_line`? (last line) | `[{line, character, label, kind, padding_left, padding_right}]` (LSP position; `kind`: `"type"`, `"parameter"` or `null`) |
//! | `rename` | `path` (open), `line`, `character`, `new_name` | `{applied, changed, failure}`: the server's workspace edit, written to disk (open documents synced); `changed` lists edited/created/renamed files, `failure` `{index, path, message}` or `null` |
//! | `code_actions` | `path` (open), `line`, `character`, `end_line`?, `end_character`? | `[{index, title, kind, preferred}]`, quick fixes first; the diagnostics touching the range are sent along |
//! | `apply_code_action` | `index` (from the last `code_actions`) | `{applied, changed, failure}` after resolving the action, applying its edit and running its command |
//! | `format` | `path` | `{text, changed}`: formats like Format Document (settings' `formatter`, else the language server, which needs the file `open`) and writes the file |
//! | `exec` | `command`, `timeout_ms`? | `{code, stdout, stderr}`; runs in the project root where it lives |
//! | `git_status` | | `{root, prefix, branch, unborn, upstream, ahead, behind, entries: [{path, orig_path, staged, unstaged}]}` |
//! | `git_stage` / `git_unstage` / `git_discard` | `paths` (repository-relative) | `null`; discarding deletes untracked files |
//! | `git_commit` | `message`, `amend`? | `{summary}` (`<hash> <subject>`) |
//! | `git_diff` | `path` (repository-relative); `staged`?, or `old` (revision) and `new`? (revision or `"worktree"`, the default); `whole_file`? | `{rows, added, removed, binary}`; index diffs come from `git diff` and include `hunk` rows `{index, header}` |
//! | `git_apply_hunk` | `path`, `hunk` (index from `git_diff`), `action` (`"stage"` \| `"unstage"` \| `"discard"`), `staged`?, `whole_file`? (as for the `git_diff` that numbered the hunk) | `null` |
//! | `git_branch_create` | `name`, `start`? (revision; HEAD), `switch`? (true) | `null` |
//! | `git_switch` | `name` (branch, remote branch: tracked locally, or tag: detached) | `null` |
//! | `git_branch_delete` | `name`, `force`? | `null` |
//! | `git_fetch` / `git_pull` / `git_push` | | `null`; fetch is `--all --prune`, pull is fast-forward only, a first push sets the upstream |
//! | `git_worktrees` | | `[{path, head, branch, current, bare, locked, prunable}]` |
//! | `git_worktree_add` | `name` (branch), `create`? (new branch), `start`?, `path`? (default: `<repository>-<branch>` next to it) | `{path}` |
//! | `git_worktree_remove` | `path`, `force`? | `null` |
//! | `git_log` | `limit`? (200), `all`? (true: every branch) | `{commits, graph}`: graph rows give each commit's lane and edges |
//! | `git_refs` | | `[{name, kind, target, head}]` |
//! | `git_changes` | `base` (revision), `target`? (revision or `"worktree"`) | `[{path, orig_path, change}]` |
//! | `git_commit_changes` | `commit` | `{commit, changes}` (against its first parent) |
//! | `python_info` | | `null` without `pyproject.toml`, else `{name, version, requires_python, locked_packages, interpreter, environment_python, pinned, uv, in_sync, dependencies: [{name, specifier, kind, locked}], scripts}` |
//! | `python_sync` / `python_lock` | `upgrade`? (lock) | `{code, stdout, stderr}` of `uv sync` / `uv lock` |
//! | `python_add` | `requirement`, `group`? or `optional`? (extra) | `{code, stdout, stderr}` of `uv add` |
//! | `python_remove` | `name`, `group`? or `optional`? | `{code, stdout, stderr}` of `uv remove` |
//! | `python_pin` | `version` | `{code, stdout, stderr}` of `uv python pin` |
//! | `python_interpreters` | | `[{version, implementation, path}]` (`path` null: downloadable) |
//! | `python_test_at` | `path`, `line` (0-based) | pytest node suffix (`TestX::test_y`) or `null` |
//! | `devcontainer_configs` | | `devcontainer.json` paths of the project folder |
//! | `shutdown` | | stops language servers and exits |
//!
//! Events: `progress` (while a dev container opens), `ready` (after
//! connecting), `files_changed`, `diagnostics`, `server_status`,
//! `disconnected`, `error` (startup failure, then exit 1).

use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use futures::{StreamExt, channel::mpsc};
use gpui::{App, AppContext, Application, AsyncApp, Context, Entity, Subscription, Task};
use lsp::types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, GotoDefinitionParams, GotoDefinitionResponse, InlayHint,
    InlayHintKind, InlayHintLabel, InlayHintParams, ParameterLabel, Position, SignatureHelp,
    SignatureHelpContext, SignatureHelpParams, SignatureHelpTriggerKind,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Uri, VersionedTextDocumentIdentifier, notification,
    request::{
        GotoDeclaration, GotoDefinition, GotoImplementation, GotoTypeDefinition, InlayHintRequest,
        SemanticTokensFullRequest, SignatureHelpRequest,
    },
};
use parking_lot::Mutex;
use search::{SearchEvent, SearchLimits, SearchQuery};
use serde_json::{Value, json};
use syntax::Language;
use text::Encoding;
use watcher::ChangeKind;
use worktree::FileKind;

use crate::{
    diff_view::DiffSource,
    editor::{semantic, signature_help},
    lsp_store::{LspStore, LspStoreEvent, RunningServer, ServerStatus, request_retrying},
    project::{Location, Project, WatchHandle},
    settings::Settings,
};

mod edits;

const DEFAULT_OPEN_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a server must be idle before waited-for diagnostics count as final.
const SETTLE: Duration = Duration::from_secs(1);
const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(60);
/// Time language servers get to exit on shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Serializes output lines from any thread.
#[derive(Clone)]
struct Out(Arc<Mutex<io::Stdout>>);

impl Out {
    fn line(&self, value: &Value) {
        let mut stdout = self.0.lock();
        writeln!(stdout, "{value}")
            .and_then(|()| stdout.flush())
            .ok();
    }

    fn reply(&self, id: &Value, result: Result<Value, String>) {
        self.line(&match result {
            Ok(result) => json!({ "id": id, "ok": true, "result": result }),
            Err(error) => json!({ "id": id, "ok": false, "error": error }),
        });
    }

    fn event(&self, name: &str, mut fields: Value) {
        fields["event"] = name.into();
        self.line(&fields);
    }
}

pub fn run(location: Location) {
    Application::headless().run(move |cx: &mut App| {
        let out = Out(Arc::new(Mutex::new(io::stdout())));
        let (lines_tx, lines) = mpsc::unbounded::<String>();
        thread::Builder::new()
            .name("headless-stdin".into())
            .spawn(move || {
                for line in io::stdin().lock().lines().map_while(Result::ok) {
                    if lines_tx.unbounded_send(line).is_err() {
                        return;
                    }
                }
            })
            .expect("spawning stdin reader");
        cx.spawn(async move |cx| serve(location, out, lines, cx).await)
            .detach();
    });
}

async fn serve(
    location: Location,
    out: Out,
    mut lines: mpsc::UnboundedReceiver<String>,
    cx: &mut AsyncApp,
) {
    let connect = location.clone();
    let progress = out.clone();
    let project = cx
        .background_spawn(async move {
            Project::open(&connect, &mut |line| {
                progress.event("progress", json!({ "message": line }));
            })
        })
        .await;
    let project = match project {
        Ok(project) => project,
        Err(err) => {
            out.event(
                "error",
                json!({ "message": format!("Cannot open {location}: {err}") }),
            );
            std::process::exit(1);
        }
    };
    let Ok(session) = cx.new(|cx| Session::new(project, out.clone(), cx)) else {
        return;
    };
    session
        .read_with(cx, |session, _| {
            out.event(
                "ready",
                json!({
                    "root": session.project.root().to_string_lossy(),
                    "remote": session.project.is_remote(),
                }),
            );
        })
        .ok();

    while let Some(line) = lines.next().await {
        if line.trim().is_empty() {
            continue;
        }
        let shutdown = session
            .update(cx, |session, cx| session.handle(&line, cx))
            .unwrap_or(true);
        if shutdown {
            break;
        }
    }

    // Stdin closed or `shutdown`: stop language servers, then exit.
    if let Ok(stopping) = session.update(cx, |session, cx| {
        session.watch = None;
        session
            .store
            .update(cx, |store, cx| store.shutdown_all(SHUTDOWN_GRACE, cx))
    }) {
        stopping.await;
    }
    cx.update(|cx| cx.quit()).ok();
}

struct Document {
    language: Language,
    uri: Uri,
    version: i32,
    text: String,
}

/// An open document with its running server, for LSP requests.
struct OpenDocument {
    uri: Uri,
    /// The text as last synced to the server.
    text: String,
    server: Arc<RunningServer>,
}

/// The request's `line` and `character` (an LSP position).
fn position(request: &Value) -> Result<Position, String> {
    match (request["line"].as_u64(), request["character"].as_u64()) {
        (Some(line), Some(character)) => Ok(Position::new(line as u32, character as u32)),
        _ => Err("missing `line` and `character`".into()),
    }
}

/// Definition-like results as `[{path, range}]`.
fn locations_json(response: Option<GotoDefinitionResponse>) -> Value {
    let locations: Vec<Value> = response
        .map(language::definition_locations)
        .unwrap_or_default()
        .into_iter()
        .map(|location| json!({ "path": path_json(&location.path), "range": location.range }))
        .collect();
    json!(locations)
}

struct Session {
    project: Project,
    store: Entity<LspStore>,
    out: Out,
    documents: HashMap<PathBuf, Document>,
    /// Files whose diagnostics a server published at least once.
    published: HashSet<PathBuf>,
    watch: Option<(WatchHandle, Task<()>)>,
    _disconnects: Option<Task<()>>,
    /// The last `code_actions` answer, for `apply_code_action`.
    code_actions: Option<(Arc<RunningServer>, Vec<lsp::types::CodeActionOrCommand>)>,
    _subscription: Subscription,
}

fn param_str<'a>(request: &'a Value, name: &str) -> Result<&'a str, String> {
    request[name]
        .as_str()
        .ok_or_else(|| format!("missing string parameter `{name}`"))
}

fn millis(request: &Value, name: &str, default: Duration) -> Duration {
    request[name]
        .as_u64()
        .map_or(default, Duration::from_millis)
}

fn path_json(path: &Path) -> Value {
    path.to_string_lossy().into()
}

fn status_text(status: &ServerStatus) -> String {
    match status {
        ServerStatus::Starting => "starting".into(),
        ServerStatus::Ready => "ready".into(),
        ServerStatus::Busy(work) => format!("busy: {work}"),
        ServerStatus::Disabled => "disabled".into(),
        ServerStatus::NotFound {
            command,
            install_hint,
        } => match install_hint {
            Some(hint) => format!("{command} not found (install: {hint})"),
            None => format!("{command} not found"),
        },
        ServerStatus::Failed(error) => format!("failed: {error}"),
    }
}

impl Session {
    fn new(project: Project, out: Out, cx: &mut Context<Self>) -> Self {
        let (settings, settings_error) = Settings::load();
        if let Some(error) = settings_error {
            out.event(
                "warning",
                json!({ "message": format!("Settings ignored: {error}") }),
            );
        }
        let store = cx.new(|_| LspStore::new(project.clone(), Arc::new(settings)));
        let subscription = cx.subscribe(&store, |this, store, event, cx| {
            this.on_lsp_event(&store, event, cx)
        });
        let disconnects = project.disconnects().map(|mut reasons| {
            let out = out.clone();
            cx.spawn(async move |_, _| {
                if let Some(reason) = reasons.next().await {
                    out.event("disconnected", json!({ "reason": reason }));
                }
            })
        });
        Self {
            project,
            store,
            out,
            documents: HashMap::new(),
            published: HashSet::new(),
            watch: None,
            _disconnects: disconnects,
            code_actions: None,
            _subscription: subscription,
        }
    }

    fn on_lsp_event(
        &mut self,
        store: &Entity<LspStore>,
        event: &LspStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            LspStoreEvent::DiagnosticsUpdated(path) => {
                self.published.insert(path.clone());
                let diagnostics = store.read(cx).diagnostics(path);
                self.out.event(
                    "diagnostics",
                    json!({ "path": path_json(path), "diagnostics": diagnostics }),
                );
            }
            &LspStoreEvent::StatusChanged(language) => {
                if let Some((name, status)) = store.read(cx).status(language) {
                    self.out.event(
                        "server_status",
                        json!({
                            "language": language.id(),
                            "server": name.as_ref(),
                            "status": status_text(&status),
                        }),
                    );
                }
            }
            // A restarted server must learn about the open documents again.
            &LspStoreEvent::ServerStarted(language) => {
                let server = store.update(cx, |store, cx| store.server(language, cx));
                if let Some(server) = server {
                    for document in self.documents.values().filter(|d| d.language == language) {
                        did_open(&server, document);
                    }
                }
            }
            LspStoreEvent::ServerStopped(_)
            | LspStoreEvent::SemanticTokensRefresh(_)
            | LspStoreEvent::InlayHintsRefresh(_) => {}
            LspStoreEvent::ApplyEdit(request) => self.apply_edit_request(request, cx),
        }
    }

    /// Handles one request line; returns whether to shut down.
    fn handle(&mut self, line: &str, cx: &mut Context<Self>) -> bool {
        let request: Value = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(err) => {
                self.out
                    .reply(&Value::Null, Err(format!("invalid JSON request: {err}")));
                return false;
            }
        };
        let id = request["id"].clone();
        let Some(cmd) = request["cmd"].as_str() else {
            self.out.reply(&id, Err("missing `cmd`".into()));
            return false;
        };
        match cmd {
            "info" => self.out.reply(
                &id,
                Ok(json!({
                    "root": path_json(self.project.root()),
                    "remote": self.project.is_remote(),
                    "location": self.project.location().to_string(),
                })),
            ),
            "list_dir" => {
                let path = request["path"].as_str().map_or_else(
                    || self.project.root().to_path_buf(),
                    |p| self.project.path(p),
                );
                self.blocking(id, cx, move |project| {
                    let entries = project.read_dir(&path)?;
                    Ok(json!(entries))
                });
            }
            "list_files" => self.blocking(id, cx, |project| Ok(json!(project.list_files()?))),
            "read" => match param_str(&request, "path") {
                Ok(path) => {
                    let path = self.project.path(path);
                    self.blocking(id, cx, move |project| {
                        Ok(json!({ "text": project.read_text(&path)? }))
                    });
                }
                Err(err) => self.out.reply(&id, Err(err)),
            },
            "stat" => match param_str(&request, "path") {
                Ok(path) => {
                    let path = self.project.path(path);
                    self.blocking(id, cx, move |project| {
                        let kind = match project.kind(&path)? {
                            Some(FileKind::File) => json!("file"),
                            Some(FileKind::Dir) => json!("dir"),
                            None => Value::Null,
                        };
                        Ok(json!({ "kind": kind }))
                    });
                }
                Err(err) => self.out.reply(&id, Err(err)),
            },
            "write" => self.write(id, &request, cx),
            "search" => self.search(id, &request, cx),
            "watch" => self.start_watch(id, cx),
            "unwatch" => {
                self.watch = None;
                self.out.reply(&id, Ok(Value::Null));
            }
            "open" => self.open(id, &request, cx),
            "close" => match param_str(&request, "path") {
                Ok(path) => {
                    let path = self.project.path(path);
                    if let Some(document) = self.documents.remove(&path)
                        && let Some(server) = self.running(document.language, cx)
                    {
                        server
                            .connection
                            .notify::<notification::DidCloseTextDocument>(
                                DidCloseTextDocumentParams {
                                    text_document: TextDocumentIdentifier::new(document.uri),
                                },
                            );
                    }
                    self.out.reply(&id, Ok(Value::Null));
                }
                Err(err) => self.out.reply(&id, Err(err)),
            },
            "diagnostics" => self.diagnostics(id, &request, cx),
            "definition" | "declaration" | "type_definition" | "implementation" => {
                self.goto(id, cmd, &request, cx)
            }
            "incoming_calls" | "outgoing_calls" => self.calls(id, cmd, &request, cx),
            "semantic_tokens" => self.semantic_tokens(id, &request, cx),
            "rename" => self.rename(id, &request, cx),
            "code_actions" => self.code_actions(id, &request, cx),
            "apply_code_action" => self.apply_code_action(id, &request, cx),
            "format" => self.format(id, &request, cx),
            "document_symbols" => self.document_symbols(id, &request, cx),
            "workspace_symbols" => self.workspace_symbols(id, &request, cx),
            "signature_help" => self.signature_help(id, &request, cx),
            "inlay_hints" => self.inlay_hints(id, &request, cx),
            "exec" => match param_str(&request, "command") {
                Ok(command) => {
                    let command = command.to_owned();
                    let timeout = millis(&request, "timeout_ms", DEFAULT_EXEC_TIMEOUT);
                    self.blocking(id, cx, move |project| {
                        let output = project.run(&command, timeout)?;
                        Ok(json!({
                            "code": output.code,
                            "stdout": String::from_utf8_lossy(&output.stdout),
                            "stderr": String::from_utf8_lossy(&output.stderr),
                        }))
                    });
                }
                Err(err) => self.out.reply(&id, Err(err)),
            },
            command if command.starts_with("git_") => self.git(id, command, &request, cx),
            command if command.starts_with("python_") => self.python(id, command, &request, cx),
            "devcontainer_configs" => {
                self.blocking(id, cx, |project| Ok(json!(project.devcontainer_configs()?)))
            }
            "shutdown" => {
                self.out.reply(&id, Ok(Value::Null));
                return true;
            }
            other => self
                .out
                .reply(&id, Err(format!("unknown command `{other}`"))),
        }
        false
    }

    /// `git_*` commands, run against the repository containing the project.
    fn git(&self, id: Value, command: &str, request: &Value, cx: &mut Context<Self>) {
        let paths: Vec<String> = request["paths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|path| path.as_str().map(str::to_owned))
            .collect();
        let message = request["message"].as_str().unwrap_or_default().to_owned();
        let path = request["path"].as_str().map(str::to_owned);
        let staged = request["staged"].as_bool().unwrap_or(false);
        let context = if request["whole_file"].as_bool().unwrap_or(false) {
            git::FULL_FILE
        } else {
            git::DIFF_CONTEXT
        };
        // `"worktree"` names the working tree; anything else is a revision.
        let side = |name: &str| -> Option<git::Side> {
            request[name].as_str().map(|value| match value {
                "worktree" => git::Side::Worktree,
                revision => git::Side::Revision(revision.to_owned()),
            })
        };
        let (old, new) = (side("old"), side("new"));
        let base = request["base"].as_str().map(str::to_owned);
        let target = side("target").unwrap_or(git::Side::Worktree);
        let commit = request["commit"].as_str().map(str::to_owned);
        let limit = request["limit"].as_u64().unwrap_or(200) as usize;
        let all = request["all"].as_bool().unwrap_or(true);
        let amend = request["amend"].as_bool().unwrap_or(false);
        let force = request["force"].as_bool().unwrap_or(false);
        let name = request["name"].as_str().map(str::to_owned);
        let start = request["start"].as_str().map(str::to_owned);
        let switch = request["switch"].as_bool().unwrap_or(true);
        let create = request["create"].as_bool().unwrap_or(false);
        let hunk = request["hunk"].as_u64().map(|hunk| hunk as usize);
        let action = request["action"].as_str().map(str::to_owned);
        let command = command.to_owned();
        self.blocking(id, cx, move |project| {
            let repository = project.repository()?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "the project is not in a git repository",
                )
            })?;
            let missing = |name: &str| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("missing parameter `{name}`"),
                )
            };
            let changed_entry = |path: &str| {
                repository
                    .status()?
                    .entries
                    .into_iter()
                    .find(|entry| entry.path == path)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, format!("{path} has no changes"))
                    })
            };
            match command.as_str() {
                "git_status" => {
                    let mut status = json!(repository.status()?);
                    status["root"] = repository.root().into();
                    status["prefix"] = repository.prefix().into();
                    Ok(status)
                }
                "git_stage" => repository.stage(&paths).map(|()| Value::Null),
                "git_unstage" => repository.unstage(&paths).map(|()| Value::Null),
                "git_discard" => {
                    let entries: Vec<git::Entry> = repository
                        .status()?
                        .entries
                        .into_iter()
                        .filter(|entry| paths.contains(&entry.path))
                        .collect();
                    repository.discard(&entries).map(|()| Value::Null)
                }
                "git_commit" => Ok(json!({ "summary": repository.commit(&message, amend)? })),
                "git_diff" => {
                    let path = path.ok_or_else(|| missing("path"))?;
                    let source = match (old, new) {
                        (None, None) => DiffSource::Index {
                            entry: changed_entry(&path)?,
                            staged,
                        },
                        (old, new) => {
                            let old = old.ok_or_else(|| missing("old"))?;
                            let new = new.unwrap_or(git::Side::Worktree);
                            let git::Side::Revision(base) = &old else {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "`old` must be a revision",
                                ));
                            };
                            let change = repository
                                .changes(base, &new)?
                                .into_iter()
                                .find(|change| change.path == path)
                                .ok_or_else(|| {
                                    io::Error::new(
                                        io::ErrorKind::NotFound,
                                        format!("{path} does not differ"),
                                    )
                                })?;
                            DiffSource::Revisions { old, new, change }
                        }
                    };
                    Ok(json!(source.compute(project, &repository, context)?.0))
                }
                "git_apply_hunk" => {
                    let path = path.ok_or_else(|| missing("path"))?;
                    let hunk = hunk.ok_or_else(|| missing("hunk"))?;
                    let action = match action.as_deref() {
                        Some("stage") => git::HunkAction::Stage,
                        Some("unstage") => git::HunkAction::Unstage,
                        Some("discard") => git::HunkAction::Discard,
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "`action` must be \"stage\", \"unstage\" or \"discard\"",
                            ));
                        }
                    };
                    let entry = changed_entry(&path)?;
                    let patch = repository
                        .index_patch(&entry, staged, context)?
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("{path} has no textual hunks on that side"),
                            )
                        })?;
                    repository.apply_hunk(&patch, hunk, action)?;
                    Ok(Value::Null)
                }
                "git_branch_create" => {
                    let name = name.ok_or_else(|| missing("name"))?;
                    repository.create_branch(&name, start.as_deref(), switch)?;
                    Ok(Value::Null)
                }
                "git_switch" => {
                    let name = name.ok_or_else(|| missing("name"))?;
                    let mut refs = repository.refs()?;
                    refs.retain(|r| r.name == name);
                    // A local branch wins over a remote branch or tag of that name.
                    refs.sort_by_key(|r| r.kind as u8);
                    let target = refs.into_iter().next().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, format!("no branch or tag {name}"))
                    })?;
                    repository.switch_branch(&target)?;
                    Ok(Value::Null)
                }
                "git_branch_delete" => {
                    let name = name.ok_or_else(|| missing("name"))?;
                    repository.delete_branch(&name, force)?;
                    Ok(Value::Null)
                }
                "git_fetch" => repository.fetch().map(|()| Value::Null),
                "git_pull" => repository.pull().map(|()| Value::Null),
                "git_push" => repository.push(&repository.status()?).map(|()| Value::Null),
                "git_worktrees" => Ok(json!(repository.worktrees()?)),
                "git_worktree_add" => {
                    let branch = name.ok_or_else(|| missing("name"))?;
                    let path = path.unwrap_or_else(|| repository.default_worktree_path(&branch));
                    repository.add_worktree(&path, &branch, create, start.as_deref())?;
                    Ok(json!({ "path": path }))
                }
                "git_worktree_remove" => {
                    let path = path.ok_or_else(|| missing("path"))?;
                    repository.remove_worktree(&path, force)?;
                    Ok(Value::Null)
                }
                "git_log" => {
                    let commits = repository.log(limit, all)?;
                    let graph = git::graph(&commits);
                    Ok(json!({ "commits": commits, "graph": graph }))
                }
                "git_refs" => Ok(json!(repository.refs()?)),
                "git_changes" => {
                    let base = base.ok_or_else(|| missing("base"))?;
                    Ok(json!(repository.changes(&base, &target)?))
                }
                "git_commit_changes" => {
                    let commit = commit.ok_or_else(|| missing("commit"))?;
                    let commit = repository.show(&commit)?;
                    Ok(json!({
                        "commit": commit,
                        "changes": repository.commit_changes(&commit)?,
                    }))
                }
                other => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown command `{other}`"),
                )),
            }
        });
    }

    /// `python_*` commands: the uv project in the project root.
    fn python(&self, id: Value, command: &str, request: &Value, cx: &mut Context<Self>) {
        let text = |name: &str| request[name].as_str().map(str::to_owned);
        let (name, requirement, version, path) = (
            text("name"),
            text("requirement"),
            text("version"),
            text("path"),
        );
        let kind = match (text("group"), text("optional")) {
            (Some(group), _) => python::DependencyKind::Group(group),
            (None, Some(extra)) => python::DependencyKind::Optional(extra),
            (None, None) => python::DependencyKind::Main,
        };
        let upgrade = request["upgrade"].as_bool().unwrap_or(false);
        let line = request["line"].as_u64().map(|line| line as usize);
        let command = command.to_owned();
        self.blocking(id, cx, move |project| {
            let missing = |name: &str| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("missing parameter `{name}`"),
                )
            };
            if command == "python_test_at" {
                let path = path.ok_or_else(|| missing("path"))?;
                let line = line.ok_or_else(|| missing("line"))?;
                let text = project.read_text(&project.path(&path))?;
                return Ok(json!(python::test_at(&text, line)));
            }
            let info = crate::python_panel::load(project)?;
            if command == "python_info" {
                return Ok(match info {
                    None => Value::Null,
                    Some(info) => {
                        let mut value = json!(info);
                        value["dependencies"] = info
                            .dependencies
                            .iter()
                            .map(|dependency| {
                                json!({
                                    "name": dependency.requirement.name,
                                    "specifier": dependency.requirement.specifier,
                                    "kind": dependency.kind.label(),
                                    "locked": dependency.locked,
                                })
                            })
                            .collect();
                        value["scripts"] = json!(info.project.scripts);
                        value
                    }
                });
            }
            let uv = info.and_then(|info| info.uv).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no pyproject.toml, or uv is not installed where the project lives",
                )
            })?;
            let args = match command.as_str() {
                "python_sync" => python::uv::sync(),
                "python_lock" => python::uv::lock(upgrade),
                "python_add" => {
                    python::uv::add(&requirement.ok_or_else(|| missing("requirement"))?, &kind)
                }
                "python_remove" => python::uv::remove(&name.ok_or_else(|| missing("name"))?, &kind),
                "python_pin" => python::uv::pin(&version.ok_or_else(|| missing("version"))?),
                "python_interpreters" => {
                    let output =
                        crate::python_panel::run_uv(project, &uv, &python::uv::python_list())?;
                    let interpreters =
                        python::parse_python_list(&String::from_utf8_lossy(&output.stdout))?;
                    return Ok(interpreters
                        .iter()
                        .map(|interpreter| {
                            json!({
                                "version": interpreter.version,
                                "implementation": interpreter.implementation,
                                "path": interpreter.path,
                            })
                        })
                        .collect());
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown command `{other}`"),
                    ));
                }
            };
            let output = crate::python_panel::run_uv(project, &uv, &args)?;
            Ok(json!({
                "code": output.code,
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
            }))
        });
    }

    /// Runs a blocking project operation off the main thread and replies.
    fn blocking(
        &self,
        id: Value,
        cx: &mut Context<Self>,
        operation: impl FnOnce(&Project) -> io::Result<Value> + Send + 'static,
    ) {
        let project = self.project.clone();
        let out = self.out.clone();
        cx.background_spawn(async move {
            out.reply(&id, operation(&project).map_err(|err| err.to_string()));
        })
        .detach();
    }

    fn running(&self, language: Language, cx: &mut Context<Self>) -> Option<Arc<RunningServer>> {
        self.store
            .update(cx, |store, cx| store.server(language, cx))
    }

    /// The open document the request's `path` names and its running
    /// server; replies with the error and returns `None` otherwise.
    fn document(
        &mut self,
        id: &Value,
        request: &Value,
        cx: &mut Context<Self>,
    ) -> Option<OpenDocument> {
        let path = match param_str(request, "path") {
            Ok(path) => self.project.path(path),
            Err(err) => {
                self.out.reply(id, Err(err));
                return None;
            }
        };
        let Some(document) = self.documents.get(&path) else {
            self.out.reply(
                id,
                Err(format!("{} is not open; send `open` first", path.display())),
            );
            return None;
        };
        let (uri, text, language) = (
            document.uri.clone(),
            document.text.clone(),
            document.language,
        );
        let Some(server) = self.running(language, cx) else {
            self.out
                .reply(id, Err("the language server is not running".into()));
            return None;
        };
        Some(OpenDocument { uri, text, server })
    }

    /// Sends `R` (retrying while the server is still analyzing, which
    /// rust-analyzer reports as "content modified"), then replies with
    /// `map` of the result, off the main thread.
    fn reply_request<R: lsp::types::request::Request>(
        &self,
        id: Value,
        server: Arc<RunningServer>,
        params: R::Params,
        map: impl FnOnce(R::Result) -> Value + Send + 'static,
        cx: &mut Context<Self>,
    ) where
        R::Params: Clone,
    {
        let out = self.out.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let result =
                request_retrying::<R>(&server, params, &executor, 10, Duration::from_millis(500))
                    .await;
            out.reply(&id, result.map(map).map_err(|err| err.to_string()));
        })
        .detach();
    }

    fn write(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let (path, text) = match (param_str(request, "path"), param_str(request, "text")) {
            (Ok(path), Ok(text)) => (self.project.path(path), text.to_owned()),
            (Err(err), _) | (_, Err(err)) => return self.out.reply(&id, Err(err)),
        };
        let project = self.project.clone();
        let data = text.clone().into_bytes();
        cx.spawn(async move |this, cx| {
            let written = cx
                .background_spawn(async move { project.write(&path, data).map(|()| path) })
                .await;
            this.update(cx, |this, cx| match written {
                Ok(path) => {
                    this.sync_document(&path, text, cx);
                    this.out.reply(&id, Ok(Value::Null));
                }
                Err(err) => this.out.reply(&id, Err(err.to_string())),
            })
            .ok();
        })
        .detach();
    }

    /// Sends an open document's new saved text to its server.
    fn sync_document(&mut self, path: &Path, text: String, cx: &mut Context<Self>) {
        let Some(language) = self.documents.get(path).map(|document| document.language) else {
            return;
        };
        let server = self.running(language, cx);
        let Some(document) = self.documents.get_mut(path) else {
            return;
        };
        document.version += 1;
        document.text = text;
        let Some(server) = server else {
            return;
        };
        server
            .connection
            .notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier::new(
                    document.uri.clone(),
                    document.version,
                ),
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: document.text.clone(),
                }],
            });
        server
            .connection
            .notify::<notification::DidSaveTextDocument>(DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier::new(document.uri.clone()),
                text: server.save_includes_text.then(|| document.text.clone()),
            });
    }

    fn search(&self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let strings = |name: &str| -> Vec<String> {
            request[name]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect()
        };
        let query = SearchQuery {
            pattern: request["pattern"].as_str().unwrap_or_default().to_owned(),
            regex: request["regex"].as_bool().unwrap_or(false),
            case_sensitive: request["case_sensitive"].as_bool().unwrap_or(false),
            whole_word: request["whole_word"].as_bool().unwrap_or(false),
            include: strings("include"),
            exclude: strings("exclude"),
        };
        let project = self.project.clone();
        let out = self.out.clone();
        cx.background_spawn(async move {
            let (_handle, mut events) = match project.search(&query, SearchLimits::default()) {
                Ok(started) => started,
                Err(err) => return out.reply(&id, Err(err)),
            };
            let mut files = Vec::new();
            while let Some(event) = events.next().await {
                match event {
                    SearchEvent::File(file) => files.push(json!({
                        "path": path_json(&file.path),
                        "lines": file.lines.iter().map(|line| json!({
                            "line": line.line,
                            "column": line.column,
                            "text": line.text,
                        })).collect::<Vec<_>>(),
                    })),
                    SearchEvent::Done {
                        files_searched,
                        matches,
                        limit_hit,
                    } => {
                        return out.reply(
                            &id,
                            Ok(json!({
                                "files": files,
                                "matches": matches,
                                "files_searched": files_searched,
                                "limit_hit": limit_hit,
                            })),
                        );
                    }
                }
            }
            out.reply(
                &id,
                Err("the search ended early (agent disconnected?)".into()),
            );
        })
        .detach();
    }

    fn start_watch(&mut self, id: Value, cx: &mut Context<Self>) {
        let (handle, mut batches) = match self.project.watch(Duration::from_millis(150)) {
            Ok(started) => started,
            Err(err) => return self.out.reply(&id, Err(err.to_string())),
        };
        let task = cx.spawn(async move |this, cx| {
            while let Some(batch) = batches.next().await {
                let alive = this.update(cx, |this, cx| {
                    this.store.read(cx).notify_file_changes(&batch.events);
                    let changes: Vec<Value> = batch
                        .events
                        .iter()
                        .map(|event| {
                            let kind = match event.kind {
                                ChangeKind::Created => "created",
                                ChangeKind::Modified => "modified",
                                ChangeKind::Removed => "removed",
                            };
                            json!({ "path": path_json(&event.path), "kind": kind })
                        })
                        .collect();
                    this.out.event(
                        "files_changed",
                        json!({
                            "changes": changes,
                            "rescan": batch.rescan,
                            "repository": batch.repository,
                        }),
                    );
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        self.watch = Some((handle, task));
        self.out.reply(&id, Ok(Value::Null));
    }

    fn open(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let path = match param_str(request, "path") {
            Ok(path) => self.project.path(path),
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let Some(language) = Language::from_path(&path) else {
            return self
                .out
                .reply(&id, Err(format!("no language for {}", path.display())));
        };
        let Some(uri) = lsp::path_to_uri(&path) else {
            return self
                .out
                .reply(&id, Err(format!("{} has no file URI", path.display())));
        };
        let timeout = millis(request, "timeout_ms", DEFAULT_OPEN_TIMEOUT);
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let read_path = path.clone();
            let text = match cx
                .background_spawn(async move { project.read_text(&read_path) })
                .await
            {
                Ok(text) => text,
                Err(err) => {
                    this.update(cx, |this, _| this.out.reply(&id, Err(err.to_string())))
                        .ok();
                    return;
                }
            };
            let deadline = Instant::now() + timeout;
            loop {
                let Ok(step) = this.update(cx, |this, cx| {
                    let server = this.running(language, cx);
                    let status = this.store.read(cx).status(language);
                    (server, status)
                }) else {
                    return;
                };
                let failure = match step {
                    (Some(server), status) => {
                        this.update(cx, |this, _| {
                            let document = Document {
                                language,
                                uri,
                                version: 1,
                                text,
                            };
                            did_open(&server, &document);
                            this.documents.insert(path, document);
                            this.out.reply(
                                &id,
                                Ok(json!({
                                    "language": language.id(),
                                    "server": status.map(|(name, _)| name.to_string()),
                                    "encoding": match server.encoding {
                                        Encoding::Utf8 => "utf-8",
                                        Encoding::Utf16 => "utf-16",
                                    },
                                })),
                            );
                        })
                        .ok();
                        return;
                    }
                    (None, None) => Some(format!("no language server for {}", language.id())),
                    (
                        None,
                        Some((
                            _,
                            status @ (ServerStatus::Starting
                            | ServerStatus::Ready
                            | ServerStatus::Busy(_)),
                        )),
                    ) => (Instant::now() >= deadline)
                        .then(|| format!("the server is still {}", status_text(&status))),
                    (None, Some((_, status))) => Some(status_text(&status)),
                };
                if let Some(error) = failure {
                    this.update(cx, |this, _| this.out.reply(&id, Err(error)))
                        .ok();
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        })
        .detach();
    }

    /// Replies with diagnostics. With `wait_ms`, first waits (up to that
    /// long) until the file's server published for it and has been idle for
    /// [`SETTLE`]: servers often publish an empty list before checking.
    fn diagnostics(&self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let path = request["path"].as_str().map(|path| self.project.path(path));
        let wait = millis(request, "wait_ms", Duration::ZERO);
        cx.spawn(async move |this, cx| {
            let deadline = Instant::now() + wait;
            let mut settled_since: Option<Instant> = None;
            loop {
                let Ok(settled) = this.update(cx, |this, cx| {
                    let languages: Vec<Language> = match &path {
                        Some(path) => Language::from_path(path).into_iter().collect(),
                        None => this.documents.values().map(|d| d.language).collect(),
                    };
                    let store = this.store.read(cx);
                    let idle = languages.iter().all(|&language| {
                        !matches!(
                            store.status(language),
                            Some((_, ServerStatus::Starting | ServerStatus::Busy(_)))
                        )
                    });
                    let published = path
                        .as_ref()
                        .is_none_or(|path| this.published.contains(path));
                    idle && published
                }) else {
                    return;
                };
                settled_since = settled.then(|| settled_since.unwrap_or_else(Instant::now));
                let now = Instant::now();
                if now >= deadline || settled_since.is_some_and(|since| now - since >= SETTLE) {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
            this.update(cx, |this, cx| {
                let store = this.store.read(cx);
                let result: Vec<Value> = match &path {
                    Some(path) => vec![json!({
                        "path": path_json(path),
                        "diagnostics": store.diagnostics(path),
                    })],
                    None => store
                        .all_diagnostics()
                        .map(|(path, diagnostics)| {
                            json!({ "path": path_json(path), "diagnostics": diagnostics })
                        })
                        .collect(),
                };
                this.out.reply(&id, Ok(json!(result)));
            })
            .ok();
        })
        .detach();
    }

    /// `definition`, `declaration`, `type_definition`, `implementation`.
    fn goto(&mut self, id: Value, cmd: &str, request: &Value, cx: &mut Context<Self>) {
        let position = match position(request) {
            Ok(position) => position,
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let Some(document) = self.document(&id, request, cx) else {
            return;
        };
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new(document.uri),
                position,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let server = document.server;
        match cmd {
            "declaration" => {
                self.reply_request::<GotoDeclaration>(id, server, params, locations_json, cx)
            }
            "type_definition" => {
                self.reply_request::<GotoTypeDefinition>(id, server, params, locations_json, cx)
            }
            "implementation" => {
                self.reply_request::<GotoImplementation>(id, server, params, locations_json, cx)
            }
            _ => self.reply_request::<GotoDefinition>(id, server, params, locations_json, cx),
        }
    }

    /// `incoming_calls` / `outgoing_calls`: prepares the call hierarchy at
    /// the position, then asks for the first item's calls.
    fn calls(&mut self, id: Value, cmd: &str, request: &Value, cx: &mut Context<Self>) {
        use crate::call_hierarchy::{self, Direction};
        use lsp::types::{CallHierarchyPrepareParams, request::CallHierarchyPrepare};

        let position = match position(request) {
            Ok(position) => position,
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let Some(document) = self.document(&id, request, cx) else {
            return;
        };
        let direction = if cmd == "outgoing_calls" {
            Direction::Outgoing
        } else {
            Direction::Incoming
        };
        let params = CallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new(document.uri),
                position,
            },
            work_done_progress_params: Default::default(),
        };
        let server = document.server;
        let out = self.out.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let delay = Duration::from_millis(500);
            let result = async {
                let items =
                    request_retrying::<CallHierarchyPrepare>(&server, params, &executor, 10, delay)
                        .await?;
                let Some(item) = items.and_then(|items| items.into_iter().next()) else {
                    return Ok(Vec::new());
                };
                call_hierarchy::calls(&server, direction, item, &executor, 10, delay).await
            }
            .await;
            let result = result.map_err(|err| err.to_string()).map(|calls| {
                let calls: Vec<Value> = calls
                    .into_iter()
                    .map(|call| {
                        json!({
                            "name": call.item.name,
                            "kind": call.item.kind,
                            "detail": call.item.detail,
                            "path": lsp::uri_to_path(&call.item.uri).as_deref().map(path_json),
                            "range": call.item.range,
                            "selection_range": call.item.selection_range,
                            "from_ranges": call.from_ranges,
                        })
                    })
                    .collect();
                json!(calls)
            });
            out.reply(&id, result);
        })
        .detach();
    }

    /// The signature help at a position, with each signature's highlighted
    /// parameter resolved as the editor's popover does.
    fn signature_help(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let position = match position(request) {
            Ok(position) => position,
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let Some(document) = self.document(&id, request, cx) else {
            return;
        };
        let encoding = document.server.encoding;
        let params = SignatureHelpParams {
            context: Some(SignatureHelpContext {
                trigger_kind: SignatureHelpTriggerKind::INVOKED,
                trigger_character: None,
                is_retrigger: false,
                active_signature_help: None,
            }),
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new(document.uri),
                position,
            },
            work_done_progress_params: Default::default(),
        };
        let map = move |help: Option<SignatureHelp>| {
            let Some(help) = help.filter(|help| !help.signatures.is_empty()) else {
                return Value::Null;
            };
            let signatures: Vec<Value> = help
                .signatures
                .iter()
                .map(|signature| {
                    let active =
                        signature_help::active_parameter(&help, signature).and_then(|index| {
                            signature_help::parameter_range(signature, index, encoding)
                        });
                    let parameters: Vec<String> = signature
                        .parameters
                        .iter()
                        .flatten()
                        .enumerate()
                        .map(|(index, parameter)| match &parameter.label {
                            ParameterLabel::Simple(text) => text.clone(),
                            ParameterLabel::LabelOffsets(_) => {
                                signature_help::parameter_range(signature, index, encoding)
                                    .map(|range| signature.label[range].to_owned())
                                    .unwrap_or_default()
                            }
                        })
                        .collect();
                    json!({
                        "label": signature.label,
                        "parameters": parameters,
                        "active": active.map(|range| [range.start, range.end]),
                    })
                })
                .collect();
            json!({
                "active_signature": help.active_signature,
                "active_parameter": help.active_parameter,
                "signatures": signatures,
            })
        };
        self.reply_request::<SignatureHelpRequest>(id, document.server, params, map, cx);
    }

    /// Inlay hints of a line range (default: the whole document).
    fn inlay_hints(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let Some(document) = self.document(&id, request, cx) else {
            return;
        };
        let buffer = text::Buffer::new(&document.text);
        let last_line = buffer.line_count() - 1;
        let line = |name: &str, default: usize| {
            request[name]
                .as_u64()
                .map_or(default, |line| (line as usize).min(last_line))
        };
        let (start, end) = (line("start_line", 0), line("end_line", last_line));
        let encoding = document.server.encoding;
        let range = lsp::types::Range::new(
            language::to_position(&buffer, buffer.line_start(start), encoding),
            language::to_position(&buffer, buffer.line_end(end.max(start)), encoding),
        );
        let params = InlayHintParams {
            text_document: TextDocumentIdentifier::new(document.uri),
            range,
            work_done_progress_params: Default::default(),
        };
        let map = |hints: Option<Vec<InlayHint>>| {
            let hints: Vec<Value> = hints
                .unwrap_or_default()
                .into_iter()
                .map(|hint| {
                    let label: String = match hint.label {
                        InlayHintLabel::String(label) => label,
                        InlayHintLabel::LabelParts(parts) => {
                            parts.into_iter().map(|part| part.value).collect()
                        }
                    };
                    json!({
                        "line": hint.position.line,
                        "character": hint.position.character,
                        "label": label,
                        "kind": match hint.kind {
                            Some(InlayHintKind::TYPE) => Some("type"),
                            Some(InlayHintKind::PARAMETER) => Some("parameter"),
                            _ => None,
                        },
                        "padding_left": hint.padding_left.unwrap_or(false),
                        "padding_right": hint.padding_right.unwrap_or(false),
                    })
                })
                .collect();
            json!(hints)
        };
        self.reply_request::<InlayHintRequest>(id, document.server, params, map, cx);
    }

    fn document_symbols(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        use language::symbols;
        use lsp::types::{DocumentSymbolParams, request::DocumentSymbolRequest};
        let Some(document) = self.document(&id, request, cx) else {
            return;
        };
        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier::new(document.uri),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let map = |response: Option<lsp::types::DocumentSymbolResponse>| {
            let list: Vec<Value> = response
                .map(symbols::flatten)
                .unwrap_or_default()
                .into_iter()
                .map(|symbol| {
                    let range = |range: std::ops::Range<Position>| {
                        lsp::types::Range::new(range.start, range.end)
                    };
                    json!({
                        "kind_name": symbols::kind_name(symbol.kind),
                        "name": symbol.name,
                        "detail": symbol.detail,
                        "kind": symbol.kind,
                        "depth": symbol.depth,
                        "parent": symbol.parent,
                        "range": range(symbol.range),
                        "selection_range": range(symbol.selection_range),
                    })
                })
                .collect();
            json!(list)
        };
        self.reply_request::<DocumentSymbolRequest>(id, document.server, params, map, cx);
    }

    /// `workspace_symbols`: asks every running server that serves
    /// `workspace/symbol` and merges their answers.
    fn workspace_symbols(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        use language::symbols;
        use lsp::types::{OneOf, WorkspaceSymbolParams, request::WorkspaceSymbolRequest};
        let query = match param_str(request, "query") {
            Ok(query) => query.to_owned(),
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let servers: Vec<Arc<RunningServer>> = self
            .store
            .read(cx)
            .running_servers()
            .into_iter()
            .map(|(_, server)| server)
            .filter(|server| {
                server
                    .capabilities
                    .workspace_symbol_provider
                    .as_ref()
                    .is_some_and(|provider| !matches!(provider, OneOf::Left(false)))
            })
            .collect();
        if servers.is_empty() {
            return self.out.reply(
                &id,
                Err(
                    "no running language server lists workspace symbols; `open` a file first"
                        .into(),
                ),
            );
        }
        let params = WorkspaceSymbolParams {
            query,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let out = self.out.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let requests = servers.iter().map(|server| {
                request_retrying::<WorkspaceSymbolRequest>(
                    server,
                    params.clone(),
                    &executor,
                    10,
                    Duration::from_millis(500),
                )
            });
            let mut items = Vec::new();
            let mut error = None;
            for result in futures::future::join_all(requests).await {
                match result {
                    Ok(response) => items.extend(
                        response
                            .map(symbols::workspace_symbols)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|symbol| {
                                json!({
                                    "name": symbol.name,
                                    "kind": symbol.kind,
                                    "container": symbol.container,
                                    "path": path_json(&symbol.path),
                                    "range": symbol.range,
                                })
                            }),
                    ),
                    Err(err) => error = Some(err.to_string()),
                }
            }
            out.reply(
                &id,
                match error {
                    Some(error) if items.is_empty() => Err(error),
                    _ => Ok(json!(items)),
                },
            );
        })
        .detach();
    }

    fn semantic_tokens(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let path = match param_str(request, "path") {
            Ok(path) => self.project.path(path),
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let Some(document) = self.documents.get(&path) else {
            return self.out.reply(
                &id,
                Err(format!("{} is not open; send `open` first", path.display())),
            );
        };
        let params = semantic::params(document.uri.clone());
        let buffer = text::Buffer::new(&document.text);
        let Some(server) = self.running(document.language, cx) else {
            return self
                .out
                .reply(&id, Err("the language server is not running".into()));
        };
        if server.semantic_legend.is_none() {
            return self.out.reply(
                &id,
                Err("the language server does not serve semantic tokens".into()),
            );
        }
        let out = self.out.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let mut attempts = 0;
            let response = loop {
                let result = server
                    .connection
                    .request::<SemanticTokensFullRequest>(params.clone())
                    .await;
                match result {
                    Err(lsp::Error::Response { code: -32801, .. }) if attempts < 10 => {
                        attempts += 1;
                        executor.timer(Duration::from_millis(500)).await;
                    }
                    other => break other,
                }
            };
            let Some(legend) = &server.semantic_legend else {
                return;
            };
            let result = response.map_err(|err| err.to_string()).map(|response| {
                let data = semantic::response_data(response);
                let tokens: Vec<Value> = semantic::decode(&data, legend, &buffer, server.encoding)
                    .into_iter()
                    .map(|token| {
                        let start = buffer.point(token.range.start);
                        json!({
                            "line": start.row,
                            "start": start.column,
                            "end": start.column + token.range.len(),
                            "text": buffer.slice(token.range.clone()),
                            "scope": token.highlight.map(|highlight| highlight.scope()),
                            "modifiers": legend.modifier_names(token.modifiers).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                json!(tokens)
            });
            out.reply(&id, result);
        })
        .detach();
    }
}

fn did_open(server: &RunningServer, document: &Document) {
    server
        .connection
        .notify::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                document.uri.clone(),
                document.language.id().into(),
                document.version,
                document.text.clone(),
            ),
        });
}
