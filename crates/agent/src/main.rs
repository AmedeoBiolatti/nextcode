//! `next_code_agent serve`: speaks the agent protocol on stdin/stdout.
//! `next_code_agent pinpoint <command> …`: asks the editor serving this
//! machine's project to list or highlight parts of its window (the same
//! commands as `next_code pinpoint`); `next_code_agent context …` is
//! `next_code_agent pinpoint context …` (what the user is looking at).
//! `next_code_agent notes <command> …`: reads and writes the project's
//! sticky notes (the same commands as `next_code notes`).
//! `next_code_agent --version`: prints the banner and exits.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => {
            #[cfg(unix)]
            link_stable_name();
            match next_code_agent::serve(std::io::stdin(), std::io::stdout()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("next_code_agent: connection failed: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        #[cfg(unix)]
        Some("pinpoint") => next_code_agent::pinpoint::cli(&args[1..]),
        #[cfg(unix)]
        Some("context") => next_code_agent::pinpoint::cli(&args),
        Some("notes") => {
            ExitCode::from(notes::cli::run(&args[1..], "next_code_agent").clamp(0, 255) as u8)
        }
        Some("--version") => {
            print!("{}", proto::banner());
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!(
                "usage: next_code_agent serve | pinpoint <command> … | context … | notes <command> … | --version"
            );
            ExitCode::from(2)
        }
    }
}

/// Points `next_code_agent` next to the installed build at it: the editor
/// installs each build under a content-hashed name, and CLI agents on this
/// machine need a path that stays the same across editor updates.
#[cfg(unix)]
fn link_stable_name() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name()) else {
        return;
    };
    // Only installed builds (`next_code_agent-<protocol>-<hash>`), not one
    // run from a build directory.
    let Some(name) = name
        .to_str()
        .filter(|name| name.starts_with("next_code_agent-"))
    else {
        return;
    };
    let link = dir.join("next_code_agent");
    if std::fs::read_link(&link).is_ok_and(|target| target.as_os_str() == name) {
        return;
    }
    // Replaced by a rename, so a concurrent caller never misses the link.
    let partial = dir.join(format!(".next_code_agent.{}", std::process::id()));
    std::fs::remove_file(&partial).ok();
    if std::os::unix::fs::symlink(name, &partial).is_ok()
        && std::fs::rename(&partial, &link).is_err()
    {
        std::fs::remove_file(&partial).ok();
    }
}
