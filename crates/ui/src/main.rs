// No console window for the editor; command-line output still works (see
// `startup::attach_console`).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod branch_picker;
mod change_tree;
mod compare_view;
mod connecting;
mod diff_view;
mod editor;
mod file_finder;
mod file_tree;
mod git_panel;
mod headless;
mod history_view;
mod layout;
mod location_picker;
mod lsp_store;
mod problems;
mod project;
mod project_picker;
mod recent;
mod revision_picker;
mod search_panel;
mod settings;
mod startup;
mod terminal_panel;
mod theme;
mod workspace;
mod worktree_picker;

use std::ffi::OsString;
#[cfg(test)]
use std::path::PathBuf;

use anyhow::Context as _;
use gpui::{App, Application};

use crate::project::Location;

const USAGE: &str = "usage: next_code [--headless] [PROJECT_DIR]\n       next_code [--headless] --wsl <DISTRO> <LINUX_PATH>\n\n--headless serves JSON-line commands on stdin instead of opening a window.";

/// Reads the command line: a local directory (default: the current one), a
/// `\\wsl.localhost\<distro>\...` path, or `--wsl <distro> <linux path>`.
fn parse_args(args: &[OsString]) -> anyhow::Result<Location> {
    match args {
        [] => Ok(Location::Local(
            std::env::current_dir().context("reading current directory")?,
        )),
        [flag, rest @ ..] if flag == "--wsl" => match rest {
            [distro, path] => Ok(Location::Wsl {
                distro: distro.to_string_lossy().into_owned(),
                path: path.to_string_lossy().into_owned(),
            }),
            _ => anyhow::bail!("--wsl needs a distro and a Linux path\n{USAGE}"),
        },
        [path] if path == "--help" || path == "-h" => anyhow::bail!("{USAGE}"),
        [path] => Location::parse(&path.to_string_lossy())
            .ok_or_else(|| anyhow::anyhow!("empty project path\n{USAGE}")),
        _ => anyhow::bail!("{USAGE}"),
    }
}

fn main() -> anyhow::Result<()> {
    startup::init();
    let result = run();
    if result.is_err() {
        // Started from a shell: show the error there.
        startup::attach_console();
    }
    result
}

fn run() -> anyhow::Result<()> {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let headless = args.first().is_some_and(|arg| arg == "--headless");
    if headless {
        args.remove(0);
    }
    let location = match parse_args(&args)? {
        Location::Local(root) => {
            let root = std::path::absolute(&root)
                .with_context(|| format!("resolving {}", root.display()))?;
            anyhow::ensure!(root.is_dir(), "{} is not a directory", root.display());
            Location::Local(root)
        }
        wsl => wsl,
    };
    if headless {
        startup::attach_console();
        headless::run(location);
        return Ok(());
    }

    // Connect to WSL or read git status while gpui sets up the platform.
    let preload = workspace::Preload::start(&location);
    let application = Application::new();
    startup::mark("gpui platform");
    application.run(move |cx: &mut App| {
        startup::mark("gpui ready");
        editor::bind_keys(cx);
        workspace::bind_keys(cx);
        file_finder::bind_keys(cx);
        location_picker::bind_keys(cx);
        project_picker::bind_keys(cx);
        terminal_panel::bind_keys(cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        workspace::open_preloaded(location, Some(preload), cx);
        startup::mark("window opened");
        cx.activate(true);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn wsl(distro: &str, path: &str) -> Location {
        Location::Wsl {
            distro: distro.into(),
            path: path.into(),
        }
    }

    #[test]
    fn wsl_projects_keep_their_linux_paths() {
        assert_eq!(
            parse_args(&args(&["--wsl", "Ubuntu", "/home/me/p"])).unwrap(),
            wsl("Ubuntu", "/home/me/p")
        );
        assert_eq!(
            parse_args(&args(&[r"\\wsl.localhost\Ubuntu-24.04\home\me\my p\"])).unwrap(),
            wsl("Ubuntu-24.04", "/home/me/my p")
        );
        assert_eq!(
            parse_args(&args(&[r"\\WSL$\Debian"])).unwrap(),
            wsl("Debian", "/")
        );
        assert_eq!(
            parse_args(&args(&[r"C:\dev\p"])).unwrap(),
            Location::Local(PathBuf::from(r"C:\dev\p"))
        );
        assert_eq!(
            parse_args(&args(&["Ubuntu:~/src/p"])).unwrap(),
            wsl("Ubuntu", "~/src/p")
        );
        assert_eq!(
            parse_args(&args(&["C:/dev/p"])).unwrap(),
            Location::Local(PathBuf::from("C:/dev/p"))
        );
        assert!(parse_args(&args(&["--wsl", "/home/me/p"])).is_err());
    }
}
