//! The agent inside a real container. Needs Docker and a built Linux agent:
//!
//! ```sh
//! NEXT_CODE_WSL_AGENT=target/release/next_code_agent \
//!     cargo test -p remote --test container -- --ignored
//! ```

use std::{io, process::Command, time::Duration};

use remote::container::{self, Container, DockerHost};

/// A throwaway container, removed when dropped.
struct Throwaway(String);

impl Throwaway {
    fn start(image: &str) -> Self {
        let output = DockerHost::Local
            .docker()
            .args(["run", "-d", "--rm", image, "sleep", "300"])
            .output()
            .expect("docker");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self(String::from_utf8(output.stdout).unwrap().trim().to_owned())
    }
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        Command::new("docker")
            .args(["rm", "-f", &self.0])
            .output()
            .ok();
    }
}

#[test]
#[ignore = "needs Docker and a built Linux agent"]
fn agent_is_installed_and_serves_a_container() {
    let throwaway = Throwaway::start("alpine:3.20");
    let target = Container {
        host: DockerHost::Local,
        id: throwaway.0.clone(),
        name: "alpine".into(),
        user: None,
        env: vec![("NEXT_CODE_TEST".into(), "from env".into())],
        workdir: Some("/tmp".into()),
    };
    let connection = container::connect(&target, "/tmp").unwrap();
    assert_eq!(connection.info.root, "/tmp");
    connection
        .client
        .write_file("/tmp/a.txt", b"hello\n".to_vec())
        .unwrap();
    assert_eq!(connection.client.list_files("/tmp").unwrap(), ["a.txt"]);
    // Processes inherit the exec environment.
    let output = connection
        .client
        .run(
            "/bin/sh",
            &["-c".into(), "echo $NEXT_CODE_TEST".into()],
            "/tmp",
            &[],
            None,
            Duration::from_secs(10),
        )
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "from env\n");
    drop(connection);

    // Reconnecting finds the installed agent.
    let again = container::connect(&target, "~").unwrap();
    assert_eq!(again.info.root, "/root");
}

#[test]
#[ignore = "needs Docker and a built Linux agent"]
fn a_missing_container_is_reported() {
    let target = Container {
        host: DockerHost::Local,
        id: "next_code_no_such_container".into(),
        name: "missing".into(),
        user: None,
        env: Vec::new(),
        workdir: None,
    };
    let err = container::connect(&target, "/").err().unwrap();
    assert_eq!(err.kind(), io::ErrorKind::Other);
    assert!(err.to_string().contains("No such container"), "{err}");
}
