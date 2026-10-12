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
//! | `definition` | `path`, `line`, `character` (LSP position) | `[{path, range}]` |
//! | `exec` | `command`, `timeout_ms`? | `{code, stdout, stderr}`; runs in the project root where it lives |
//! | `git_status` | | `{root, prefix, branch, unborn, upstream, ahead, behind, entries: [{path, orig_path, staged, unstaged}]}` |
//! | `git_stage` / `git_unstage` / `git_discard` | `paths` (repository-relative) | `null`; discarding deletes untracked files |
//! | `git_commit` | `message` | `{summary}` (`<hash> <subject>`) |
//! | `git_diff` | `path` (repository-relative); `staged`?, or `old` (revision) and `new`? (revision or `"worktree"`, the default); `whole_file`? | `{rows, added, removed, binary}` |
//! | `git_log` | `limit`? (200), `all`? (true: every branch) | `{commits, graph}`: graph rows give each commit's lane and edges |
//! | `git_refs` | | `[{name, kind, target, head}]` |
//! | `git_changes` | `base` (revision), `target`? (revision or `"worktree"`) | `[{path, orig_path, change}]` |
//! | `git_commit_changes` | `commit` | `{commit, changes}` (against its first parent) |
//! | `shutdown` | | stops language servers and exits |
//!
//! Events: `ready` (after connecting), `files_changed`, `diagnostics`,
//! `server_status`, `disconnected`, `error` (startup failure, then exit 1).

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
    DidSaveTextDocumentParams, GotoDefinitionParams, Position, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Uri,
    VersionedTextDocumentIdentifier, notification, request::GotoDefinition,
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
    lsp_store::{LspStore, LspStoreEvent, RunningServer, ServerStatus},
    project::{Location, Project, WatchHandle},
    settings::Settings,
};

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
    let project = cx
        .background_spawn(async move {
            match connect {
                Location::Local(root) => Ok(Project::local(root)),
                Location::Wsl { distro, path } => Project::connect_wsl(&distro, &path),
            }
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

struct Session {
    project: Project,
    store: Entity<LspStore>,
    out: Out,
    documents: HashMap<PathBuf, Document>,
    /// Files whose diagnostics a server published at least once.
    published: HashSet<PathBuf>,
    watch: Option<(WatchHandle, Task<()>)>,
    _disconnects: Option<Task<()>>,
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
            LspStoreEvent::ServerStopped(_) => {}
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
            "definition" => self.definition(id, &request, cx),
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
                "git_commit" => Ok(json!({ "summary": repository.commit(&message)? })),
                "git_diff" => {
                    let path = path.ok_or_else(|| missing("path"))?;
                    let source = match (old, new) {
                        (None, None) => {
                            let entry = repository
                                .status()?
                                .entries
                                .into_iter()
                                .find(|entry| entry.path == path)
                                .ok_or_else(|| {
                                    io::Error::new(
                                        io::ErrorKind::NotFound,
                                        format!("{path} has no changes"),
                                    )
                                })?;
                            DiffSource::Index { entry, staged }
                        }
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
                    Ok(json!(source.compute(project, &repository, context)?))
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

    fn definition(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let path = match param_str(request, "path") {
            Ok(path) => self.project.path(path),
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let (Some(line), Some(character)) =
            (request["line"].as_u64(), request["character"].as_u64())
        else {
            return self
                .out
                .reply(&id, Err("missing `line` and `character`".into()));
        };
        let Some(document) = self.documents.get(&path) else {
            return self.out.reply(
                &id,
                Err(format!("{} is not open; send `open` first", path.display())),
            );
        };
        let uri = document.uri.clone();
        let Some(server) = self.running(document.language, cx) else {
            return self
                .out
                .reply(&id, Err("the language server is not running".into()));
        };
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new(uri),
                position: Position::new(line as u32, character as u32),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let out = self.out.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            // Servers still analyzing answer "content modified"; retry like
            // the editor does.
            let mut attempts = 0;
            let response = loop {
                let result = server.connection.request::<GotoDefinition>(params.clone()).await;
                match result {
                    Err(lsp::Error::Response { code: -32801, .. }) if attempts < 10 => {
                        attempts += 1;
                        executor.timer(Duration::from_millis(500)).await;
                    }
                    other => break other,
                }
            };
            let result = response.map_err(|err| err.to_string()).map(|response| {
                let locations: Vec<Value> = response
                    .map(language::definition_locations)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|location| json!({ "path": path_json(&location.path), "range": location.range }))
                    .collect();
                json!(locations)
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
