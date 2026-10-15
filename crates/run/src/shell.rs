//! Command lines typed into a terminal: POSIX shells (WSL, containers,
//! Linux) and PowerShell (local Windows terminals).

use crate::config::{Command, CommandLine};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Posix,
    PowerShell,
}

/// `text` as one shell word.
pub fn quote(text: &str, shell: Shell) -> String {
    let plain =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=');
    match shell {
        Shell::Posix => {
            if !text.is_empty()
                && text
                    .chars()
                    .all(|c| plain(c) || matches!(c, '@' | '%' | '+' | ','))
            {
                text.to_owned()
            } else {
                format!("'{}'", text.replace('\'', r"'\''"))
            }
        }
        // PowerShell 5.1 drops a bare `--` passed to native programs and
        // reads `a,b` as an array: both stay quoted.
        Shell::PowerShell => {
            if !text.is_empty() && text != "--" && text.chars().all(|c| plain(c) || c == '\\') {
                text.to_owned()
            } else {
                format!("'{}'", text.replace('\'', "''"))
            }
        }
    }
}

/// One line that runs `command` in a fresh terminal (whose shell starts in
/// `root`): changes directory only when the command's differs, exports
/// its environment, then runs it.
pub fn terminal_line(command: &Command, root: &str, shell: Shell) -> String {
    let mut parts = Vec::new();
    let same_dir = command.cwd.trim_end_matches(['/', '\\']) == root.trim_end_matches(['/', '\\']);
    if !same_dir {
        parts.push(match shell {
            Shell::Posix => format!("cd {}", quote(&command.cwd, shell)),
            Shell::PowerShell => {
                format!("Set-Location -LiteralPath {}", quote(&command.cwd, shell))
            }
        });
    }
    for (name, value) in &command.env {
        parts.push(match shell {
            Shell::Posix => format!("export {name}={}", quote(value, shell)),
            // An assignment's bare word would be a command call.
            Shell::PowerShell => format!("$env:{name} = '{}'", value.replace('\'', "''")),
        });
    }
    parts.push(match &command.line {
        CommandLine::Shell(line) => line.clone(),
        CommandLine::Process { program, args } => {
            let mut words = vec![match shell {
                // A quoted program is a string to PowerShell: call it.
                Shell::PowerShell if quote(program, shell) != *program => {
                    format!("& {}", quote(program, shell))
                }
                _ => quote(program, shell),
            }];
            words.extend(args.iter().map(|arg| quote(arg, shell)));
            words.join(" ")
        }
    });
    match shell {
        Shell::Posix => parts.join(" && "),
        Shell::PowerShell => parts.join("; "),
    }
}

/// Splits a command-line string into words (`launch.json` allows `args`
/// as one string): whitespace separates, single and double quotes group,
/// backslash escapes the next character outside single quotes.
pub fn split(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => word.push(c),
            (_, '\\') => {
                in_word = true;
                match chars.next() {
                    Some(next) => word.push(next),
                    None => word.push('\\'),
                }
            }
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                in_word = true;
                quote = Some(c);
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn command(program: &str, args: &[&str], cwd: &str, env: &[(&str, &str)]) -> Command {
        Command {
            line: CommandLine::Process {
                program: program.into(),
                args: args.iter().map(|&arg| arg.into()).collect(),
            },
            cwd: cwd.into(),
            env: env
                .iter()
                .map(|&(name, value)| (name.into(), value.into()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn posix_lines_quote_change_directory_and_export() {
        let run = command(
            "uv",
            &["run", "python", "my app.py", "it's"],
            "/p/sub",
            &[("A", "1 2")],
        );
        assert_eq!(
            terminal_line(&run, "/p", Shell::Posix),
            r"cd /p/sub && export A='1 2' && uv run python 'my app.py' 'it'\''s'"
        );
        let here = command("cargo", &["run", "--bin", "tool", "--", "x"], "/p/", &[]);
        assert_eq!(
            terminal_line(&here, "/p", Shell::Posix),
            "cargo run --bin tool -- x"
        );
    }

    #[test]
    fn powershell_lines_call_quoted_programs_and_keep_double_dash() {
        let run = command(
            r"C:\Program Files\py.exe",
            &["--", "a,b", "x"],
            r"C:\p\sub",
            &[("RUST_LOG", "debug")],
        );
        assert_eq!(
            terminal_line(&run, r"C:\p", Shell::PowerShell),
            r"Set-Location -LiteralPath C:\p\sub; $env:RUST_LOG = 'debug'; & 'C:\Program Files\py.exe' '--' 'a,b' x"
        );
    }

    #[test]
    fn shell_lines_run_verbatim() {
        let run = Command {
            line: CommandLine::Shell("make test | tee log".into()),
            cwd: "/p".into(),
            env: BTreeMap::new(),
        };
        assert_eq!(
            terminal_line(&run, "/p", Shell::Posix),
            "make test | tee log"
        );
    }

    #[test]
    fn split_handles_quotes_and_escapes() {
        assert_eq!(
            split(r#"--name "a b" 'c "d"' e\ f  -v"#),
            ["--name", "a b", r#"c "d""#, "e f", "-v"]
        );
        assert_eq!(split(r#""""#), [""]);
        assert!(split("   ").is_empty());
    }
}
