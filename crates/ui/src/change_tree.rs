//! Changed files as a folder tree with compact folders (a chain of folders
//! holding only one folder shows as `a/b/c`), or as a flat list.

use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Dir {
        /// Full `/`-separated path: identifies the folder for collapsing.
        path: String,
        /// Shown name, compacted (`src/editor`).
        label: String,
        depth: usize,
        collapsed: bool,
    },
    File {
        /// Index of the item the file came from.
        item: usize,
        name: String,
        /// Containing folder (only filled in list mode).
        dir: String,
        depth: usize,
    },
}

#[derive(Default)]
struct Folder {
    folders: BTreeMap<String, Folder>,
    files: Vec<(String, usize)>,
}

/// `paths` (`/`-separated, item order) as tree rows. Folders come before
/// files; children of `collapsed` folders are left out.
pub fn tree(paths: &[&str], collapsed: &HashSet<String>) -> Vec<Node> {
    let mut root = Folder::default();
    for (item, path) in paths.iter().enumerate() {
        let mut parts: Vec<&str> = path.split('/').collect();
        let name = parts.pop().unwrap_or_default();
        let mut folder = &mut root;
        for part in parts {
            folder = folder.folders.entry(part.to_owned()).or_default();
        }
        folder.files.push((name.to_owned(), item));
    }
    let mut nodes = Vec::new();
    emit(&root, "", 0, collapsed, &mut nodes);
    nodes
}

fn emit(
    folder: &Folder,
    prefix: &str,
    depth: usize,
    collapsed: &HashSet<String>,
    nodes: &mut Vec<Node>,
) {
    for (name, mut child) in &folder.folders {
        let mut label = name.clone();
        let mut path = format!("{prefix}{name}");
        while child.files.is_empty() && child.folders.len() == 1 {
            let (next, grandchild) = child.folders.iter().next().expect("one folder");
            label = format!("{label}/{next}");
            path = format!("{path}/{next}");
            child = grandchild;
        }
        let is_collapsed = collapsed.contains(&path);
        nodes.push(Node::Dir {
            path: path.clone(),
            label,
            depth,
            collapsed: is_collapsed,
        });
        if !is_collapsed {
            emit(child, &format!("{path}/"), depth + 1, collapsed, nodes);
        }
    }
    let mut files = folder.files.clone();
    files.sort();
    nodes.extend(files.into_iter().map(|(name, item)| Node::File {
        item,
        name,
        dir: String::new(),
        depth,
    }));
}

/// `paths` as a flat list in their given order.
pub fn list(paths: &[&str]) -> Vec<Node> {
    paths
        .iter()
        .enumerate()
        .map(|(item, path)| {
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
            Node::File {
                item,
                name: name.to_owned(),
                dir: dir.to_owned(),
                depth: 0,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(nodes: &[Node]) -> Vec<String> {
        nodes
            .iter()
            .map(|node| match node {
                Node::Dir { label, depth, .. } => format!("{}{label}/", "  ".repeat(*depth)),
                Node::File { name, depth, .. } => format!("{}{name}", "  ".repeat(*depth)),
            })
            .collect()
    }

    #[test]
    fn single_folder_chains_are_compacted_and_folders_come_first() {
        let paths = [
            "README.md",
            "crates/ui/src/main.rs",
            "crates/ui/src/editor/view.rs",
            "crates/git/src/lib.rs",
            "Cargo.toml",
        ];
        assert_eq!(
            render(&tree(&paths, &HashSet::new())),
            [
                "crates/",
                "  git/src/",
                "    lib.rs",
                "  ui/src/",
                "    editor/",
                "      view.rs",
                "    main.rs",
                "Cargo.toml",
                "README.md",
            ]
        );
    }

    #[test]
    fn collapsed_folders_hide_their_contents_but_keep_file_items() {
        let paths = ["a/b/x.rs", "a/c.rs"];
        let collapsed = HashSet::from(["a".to_owned()]);
        assert_eq!(render(&tree(&paths, &collapsed)), ["a/"]);
        let open = tree(&paths, &HashSet::new());
        assert!(open.contains(&Node::File {
            item: 0,
            name: "x.rs".into(),
            dir: String::new(),
            depth: 2,
        }));
    }
}
