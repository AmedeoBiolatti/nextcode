//! `next_code_agent serve`: speaks the agent protocol on stdin/stdout.
//! `next_code_agent --version`: prints the banner and exits.

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("serve") => match next_code_agent::serve(std::io::stdin(), std::io::stdout()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("next_code_agent: connection failed: {err}");
                ExitCode::FAILURE
            }
        },
        Some("--version") => {
            print!("{}", proto::banner());
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: next_code_agent serve | --version");
            ExitCode::from(2)
        }
    }
}
