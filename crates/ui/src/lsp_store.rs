//! Starts, supervises and answers language servers for one project.

use std::{
    collections::HashMap,
    fs::File,
    io::{LineWriter, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use futures::StreamExt;
use gpui::{Context, EventEmitter, SharedString, Task};
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

use crate::settings::{self, ServerConfig, Settings};

/// Restarts allowed per server before giving up for the session.
const MAX_RESTARTS: u32 = 3;

/// An initialized server, shared with the editors that sync documents to it.
pub struct RunningServer {
    pub connection: Arc<LanguageServer>,
    pub encoding: Encoding,
    pub sync: TextDocumentSyncKind,
    pub save_includes_text: bool,
    pub capabilities: ServerCapabilities,
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
    ServerStarted(Language),
    ServerStopped(Language),
    StatusChanged(Language),
    DiagnosticsUpdated(PathBuf),
}

struct Slot {
    name: SharedString,
    status: ServerStatus,
    config: Option<ServerConfig>,
    connection: Option<Arc<LanguageServer>>,
    running: Option<Arc<RunningServer>>,
    /// Active work-done progress by token, in start order.
    progress: Vec<(String, String)>,
    restarts: u32,
    _tasks: Vec<Task<()>>,
}

pub struct LspStore {
    root: PathBuf,
    settings: Arc<Settings>,
    slots: HashMap<Language, Slot>,
    diagnostics: HashMap<PathBuf, Vec<lsp::types::Diagnostic>>,
}

impl EventEmitter<LspStoreEvent> for LspStore {}

impl LspStore {
    pub fn new(root: PathBuf, settings: Arc<Settings>) -> Self {
        Self {
            root,
            settings,
            slots: HashMap::new(),
            diagnostics: HashMap::new(),
        }
    }

    /// The running server for `language`, starting it on first use.
    pub fn server(
        &mut self,
        language: Language,
        cx: &mut Context<Self>,
    ) -> Option<Arc<RunningServer>> {
        if !self.slots.contains_key(&language) {
            self.start(language, cx);
        }
        self.slots.get(&language)?.running.clone()
    }

    /// Server name and status, for the status bar.
    pub fn status(&self, language: Language) -> Option<(SharedString, ServerStatus)> {
        self.slots
            .get(&language)
            .map(|slot| (slot.name.clone(), slot.status.clone()))
    }

    pub fn diagnostics(&self, path: &Path) -> &[lsp::types::Diagnostic] {
        self.diagnostics.get(path).map_or(&[], Vec::as_slice)
    }

    /// Tells every running server about files changed outside the editor.
    /// Servers may register narrower globs; all project changes are sent,
    /// which servers accept and filter themselves.
    pub fn notify_file_changes(&self, events: &[watcher::FileEvent]) {
        use lsp::types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};
        let changes: Vec<FileEvent> = events
            .iter()
            .filter_map(|event| {
                let typ = match event.kind {
                    watcher::ChangeKind::Created => FileChangeType::CREATED,
                    watcher::ChangeKind::Modified => FileChangeType::CHANGED,
                    watcher::ChangeKind::Removed => FileChangeType::DELETED,
                };
                Some(FileEvent::new(path_to_uri(&event.path)?, typ))
            })
            .collect();
        if changes.is_empty() {
            return;
        }
        for server in self.slots.values().filter_map(|slot| slot.running.as_ref()) {
            server
                .connection
                .notify::<notification::DidChangeWatchedFiles>(DidChangeWatchedFilesParams {
                    changes: changes.clone(),
                });
        }
    }

    /// Every file's diagnostics, in no particular order.
    pub fn all_diagnostics(&self) -> impl Iterator<Item = (&Path, &[lsp::types::Diagnostic])> {
        self.diagnostics
            .iter()
            .map(|(path, diagnostics)| (path.as_path(), diagnostics.as_slice()))
    }

    fn start(&mut self, language: Language, cx: &mut Context<Self>) {
        let restarts = self.slots.get(&language).map_or(0, |slot| slot.restarts);
        let Some(mut config) = self.settings.server_config(language) else {
            return;
        };
        if language == Language::Python
            && let Some(python) = settings::python_interpreter(&self.root)
        {
            settings::merge(
                &mut config.settings,
                json!({ "python": { "pythonPath": python } }),
            );
        }
        let mut slot = Slot {
            name: config.command.clone().into(),
            status: ServerStatus::Starting,
            config: None,
            connection: None,
            running: None,
            progress: Vec::new(),
            restarts,
            _tasks: Vec::new(),
        };

        if !config.enabled {
            slot.status = ServerStatus::Disabled;
        } else if let Some(program) = settings::find_executable(&config.command, &self.root) {
            let mut command = Command::new(program);
            command.args(&config.args).current_dir(&self.root);
            #[cfg(windows)]
            {
                // Don't flash a console window from a GUI process.
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            match LanguageServer::spawn(&mut command, open_log(language)) {
                Ok((connection, incoming)) => {
                    let connection = Arc::new(connection);
                    slot._tasks = vec![
                        self.listen(language, incoming, cx),
                        self.initialize(language, &connection, &config, cx),
                    ];
                    slot.connection = Some(connection);
                }
                Err(err) => {
                    slot.status = ServerStatus::Failed(format!("failed to start: {err}").into())
                }
            }
        } else {
            slot.status = ServerStatus::NotFound {
                command: config.command.clone().into(),
                install_hint: config.install_hint,
            };
        }
        slot.config = Some(config);
        self.slots.insert(language, slot);
        cx.emit(LspStoreEvent::StatusChanged(language));
    }

    fn initialize(
        &self,
        language: Language,
        connection: &Arc<LanguageServer>,
        config: &ServerConfig,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let root_uri = path_to_uri(&self.root);
        let folder_name = self
            .root
            .file_name()
            .map_or_else(|| "root".into(), |name| name.to_string_lossy().into_owned());
        #[allow(deprecated)] // root_uri: older servers still read it
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: root_uri.clone(),
            workspace_folders: root_uri.map(|uri| {
                vec![WorkspaceFolder {
                    uri,
                    name: folder_name,
                }]
            }),
            capabilities: client_capabilities(),
            initialization_options: (!config.initialization_options.is_null())
                .then(|| config.initialization_options.clone()),
            client_info: Some(lsp::types::ClientInfo {
                name: "next_code".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            ..Default::default()
        };
        let request = connection.request::<Initialize>(params);
        cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |store, cx| store.finish_start(language, result, cx))
                .ok();
        })
    }

    fn finish_start(
        &mut self,
        language: Language,
        result: Result<InitializeResult, lsp::Error>,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.slots.get_mut(&language) else {
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
                slot.running = Some(Arc::new(RunningServer {
                    connection,
                    encoding,
                    sync,
                    save_includes_text,
                    capabilities,
                }));
                slot.status = ServerStatus::Ready;
                cx.emit(LspStoreEvent::ServerStarted(language));
            }
            Err(err) => slot.status = ServerStatus::Failed(err.to_string().into()),
        }
        cx.emit(LspStoreEvent::StatusChanged(language));
    }

    fn listen(
        &self,
        language: Language,
        mut incoming: futures::channel::mpsc::UnboundedReceiver<Incoming>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            while let Some(message) = incoming.next().await {
                let exited = message == Incoming::Exited;
                if this
                    .update(cx, |store, cx| store.handle_incoming(language, message, cx))
                    .is_err()
                    || exited
                {
                    break;
                }
            }
        })
    }

    fn handle_incoming(&mut self, language: Language, message: Incoming, cx: &mut Context<Self>) {
        let Some(slot) = self.slots.get_mut(&language) else {
            return;
        };
        match message {
            Incoming::Request { id, method, params } => {
                let Some(connection) = &slot.connection else {
                    return;
                };
                let settings = slot.config.as_ref().map_or(&Value::Null, |c| &c.settings);
                let result = match method.as_str() {
                    "workspace/configuration" => Ok(Value::Array(
                        params["items"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|item| settings::section(settings, item["section"].as_str()))
                            .collect(),
                    )),
                    "workspace/workspaceFolders" => Ok(json!([{
                        "uri": path_to_uri(&self.root),
                        "name": self.root.file_name().map(|n| n.to_string_lossy().into_owned()),
                    }])),
                    "window/workDoneProgress/create"
                    | "client/registerCapability"
                    | "client/unregisterCapability"
                    | "window/showMessageRequest" => Ok(Value::Null),
                    "workspace/applyEdit" => Ok(json!({ "applied": false })),
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
                        self.diagnostics.insert(path.clone(), published.diagnostics);
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
                    self.start(language, cx);
                } else {
                    cx.emit(LspStoreEvent::StatusChanged(language));
                }
            }
        }
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
            "references": {},
            "completion": {
                "contextSupport": true,
                "completionItem": {
                    // Snippet placeholders are not supported yet: servers send plain text.
                    "snippetSupport": false,
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
        },
        "workspace": {
            "configuration": true,
            "workspaceFolders": true,
            "didChangeWatchedFiles": { "dynamicRegistration": true },
        },
        "window": { "workDoneProgress": true },
    }))
    .expect("valid client capabilities")
}

/// Directory holding one traffic log per language server.
pub fn log_dir() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("next_code").join("logs"))
}

/// Truncates and opens `lsp-<language>.log`; logging is skipped if that fails.
fn open_log(language: Language) -> Option<Logger> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let file = File::create(dir.join(format!("lsp-{}.log", language.id()))).ok()?;
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
