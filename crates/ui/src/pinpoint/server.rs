//! The control server agents reach (loopback TCP, one JSON request per
//! line, answered by one JSON line, each request carrying the instance's
//! `token`), its instance file `<state_root>/pinpoint/<pid>.json`, and the
//! one-shot client behind `next_code pinpoint …`.

use std::{
    collections::hash_map::RandomState,
    fs,
    hash::{BuildHasher, Hasher},
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gpui::App;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::control;

/// What `<state_root>/pinpoint/<pid>.json` holds.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Instance {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    /// Unix milliseconds.
    pub started: u64,
    /// Project roots of the open windows (refreshed while running).
    #[serde(default)]
    pub roots: Vec<String>,
}

fn dir() -> Option<PathBuf> {
    crate::layout::state_root().map(|root| root.join("pinpoint"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// 128 random bits from the standard library's per-process hash keys.
fn random_token() -> String {
    let mut token = String::new();
    for salt in 0..2u64 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(now_ms() ^ salt);
        hasher.write_u32(std::process::id());
        token.push_str(&format!("{:016x}", hasher.finish()));
    }
    token
}

/// Starts the server and writes the instance file (removed on quit).
pub fn start(cx: &mut App) {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("pinpoint: no control server: {error}");
            return;
        }
    };
    let Ok(port) = listener.local_addr().map(|address| address.port()) else {
        return;
    };
    let mut instance = Instance {
        port,
        token: random_token(),
        pid: std::process::id(),
        started: now_ms(),
        roots: Vec::new(),
    };
    let Some(dir) = dir() else {
        return;
    };
    let file = dir.join(format!("{}.json", instance.pid));
    if let Err(error) = fs::create_dir_all(&dir).and_then(|()| write_instance(&file, &instance)) {
        eprintln!("pinpoint: cannot write {}: {error}", file.display());
        return;
    }
    let token = instance.token.clone();
    thread::Builder::new()
        .name("pinpoint-server".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let token = token.clone();
                thread::spawn(move || serve(stream, &token));
            }
        })
        .ok();
    let quit_file = file.clone();
    cx.on_app_quit(move |_| {
        fs::remove_file(&quit_file).ok();
        async {}
    })
    .detach();
    // Keep the file's roots current, for clients choosing an instance.
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let Ok(roots) = cx.update(control::all_roots) else {
                break;
            };
            if roots != instance.roots {
                instance.roots = roots;
                write_instance(&file, &instance).ok();
            }
        }
    })
    .detach();
}

fn write_instance(file: &std::path::Path, instance: &Instance) -> std::io::Result<()> {
    // Readers never see half a file.
    let partial = file.with_extension("json.partial");
    fs::write(&partial, serde_json::to_vec(instance).unwrap_or_default())?;
    fs::rename(&partial, file)
}

fn serve(stream: TcpStream, token: &str) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else {
            return;
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Object(mut request)) => {
                let given = request.remove("token");
                if given.as_ref().and_then(Value::as_str) != Some(token) {
                    let reply = control::error_reply(
                        "bad or missing \"token\" (read it from the instance file)",
                        Value::Null,
                    );
                    writeln!(writer, "{reply}").ok();
                    return;
                }
                let (sender, receiver) = mpsc::channel();
                control::dispatch(Value::Object(request), move |reply| {
                    sender.send(reply).ok();
                });
                receiver.recv().unwrap_or_else(|_| {
                    control::error_reply("the request was dropped (window closed?)", Value::Null)
                })
            }
            Ok(_) => control::error_reply("a request is one JSON object per line", Value::Null),
            Err(error) => control::error_reply(format!("bad JSON: {error}"), Value::Null),
        };
        if writeln!(writer, "{reply}").is_err() {
            return;
        }
    }
}

/// Sends `request` to `instance`, waiting at most `timeout` for the reply.
pub fn send(instance: &Instance, request: &Value, timeout: Duration) -> Result<Value, String> {
    let address = SocketAddr::from(([127, 0, 0, 1], instance.port));
    let stream = TcpStream::connect_timeout(&address, Duration::from_millis(700))
        .map_err(|error| format!("cannot reach next_code (pid {}): {error}", instance.pid))?;
    stream.set_read_timeout(Some(timeout)).ok();
    let mut request = request.clone();
    request["token"] = instance.token.clone().into();
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    writeln!(writer, "{request}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| format!("no reply from next_code (pid {}): {error}", instance.pid))?;
    serde_json::from_str(&line).map_err(|error| format!("bad reply from next_code: {error}"))
}

/// The instance files (unreadable ones skipped).
fn instances() -> Vec<(PathBuf, Instance)> {
    let Some(dir) = dir() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "json"))
        .filter_map(|path| {
            let instance = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            Some((path, instance))
        })
        .collect()
}

/// The instance to talk to: one with a root holding `cwd` (the deepest),
/// else the most recently started. `instances`: (started, roots).
pub fn choose_instance(instances: &[(u64, Vec<String>)], cwd: &str) -> Option<usize> {
    let holding = instances
        .iter()
        .enumerate()
        .flat_map(|(index, (_, roots))| {
            roots
                .iter()
                .filter(|root| control::contains(root, cwd))
                .map(move |root| (index, control::normalize_path(root).len()))
        })
        .max_by_key(|(_, len)| *len)
        .map(|(index, _)| index);
    holding.or_else(|| {
        instances
            .iter()
            .enumerate()
            .max_by_key(|(_, (started, _))| *started)
            .map(|(index, _)| index)
    })
}

/// `next_code pinpoint <args>`: one request to the running editor; prints
/// the reply. The exit code.
pub fn run_cli(args: &[String]) -> i32 {
    let mut request = match pinpoint_cli::parse(args) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("{error}\n\n{}", pinpoint_cli::USAGE);
            return 2;
        }
    };
    if request.get("cmd").and_then(Value::as_str) == Some("help") {
        println!("{}", pinpoint_cli::USAGE);
        return 0;
    }
    let cwd = std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    if request.get("window").is_none() && request.get("project_roots").is_none() {
        request["cwd"] = cwd.clone().into();
    }
    // Live instances, with their windows' roots.
    let mut live = Vec::new();
    for (path, instance) in instances() {
        match send(
            &instance,
            &json!({ "cmd": "windows" }),
            Duration::from_secs(5),
        ) {
            Ok(reply) => {
                let roots: Vec<String> = reply["windows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|w| w["roots"].as_array().cloned().unwrap_or_default())
                    .filter_map(|root| root.as_str().map(str::to_owned))
                    .collect();
                live.push((instance, roots));
            }
            // A crashed or killed editor left its file behind.
            Err(_) => {
                fs::remove_file(path).ok();
            }
        }
    }
    let candidates: Vec<_> = live
        .iter()
        .map(|(instance, roots)| (instance.started, roots.clone()))
        .collect();
    let Some(index) = choose_instance(&candidates, &cwd) else {
        let reply = control::error_reply(
            "no running next_code found",
            json!({ "hint": format!(
                "start NextCode first; instances register in {}",
                dir().map_or_else(String::new, |d| d.display().to_string())
            ) }),
        );
        return pinpoint_cli::print_reply(&request, &reply);
    };
    let wait = request
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(0.)
        .clamp(0., 3600.);
    let timeout = Duration::from_secs_f64(wait + 30.);
    let reply = send(&live[index].0, &request, timeout)
        .unwrap_or_else(|error| control::error_reply(error, Value::Null));
    pinpoint_cli::print_reply(&request, &reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_choice() {
        let instances = vec![
            (100, vec!["/home/me/api".to_owned()]),
            (300, vec![r"C:\work\web".to_owned()]),
            (200, vec!["/home/me".to_owned()]),
        ];
        assert_eq!(choose_instance(&instances, "/home/me/api/src"), Some(0));
        assert_eq!(choose_instance(&instances, "/home/me/notes"), Some(2));
        assert_eq!(choose_instance(&instances, r"c:\Work\web\src"), Some(1));
        // Elsewhere: the newest.
        assert_eq!(choose_instance(&instances, "/tmp"), Some(1));
        assert_eq!(choose_instance(&[], "/tmp"), None);
    }

    #[test]
    fn tokens_differ() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }
}
