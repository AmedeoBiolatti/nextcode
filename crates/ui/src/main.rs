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
mod file_icons;
mod file_ops;
mod file_tree;
mod git_dialogs;
mod git_log;
mod headless;
mod layout;
mod location_picker;
mod lsp_store;
mod menus;
mod outline_panel;
mod notebook;
mod ports;
mod problems;
mod project;
mod project_picker;
mod python_panel;
mod recent;
mod revision_picker;
mod run_config;
mod search_panel;
mod selectable;
mod settings;
mod shelf_view;
mod shell;
mod startup;
mod stash_dialogs;
mod symbol_picker;
mod terminal_panel;
mod test_explorer;
mod theme;
mod theme_picker;
mod workflows;
mod workspace;
mod workspace_edit;
mod workspace_file;
mod worktree_picker;

use std::{ffi::OsString, path::PathBuf};

use anyhow::Context as _;
use gpui::{App, Application};

use crate::project::Location;

const USAGE: &str = "usage: next_code [--headless] [--devcontainer] [PROJECT_DIR]\n       next_code [--headless] [--devcontainer] --wsl <DISTRO> <LINUX_PATH>\n       next_code [--headless] --workspace <FILE>\n       next_code --automation [--port PORT] [--width PX] [--height PX] [--state-dir DIR] -- [PROJECT_DIR]\n\nWithout a project, the windows open when next_code last closed reopen (else the welcome screen;\n\"restore_windows\": false in settings.json always shows it). PROJECT_DIR . is the current directory,\nwhich --headless and --devcontainer also use when no project is given.\n--automation exposes the GUI on a private Linux display over authenticated HTTP.\n--headless serves JSON-line commands on stdin instead of opening a window.\n--devcontainer opens the folder inside its dev container (.devcontainer/devcontainer.json).\n--workspace opens a multi-root workspace file (local, or <DISTRO>:/path in WSL); PROJECT_DIR\nnaming a *.next_code-workspace or VS Code *.code-workspace file opens it too.";

/// Reads the command line: a local directory (`.` is the current one), a
/// `\\wsl.localhost\<distro>\...` path, or `--wsl <distro> <linux path>`,
/// optionally preceded by `--devcontainer` (alone: the current directory);
/// or a workspace file (`--workspace <file>`, or a path with a workspace
/// file extension). `None`: no project, reopen the last session's windows.
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
        [flag, rest @ ..] if flag == "--workspace" => {
            let location = match rest {
                [file] => Location::parse(&file.to_string_lossy()),
                _ => None,
            };
            match location {
                Some(Location::Local(file)) => Ok(Some(Location::Workspace {
                    distro: None,
                    file: Some(file.to_string_lossy().into_owned()),
                    folders: Vec::new(),
                })),
                Some(Location::Wsl { distro, path }) => Ok(Some(Location::Workspace {
                    distro: Some(distro),
                    file: Some(path),
                    folders: Vec::new(),
                })),
                Some(workspace @ Location::Workspace { .. }) => Ok(Some(workspace)),
                _ => anyhow::bail!("--workspace needs one workspace file\n{USAGE}"),
            }
        }
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

/// Local folders and workspace files made absolute and checked.
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
        Location::Workspace {
            distro: None,
            file: Some(file),
            folders,
        } => {
            let file = std::path::absolute(&file).with_context(|| format!("resolving {file}"))?;
            anyhow::ensure!(file.is_file(), "{} is not a file", file.display());
            Location::Workspace {
                distro: None,
                file: Some(file.to_string_lossy().into_owned()),
                folders,
            }
        }
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
    let application = Application::new().with_assets(file_icons::Assets);
    startup::mark("gpui platform");
    application.run(move |cx: &mut App| {
        startup::mark("gpui ready");
        editor::bind_keys(cx);
        selectable::bind_keys(cx);
        shell::bind_keys(cx);
        command_palette::bind_keys(cx);
        workspace::bind_keys(cx);
        search_panel::bind_keys(cx);
        file_finder::bind_keys(cx);
        file_tree::bind_keys(cx);
        location_picker::bind_keys(cx);
        call_hierarchy::bind_keys(cx);
        outline_panel::bind_keys(cx);
        symbol_picker::bind_keys(cx);
        project_picker::bind_keys(cx);
        theme_picker::bind_keys(cx);
        terminal_panel::bind_keys(cx);
        ports::panel::bind_keys(cx);
        git_log::bind_keys(cx);
        workflows::bind_keys(cx);
        // After `editor::bind_keys`: cell editors' keys win over the editor's.
        notebook::view::bind_keys(cx);
        notebook::picker::bind_keys(cx);
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

    fn workspace(distro: Option<&str>, file: &str) -> Location {
        Location::Workspace {
            distro: distro.map(str::to_owned),
            file: Some(file.into()),
            folders: Vec::new(),
        }
    }

    #[test]
    fn workspace_files_open_as_multi_root_workspaces() {
        // `--workspace` takes any file name, local or typed WSL.
        assert_eq!(
            project(&["--workspace", "team.json"]),
            workspace(None, "team.json")
        );
        assert_eq!(
            project(&["--workspace", "Ubuntu:/home/me/all.next_code-workspace"]),
            workspace(Some("Ubuntu"), "/home/me/all.next_code-workspace")
        );
        // Plain paths with a workspace extension (any case) are workspaces.
        assert_eq!(
            project(&["all.next_code-workspace"]),
            workspace(None, "all.next_code-workspace")
        );
        assert_eq!(
            project(&[r"C:\src\App.Code-Workspace"]),
            workspace(None, r"C:\src\App.Code-Workspace")
        );
        assert_eq!(
            project(&["Ubuntu:~/w.code-workspace"]),
            workspace(Some("Ubuntu"), "~/w.code-workspace")
        );
        assert!(parse_args(&args(&["--workspace"])).is_err());
        assert!(parse_args(&args(&["--workspace", "a", "b"])).is_err());
        assert!(parse_args(&args(&["--workspace", "devcontainer:p"])).is_err());

        // Local files are made absolute and must exist; WSL ones are read
        // where they live, when opened.
        let dir = std::env::temp_dir().join(format!("next_code_main_ws_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("w.code-workspace");
        std::fs::write(&file, "{\"folders\": []}").unwrap();
        let named = file.to_string_lossy().into_owned();
        assert_eq!(
            checked(project(&["--workspace", &named])).unwrap(),
            workspace(None, &named)
        );
        assert_eq!(
            checked(project(&[&named])).unwrap(),
            workspace(None, &named)
        );
        let missing = dir.join("missing.next_code-workspace");
        assert!(checked(workspace(None, &missing.to_string_lossy())).is_err());
        assert!(
            checked(workspace(None, &dir.to_string_lossy())).is_err(),
            "a folder"
        );
        assert_eq!(
            checked(workspace(Some("Ubuntu"), "/nowhere.code-workspace")).unwrap(),
            workspace(Some("Ubuntu"), "/nowhere.code-workspace")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
