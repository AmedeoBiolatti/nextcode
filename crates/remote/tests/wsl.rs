//! Real WSL bootstrap: installs and starts the agent through `wsl.exe`.
//!
//! Ignored by default. Run from Windows after `scripts/build-wsl-agent.ps1`:
//!
//! ```powershell
//! $env:NEXT_CODE_WSL_AGENT = "target\release\next_code_agent"
//! $env:NEXT_CODE_TEST_WSL_DISTRO = "Ubuntu"
//! cargo test -p remote --test wsl -- --ignored
//! ```

use std::{env, io, sync::mpsc, time::Duration};

use proto::{Event, Request, SpawnRequest};
use remote::wsl;

fn distro() -> String {
    env::var("NEXT_CODE_TEST_WSL_DISTRO").expect("set NEXT_CODE_TEST_WSL_DISTRO")
}

#[test]
#[ignore = "needs WSL and a built Linux agent"]
fn agent_is_installed_started_and_serves_a_linux_project() {
    let distro = distro();
    let home = wsl::connect(&distro, "~").unwrap();
    assert!(home.info.root.starts_with('/'), "{}", home.info.root);

    // Make a project inside the distro through the agent, then open it.
    let root = format!("/tmp/next_code_wsl_test_{}", std::process::id());
    let (tx, rx) = mpsc::channel();
    let _mkdir = home
        .client
        .start_stream(
            Request::Spawn(SpawnRequest {
                program: Some("mkdir".into()),
                args: vec!["-p".into(), format!("{root}/src")],
                cwd: "/".into(),
                env: Vec::new(),
                pty: None,
            }),
            move |event| {
                if let Event::Exited { code } = event {
                    tx.send(code).ok();
                }
            },
        )
        .unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), Some(0));
    let file = format!("{root}/src/main.rs");
    home.client
        .write_file(&file, b"fn main() {}\n".to_vec())
        .unwrap();

    let project = wsl::connect(&distro, &root).unwrap();
    assert_eq!(project.info.root, root);
    assert_eq!(project.client.list_files(&root).unwrap(), ["src/main.rs"]);
    assert_eq!(project.client.read_file(&file).unwrap(), b"fn main() {}\n");
}

#[test]
#[ignore = "needs WSL and a built Linux agent"]
fn failures_name_the_problem() {
    let missing = wsl::connect("NoSuchDistro-next_code", "/").err().unwrap();
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    assert!(
        missing.to_string().contains("Installed distros"),
        "{missing}"
    );

    let path = wsl::connect(&distro(), "/no/such/next_code/dir")
        .err()
        .unwrap();
    assert_eq!(path.kind(), io::ErrorKind::NotFound);
    assert!(
        path.to_string().contains("/no/such/next_code/dir"),
        "{path}"
    );

    let relative = wsl::connect(&distro(), "relative/dir").err().unwrap();
    assert_eq!(relative.kind(), io::ErrorKind::InvalidInput);
}
