// No console window for the editor; command-line output still works (see
// `startup::attach_console`).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app_state;
mod branches_popup;
mod call_hierarchy;
mod change_tree;
mod command_palette;
mod commands;
mod commit_panel;
mod compare_view;
mod connecting;
mod debugger;
mod diff_view;
mod editor;
mod file_finder;
mod file_tree;
mod git_dialogs;
mod git_log;
mod headless;
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
mod workflows;
mod workspace;
mod workspace_edit;
mod worktree_picker;

use std::{ffi::OsString, path::PathBuf};

use anyhow::Context as _;
use gpui::{App, Application};

use crate::project::Location;

const USAGE: &str = "usage: next_code [--headless] [--devcontainer] [PROJECT_DIR]\n       next_code [--headless] [--devcontainer] --wsl <DISTRO> <LINUX_PATH>\n       next_code --automation [--port PORT] [--width PX] [--height PX] [--state-dir DIR] -- [PROJECT_DIR]\n\nWithout a project, the windows open when next_code last closed reopen (else the welcome screen;\n\"restore_windows\": false in settings.json always shows it). PROJECT_DIR . is the current directory,\nwhich --headless and --devcontainer also use when no project is given.\n--automation exposes the GUI on a private Linux display over authenticated HTTP.\n--headless serves JSON-line commands on stdin instead of opening a window.\n--devcontainer opens the folder inside its dev container (.devcontainer/devcontainer.json).";

/// Reads the command line: a local directory (`.` is the current one), a
/// `\\wsl.localhost\<distro>\...` path, or `--wsl <distro> <linux path>`,
/// optionally preceded by `--devcontainer` (alone: the current directory).
/// `None`: no project, reopen the last session's windows.
fn parse_args(args: &[OsString]) -> anyhow::Result<Option<Location>> {
    match args {
        [flag, rest @ ..] if flag == "--devcontainer" => Ok(Some(Location::DevContainer {
            folder: Box::new(parse_args(rest)?.unwrap_or_else(current_dir)),
            config: None,
        })),
        [] => Ok(None),
        [flag, rest @ ..] if flag == "--wsl" => match rest {
            [distro, path] => Ok(Some(Location::Wsl {
                distro: distro.to_string_lossy().into_owned(),
                path: path.to_string_lossy().into_owned(),
            })),
            _ => anyhow::bail!("--wsl needs a distro and a Linux path\n{USAGE}"),
        },
        [path] if path == "--help" || path == "-h" => anyhow::bail!("{USAGE}"),
        [path] => Location::parse(&path.to_string_lossy())
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("empty project path\n{USAGE}")),
        _ => anyhow::bail!("{USAGE}"),
    }
}

/// The current directory, made absolute by [`checked`].
fn current_dir() -> Location {
    Location::Local(PathBuf::from("."))
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

/// What the window(s) at startup show.
enum Launch {
    /// The project on the command line, alone (as in VS Code).
    Open(Location),
    /// The last session's windows (none: the welcome screen).
    Restore(app_state::AppState),
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
    let location = parse_args(&args)?.map(checked).transpose()?;
    if headless {
        startup::attach_console();
        headless::run(match location {
            Some(location) => location,
            None => checked(current_dir())?,
        });
        return Ok(());
    }
    let launch = match location {
        Some(location) => Launch::Open(location),
        None => Launch::Restore(app_state::restorable()),
    };

    startup::set_app_id();
    // Warm up graphics, and connect to WSL or read git status, while gpui
    // sets up the platform.
    let prewarm = startup::prewarm();
    let preload = match &launch {
        Launch::Open(location) => Some(location),
        Launch::Restore(state) => state.focused_tab(),
    }
    .map(workspace::Preload::start);
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
        git_log::bind_keys(cx);
        theme::refresh(cx);
        app_state::init(cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        startup::close_on_termination(cx);
        match launch {
            Launch::Open(location) => shell::open_window(location, preload, cx),
            Launch::Restore(state) => shell::restore(state, preload, cx),
        }
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

    /// The project `args` name.
    fn project(list: &[&str]) -> Location {
        parse_args(&args(list))
            .unwrap()
            .expect("a project is named")
    }

    fn wsl(distro: &str, path: &str) -> Location {
        Location::Wsl {
            distro: distro.into(),
            path: path.into(),
        }
    }

    #[test]
    fn no_project_restores_and_a_dot_is_the_current_directory() {
        assert_eq!(parse_args(&[]).unwrap(), None, "reopen the last windows");
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            checked(project(&["."])).unwrap(),
            Location::Local(cwd.clone())
        );
        assert_eq!(
            checked(project(&["--devcontainer"])).unwrap(),
            Location::DevContainer {
                folder: Box::new(Location::Local(cwd)),
                config: None,
            },
            "--devcontainer alone: the current directory's container"
        );
        assert!(parse_args(&args(&["--help"])).is_err());
        assert!(parse_args(&args(&["a", "b"])).is_err());
    }

    #[test]
    fn wsl_projects_keep_their_linux_paths() {
        assert_eq!(
            project(&["--wsl", "Ubuntu", "/home/me/p"]),
            wsl("Ubuntu", "/home/me/p")
        );
        assert_eq!(
            project(&[r"\\wsl.localhost\Ubuntu-24.04\home\me\my p\"]),
            wsl("Ubuntu-24.04", "/home/me/my p")
        );
        assert_eq!(project(&[r"\\WSL$\Debian"]), wsl("Debian", "/"));
        assert_eq!(
            project(&[r"C:\dev\p"]),
            Location::Local(PathBuf::from(r"C:\dev\p"))
        );
        assert_eq!(project(&["Ubuntu:~/src/p"]), wsl("Ubuntu", "~/src/p"));
        assert_eq!(
            project(&["C:/dev/p"]),
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
            project(&["--devcontainer", "--wsl", "Ubuntu", "/home/me/p"]),
            in_container(wsl("Ubuntu", "/home/me/p"))
        );
        assert_eq!(
            project(&["--devcontainer", r"C:\dev\p"]),
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
