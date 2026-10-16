//! Headless file operations, the Files tree's: `create_file`, `create_dir`,
//! `rename_path` (with the running language servers' `willRenameFiles`
//! edits and open documents following), `copy_path`, `delete_path`.

use futures::future::{Either, select};

use super::*;
use crate::file_ops::{self, NewKind};

/// How long `rename_path` waits for each language server's edits.
const WILL_RENAME_TIMEOUT: Duration = Duration::from_secs(5);

/// The last component of `path` is a valid name where the project lives.
fn check_last_name(project: &Project, path: &Path) -> io::Result<()> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    file_ops::check_name(&name, file_ops::windows_names(project))
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))
}

impl Session {
    pub(super) fn files(&mut self, id: Value, cmd: &str, request: &Value, cx: &mut Context<Self>) {
        match cmd {
            "create_file" | "create_dir" => {
                let path = match param_str(request, "path") {
                    Ok(path) => self.path(request, path),
                    Err(err) => return self.out.reply(&id, Err(err)),
                };
                let kind = if cmd == "create_dir" {
                    NewKind::Folder
                } else {
                    NewKind::File
                };
                self.blocking(id, cx, move |project| {
                    check_last_name(project, &path)?;
                    let path = file_ops::create_at(project, &path, kind)?;
                    Ok(json!({ "path": path_json(&path) }))
                });
            }
            "copy_path" => {
                let path = match param_str(request, "path") {
                    Ok(path) => self.path(request, path),
                    Err(err) => return self.out.reply(&id, Err(err)),
                };
                let dir = match request["dir"].as_str() {
                    Some(dir) => self.path(request, dir),
                    None => path.parent().unwrap_or(self.project.root()).to_path_buf(),
                };
                self.blocking(id, cx, move |project| {
                    let copies = file_ops::copy_into(project, std::slice::from_ref(&path), &dir)?;
                    Ok(json!({ "path": copies.first().map(|copy| path_json(copy)) }))
                });
            }
            "delete_path" => self.delete_path(id, request, cx),
            _ => self.rename_path(id, request, cx),
        }
    }

    /// `delete_path`: `path` or `paths`, `trash`?; open documents close.
    fn delete_path(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let mut paths: Vec<PathBuf> = request["paths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|path| self.path(request, path))
            .collect();
        if let Some(path) = request["path"].as_str() {
            paths.push(self.path(request, path));
        }
        if paths.is_empty() {
            return self.out.reply(
                &id,
                Err("missing string parameter `path` (or `paths`)".into()),
            );
        }
        let trash = request["trash"].as_bool().unwrap_or(false);
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let deleted = cx
                .background_spawn(async move {
                    for path in &paths {
                        if project.kind(path)?.is_none() {
                            return Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("{} does not exist", path.display()),
                            ));
                        }
                    }
                    file_ops::delete(&project, &paths, trash).map(|()| paths)
                })
                .await;
            this.update(cx, |this, cx| match deleted {
                Ok(paths) => {
                    for path in &paths {
                        this.documents_deleted(path, cx);
                    }
                    this.out.reply(&id, Ok(Value::Null));
                }
                Err(err) => this.out.reply(&id, Err(err.to_string())),
            })
            .ok();
        })
        .detach();
    }

    /// `rename_path`: `path`, `new_path`. Servers that asked for
    /// `willRenameFiles` edit first; `{path, edits: [{server, applied,
    /// changed, failure}], problems}`.
    fn rename_path(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let (from, to) = match (param_str(request, "path"), param_str(request, "new_path")) {
            (Ok(from), Ok(to)) => (self.path(request, from), self.path(request, to)),
            (Err(err), _) | (_, Err(err)) => return self.out.reply(&id, Err(err)),
        };
        let servers: Vec<(String, Arc<RunningServer>)> = self
            .store
            .read(cx)
            .running_servers()
            .into_iter()
            .map(|(_, name, server)| (name.to_string(), server))
            .collect();
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let check = (project.clone(), from.clone(), to.clone());
            let checked = cx
                .background_spawn(async move {
                    let (project, from, to) = check;
                    check_last_name(&project, &to)?;
                    file_ops::check_move(&project, &from, &to)?;
                    Ok::<_, io::Error>(project.kind(&from)? == Some(FileKind::Dir))
                })
                .await;
            let is_dir = match checked {
                Ok(is_dir) => is_dir,
                Err(err) => {
                    this.update(cx, |this, _| this.out.reply(&id, Err(err.to_string())))
                        .ok();
                    return;
                }
            };
            let moves = vec![(from.clone(), to.clone(), is_dir)];
            let running: Vec<Arc<RunningServer>> =
                servers.iter().map(|(_, server)| server.clone()).collect();
            let mut edits = Vec::new();
            let mut problems = Vec::new();
            for (server, params) in file_ops::rename_requests(&running, &moves, true) {
                let name = servers
                    .iter()
                    .find(|(_, running)| Arc::ptr_eq(running, &server))
                    .map(|(name, _)| name.clone())
                    .unwrap_or_default();
                let timer = cx.background_executor().timer(WILL_RENAME_TIMEOUT);
                let request = file_ops::will_rename(&server, params);
                futures::pin_mut!(request);
                match select(request, timer).await {
                    Either::Left((Ok(Some(edit)), _)) => edits.push((name, server.encoding, edit)),
                    Either::Left((Ok(None), _)) => {}
                    Either::Left((Err(err), _)) => problems.push(format!("{name}: {err}")),
                    Either::Right(_) => problems.push(format!("{name}: no answer")),
                }
            }
            this.update(cx, |this, cx| {
                let edits: Vec<Value> = edits
                    .into_iter()
                    .map(|(name, encoding, edit)| {
                        let mut report = edits::report_json(&this.apply_edit(edit, encoding, cx));
                        report["server"] = json!(name);
                        report
                    })
                    .collect();
                match file_ops::move_path(&project, &from, &to) {
                    Ok(()) => {
                        this.documents_renamed(&from, &to, cx);
                        file_ops::did_rename(&running, &moves);
                        this.out.reply(
                            &id,
                            Ok(json!({
                                "path": path_json(&to),
                                "edits": edits,
                                "problems": problems,
                            })),
                        );
                    }
                    Err(err) => this.out.reply(&id, Err(err.to_string())),
                }
            })
            .ok();
        })
        .detach();
    }
}
