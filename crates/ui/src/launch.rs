//! The command line: which project (or the last session) a launch opens.
//! Used for this process's own arguments and for launches handed over by
//! the launcher (`instance`), whose relative paths are relative to the
//! directory they were typed in.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::Context as _;

use crate::project::Location;

pub const USAGE: &str = "usage: next_code [--headless] [--devcontainer] [PROJECT_DIR]\n       next_code [--headless] [--devcontainer] --wsl <DISTRO> <LINUX_PATH>\n       next_code [--headless] --workspace <FILE>\n       next_code --automation [--port PORT] [--width PX] [--height PX] [--state-dir DIR] -- [PROJECT_DIR]\n       next_code pinpoint <COMMAND> [OPTIONS]\n       next_code --quit\n\nWithout a project, the windows open when next_code last closed reopen (else the welcome screen;\n\"restore_windows\": false in settings.json always shows it). PROJECT_DIR . is the current directory,\nwhich --headless and --devcontainer also use when no project is given.\nA launch opens its project in the editor already running; --new-instance starts a separate editor\ninstead, and --quit asks the running one to exit.\n--automation exposes the GUI on a private Linux display over authenticated HTTP.\n--headless serves JSON-line commands on stdin instead of opening a window.\n--devcontainer opens the folder inside its dev container (.devcontainer/devcontainer.json).\n--workspace opens a multi-root workspace file (local, or <DISTRO>:/path in WSL); PROJECT_DIR\nnaming a *.next_code-workspace or VS Code *.code-workspace file opens it too.\nPROJECT_DIR ssh://[USER@]HOST[:PORT]/PATH (or /~/PATH) opens a folder over SSH (Remote - SSH).\npinpoint lists and highlights parts of the running editor for agents (next_code pinpoint --help).";

/// Reads the command line: a local directory (`.` is the current one), a
/// `\\wsl.localhost\<distro>\...` path, or `--wsl <distro> <linux path>`,
/// optionally preceded by `--devcontainer` (alone: the current directory);
/// or a workspace file (`--workspace <file>`, or a path with a workspace
/// file extension). `None`: no project, reopen the last session's windows.
pub fn parse_args(args: &[OsString]) -> anyhow::Result<Option<Location>> {
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
        [path] if path.to_string_lossy().starts_with("--") => {
            anyhow::bail!("unknown option {}\n{USAGE}", path.to_string_lossy())
        }
        [path] => Location::parse(&path.to_string_lossy())
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("empty project path\n{USAGE}")),
        _ => anyhow::bail!("{USAGE}"),
    }
}

/// The current directory, made absolute by [`checked`].
pub fn current_dir() -> Location {
    Location::Local(PathBuf::from("."))
}

/// `path` made absolute: relative to `base` (the directory the command
/// line was typed in) when there is one, else to this process's current
/// directory.
fn absolute(path: &Path, base: Option<&Path>) -> std::io::Result<PathBuf> {
    match base {
        Some(base) if path.is_relative() => std::path::absolute(base.join(path)),
        _ => std::path::absolute(path),
    }
}

/// Local folders and workspace files made absolute (relative to `base`,
/// see [`absolute`]) and checked.
pub fn checked(location: Location, base: Option<&Path>) -> anyhow::Result<Location> {
    Ok(match location {
        Location::Local(root) => {
            let root =
                absolute(&root, base).with_context(|| format!("resolving {}", root.display()))?;
            anyhow::ensure!(root.is_dir(), "{} is not a directory", root.display());
            Location::Local(root)
        }
        Location::DevContainer { folder, config } => Location::DevContainer {
            folder: Box::new(checked(*folder, base)?),
            config,
        },
        Location::Workspace {
            distro: None,
            file: Some(file),
            folders,
        } => {
            let file =
                absolute(Path::new(&file), base).with_context(|| format!("resolving {file}"))?;
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

/// The project a command line opens, checked, with relative paths
/// relative to `base`; `None` for a launch without a project.
pub fn resolve(args: &[OsString], base: Option<&Path>) -> anyhow::Result<Option<Location>> {
    parse_args(args)?
        .map(|location| checked(location, base))
        .transpose()
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
            checked(project(&["."]), None).unwrap(),
            Location::Local(cwd.clone())
        );
        assert_eq!(
            checked(project(&["--devcontainer"]), None).unwrap(),
            Location::DevContainer {
                folder: Box::new(Location::Local(cwd)),
                config: None,
            },
            "--devcontainer alone: the current directory's container"
        );
        assert!(parse_args(&args(&["--help"])).is_err());
        assert!(parse_args(&args(&["--bogus"])).is_err());
        assert!(parse_args(&args(&["a", "b"])).is_err());
    }

    #[test]
    fn relative_paths_follow_the_base_directory() {
        // A launch handed over by the launcher resolves `.` and relative
        // paths where it was typed, not in the editor's directory.
        let base = std::env::temp_dir().join(format!("next_code_launch_{}", std::process::id()));
        std::fs::create_dir_all(base.join("sub")).unwrap();
        let canonical = std::path::absolute(&base).unwrap();
        assert_eq!(
            resolve(&args(&["."]), Some(&base)).unwrap(),
            Some(Location::Local(canonical.clone()))
        );
        assert_eq!(
            resolve(&args(&["sub"]), Some(&base)).unwrap(),
            Some(Location::Local(canonical.join("sub")))
        );
        assert_eq!(
            resolve(&args(&["--devcontainer", "sub"]), Some(&base)).unwrap(),
            Some(Location::DevContainer {
                folder: Box::new(Location::Local(canonical.join("sub"))),
                config: None,
            })
        );
        // Absolute paths ignore the base; WSL locations have no local path.
        assert_eq!(
            resolve(
                &args(&[&canonical.join("sub").to_string_lossy()]),
                Some(Path::new("/elsewhere"))
            )
            .unwrap(),
            Some(Location::Local(canonical.join("sub")))
        );
        assert_eq!(
            resolve(&args(&["Ubuntu:~/p"]), Some(&base)).unwrap(),
            Some(wsl("Ubuntu", "~/p"))
        );
        assert_eq!(
            resolve(&args(&["ssh://me@box/~/p"]), Some(&base)).unwrap(),
            Some(Location::Remote {
                scheme: "ssh".into(),
                host: "me@box".into(),
                path: "~/p".into(),
            })
        );
        assert!(resolve(&args(&["missing"]), Some(&base)).is_err());
        assert_eq!(resolve(&[], Some(&base)).unwrap(), None);
        std::fs::remove_dir_all(&base).unwrap();
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
            checked(project(&["--workspace", &named]), None).unwrap(),
            workspace(None, &named)
        );
        assert_eq!(
            checked(project(&[&named]), None).unwrap(),
            workspace(None, &named)
        );
        // Relative to a base directory.
        assert_eq!(
            checked(project(&["--workspace", "w.code-workspace"]), Some(&dir)).unwrap(),
            workspace(None, &std::path::absolute(&file).unwrap().to_string_lossy())
        );
        let missing = dir.join("missing.next_code-workspace");
        assert!(checked(workspace(None, &missing.to_string_lossy()), None).is_err());
        assert!(
            checked(workspace(None, &dir.to_string_lossy()), None).is_err(),
            "a folder"
        );
        assert_eq!(
            checked(workspace(Some("Ubuntu"), "/nowhere.code-workspace"), None).unwrap(),
            workspace(Some("Ubuntu"), "/nowhere.code-workspace")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
