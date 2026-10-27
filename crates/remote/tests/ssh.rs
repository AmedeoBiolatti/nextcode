//! The agent started over `ssh`, with a stand-in `ssh` that runs the remote
//! command in a local `/bin/sh` (as sshd hands it to the login shell) with
//! a temporary home, and prints a Tailscale SSH check-mode banner first.
//! Needs a built agent:
//!
//! ```sh
//! cargo build -p next_code_agent
//! NEXT_CODE_WSL_AGENT=target/debug/next_code_agent \
//!     cargo test -p remote --test ssh -- --ignored
//! ```
//!
//! Its own test binary: it sets `NEXT_CODE_SSH` before any thread starts.
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

use remote::ssh::{self, Host, SSH_PROGRAM_ENV};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("next_code_ssh_{name}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

#[test]
#[ignore = "needs a built Linux agent"]
fn agent_is_installed_and_served_over_ssh() {
    let dir = temp_dir("e2e");
    let home = dir.join("home");
    let project = dir.join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("a.txt"), "hello\n").unwrap();
    let log = dir.join("ssh.log");
    let stub = dir.join("ssh");
    fs::write(
        &stub,
        format!(
            r#"#!/bin/sh
echo "$*" >> '{log}'
case " $* " in *" -G "*) echo 'stricthostkeychecking ask'; exit 0;; esac
for command; do :; done
echo '# Tailscale SSH requires an additional check.' >&2
echo '# To authenticate, visit: https://login.tailscale.com/a/test' >&2
HOME='{home}' exec /bin/sh -c "$command"
"#,
            log = log.display(),
            home = home.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: set before this binary's only test starts any thread.
    unsafe { std::env::set_var(SSH_PROGRAM_ENV, &stub) };

    let host = Host::parse("me@box.tail1.ts.net:2222").unwrap();
    let mut progress = Vec::new();
    let connection = ssh::connect(&host, project.to_str().unwrap(), &mut |line| {
        progress.push(line.to_owned())
    })
    .unwrap();
    assert_eq!(connection.info.root, project.to_str().unwrap());
    assert_eq!(
        connection
            .client
            .list_files(project.to_str().unwrap())
            .unwrap(),
        ["a.txt"]
    );
    let output = connection
        .client
        .run(
            "/bin/sh",
            &["-c".into(), "echo \"$HOME\"".into()],
            project.to_str().unwrap(),
            &[],
            None,
            Duration::from_secs(10),
        )
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        home.to_str().unwrap()
    );
    // The sign-in link reached the progress (the first launch, the install
    // and the second launch each printed it).
    assert!(
        progress
            .iter()
            .any(|line| line == "# To authenticate, visit: https://login.tailscale.com/a/test"),
        "{progress:?}"
    );
    // Installed under the temporary home; the next connection starts it
    // with one command.
    let installed = fs::read_dir(home.join(".local/share/next_code"))
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("next_code_agent-")
        })
        .count();
    assert_eq!(installed, 1);
    drop(connection);
    let commands = || fs::read_to_string(&log).unwrap().lines().count();
    let before = commands();
    let again = ssh::connect(&host, "~", &mut |_| {}).unwrap();
    assert_eq!(again.info.root, home.to_str().unwrap());
    // `ssh -G`, then one launch.
    assert_eq!(commands() - before, 2);
    let calls = fs::read_to_string(&log).unwrap();
    assert!(
        calls
            .lines()
            .all(|line| line.contains("-p 2222 -- me@box.tail1.ts.net"))
    );
    assert!(calls.contains("StrictHostKeyChecking=accept-new"));
    drop(again);

    // A host that refuses the key says what to do.
    fs::write(
        &stub,
        "#!/bin/sh\ncase \" $* \" in *\" -G \"*) echo 'stricthostkeychecking true'; exit 0;; esac\n\
         echo 'me@box: Permission denied (publickey).' >&2\nexit 255\n",
    )
    .unwrap();
    let err = ssh::connect(&host, "~", &mut |_| {})
        .err()
        .unwrap()
        .to_string();
    assert!(
        err.contains("Permission denied") && err.contains("ssh-add"),
        "{err}"
    );
    fs::remove_dir_all(&dir).ok();
}
