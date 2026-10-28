//! Bounded, cancellable panel commands. Output is drained even after its cap.
use super::*;
use extension_api::{MAX_PROCESS_OUTPUT_BYTES, ProcessExit, ProcessRequest};
use std::sync::mpsc;

#[derive(Default)]
struct Output {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}
impl Output {
    fn append(&mut self, data: &[u8], stderr: bool) {
        let out = if stderr {
            &mut self.stderr
        } else {
            &mut self.stdout
        };
        let take = data
            .len()
            .min(MAX_PROCESS_OUTPUT_BYTES.saturating_sub(out.len()));
        out.extend_from_slice(&data[..take]);
        self.truncated |= take < data.len();
    }
    fn finish(&self, id: String, code: Option<i32>, error: Option<String>) -> ProcessExit {
        // Decode after collection so a UTF-8 character split between reads survives.
        ProcessExit {
            id,
            exit_code: code,
            stdout: String::from_utf8_lossy(&self.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&self.stderr).into_owned(),
            truncated: self.truncated,
            error,
        }
    }
}

impl Project {
    pub(crate) fn run_extension_process(
        &self,
        request: &ProcessRequest,
        cancel: &CancelToken,
        lifetime: &CancelToken,
    ) -> ProcessExit {
        let output = Arc::new(Mutex::new(Output::default()));
        let result = self.collect_extension_process(request, cancel, lifetime, &output);
        let (code, error) = match result {
            Ok(Some(code)) => (Some(code), None),
            Ok(None) => (None, Some("process terminated without an exit code".into())),
            Err(err) => (None, Some(format!("{}: {err}", request.program))),
        };
        output.lock().finish(request.id.clone(), code, error)
    }

    fn collect_extension_process(
        &self,
        request: &ProcessRequest,
        cancel: &CancelToken,
        lifetime: &CancelToken,
        output: &Arc<Mutex<Output>>,
    ) -> io::Result<Option<i32>> {
        let stopped = || cancel.is_cancelled() || lifetime.is_cancelled();
        if stopped() {
            return Err(cancelled(&request.program));
        }
        let deadline = Instant::now() + Duration::from_millis(request.timeout_ms);
        let program = self.find_executable(&request.program)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "not found on the project host's PATH or tool directories",
            )
        })?;
        #[cfg(windows)]
        if program
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shell script launchers are not supported",
            ));
        }
        if stopped() {
            return Err(cancelled(&request.program));
        }
        if let Backend::Remote { client, .. } = &self.0.backend {
            let (tx, rx) = mpsc::sync_channel(1);
            let collected = output.clone();
            let handle = client.start_stream(
                proto::Request::Spawn(proto::SpawnRequest {
                    program: Some(agent_path(&program)?.into()),
                    args: request.args.clone(),
                    cwd: agent_path(self.root())?.into(),
                    env: vec![],
                    pty: None,
                }),
                move |event| match event {
                    proto::Event::Stdout(data) => collected.lock().append(&data, false),
                    proto::Event::Stderr(data) => collected.lock().append(&data, true),
                    proto::Event::Exited { code } => {
                        let _ = tx.try_send(code);
                    }
                    _ => {}
                },
            )?;
            handle.close_input()?;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let failure = if stopped() {
                    Some(cancelled(&request.program))
                } else if left.is_zero() {
                    Some(io::Error::new(io::ErrorKind::TimedOut, "process timed out"))
                } else {
                    None
                };
                if let Some(error) = failure {
                    handle.cancel();
                    // Hold the permit through the agent's two-second grace.
                    let _ = rx.recv_timeout(Duration::from_secs(3));
                    return Err(error);
                }
                match rx.recv_timeout(left.min(Duration::from_millis(50))) {
                    Ok(code) => return Ok(code),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "project agent disconnected",
                        ));
                    }
                }
                // Dropping the handle on every error sends Cancel to the agent.
            }
        }
        let mut command = Command::new(program);
        command
            .args(&request.args)
            .current_dir(self.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A GUI launch may omit per-user tool directories that a login
        // terminal adds. Keep them available to the declared tool's children.
        let mut paths: Vec<_> = std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .collect();
        if let Some(home) = dirs::home_dir() {
            for dir in [home.join(".local/bin"), home.join(".cargo/bin")] {
                if !paths.contains(&dir) {
                    paths.push(dir);
                }
            }
        }
        if let Ok(path) = std::env::join_paths(paths) {
            command.env("PATH", path);
        }
        // Lets a declared tool adapt its output to this host (optional fields added after API 3).
        command.env("NEXT_CODE_VERSION", env!("CARGO_PKG_VERSION"));
        remote::launch::no_window(&mut command);
        let mut spawned = Tree::spawn(&mut command)?;
        let child = spawned.child.as_mut().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut tree = spawned.watch()?;
        let collect = |mut reader: Box<dyn Read + Send>, stderr| {
            let output = output.clone();
            thread::Builder::new()
                .name("extension-output".into())
                .spawn(move || {
                    let mut bytes = [0; 16384];
                    loop {
                        match reader.read(&mut bytes) {
                            Ok(0) => break,
                            Ok(n) => output.lock().append(&bytes[..n], stderr),
                            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                            Err(_) => break,
                        }
                    }
                })
        };
        let out = collect(Box::new(stdout), false)?;
        let err = collect(Box::new(stderr), true)?;
        let result = loop {
            if stopped() {
                break Err(cancelled(&request.program));
            }
            let now = Instant::now();
            if now >= deadline {
                break Err(io::Error::new(io::ErrorKind::TimedOut, "process timed out"));
            }
            if let Outcome::Exited(status) =
                tree.wait_until(deadline.min(now + Duration::from_millis(50)), Some(cancel))
            {
                break Ok(status.and_then(|s| s.code()));
            }
        };
        tree.kill_remaining();
        out.join().ok();
        err.join().ok();
        result
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn project(remote: bool) -> Project {
        let root = std::env::temp_dir().join(format!(
            "nc-extension-process-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        if !remote {
            return Project::local(root);
        }
        let (reader, agent_writer) = io::pipe().unwrap();
        let (agent_reader, writer) = io::pipe().unwrap();
        thread::spawn(move || next_code_agent::serve(agent_reader, agent_writer).unwrap());
        let (client, _) = AgentClient::connect(
            remote::Transport {
                reader: Box::new(reader),
                writer: Box::new(writer),
                child: None,
                stderr: None,
            },
            "test",
            root.to_str().unwrap(),
            Duration::from_secs(5),
        )
        .unwrap();
        Project::build(
            vec![root_of_path(root, None, true)],
            None,
            Backend::Remote {
                client,
                host: RemoteHost::Connector {
                    scheme: "test".into(),
                    host: "test".into(),
                },
            },
        )
    }
    fn request(script: &str) -> ProcessRequest {
        ProcessRequest {
            id: "status".into(),
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            timeout_ms: 5000,
            response: None,
        }
    }
    fn run(project: &Project, req: &ProcessRequest) -> ProcessExit {
        project.run_extension_process(req, &CancelToken::new(), &CancelToken::new())
    }
    #[test]
    fn panel_process_uses_project_cwd_literal_argv_and_closed_stdin_on_both_hosts() {
        for remote in [false, true] {
            let p = project(remote);
            let mut req = request("read line; printf '%s\\n%s' \"$PWD\" \"$1\"; printf error >&2");
            req.args
                .extend(["arg0".into(), "literal; $(touch unexpected)".into()]);
            let out = run(&p, &req);
            assert_eq!(out.exit_code, Some(0), "{out:?}");
            assert_eq!(
                out.stdout,
                format!("{}\nliteral; $(touch unexpected)", p.root().display())
            );
            assert_eq!(out.stderr, "error");
            assert!(!p.root().join("unexpected").exists());
        }
    }
    #[test]
    fn panel_process_drains_both_streams_beyond_their_caps_on_both_hosts() {
        for remote in [false, true] {
            let out = run(
                &project(remote),
                &request("head -c 400000 /dev/zero; head -c 400000 /dev/zero >&2; exit 7"),
            );
            assert_eq!(out.exit_code, Some(7));
            assert!(out.truncated);
            assert_eq!(out.stdout.len(), MAX_PROCESS_OUTPUT_BYTES);
            assert_eq!(out.stderr.len(), MAX_PROCESS_OUTPUT_BYTES);
        }
    }
    fn alive(path: &Path) -> bool {
        let Ok(pid) = fs::read_to_string(path) else {
            return true;
        };
        fs::read_to_string(format!("/proc/{}/stat", pid.trim())).is_ok_and(|s| {
            s.rsplit_once(") ")
                .is_some_and(|(_, s)| !s.starts_with('Z'))
        })
    }
    #[test]
    fn panel_process_cancel_and_timeout_end_remote_and_local_descendants() {
        for remote in [false, true] {
            for timeout in [false, true] {
                let p = project(remote);
                let mut req = request("sleep 60 & echo $! > child; wait");
                req.timeout_ms = if timeout { 200 } else { 5000 };
                let token = CancelToken::new();
                let worker_token = token.clone();
                let worker_project = p.clone();
                let job = thread::spawn(move || {
                    worker_project.run_extension_process(&req, &worker_token, &CancelToken::new())
                });
                let until = Instant::now() + Duration::from_secs(4);
                while !p.root().join("child").exists() && Instant::now() < until {
                    thread::sleep(Duration::from_millis(10));
                }
                assert!(p.root().join("child").exists());
                if !timeout {
                    token.cancel();
                }
                let result = job.join().unwrap();
                assert!(
                    result
                        .error
                        .as_ref()
                        .is_some_and(|s| s.contains(if timeout {
                            "timed out"
                        } else {
                            "cancelled"
                        })),
                    "{result:?}"
                );
                let until = Instant::now() + Duration::from_secs(5);
                while alive(&p.root().join("child")) && Instant::now() < until {
                    thread::sleep(Duration::from_millis(20));
                }
                assert!(
                    !alive(&p.root().join("child")),
                    "remote={remote}, timeout={timeout}"
                );
            }
        }
    }
    #[test]
    fn panel_process_ends_descendants_retaining_output_after_the_parent_exits() {
        for remote in [false, true] {
            let p = project(remote);
            let started = Instant::now();
            let out = run(&p, &request("sleep 60 & echo $! > child; exit 0"));
            assert_eq!(out.exit_code, Some(0), "{out:?}");
            assert!(started.elapsed() < Duration::from_secs(4));
            // SIGKILL delivery and /proc's state transition are asynchronous.
            // Keep asserting that the descendant dies, with the same bounded
            // wait as the cancellation test rather than racing the scheduler.
            let until = Instant::now() + Duration::from_secs(5);
            while alive(&p.root().join("child")) && Instant::now() < until {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(!alive(&p.root().join("child")), "remote={remote}");
        }
    }
    #[test]
    fn panel_process_start_failure_is_a_completion_with_an_error() {
        for remote in [false, true] {
            let mut req = request("");
            req.program = "nextcode-missing-program-71cf".into();
            let out = run(&project(remote), &req);
            assert_eq!(out.exit_code, None);
            assert!(out.error.is_some());
        }
    }
}
