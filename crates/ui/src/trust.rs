//! Workspace trust: which project folders may run the programs they
//! configure.
//!
//! A folder's `.next_code/settings.json` (and a workspace file's
//! `settings`) can name language server, formatter and debug adapter
//! commands, and a dev container's `initializeCommand` runs on this
//! machine: a cloned repository could run code the moment it opens. Those
//! keys only take effect in folders the user trusted
//! (`Project::settings_for` layers untrusted folders through
//! [`crate::settings::without_commands`]), and the setup command only runs
//! there (`devcontainer::Host::trusted`). Everything else in the project's
//! settings applies as usual.
//!
//! Trusted folders are kept in `trusted.json` in the state directory (next
//! to `windows.json`), one key per folder: a local path,
//! `wsl:<distro>:<path>`; a dev container is trusted with its host folder.
//! A workspace opened in a window with an untrusted folder offers "Trust" /
//! "Not now" once per opening ([`prompt`], `OfferKind::TrustFolder`).
//!
//! Headless sessions ([`trust_all`]: nothing there can show the prompt, and
//! the user named the folder), unit tests and `NEXT_CODE_TRUST_ALL=1` trust
//! every folder.

use std::{
    collections::BTreeSet,
    io,
    path::PathBuf,
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::project::{Location, Project};

static TRUST_ALL: AtomicBool = AtomicBool::new(false);

/// Every folder counts as trusted from now on (headless sessions).
pub fn trust_all() {
    TRUST_ALL.store(true, Ordering::Relaxed);
}

fn everything_trusted() -> bool {
    static ENV: LazyLock<bool> =
        LazyLock::new(|| std::env::var("NEXT_CODE_TRUST_ALL").is_ok_and(|value| value == "1"));
    cfg!(test) || TRUST_ALL.load(Ordering::Relaxed) || *ENV
}

/// The persisted set of trusted folders (their [`keys`]).
pub struct TrustStore {
    /// `None`: nothing is written (no state directory).
    path: Option<PathBuf>,
    keys: BTreeSet<String>,
}

/// `trusted.json`.
#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    trusted: Vec<String>,
}

impl TrustStore {
    /// Reads `path`; missing or unreadable, nothing is trusted.
    pub fn load(path: Option<PathBuf>) -> Self {
        let keys = path
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<File>(&text).ok())
            .map(|file| file.trusted.into_iter().collect())
            .unwrap_or_default();
        Self { path, keys }
    }

    pub fn is_trusted(&self, key: &str) -> bool {
        self.keys.contains(key)
    }

    /// Adds `keys` and writes the file (whole, to a temporary name renamed
    /// over the old one, so a crash leaves the old file).
    pub fn trust(&mut self, keys: impl IntoIterator<Item = String>) -> io::Result<()> {
        let before = self.keys.len();
        self.keys.extend(keys);
        if self.keys.len() == before {
            return Ok(());
        }
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = File {
            trusted: self.keys.iter().cloned().collect(),
        };
        let text = serde_json::to_string_pretty(&file).map_err(io::Error::other)? + "\n";
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, path).inspect_err(|_| {
            std::fs::remove_file(&temp).ok();
        })
    }
}

fn path() -> Option<PathBuf> {
    Some(crate::layout::state_root()?.join("trusted.json"))
}

static STORE: LazyLock<Mutex<TrustStore>> = LazyLock::new(|| Mutex::new(TrustStore::load(path())));

/// The trust keys of `location`'s folders, in the project's root order
/// (see `Project::roots`): one for a folder, its host folder's for a dev
/// container, one per folder of a workspace.
pub fn keys(location: &Location) -> Vec<String> {
    match location {
        Location::Local(root) => vec![local_key(&root.to_string_lossy())],
        Location::Wsl { distro, path } => vec![wsl_key(distro, path)],
        Location::DevContainer { folder, .. } => keys(folder),
        Location::Workspace {
            distro, folders, ..
        } => folders
            .iter()
            .map(|folder| match distro {
                Some(distro) => wsl_key(distro, &folder.path),
                None => local_key(&folder.path),
            })
            .collect(),
    }
}

/// A local folder's key: without the `\\?\` prefix and trailing
/// separators; case-folded on Windows, where paths are.
fn local_key(path: &str) -> String {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let trimmed = path.trim_end_matches(['/', '\\']);
    let path = if trimmed.is_empty() { path } else { trimmed };
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.to_owned()
    }
}

fn wsl_key(distro: &str, path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let path = if trimmed.is_empty() { "/" } else { trimmed };
    format!("wsl:{distro}:{path}")
}

/// The keys of `project`'s folders the user has not trusted.
fn untrusted(project: &Project) -> Vec<(usize, String)> {
    let store = STORE.lock();
    keys(&project.location())
        .into_iter()
        .enumerate()
        .filter(|(_, key)| !store.is_trusted(key))
        .collect()
}

/// Whether folder `root` of `project` (an index into `Project::roots`)
/// may run the programs its settings name.
pub fn is_root_trusted(project: &Project, root: usize) -> bool {
    if everything_trusted() {
        return true;
    }
    let keys = keys(&project.location());
    keys.get(root)
        .is_some_and(|key| STORE.lock().is_trusted(key))
}

/// Whether every folder of `project` is trusted.
pub fn is_trusted(project: &Project) -> bool {
    everything_trusted() || untrusted(project).is_empty()
}

/// The question to show for `project`'s untrusted folders; `None` when
/// they are all trusted.
pub fn prompt(project: &Project) -> Option<String> {
    if everything_trusted() {
        return None;
    }
    let untrusted = untrusted(project);
    let roots = project.roots();
    let names: Vec<&str> = untrusted
        .iter()
        .filter_map(|(index, _)| roots.get(*index))
        .map(|root| root.name.as_str())
        .collect();
    let what = match names.as_slice() {
        [] => return None,
        [name] => format!("Trust this folder ({name})?"),
        names => format!("Trust these folders ({})?", names.join(", ")),
    };
    Some(format!(
        "{what} Language servers, formatters and debug adapters configured by the project, \
         and dev container setup commands, run only in trusted folders"
    ))
}

/// Trusts every folder of `project` for good. Settings already layered
/// for its folders are rebuilt (`Project::settings_for` caches them by the
/// user settings' identity, which this renews).
pub fn grant(project: &Project) -> io::Result<()> {
    STORE.lock().trust(keys(&project.location()))?;
    crate::settings::set_current((*crate::settings::current()).clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_file::Folder;

    #[test]
    fn the_store_round_trips_through_its_file() {
        let dir = std::env::temp_dir().join(format!(
            "next_code-test-trust-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state").join("trusted.json");
        let mut store = TrustStore::load(Some(path.clone()));
        assert!(!store.is_trusted("/home/me/proj"));
        store.trust(["/home/me/proj".to_owned()]).unwrap();
        assert!(store.is_trusted("/home/me/proj"));
        // Written whole, with no temporary file left behind.
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!({ "trusted": ["/home/me/proj"] })
        );
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["trusted.json"]);

        let again = TrustStore::load(Some(path.clone()));
        assert!(again.is_trusted("/home/me/proj"));
        assert!(!again.is_trusted("/home/me/other"));
        // Nothing new: nothing written.
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let mut again = again;
        again.trust(["/home/me/proj".to_owned()]).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );

        // A broken file trusts nothing (and is replaced on the next grant).
        std::fs::write(&path, "{ broken").unwrap();
        let mut broken = TrustStore::load(Some(path.clone()));
        assert!(!broken.is_trusted("/home/me/proj"));
        broken.trust(["/x".to_owned()]).unwrap();
        assert!(TrustStore::load(Some(path.clone())).is_trusted("/x"));
        // No path: nothing persists, nothing fails.
        let mut memory = TrustStore::load(None);
        memory.trust(["/y".to_owned()]).unwrap();
        assert!(memory.is_trusted("/y"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn keys_identify_folders_across_locations() {
        assert_eq!(local_key(r"\\?\C:\Users\me\proj\"), {
            if cfg!(windows) {
                r"c:\users\me\proj"
            } else {
                r"C:\Users\me\proj"
            }
        });
        assert_eq!(local_key("/home/me/proj/"), "/home/me/proj");
        assert_eq!(local_key("/"), "/");
        assert_eq!(
            wsl_key("Ubuntu", "/home/me/proj/"),
            "wsl:Ubuntu:/home/me/proj"
        );
        assert_eq!(wsl_key("Ubuntu", "/"), "wsl:Ubuntu:/");

        let folder = Location::Wsl {
            distro: "Ubuntu".into(),
            path: "/home/me/proj".into(),
        };
        assert_eq!(keys(&folder), ["wsl:Ubuntu:/home/me/proj"]);
        // A dev container is trusted with the folder it is opened from.
        let container = Location::DevContainer {
            folder: Box::new(folder.clone()),
            config: Some("/home/me/proj/.devcontainer/devcontainer.json".into()),
        };
        assert_eq!(keys(&container), keys(&folder));
        let workspace = Location::Workspace {
            distro: Some("Ubuntu".into()),
            file: Some("/home/me/all.code-workspace".into()),
            folders: vec![
                Folder {
                    path: "/home/me/proj".into(),
                    name: None,
                },
                Folder {
                    path: "/home/me/lib/".into(),
                    name: Some("lib".into()),
                },
            ],
        };
        assert_eq!(
            keys(&workspace),
            ["wsl:Ubuntu:/home/me/proj", "wsl:Ubuntu:/home/me/lib"]
        );
        assert_eq!(
            keys(&Location::Local(PathBuf::from("/home/me/proj"))),
            ["/home/me/proj"]
        );
    }
}
