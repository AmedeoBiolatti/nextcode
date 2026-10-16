//! Headless refactoring and formatting: `rename`, `code_actions` /
//! `apply_code_action`, `format`, and servers' `workspace/applyEdit`
//! requests. Edits are written to the files on disk; open documents are
//! synced to their servers (didChange + didSave).

use lsp::types::{
    CodeActionContext, CodeActionOrCommand, CodeActionParams, CodeActionTriggerKind,
    FormattingOptions, RenameParams, WorkspaceEdit,
    request::{CodeActionRequest, Rename},
};

use super::*;
use crate::{
    editor::{execute_command, format, resolve_code_action},
    lsp_store::ApplyEditRequest,
    workspace_edit::{self, Documents, Failure, FileTargets, Report},
};

/// The session's open documents as targets of a workspace edit: their
/// files are rewritten and the new text synced.
struct OpenDocuments<'a, 'b> {
    session: &'a mut Session,
    cx: &'a mut Context<'b, Session>,
}

impl Documents for OpenDocuments<'_, '_> {
    fn version(&mut self, path: &Path) -> Option<i32> {
        self.session
            .documents
            .get(path)
            .map(|document| document.version)
    }

    fn text(&mut self, path: &Path) -> Option<String> {
        self.session
            .documents
            .get(path)
            .map(|document| document.text.clone())
    }

    fn edit(
        &mut self,
        path: &Path,
        edits: &[lsp::types::TextEdit],
        encoding: Encoding,
    ) -> Option<Result<(), String>> {
        let text = &self.session.documents.get(path)?.text;
        let edited = match workspace_edit::edit_text(text, edits, encoding) {
            Ok(edited) => edited,
            Err(err) => return Some(Err(err)),
        };
        if let Err(err) = self
            .session
            .project
            .write(path, edited.clone().into_bytes())
        {
            return Some(Err(err.to_string()));
        }
        self.session.sync_document(path, edited, self.cx);
        Some(Ok(()))
    }

    fn renamed(&mut self, from: &Path, to: &Path) {
        let moved: Vec<PathBuf> = self
            .session
            .documents
            .keys()
            .filter(|path| path.starts_with(from))
            .cloned()
            .collect();
        for old in moved {
            let Some(mut document) = self.session.documents.remove(&old) else {
                continue;
            };
            let rest = old.strip_prefix(from).unwrap_or(Path::new(""));
            let new = if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                self.session.project.join(to, &rest.to_string_lossy())
            };
            let server = self.session.running(document.language, self.cx);
            if let Some(server) = &server {
                close(server, &document);
            }
            let Some(uri) = lsp::path_to_uri(&new) else {
                continue;
            };
            document.uri = uri;
            if let Some(server) = &server {
                did_open(server, &document);
            }
            self.session.documents.insert(new, document);
        }
    }

    fn deleted(&mut self, path: &Path) {
        let gone: Vec<PathBuf> = self
            .session
            .documents
            .keys()
            .filter(|open| open.starts_with(path))
            .cloned()
            .collect();
        for path in gone {
            if let Some(document) = self.session.documents.remove(&path)
                && let Some(server) = self.session.running(document.language, self.cx)
            {
                close(&server, &document);
            }
        }
    }
}

fn close(server: &RunningServer, document: &Document) {
    server
        .connection
        .notify::<notification::DidCloseTextDocument>(DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier::new(document.uri.clone()),
        });
}

pub(super) fn report_json(report: &Report) -> Value {
    json!({
        "applied": report.failure.is_none(),
        "changed": report.changed.iter().map(|path| path_json(path)).collect::<Vec<_>>(),
        "failure": report.failure.as_ref().map(|failure| json!({
            "index": failure.index,
            "path": path_json(&failure.path),
            "message": failure.message,
        })),
    })
}

fn action_json(index: usize, action: &CodeActionOrCommand) -> Value {
    use crate::editor::code_action_info;
    let (title, kind, preferred) = code_action_info(action);
    json!({ "index": index, "title": title, "kind": kind, "preferred": preferred })
}

impl Session {
    /// Open documents of `from` (or inside it) now live at `to`: reopened
    /// under the new URI with their servers.
    pub(super) fn documents_renamed(&mut self, from: &Path, to: &Path, cx: &mut Context<Self>) {
        OpenDocuments { session: self, cx }.renamed(from, to);
    }

    /// Open documents of `path` (or inside it) are gone: closed.
    pub(super) fn documents_deleted(&mut self, path: &Path, cx: &mut Context<Self>) {
        OpenDocuments { session: self, cx }.deleted(path);
    }

    pub(super) fn apply_edit(
        &mut self,
        edit: WorkspaceEdit,
        encoding: Encoding,
        cx: &mut Context<Self>,
    ) -> Report {
        let operations = match workspace_edit::operations(edit) {
            Ok(operations) => operations,
            Err(message) => {
                return Report {
                    changed: Vec::new(),
                    failure: Some(Failure {
                        index: 0,
                        path: self.project.root().to_path_buf(),
                        message,
                    }),
                };
            }
        };
        let project = self.project.clone();
        let mut documents = OpenDocuments { session: self, cx };
        workspace_edit::apply(&project, &operations, encoding, &mut documents)
    }

    /// A server's `workspace/applyEdit`: applied, answered, and reported as
    /// an `applied_edit` event.
    pub(super) fn apply_edit_request(
        &mut self,
        request: &ApplyEditRequest,
        cx: &mut Context<Self>,
    ) {
        let report = self.apply_edit(request.edit.clone(), request.encoding, cx);
        request.respond(report.response());
        let mut event = report_json(&report);
        event["label"] = json!(request.label);
        self.out.event("applied_edit", event);
    }

    /// `replace`: searches like `search` (only files under `paths`, when
    /// given) and replaces every match with `replacement`; open documents
    /// are edited from their text and synced, files changed meanwhile are
    /// skipped.
    pub(super) fn replace(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let query = search_query(request);
        let replacer = match param_str(request, "replacement")
            .and_then(|text| search::Replacer::new(&query, text).map_err(|err| err.to_string()))
        {
            Ok(replacer) => replacer,
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let paths: Vec<PathBuf> = strings(request, "paths")
            .iter()
            .map(|path| self.project.path(path))
            .collect();
        let started = self.project.search(&query, SearchLimits::default());
        cx.spawn(async move |this, cx| {
            let (_handle, mut events) = match started {
                Ok(started) => started,
                Err(err) => {
                    this.update(cx, |this, _| this.out.reply(&id, Err(err)))
                        .ok();
                    return;
                }
            };
            let mut files = Vec::new();
            let mut limit_hit = None;
            while let Some(event) = events.next().await {
                match event {
                    SearchEvent::File(file) => {
                        if paths.is_empty() || paths.iter().any(|dir| file.path.starts_with(dir)) {
                            files.push(FileTargets {
                                targets: file
                                    .lines
                                    .iter()
                                    .flat_map(search::Target::of_line)
                                    .map(|(_, target)| target)
                                    .collect(),
                                path: file.path,
                            });
                        }
                    }
                    SearchEvent::Done { limit_hit: hit, .. } => {
                        limit_hit = Some(hit);
                        break;
                    }
                }
            }
            this.update(cx, |this, cx| {
                let Some(limit_hit) = limit_hit else {
                    return this.out.reply(
                        &id,
                        Err("the search ended early (agent disconnected?)".into()),
                    );
                };
                let project = this.project.clone();
                let mut documents = OpenDocuments { session: this, cx };
                let report = workspace_edit::replace(&project, &replacer, &files, &mut documents);
                let pairs = |list: &[(PathBuf, String)], key: &str| -> Vec<Value> {
                    list.iter()
                        .map(|(path, text)| json!({ "path": path_json(path), key: text }))
                        .collect()
                };
                this.out.reply(
                    &id,
                    Ok(json!({
                        "files": report.replaced.iter().map(|(path, count)| json!({
                            "path": path_json(path),
                            "replacements": count,
                        })).collect::<Vec<_>>(),
                        "replacements": report.matches(),
                        "skipped": pairs(&report.skipped, "reason"),
                        "failed": pairs(&report.failed, "error"),
                        "limit_hit": limit_hit,
                    })),
                );
            })
            .ok();
        })
        .detach();
    }

    /// `rename`: `path`, `line`, `character`, `new_name`.
    pub(super) fn rename(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let (position, new_name) = match (position(request), param_str(request, "new_name")) {
            (Ok(position), Ok(name)) => (position, name.to_owned()),
            (Err(err), _) | (_, Err(err)) => return self.out.reply(&id, Err(err)),
        };
        let Some(document) = self.document(&id, request, cx) else {
            return;
        };
        let params = RenameParams {
            text_document_position: TextDocumentPositionParams::new(
                TextDocumentIdentifier::new(document.uri),
                position,
            ),
            new_name,
            work_done_progress_params: Default::default(),
        };
        let server = document.server;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let result = request_retrying::<Rename>(
                &server,
                params,
                &executor,
                10,
                Duration::from_millis(500),
            )
            .await;
            this.update(cx, |this, cx| match result {
                Ok(Some(edit)) => {
                    let report = this.apply_edit(edit, server.encoding, cx);
                    this.out.reply(&id, Ok(report_json(&report)));
                }
                Ok(None) => this.out.reply(&id, Err("nothing to rename here".into())),
                Err(err) => this.out.reply(&id, Err(err.to_string())),
            })
            .ok();
        })
        .detach();
    }

    /// `code_actions`: `path`, `line`, `character`, optional `end_line` /
    /// `end_character`; the diagnostics touching the range go along.
    pub(super) fn code_actions(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let start = match position(request) {
            Ok(position) => position,
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let end = match (
            request["end_line"].as_u64(),
            request["end_character"].as_u64(),
        ) {
            (Some(line), Some(character)) => Position::new(line as u32, character as u32),
            _ => start,
        };
        let Some(document) = self.document(&id, request, cx) else {
            return;
        };
        let path = self
            .project
            .path(request["path"].as_str().unwrap_or_default());
        let range = lsp::types::Range::new(start, end);
        let key = |position: &Position| (position.line, position.character);
        let diagnostics = self
            .store
            .read(cx)
            .diagnostics(&path)
            .iter()
            .filter(|d| key(&d.range.start) <= key(&end) && key(&start) <= key(&d.range.end))
            .cloned()
            .collect();
        let params = CodeActionParams {
            text_document: TextDocumentIdentifier::new(document.uri),
            range,
            context: CodeActionContext {
                diagnostics,
                only: None,
                trigger_kind: Some(CodeActionTriggerKind::INVOKED),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let server = document.server;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let result = request_retrying::<CodeActionRequest>(
                &server,
                params,
                &executor,
                10,
                Duration::from_millis(500),
            )
            .await;
            this.update(cx, |this, _| match result {
                Ok(actions) => {
                    let actions = crate::editor::usable_code_actions(actions.unwrap_or_default());
                    let listed: Vec<Value> = actions
                        .iter()
                        .enumerate()
                        .map(|(index, action)| action_json(index, action))
                        .collect();
                    this.code_actions = Some((server, actions));
                    this.out.reply(&id, Ok(json!(listed)));
                }
                Err(err) => this.out.reply(&id, Err(err.to_string())),
            })
            .ok();
        })
        .detach();
    }

    /// `apply_code_action`: `index` into the last `code_actions` answer.
    pub(super) fn apply_code_action(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let Some((server, actions)) = &self.code_actions else {
            return self.out.reply(&id, Err("send `code_actions` first".into()));
        };
        let index = request["index"].as_u64().unwrap_or(0) as usize;
        let Some(action) = actions.get(index).cloned() else {
            return self.out.reply(&id, Err(format!("no code action {index}")));
        };
        let server = server.clone();
        cx.spawn(async move |this, cx| {
            let (edit, command) = match action {
                CodeActionOrCommand::Command(command) => (None, Some(command)),
                CodeActionOrCommand::CodeAction(action) => {
                    match resolve_code_action(&server, action).await {
                        Ok(action) => (action.edit, action.command),
                        Err(err) => {
                            this.update(cx, |this, _| this.out.reply(&id, Err(err)))
                                .ok();
                            return;
                        }
                    }
                }
            };
            let Ok(report) = this.update(cx, |this, cx| match edit {
                Some(edit) => this.apply_edit(edit, server.encoding, cx),
                None => Report::default(),
            }) else {
                return;
            };
            let command_result = match command.filter(|_| report.failure.is_none()) {
                Some(command) => execute_command(&server, command).await,
                None => Ok(()),
            };
            this.update(cx, |this, _| match command_result {
                Ok(()) => this.out.reply(&id, Ok(report_json(&report))),
                Err(err) => this.out.reply(&id, Err(format!("command failed: {err}"))),
            })
            .ok();
        })
        .detach();
    }

    /// `format`: formats the file like Format Document (the configured
    /// formatter, else the language server, which needs the file `open`),
    /// writes it and replies `{text, changed}`.
    pub(super) fn format(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let path = match param_str(request, "path") {
            Ok(path) => self.project.path(path),
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let Some(language) = Language::from_path(&path) else {
            return self
                .out
                .reply(&id, Err(format!("no language for {}", path.display())));
        };
        let open = self
            .documents
            .get(&path)
            .map(|document| (document.uri.clone(), document.text.clone()));
        let server = open.as_ref().and_then(|_| self.running(language, cx));
        let settings = self.store.read(cx).settings();
        let plan = match format::plan(&settings, language, server) {
            Ok(plan) => plan,
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let project = self.project.clone();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let (uri, text) = match open {
                Some((uri, text)) => (Some(uri), text),
                None => {
                    let (project, path) = (project.clone(), path.clone());
                    match cx
                        .background_spawn(async move { project.read_text(&path) })
                        .await
                    {
                        Ok(text) => (None, text),
                        Err(err) => {
                            this.update(cx, |this, _| this.out.reply(&id, Err(err.to_string())))
                                .ok();
                            return;
                        }
                    }
                }
            };
            let job = format::Job {
                path: path.clone(),
                uri,
                text: text.clone(),
                range: None,
                options: FormattingOptions {
                    tab_size: 4,
                    insert_spaces: true,
                    ..FormattingOptions::default()
                },
            };
            let result = format::run(plan, Some(project.clone()), job, executor).await;
            let formatted = match result {
                Ok(formatted) => formatted,
                Err(err) => {
                    this.update(cx, |this, _| this.out.reply(&id, Err(err)))
                        .ok();
                    return;
                }
            };
            let changed = formatted != text;
            if changed {
                let (project, write_path, data) = (project, path.clone(), formatted.clone());
                let written = cx
                    .background_spawn(async move { project.write(&write_path, data.into_bytes()) })
                    .await;
                if let Err(err) = written {
                    this.update(cx, |this, _| this.out.reply(&id, Err(err.to_string())))
                        .ok();
                    return;
                }
            }
            this.update(cx, |this, cx| {
                if changed {
                    this.sync_document(&path, formatted.clone(), cx);
                }
                this.out
                    .reply(&id, Ok(json!({ "text": formatted, "changed": changed })));
            })
            .ok();
        })
        .detach();
    }
}
