//! Host services through a real agent (over pipes): `next_code_agent
//! git-credential` calls and ssh-agent connections on the agent's machine
//! answered by a fake editor, then by the editor's real handler
//! (`remote::host::Host`) over a real `git credential` with a `store`
//! helper and a fake ssh-agent. Its own test binary: it sets the socket
//! directory and the global git config in the environment before any thread
//! starts.
#![cfg(unix)]

use std::{
    fs,
    io::{self, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use next_code_agent::{host::credential_call, pinpoint};
use parking_lot::Mutex;
use proto::{
    HostRequest, HostServices,
    credential::{Credential, Operation},
};
use remote::{
    AgentClient, Transport,
    host::{Git, Host},
};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("next_code_host_{name}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn connect(root: &Path) -> Arc<AgentClient> {
    let (client_reader, agent_writer) = io::pipe().unwrap();
    let (agent_reader, client_writer) = io::pipe().unwrap();
    thread::spawn(move || next_code_agent::serve(agent_reader, agent_writer));
    let transport = Transport {
        reader: Box::new(client_reader),
        writer: Box::new(client_writer),
        child: None,
        stderr: None,
    };
    AgentClient::connect(
        transport,
        "test agent",
        root.to_str().unwrap(),
        Duration::from_secs(10),
    )
    .unwrap()
    .0
}

fn sockets(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    fs::read_dir(dir.join("host"))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.to_string_lossy().ends_with(suffix))
                .collect()
        })
        .unwrap_or_default()
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn git_config(file: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["config", "--file"])
        .arg(file)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap()
}

/// One ssh-agent request on a new connection to `socket`; the reply body.
fn ssh_request(stream: &mut UnixStream, body: &[u8]) -> Vec<u8> {
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(body).unwrap();
    let mut length = [0; 4];
    stream.read_exact(&mut length).unwrap();
    let mut reply = vec![0; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut reply).unwrap();
    reply
}

#[test]
fn git_credentials_and_ssh_agent_reach_the_editor_through_the_agent() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipped: no git");
        return;
    }
    let sockets_dir = temp_dir("sockets").join("run");
    let config = temp_dir("config").join("gitconfig");
    // SAFETY: set before this binary's only test starts any thread.
    unsafe {
        std::env::set_var(pinpoint::DIR_ENV, &sockets_dir);
        std::env::set_var("GIT_CONFIG_GLOBAL", &config);
        std::env::remove_var(next_code_agent::host::GIT_SOCKET_ENV);
    }
    git_config(&config, &["user.email", "me@container"]);
    let root = temp_dir("project");
    let client = connect(&root);

    // A fake editor: one credential, ssh requests echoed per connection.
    let closed = Arc::new(Mutex::new(Vec::new()));
    let seen_closed = closed.clone();
    let serving = client
        .serve_host(
            HostServices {
                git_credentials: true,
                ssh_agent: true,
                git_user_name: Some("Ada Lovelace".into()),
                git_user_email: Some("ada@host".into()),
            },
            |request, reply| {
                thread::spawn(move || {
                    reply.send(match request {
                        HostRequest::GitCredential {
                            operation: Operation::Get,
                            credential,
                        } if credential.contains("host=example.test\n") => {
                            Ok(format!("{credential}username=ada\npassword=s3cret\n").into_bytes())
                        }
                        HostRequest::GitCredential { .. } => Ok(Vec::new()),
                        HostRequest::SshAgent {
                            connection,
                            mut message,
                        } => {
                            message.extend_from_slice(&connection.to_be_bytes());
                            Ok(message)
                        }
                    })
                });
            },
            move |connection| seen_closed.lock().push(connection),
        )
        .unwrap();

    // The agent set up the user's global config: our helper, the editor's
    // name, and not its e-mail (the user has one).
    let helpers = git_config(&config, &["--get-all", "credential.helper"]);
    assert!(
        helpers.trim().ends_with("' git-credential") && helpers.lines().count() == 1,
        "{helpers}"
    );
    assert_eq!(git_config(&config, &["user.name"]).trim(), "Ada Lovelace");
    assert_eq!(git_config(&config, &["user.email"]).trim(), "me@container");

    let answer = credential_call(Operation::Get, "protocol=https\nhost=example.test\n").unwrap();
    assert_eq!(
        Credential::parse(&answer).to_text(),
        "protocol=https\nhost=example.test\nusername=ada\npassword=s3cret\n"
    );
    assert_eq!(
        credential_call(Operation::Get, "protocol=https\nhost=other.test\n"),
        None
    );

    let [ssh] = &sockets(&sockets_dir, "-ssh.sock")[..] else {
        panic!("one ssh socket expected");
    };
    let mut first = UnixStream::connect(ssh).unwrap();
    let mut second = UnixStream::connect(ssh).unwrap();
    let a = ssh_request(&mut first, &[11]);
    let b = ssh_request(&mut second, &[11]);
    assert_eq!(a[0], 11);
    assert_ne!(a[1..], b[1..], "each connection is its own");
    assert_eq!(ssh_request(&mut first, &[13, 1])[..2], [13, 1]);
    drop(first);
    wait_until("the closed connection was not reported", || {
        closed.lock().len() == 1
    });

    // Stopping removes the sockets; the helper then answers nothing.
    drop(serving);
    wait_until("the sockets were not removed", || {
        sockets(&sockets_dir, ".sock").is_empty()
    });
    assert_eq!(
        credential_call(Operation::Get, "protocol=https\nhost=example.test\n"),
        None
    );

    // The real handler: this machine's git with a `store` helper (the global
    // helpers, ours included, reset) and a fake ssh-agent.
    let store = temp_dir("store").join("credentials");
    fs::write(&store, "https://ada:fr0m-store@example.test\n").unwrap();
    let agent_socket = temp_dir("ssh").join("agent.sock");
    let listener = UnixListener::bind(&agent_socket).unwrap();
    let agent_connections = Arc::new(AtomicUsize::new(0));
    let counted = agent_connections.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            counted.fetch_add(1, Ordering::Relaxed);
            thread::spawn(move || {
                let mut length = [0; 4];
                while stream.read_exact(&mut length).is_ok() {
                    let mut body = vec![0; u32::from_be_bytes(length) as usize];
                    stream.read_exact(&mut body).unwrap();
                    // SSH_AGENT_IDENTITIES_ANSWER with no keys.
                    let reply = [0, 0, 0, 5, 12, 0, 0, 0, 0];
                    stream.write_all(&reply).unwrap();
                }
            });
        }
    });
    let host = Arc::new(Host::new(
        Git {
            program: "git".into(),
            args: vec![
                "-c".into(),
                "credential.helper=".into(),
                "-c".into(),
                format!("credential.helper=store --file={}", store.display()),
            ],
        },
        Some(agent_socket.clone()),
    ));
    let serving = remote::host::serve_with(
        &client,
        host,
        HostServices {
            git_credentials: true,
            ssh_agent: true,
            ..HostServices::default()
        },
    )
    .unwrap();
    let answer = credential_call(Operation::Get, "protocol=https\nhost=example.test\n").unwrap();
    let answer = Credential::parse(&answer);
    assert_eq!(answer.get("username"), Some("ada"));
    assert_eq!(answer.get("password"), Some("fr0m-store"));
    for refused in [
        "protocol=https\nhost=unknown.test\n",
        "protocol=ssh\nhost=example.test\n",
    ] {
        assert_eq!(credential_call(Operation::Get, refused), None, "{refused}");
    }
    // `store` and `erase` reach the editor's helper.
    credential_call(
        Operation::Store,
        "protocol=https\nhost=new.test\nusername=bob\npassword=pw\n",
    );
    let stored = fs::read_to_string(&store).unwrap();
    assert!(stored.contains("https://bob:pw@new.test"), "{stored}");
    credential_call(
        Operation::Erase,
        "protocol=https\nhost=example.test\nusername=ada\npassword=fr0m-store\n",
    );
    let stored = fs::read_to_string(&store).unwrap();
    assert!(!stored.contains("example.test"), "{stored}");

    let [ssh] = &sockets(&sockets_dir, "-ssh.sock")[..] else {
        panic!("one ssh socket expected");
    };
    let mut stream = UnixStream::connect(ssh).unwrap();
    assert_eq!(ssh_request(&mut stream, &[11]), [12, 0, 0, 0, 0]);
    assert_eq!(ssh_request(&mut stream, &[11]), [12, 0, 0, 0, 0]);
    assert_eq!(agent_connections.load(Ordering::Relaxed), 1);
    drop(stream);
    drop(serving);

    // Turned off: nothing is served and our helper leaves the config; the
    // identity stays.
    let _off = remote::host::serve(&client, false).unwrap();
    assert!(sockets(&sockets_dir, ".sock").is_empty());
    assert_eq!(git_config(&config, &["--get-all", "credential.helper"]), "");
    assert_eq!(git_config(&config, &["user.name"]).trim(), "Ada Lovelace");

    drop(client);
    for dir in [&root, &sockets_dir] {
        fs::remove_dir_all(dir).ok();
    }
}
