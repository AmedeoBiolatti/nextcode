//! Recently opened projects, most recent first, kept in the platform's
//! local data dir (`next_code/recent.json`).

use std::path::PathBuf;

use crate::project::Location;

const LIMIT: usize = 20;

pub(crate) fn path() -> Option<PathBuf> {
    // Unit tests open real workspaces: keep them out of the user's list,
    // and out of each other's (tests run on threads of one process).
    if cfg!(test) {
        let thread = format!("{:?}", std::thread::current().id());
        let thread: String = thread.chars().filter(char::is_ascii_digit).collect();
        return Some(std::env::temp_dir().join(format!(
            "next_code-test-recent-{}-{thread}.json",
            std::process::id()
        )));
    }
    Some(
        dirs::data_local_dir()?
            .join("next_code")
            .join("recent.json"),
    )
}

/// The list; unreadable or missing files mean an empty list.
pub fn load() -> Vec<Location> {
    path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

/// Moves `location` to the front. Failing to save only costs the history.
pub fn record(location: &Location) {
    let Some(path) = path() else {
        return;
    };
    let mut list = load();
    push_front(&mut list, location.clone());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if let Ok(json) = serde_json::to_string_pretty(&list) {
        std::fs::write(path, json).ok();
    }
}

fn push_front(list: &mut Vec<Location>, location: Location) {
    list.retain(|known| *known != location);
    list.insert(0, location);
    list.truncate(LIMIT);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reopening_moves_a_project_to_the_front_without_duplicates() {
        let local = |name: &str| Location::Local(PathBuf::from(name));
        let mut list: Vec<Location> = (0..LIMIT).map(|n| local(&n.to_string())).collect();
        push_front(&mut list, local("5"));
        assert_eq!(list.len(), LIMIT);
        assert_eq!(list[0], local("5"));
        assert_eq!(list.iter().filter(|l| **l == local("5")).count(), 1);
        push_front(&mut list, local("new"));
        assert_eq!(list.len(), LIMIT, "the oldest entry falls off");
        assert_eq!(list[0], local("new"));
    }
}
