// No console window for the editor; command-line output still works (see
// `startup::attach_console`).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod branch_picker;
mod call_hierarchy;
mod change_tree;
mod command_palette;
mod commands;
mod compare_view;
mod connecting;
mod debugger;
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
mod menus;
mod outline_panel;
mod ports;
mod problems;
mod project;
mod project_picker;
mod python_panel;
mod recent;
mod revision_picker;
mod run_config;
mod search_panel;
mod settings;
mod shell;
mod startup;
mod symbol_picker;
mod terminal_panel;
mod test_explorer;
mod theme;
mod theme_picker;
mod workspace;
mod workspace_edit;
mod worktree_picker;

use std::ffi::OsString;
#[cfg(test)]
use std::path::PathBuf;

use anyhow::Context as _;
use gpui::{App, Application};

use crate::project::Location;

const USAGE: &str = "usage: next_code [--headless] [--devcontainer] [PROJECT_DIR]\n       next_code [--headless] [--devcontainer] --wsl <DISTRO> <LINUX_PATH>\n       next_code --automation [--port PORT] [--width PX] [--height PX] [--state-dir DIR] -- [PROJECT_DIR]\n\n--automation exposes the GUI on a private Linux display over authenticated HTTP.\n--headless serves JSON-line commands on stdin instead of opening a window.\n--devcontainer opens the folder inside its dev container (.devcontainer/devcontainer.json).";

/// Reads the command line: a local directory (default: the current one), a
/// `\\wsl.localhost\<distro>\...` path, or `--wsl <distro> <linux path>`,
/// optionally preceded by `--devcontainer`.
fn parse_args(args: &[OsString]) -> anyhow::Result<Location> {
    match args {
        [flag, rest @ ..] if flag == "--devcontainer" => Ok(Location::DevContainer {
            folder: Box::new(parse_args(rest)?),
            config: None,
        }),
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

/// Local folders made absolute and checked.
fn checked(location: Location) -> anyhow::Result<Location> {
    Ok(match location {
        Location::Local(root) => {
            let root = std::path::absolute(&root)
                .with_context(|| format!("resolving {}", root.display()))?;
            anyhow::ensure!(root.is_dir(), "{} is not a directory", root.display());
            Location::Local(root)
        }
        Location::DevContainer { folder, config } => Location::DevContainer {
            folder: Box::new(checked(*folder)?),
            config,
        },
        wsl => wsl,
    })
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
    if args.first().is_some_and(|arg| arg == "--automation") {
        startup::attach_console();
        let status = std::process::Command::new("python3")
            .arg("-c")
            .arg(include_str!("../../../scripts/automation.py"))
            .arg("--binary")
            .arg(std::env::current_exe()?)
            .args(&args[1..])
            .status()
            .context("starting UI automation (requires Linux and Python 3)")?;
        anyhow::ensure!(status.success(), "UI automation exited with {status}");
        return Ok(());
    }
    let headless = args.first().is_some_and(|arg| arg == "--headless");
    if headless {
        args.remove(0);
    }
    let location = checked(parse_args(&args)?)?;
    if headless {
        startup::attach_console();
        headless::run(location);
        return Ok(());
    }

    startup::set_app_id();
    // Warm up graphics, and connect to WSL or read git status, while gpui
    // sets up the platform.
    let prewarm = startup::prewarm();
    let preload = workspace::Preload::start(&location);
    let application = Application::new();
    startup::mark("gpui platform");
    application.run(move |cx: &mut App| {
        startup::mark("gpui ready");
        editor::bind_keys(cx);
        shell::bind_keys(cx);
        command_palette::bind_keys(cx);
        workspace::bind_keys(cx);
        file_finder::bind_keys(cx);
        location_picker::bind_keys(cx);
        call_hierarchy::bind_keys(cx);
        outline_panel::bind_keys(cx);
        symbol_picker::bind_keys(cx);
        project_picker::bind_keys(cx);
        theme_picker::bind_keys(cx);
        terminal_panel::bind_keys(cx);
        ports::panel::bind_keys(cx);
        theme::refresh(cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        startup::close_on_termination(cx);
        shell::open_window(location, Some(preload), cx);
        // The window's renderer exists now.
        prewarm.finish();
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

    #[test]
    fn dev_container_locations_wrap_a_folder() {
        let in_container = |folder: Location| Location::DevContainer {
            folder: Box::new(folder),
            config: None,
        };
        assert_eq!(
            parse_args(&args(&["--devcontainer", "--wsl", "Ubuntu", "/home/me/p"])).unwrap(),
            in_container(wsl("Ubuntu", "/home/me/p"))
        );
        assert_eq!(
            parse_args(&args(&["--devcontainer", r"C:\dev\p"])).unwrap(),
            in_container(Location::Local(PathBuf::from(r"C:\dev\p")))
        );
        assert_eq!(
            Location::parse("devcontainer:Ubuntu:~/p").unwrap(),
            in_container(wsl("Ubuntu", "~/p"))
        );
        // The config a running project reports matches a request without one.
        let opened = Location::DevContainer {
            folder: Box::new(wsl("Ubuntu", "/p")),
            config: Some("/p/.devcontainer/devcontainer.json".into()),
        };
        assert!(opened.opens_same(&in_container(wsl("Ubuntu", "/p"))));
        assert!(!opened.opens_same(&wsl("Ubuntu", "/p")));
    }
}
