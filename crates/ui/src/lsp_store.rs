//! Starts, supervises and answers language servers for one project.

use std::{
    cell::RefCell,
    collections::HashMap,
    fs::File,
    io::{LineWriter, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use futures::StreamExt;
use gpui::{AppContext, Context, EventEmitter, SharedString, Task};
use lsp::{
    Incoming, LanguageServer, LogKind, Logger, path_to_uri,
    types::{
        ClientCapabilities, InitializeParams, InitializeResult, InitializedParams,
        PositionEncodingKind, ProgressParams, ProgressParamsValue, PublishDiagnosticsParams,
        ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
        TextDocumentSyncSaveOptions, WorkDoneProgress, WorkspaceFolder, notification,
        request::Initialize,
    },
    uri_to_path,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use syntax::Language;
use text::Encoding;

use crate::{
    editor::semantic,
    notebook::lsp::{Registered, VirtualDocument},
    project::{Project, Root},
    settings::{self, ServerConfig, Settings},
};

/// Restarts allowed per server before giving up for the session.
const MAX_RESTARTS: u32 = 3;

/// LSP error code servers answer while their analysis is out of date
/// (rust-analyzer while indexing).
pub const CONTENT_MODIFIED: i64 = -32801;

/// Sends `R`, retrying up to `retries` times `delay` apart while the server
/// answers "content modified".
pub async fn request_retrying<R: lsp::types::request::Request>(
    server: &RunningServer,
    params: R::Params,
    executor: &gpui::BackgroundExecutor,
    retries: u32,
    delay: std::time::Duration,
) -> Result<R::Result, lsp::Error>
where
    R::Params: Clone,
{
    let mut attempt = 0;
    loop {
        let result = server.connection.request::<R>(params.clone()).await;
        match result {
            Err(lsp::Error::Response { code, .. })
                if code == CONTENT_MODIFIED && attempt < retries =>
            {
                attempt += 1;
                executor.timer(delay).await;
            }
            other => return other,
        }
    }
}

/// An initialized server, shared with the editors that sync documents to it.
pub struct RunningServer {
    pub connection: Arc<LanguageServer>,
    pub encoding: Encoding,
    pub sync: TextDocumentSyncKind,
    pub save_includes_text: bool,
    pub capabilities: ServerCapabilities,
    /// The semantic-token legend, if the server serves full-document tokens.
    pub semantic_legend: Option<semantic::Legend>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerStatus {
    Starting,
    Ready,
    /// Work-done progress, e.g. "indexing 40%".
    Busy(SharedString),
    NotFound {
        command: SharedString,
        install_hint: Option<&'static str>,
    },
    Disabled,
    Failed(SharedString),
}

pub enum LspStoreEvent {
    /// A server finished initializing. A language may have one server per
    /// folder (see [`LspStore::server`]), so this names the one that started.
    ServerStarted(Language, Arc<RunningServer>),
    /// A server of the language exited; its documents' connections report
    /// `has_exited`.
    ServerStopped(Language),
    StatusChanged(Language),
    DiagnosticsUpdated(PathBuf),
    /// The server asked to re-request semantic tokens of open documents.
    SemanticTokensRefresh(Language),
    /// The server asked to re-request inlay hints of open documents.
    InlayHintsRefresh(Language),
    /// The server asked to apply a workspace edit (`workspace/applyEdit`);
    /// unanswered requests answer "not applied" when dropped.
    ApplyEdit(Arc<ApplyEditRequest>),
}

/// A server's `workspace/applyEdit` request.
pub struct ApplyEditRequest {
    pub label: Option<String>,
    pub edit: lsp::types::WorkspaceEdit,
    /// The server's position encoding.
    pub encoding: Encoding,
    reply: Mutex<Option<(Arc<LanguageServer>, lsp::RequestId)>>,
}

impl ApplyEditRequest {
    /// Answers the server (only the first answer counts).
    pub fn respond(&self, response: lsp::types::ApplyWorkspaceEditResponse) {
        if let Some((connection, id)) = self.reply.lock().take() {
            connection.respond(id, Ok(json!(response)));
        }
    }
}

impl Drop for ApplyEditRequest {
    fn drop(&mut self) {
        self.respond(lsp::types::ApplyWorkspaceEditResponse {
            applied: false,
            failure_reason: Some("no editor applied the edit".into()),
            failed_change: None,
        });
    }
}

/// A server: its language and the index of the folder it was started for.
///
/// A project's folders are fixed for its lifetime (changing them reopens
/// the project), so `workspace/didChangeWorkspaceFolders` is never needed.
/// Each language's first-folder server is initialized with every folder;
/// in a multi-root project whose server does not advertise
/// `workspace.workspaceFolders.supported`, every other folder gets its own
/// server (see [`serving_root`]).
type SlotKey = (Language, usize);

struct Slot {
    name: SharedString,
    status: ServerStatus,
    config: Option<ServerConfig>,
    connection: Option<Arc<LanguageServer>>,
    running: Option<Arc<RunningServer>>,
    /// Active work-done progress by token, in start order.
    progress: Vec<(String, String)>,
    restarts: u32,
    /// Whether this first-folder server serves every folder; `None` until
    /// it answered `initialize`. One that could not start does not, so the
    /// other folders try their own.
    serves_all: Option<bool>,
    /// Per folder, the settings answered to `workspace/configuration`
    /// items scoped to it (first-folder servers of multi-root projects).
    folder_settings: Vec<Value>,
    _tasks: Vec<Task<()>>,
}

pub struct LspStore {
    project: Project,
    settings: Arc<Settings>,
    slots: HashMap<SlotKey, Slot>,
    diagnostics: HashMap<PathBuf, Vec<lsp::types::Diagnostic>>,
    /// Open notebooks' virtual documents by their (virtual) path.
    notebooks: HashMap<PathBuf, Registered>,
}

impl EventEmitter<LspStoreEvent> for LspStore {}

impl LspStore {
    pub fn new(project: Project, settings: Arc<Settings>) -> Self {
        Self {
            project,
            settings,
            slots: HashMap::new(),
            diagnostics: HashMap::new(),
            notebooks: HashMap::new(),
        }
    }

    /// The running server for `language` that serves `path`, starting it
    /// on first use. That is the first folder's server unless the project
    /// has several folders and that server cannot serve them all; then
    /// `path`'s folder gets its own (`None` until the first one answered
    /// whether it can).
    pub fn server(
        &mut self,
        language: Language,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> Option<Arc<RunningServer>> {
        if !self.slots.contains_key(&(language, 0)) {
            self.start(language, 0, cx);
        }
        let root = self.serving(language, path)?;
        if !self.slots.contains_key(&(language, root)) {
            self.start(language, root, cx);
        }
        self.slots.get(&(language, root))?.running.clone()
    }

    /// The folder whose `language` server serves `path` (see
    /// [`serving_root`]).
    fn serving(&self, language: Language, path: &Path) -> Option<usize> {
        serving_root(
            self.project.is_multi_root(),
            self.project.root_index_of(path),
            self.slots
                .get(&(language, 0))
                .and_then(|slot| slot.serves_all),
        )
    }

    /// The settings the store was created with.
    pub fn settings(&self) -> Arc<Settings> {
        self.settings.clone()
    }

    /// Name and status of the server serving `path`, for the status bar
    /// (the first folder's while it decides whether it serves `path`).
    pub fn status(&self, language: Language, path: &Path) -> Option<(SharedString, ServerStatus)> {
        let root = self.serving(language, path).unwrap_or(0);
        self.slots
            .get(&(language, root))
            .map(|slot| (slot.name.clone(), slot.status.clone()))
    }

    /// Every running server with its language and name (none are started).
    pub fn running_servers(&self) -> Vec<(Language, SharedString, Arc<RunningServer>)> {
        self.slots
            .iter()
            .filter_map(|((language, _), slot)| {
                Some((*language, slot.name.clone(), slot.running.clone()?))
            })
            .collect()
    }

    pub fn diagnostics(&self, path: &Path) -> &[lsp::types::Diagnostic] {
        self.diagnostics.get(path).map_or(&[], Vec::as_slice)
    }

    /// Registers a notebook's virtual document: its diagnostics are
    /// filtered (see `VirtualDocument::filter_diagnostics`), and workspace
    /// edits, navigation and Problems map its positions to cells.
    pub fn register_notebook(&mut self, document: &Rc<RefCell<VirtualDocument>>) {
        self.notebooks.retain(|_, entry| entry.strong_count() > 0);
        let path = document.borrow().path().to_path_buf();
        self.diagnostics.remove(&path);
        self.notebooks.insert(path, Rc::downgrade(document));
    }

    /// The open notebook whose virtual document is `path`.
    pub fn notebook(&self, path: &Path) -> Option<Rc<RefCell<VirtualDocument>>> {
        self.notebooks.get(path)?.upgrade()
    }

    /// Tells every running server about files changed outside the editor.
    /// Servers may register narrower globs; all project changes are sent,
    /// which servers accept and filter themselves. A server for one folder
    /// gets that folder's changes only.
    pub fn notify_file_changes(&self, events: &[watcher::FileEvent]) {
        use lsp::types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};
        let changes: Vec<(usize, FileEvent)> = events
            .iter()
            .filter_map(|event| {
                let typ = match event.kind {
                    watcher::ChangeKind::Created => FileChangeType::CREATED,
                    watcher::ChangeKind::Modified => FileChangeType::CHANGED,
                    watcher::ChangeKind::Removed => FileChangeType::DELETED,
                };
                let root = self.project.root_index_of(&event.path).unwrap_or(0);
                Some((root, FileEvent::new(path_to_uri(&event.path)?, typ)))
            })
            .collect();
        for (&(_, root), slot) in &self.slots {
            let Some(server) = &slot.running else {
                continue;
            };
            let all = !self.project.is_multi_root() || slot.serves_all == Some(true);
            let changes: Vec<FileEvent> = changes
                .iter()
                .filter(|(of, _)| all || *of == root)
                .map(|(_, change)| change.clone())
                .collect();
            if changes.is_empty() {
                continue;
            }
            server
                .connection
                .notify::<notification::DidChangeWatchedFiles>(DidChangeWatchedFilesParams {
                    changes,
                });
        }
    }

    /// Re-reads each folder's Python interpreter (after `uv sync` made or
    /// changed `.venv`) and tells running Python servers, which pull their
    /// settings again.
    pub fn refresh_python_interpreter(&mut self, cx: &mut Context<Self>) {
        if !self
            .slots
            .keys()
            .any(|(language, _)| *language == Language::Python)
        {
            return;
        }
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let interpreters: Vec<Option<PathBuf>> = cx
                .background_spawn(async move {
                    project
                        .roots()
                        .iter()
                        .map(|root| project.python_interpreter_in(&root.path).ok().flatten())
                        .collect()
                })
                .await;
            this.update(cx, |this, _| {
                let set = |settings: &mut Value, python: &Option<PathBuf>| {
                    if let Some(python) = python {
                        settings::merge(settings, json!({ "python": { "pythonPath": python } }));
                    }
                };
                for (&(language, root), slot) in &mut this.slots {
                    if language != Language::Python {
                        continue;
                    }
                    if let Some(config) = &mut slot.config {
                        set(&mut config.settings, &interpreters[root]);
                    }
                    for (settings, python) in slot.folder_settings.iter_mut().zip(&interpreters) {
                        set(settings, python);
                    }
                    if let Some(server) = &slot.running {
                        server
                            .connection
                            .notify::<notification::DidChangeConfiguration>(
                                lsp::types::DidChangeConfigurationParams {
                                    settings: Value::Null,
                                },
                            );
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Every file's diagnostics, in no particular order (closed notebooks'
    /// left out).
    pub fn all_diagnostics(&self) -> impl Iterator<Item = (&Path, &[lsp::types::Diagnostic])> {
        self.diagnostics
            .iter()
            .filter(|(path, _)| {
                self.notebooks
                    .get(*path)
                    .is_none_or(|entry| entry.strong_count() > 0)
            })
            .map(|(path, diagnostics)| (path.as_path(), diagnostics.as_slice()))
    }

    /// Asks every server to shut down and exit, giving them `grace` before
    /// their processes are ended. No server restarts afterwards.
    pub fn shutdown_all(&mut self, grace: std::time::Duration, cx: &mut Context<Self>) -> Task<()> {
        let connections: Vec<Arc<LanguageServer>> = self
            .slots
            .drain()
            .filter_map(|(_, slot)| slot.connection)
            .collect();
        let timer = cx.background_executor().timer(grace);
        cx.background_spawn(async move {
            let all = futures::future::join_all(connections.iter().map(|c| c.shutdown()));
            futures::future::select(Box::pin(all), timer).await;
            // Dropping the connections ends any process still running.
            drop(connections);
        })
    }

    /// `language`'s server config for folder `folder`: its layered settings
    /// (user < workspace file < folder) and, for Python, its interpreter.
    /// Blocks on the folder's settings file and `.venv` lookup.
    fn folder_config(&self, language: Language, folder: &Path) -> Option<ServerConfig> {
        let mut config = self
            .project
            .settings_for(Some(folder))
            .server_config(language)?;
        if language == Language::Python
            && let Ok(Some(python)) = self.project.python_interpreter_in(folder)
        {
            settings::merge(
                &mut config.settings,
                json!({ "python": { "pythonPath": python } }),
            );
        }
        Some(config)
    }

    fn start(&mut self, language: Language, root: usize, cx: &mut Context<Self>) {
        let key = (language, root);
        let (restarts, serves_all) = self
            .slots
            .get(&key)
            .map_or((0, None), |slot| (slot.restarts, slot.serves_all));
        let folder = self.project.roots()[root].path.clone();
        let Some(config) = self.folder_config(language, &folder) else {
            return;
        };
        // A first-folder server answers configuration scoped to any folder.
        let folder_settings = if root == 0 && self.project.is_multi_root() {
            let others = self.project.roots()[1..].iter().map(|other| {
                self.folder_config(language, &other.path)
                    .map_or(Value::Null, |config| config.settings)
            });
            std::iter::once(config.settings.clone())
                .chain(others)
                .collect()
        } else {
            Vec::new()
        };
        let mut slot = Slot {
            name: config.command.clone().into(),
            status: ServerStatus::Starting,
            config: None,
            connection: None,
            running: None,
            progress: Vec::new(),
            restarts,
            serves_all,
            folder_settings,
            _tasks: Vec::new(),
        };

        if !config.enabled {
            slot.status = ServerStatus::Disabled;
        } else if config.command == settings::WORKFLOW_SERVER {
            // Built in: in-process, the same for every kind of project.
            match start_workflow_server(open_log(language, root)) {
                Ok((connection, incoming)) => {
                    let connection = Arc::new(connection);
                    slot._tasks = vec![
                        self.listen(key, incoming, cx),
                        self.initialize(key, &connection, &config, cx),
                    ];
                    slot.connection = Some(connection);
                }
                Err(err) => {
                    slot.status = ServerStatus::Failed(format!("failed to start: {err}").into())
                }
            }
        } else {
            match self.project.find_executable_in(&config.command, &folder) {
                Ok(Some(program)) => {
                    let started = self.project.spawn_language_server(
                        &program,
                        &config.args,
                        &folder,
                        open_log(language, root),
                    );
                    match started {
                        Ok((connection, incoming)) => {
                            let connection = Arc::new(connection);
                            slot._tasks = vec![
                                self.listen(key, incoming, cx),
                                self.initialize(key, &connection, &config, cx),
                            ];
                            slot.connection = Some(connection);
                        }
                        Err(err) => {
                            slot.status =
                                ServerStatus::Failed(format!("failed to start: {err}").into())
                        }
                    }
                }
                Ok(None) => {
                    slot.status = ServerStatus::NotFound {
                        command: config.command.clone().into(),
                        install_hint: config.install_hint,
                    };
                }
                Err(err) => {
                    slot.status = ServerStatus::Failed(format!("cannot look up: {err}").into())
                }
            }
        }
        if slot.connection.is_none() {
            slot.serves_all = Some(false);
        }
        slot.config = Some(config);
        self.slots.insert(key, slot);
        cx.emit(LspStoreEvent::StatusChanged(language));
    }

    fn initialize(
        &self,
        key: SlotKey,
        connection: &Arc<LanguageServer>,
        config: &ServerConfig,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        // Servers exit when this process dies; a Windows pid means nothing
        // (or another process) to a server running in WSL.
        let process_id = (!self.project.is_remote()).then(std::process::id);
        let params = initialize_params(
            server_folders(self.project.roots(), key.1),
            process_id,
            config,
        );
        let request = connection.request::<Initialize>(params);
        cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |store, cx| store.finish_start(key, result, cx))
                .ok();
        })
    }

    fn finish_start(
        &mut self,
        (language, root): SlotKey,
        result: Result<InitializeResult, lsp::Error>,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.slots.get_mut(&(language, root)) else {
            return;
        };
        let Some(connection) = slot.connection.clone() else {
            return;
        };
        match result {
            Ok(init) => {
                connection.notify::<notification::Initialized>(InitializedParams {});
                if let Some(info) = init.server_info {
                    slot.name = info.name.into();
                }
                let capabilities = init.capabilities;
                let encoding = match &capabilities.position_encoding {
                    Some(kind) if *kind == PositionEncodingKind::UTF8 => Encoding::Utf8,
                    _ => Encoding::Utf16,
                };
                let (sync, save_includes_text) = match &capabilities.text_document_sync {
                    Some(TextDocumentSyncCapability::Kind(kind)) => (*kind, false),
                    Some(TextDocumentSyncCapability::Options(options)) => (
                        options.change.unwrap_or(TextDocumentSyncKind::NONE),
                        matches!(
                            &options.save,
                            Some(TextDocumentSyncSaveOptions::SaveOptions(save))
                                if save.include_text == Some(true)
                        ),
                    ),
                    None => (TextDocumentSyncKind::NONE, false),
                };
                slot.serves_all = Some(serves_all_folders(&capabilities));
                let running = Arc::new(RunningServer {
                    connection,
                    encoding,
                    sync,
                    save_includes_text,
                    semantic_legend: semantic::Legend::from_capabilities(&capabilities),
                    capabilities,
                });
                slot.running = Some(running.clone());
                slot.status = ServerStatus::Ready;
                cx.emit(LspStoreEvent::ServerStarted(language, running));
            }
            Err(err) => {
                slot.serves_all = Some(false);
                slot.status = ServerStatus::Failed(err.to_string().into());
            }
        }
        cx.emit(LspStoreEvent::StatusChanged(language));
    }

    fn listen(
        &self,
        key: SlotKey,
        mut incoming: futures::channel::mpsc::UnboundedReceiver<Incoming>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            while let Some(message) = incoming.next().await {
                let exited = message == Incoming::Exited;
                if this
                    .update(cx, |store, cx| store.handle_incoming(key, message, cx))
                    .is_err()
                    || exited
                {
                    break;
                }
            }
        })
    }

    fn handle_incoming(
        &mut self,
        (language, root): SlotKey,
        message: Incoming,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.slots.get_mut(&(language, root)) else {
            return;
        };
        match message {
            Incoming::Request { id, method, params } => {
                let Some(connection) = &slot.connection else {
                    return;
                };
                let settings = slot.config.as_ref().map_or(&Value::Null, |c| &c.settings);
                let result = match method.as_str() {
                    // Items scoped to a folder get that folder's settings.
                    "workspace/configuration" => Ok(Value::Array(
                        params["items"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|item| {
                                let settings = serde_json::from_value::<lsp::types::Uri>(
                                    item["scopeUri"].clone(),
                                )
                                .ok()
                                .and_then(|uri| uri_to_path(&uri))
                                .and_then(|path| self.project.root_index_of(&path))
                                .and_then(|folder| slot.folder_settings.get(folder))
                                .unwrap_or(settings);
                                settings::section(settings, item["section"].as_str())
                            })
                            .collect(),
                    )),
                    "workspace/workspaceFolders" => Ok(json!(workspace_folders(server_folders(
                        self.project.roots(),
                        root
                    )))),
                    "window/workDoneProgress/create"
                    | "client/registerCapability"
                    | "client/unregisterCapability"
                    | "window/showMessageRequest" => Ok(Value::Null),
                    "workspace/semanticTokens/refresh" => {
                        cx.emit(LspStoreEvent::SemanticTokensRefresh(language));
                        Ok(Value::Null)
                    }
                    "workspace/inlayHint/refresh" => {
                        cx.emit(LspStoreEvent::InlayHintsRefresh(language));
                        Ok(Value::Null)
                    }
                    "workspace/applyEdit" => {
                        match serde_json::from_value::<lsp::types::ApplyWorkspaceEditParams>(params)
                        {
                            Ok(params) => {
                                let encoding = slot
                                    .running
                                    .as_ref()
                                    .map_or(Encoding::Utf16, |server| server.encoding);
                                cx.emit(LspStoreEvent::ApplyEdit(Arc::new(ApplyEditRequest {
                                    label: params.label,
                                    edit: params.edit,
                                    encoding,
                                    reply: Mutex::new(Some((connection.clone(), id))),
                                })));
                                return;
                            }
                            Err(err) => Err(json!({ "code": -32602, "message": err.to_string() })),
                        }
                    }
                    _ => {
                        Err(json!({ "code": -32601, "message": format!("unsupported: {method}") }))
                    }
                };
                connection.respond(id, result);
            }
            Incoming::Notification { method, params } => match method.as_str() {
                "$/progress" => {
                    let Ok(progress) = serde_json::from_value::<ProgressParams>(params) else {
                        return;
                    };
                    let token = match progress.token {
                        lsp::types::NumberOrString::Number(n) => n.to_string(),
                        lsp::types::NumberOrString::String(s) => s,
                    };
                    let ProgressParamsValue::WorkDone(work) = progress.value;
                    match work {
                        WorkDoneProgress::Begin(begin) => {
                            let text = progress_text(
                                &begin.title,
                                begin.message.as_deref(),
                                begin.percentage,
                            );
                            slot.progress.push((token, text));
                        }
                        WorkDoneProgress::Report(report) => {
                            if let Some(entry) = slot.progress.iter_mut().find(|(t, _)| *t == token)
                            {
                                let title =
                                    entry.1.split(':').next().unwrap_or_default().to_owned();
                                entry.1 = progress_text(
                                    &title,
                                    report.message.as_deref(),
                                    report.percentage,
                                );
                            }
                        }
                        WorkDoneProgress::End(_) => slot.progress.retain(|(t, _)| *t != token),
                    }
                    if slot.running.is_some() {
                        slot.status = match slot.progress.last() {
                            Some((_, text)) => ServerStatus::Busy(text.clone().into()),
                            None => ServerStatus::Ready,
                        };
                        cx.emit(LspStoreEvent::StatusChanged(language));
                    }
                }
                "textDocument/publishDiagnostics" => {
                    let Ok(published) = serde_json::from_value::<PublishDiagnosticsParams>(params)
                    else {
                        return;
                    };
                    if let Some(path) = uri_to_path(&published.uri) {
                        let diagnostics = match self.notebook(&path) {
                            Some(notebook) => {
                                let notebook = notebook.borrow();
                                // Positions of an older text may now be
                                // in another cell; the latest text's come.
                                if published
                                    .version
                                    .is_some_and(|version| version != notebook.version())
                                {
                                    return;
                                }
                                notebook.filter_diagnostics(published.diagnostics)
                            }
                            None => published.diagnostics,
                        };
                        if diagnostics.is_empty() {
                            // Servers publish an empty list for every file
                            // they fixed or closed: no entry stays for it.
                            self.diagnostics.remove(&path);
                        } else {
                            self.diagnostics.insert(path.clone(), diagnostics);
                        }
                        cx.emit(LspStoreEvent::DiagnosticsUpdated(path));
                    }
                }
                _ => {}
            },
            Incoming::Exited => {
                slot.connection = None;
                slot.running = None;
                slot.progress.clear();
                let restart = slot.restarts < MAX_RESTARTS;
                if restart {
                    slot.restarts += 1;
                } else {
                    slot.status = ServerStatus::Failed("exited repeatedly; see the log".into());
                }
                cx.emit(LspStoreEvent::ServerStopped(language));
                if restart {
                    self.start(language, root, cx);
                } else {
                    cx.emit(LspStoreEvent::StatusChanged(language));
                }
            }
        }
    }
}

/// The folders a server started for folder `root` is initialized with and
/// answers `workspace/workspaceFolders` with: every folder for the first
/// folder's (it may serve them all), else just its own.
/// The workflow language server (`workflow_lsp`) on a thread of this
/// process, over pipes; it ends when the client says `exit` or drops its
/// end.
fn start_workflow_server(
    logger: Option<lsp::Logger>,
) -> std::io::Result<(
    LanguageServer,
    futures::channel::mpsc::UnboundedReceiver<Incoming>,
)> {
    let (server_reader, client_writer) = std::io::pipe()?;
    let (client_reader, server_writer) = std::io::pipe()?;
    std::thread::Builder::new()
        .name("workflow-lsp".into())
        .spawn(move || workflow_lsp::serve(server_reader, server_writer))?;
    Ok(LanguageServer::new(client_reader, client_writer, logger))
}

fn server_folders(roots: &[Root], root: usize) -> &[Root] {
    match root {
        0 => roots,
        _ => &roots[root..=root],
    }
}

fn workspace_folders(folders: &[Root]) -> Vec<WorkspaceFolder> {
    folders
        .iter()
        .filter_map(|folder| {
            Some(WorkspaceFolder {
                uri: path_to_uri(&folder.path)?,
                name: folder.name.clone(),
            })
        })
        .collect()
}

/// `initialize` for a server of `folders` (the first is its root and
/// working directory).
#[allow(deprecated)] // root_uri: older servers still read it
fn initialize_params(
    folders: &[Root],
    process_id: Option<u32>,
    config: &ServerConfig,
) -> InitializeParams {
    let folders = workspace_folders(folders);
    InitializeParams {
        process_id,
        root_uri: folders.first().map(|folder| folder.uri.clone()),
        workspace_folders: (!folders.is_empty()).then_some(folders),
        capabilities: client_capabilities(),
        initialization_options: (!config.initialization_options.is_null())
            .then(|| config.initialization_options.clone()),
        client_info: Some(lsp::types::ClientInfo {
            name: "next_code".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
        }),
        ..Default::default()
    }
}

/// Whether a server advertised `workspace.workspaceFolders.supported`.
fn serves_all_folders(capabilities: &ServerCapabilities) -> bool {
    capabilities
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.workspace_folders.as_ref())
        .and_then(|folders| folders.supported)
        .unwrap_or(false)
}

/// Which folder's server serves a document in folder `document_root`
/// (`None` outside every folder): the first folder's, unless the project
/// has several and that server does not serve them all (`first_serves_all`,
/// `None` while it starts); then the document's own folder's. `None`:
/// undecided until the first server answers.
fn serving_root(
    multi_root: bool,
    document_root: Option<usize>,
    first_serves_all: Option<bool>,
) -> Option<usize> {
    match document_root {
        Some(root) if multi_root && root > 0 => match first_serves_all? {
            true => Some(0),
            false => Some(root),
        },
        _ => Some(0),
    }
}

/// Where a diagnostic comes from, as VS Code shows it: `rustc(E0308)`,
/// the source or the code alone, or `None`.
pub fn diagnostic_origin(diagnostic: &lsp::types::Diagnostic) -> Option<String> {
    let code = diagnostic.code.as_ref().map(|code| match code {
        lsp::types::NumberOrString::Number(number) => number.to_string(),
        lsp::types::NumberOrString::String(text) => text.clone(),
    });
    match (&diagnostic.source, code) {
        (Some(source), Some(code)) => Some(format!("{source}({code})")),
        (None, Some(code)) => Some(code),
        (Some(source), None) => Some(source.clone()),
        (None, None) => None,
    }
}

fn progress_text(title: &str, message: Option<&str>, percentage: Option<u32>) -> String {
    let mut text = title.to_owned();
    if let Some(message) = message.filter(|m| !m.is_empty()) {
        text = format!("{text}: {message}");
    }
    if let Some(percentage) = percentage {
        text = format!("{text} {percentage}%");
    }
    text
}

/// What this client supports; grows with each LSP feature.
fn client_capabilities() -> ClientCapabilities {
    serde_json::from_value(json!({
        "general": { "positionEncodings": ["utf-8", "utf-16"] },
        "textDocument": {
            "synchronization": { "didSave": true },
            "publishDiagnostics": {
                "relatedInformation": true,
                "versionSupport": true,
                "tagSupport": { "valueSet": [1, 2] },
            },
            "hover": { "contentFormat": ["markdown", "plaintext"] },
            "definition": { "linkSupport": true },
            "declaration": { "linkSupport": true },
            "typeDefinition": { "linkSupport": true },
            "implementation": { "linkSupport": true },
            "references": {},
            "signatureHelp": {
                "contextSupport": true,
                "signatureInformation": {
                    "documentationFormat": ["markdown", "plaintext"],
                    "parameterInformation": { "labelOffsetSupport": true },
                    "activeParameterSupport": true,
                },
            },
            "inlayHint": {},
            "documentSymbol": {
                "hierarchicalDocumentSymbolSupport": true,
                "symbolKind": { "valueSet": (1..=26).collect::<Vec<u32>>() },
            },
            "callHierarchy": {},
            "completion": {
                "contextSupport": true,
                "completionItem": {
                    "snippetSupport": true,
                    "insertReplaceSupport": true,
                    "labelDetailsSupport": true,
                    "deprecatedSupport": true,
                    "documentationFormat": ["markdown", "plaintext"],
                    // Filled lazily via completionItem/resolve. Declaring
                    // additionalTextEdits makes rust-analyzer offer auto-imports.
                    "resolveSupport": {
                        "properties": ["documentation", "detail", "additionalTextEdits"],
                    },
                },
            },
            "semanticTokens": {
                // Deltas would only save transfer: decoding is O(tokens) either way.
                "requests": { "full": true },
                "tokenTypes": semantic::TOKEN_TYPES.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
                "tokenModifiers": semantic::TOKEN_MODIFIERS,
                "formats": ["relative"],
                "overlappingTokenSupport": false,
                "multilineTokenSupport": false,
                "augmentsSyntaxTokens": true,
            },
            "codeAction": {
                "codeActionLiteralSupport": {
                    "codeActionKind": {
                        "valueSet": [
                            "", "quickfix", "refactor", "refactor.extract", "refactor.inline",
                            "refactor.rewrite", "source", "source.organizeImports",
                            "source.fixAll",
                        ],
                    },
                },
                "isPreferredSupport": true,
                "dataSupport": true,
                "resolveSupport": { "properties": ["edit"] },
            },
            "rename": { "prepareSupport": true },
            "formatting": {},
            "rangeFormatting": {},
        },
        "workspace": {
            "applyEdit": true,
            "workspaceEdit": {
                "documentChanges": true,
                "resourceOperations": ["create", "rename", "delete"],
                // Versioned edits are checked before anything changes; any
                // other failure stops the remaining operations.
                "failureHandling": "abort",
                "normalizesLineEndings": true,
            },
            "executeCommand": {},
            "configuration": true,
            "workspaceFolders": true,
            "didChangeWatchedFiles": { "dynamicRegistration": true },
            "semanticTokens": { "refreshSupport": true },
            "inlayHint": { "refreshSupport": true },
            "symbol": { "symbolKind": { "valueSet": (1..=26).collect::<Vec<u32>>() } },
            // Renames and moves from the Files tree (`file_ops`).
            "fileOperations": { "willRename": true, "didRename": true },
        },
        "window": { "workDoneProgress": true },
    }))
    .expect("valid client capabilities")
}

/// Directory holding one traffic log per language server.
pub fn log_dir() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("next_code").join("logs"))
}

/// Truncates and opens `lsp-<language>.log` (`lsp-<language>-<root>.log`
/// for a server of a folder other than the first); logging is skipped if
/// that fails.
fn open_log(language: Language, root: usize) -> Option<Logger> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let name = match root {
        0 => format!("lsp-{}.log", language.id()),
        _ => format!("lsp-{}-{root}.log", language.id()),
    };
    let file = File::create(dir.join(name)).ok()?;
    let file = Mutex::new(LineWriter::new(file));
    Some(Arc::new(move |kind, line| {
        let prefix = match kind {
            LogKind::Sent => "-->",
            LogKind::Received => "<--",
            LogKind::Stderr => "!!!",
        };
        writeln!(file.lock(), "{prefix} {line}").ok();
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<Root> {
        ["api", "web"]
            .into_iter()
            .map(|name| Root {
                path: std::env::temp_dir().join("next_code_lsp_roots").join(name),
                name: format!("{name} service"),
                custom_name: Some(format!("{name} service")),
            })
            .collect()
    }

    #[test]
    fn initialize_names_every_folder_and_roots_at_the_first() {
        let roots = roots();
        let config = Settings::default().server_config(Language::Rust).unwrap();
        let params = initialize_params(server_folders(&roots, 0), Some(7), &config);
        let folders = params.workspace_folders.unwrap();
        let named: Vec<(&str, PathBuf)> = folders
            .iter()
            .map(|folder| (folder.name.as_str(), uri_to_path(&folder.uri).unwrap()))
            .collect();
        assert_eq!(
            named,
            [
                ("api service", roots[0].path.clone()),
                ("web service", roots[1].path.clone()),
            ]
        );
        #[allow(deprecated)]
        let root_uri = params.root_uri.unwrap();
        assert_eq!(uri_to_path(&root_uri).unwrap(), roots[0].path);
        assert_eq!(params.process_id, Some(7));
    }

    #[test]
    fn a_folder_server_is_initialized_with_its_folder_only() {
        let roots = roots();
        let config = Settings::default().server_config(Language::Rust).unwrap();
        let params = initialize_params(server_folders(&roots, 1), None, &config);
        let folders = params.workspace_folders.unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "web service");
        #[allow(deprecated)]
        let root_uri = params.root_uri.unwrap();
        assert_eq!(uri_to_path(&root_uri).unwrap(), roots[1].path);
    }

    #[test]
    fn workspace_folder_support_comes_from_the_capabilities() {
        let capabilities =
            |value: Value| -> ServerCapabilities { serde_json::from_value(value).unwrap() };
        assert!(serves_all_folders(&capabilities(
            json!({ "workspace": { "workspaceFolders": { "supported": true } } })
        )));
        assert!(!serves_all_folders(&capabilities(
            json!({ "workspace": { "workspaceFolders": { "supported": false } } })
        )));
        assert!(!serves_all_folders(&capabilities(
            json!({ "workspace": { "fileOperations": {} } })
        )));
        assert!(!serves_all_folders(&capabilities(json!({}))));
    }

    #[test]
    fn documents_route_to_their_folders_server_only_without_folder_support() {
        // One folder: always the first server, whatever it supports.
        for support in [None, Some(false), Some(true)] {
            assert_eq!(serving_root(false, Some(0), support), Some(0));
            assert_eq!(serving_root(false, None, support), Some(0));
        }
        // Several folders, the server serves them all.
        assert_eq!(serving_root(true, Some(2), Some(true)), Some(0));
        // It does not (or could not start): each folder its own.
        assert_eq!(serving_root(true, Some(2), Some(false)), Some(2));
        assert_eq!(serving_root(true, Some(1), Some(false)), Some(1));
        // Undecided while the first server starts.
        assert_eq!(serving_root(true, Some(1), None), None);
        // The first folder and files outside every folder: the first server.
        for support in [None, Some(false), Some(true)] {
            assert_eq!(serving_root(true, Some(0), support), Some(0));
            assert_eq!(serving_root(true, None, support), Some(0));
        }
    }
}
