//! Conversions between filesystem paths and `file://` URIs.

use std::path::{Path, PathBuf};

use lsp_types::Uri;
use percent_encoding::percent_decode_str;
use url::Url;

/// `file://` URI for an absolute path, percent-encoded (`C:\a b` → `file:///C:/a%20b`).
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    Url::from_file_path(path).ok()?.as_str().parse().ok()
}

/// Filesystem path for a `file://` URI; `None` for other schemes.
///
/// Servers spell the same file differently (pyright sends `file:///c%3A/...`),
/// so this decodes fully and upper-cases Windows drive letters; compare paths,
/// never URI strings.
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let url = Url::parse(uri.as_str()).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    let decoded = percent_decode_str(url.path()).decode_utf8().ok()?;
    if cfg!(windows) {
        // `/c:/dir/file` → `C:\dir\file`
        let path = decoded.strip_prefix('/').unwrap_or(&decoded);
        let mut path = path.replace('/', "\\");
        if path.as_bytes().get(1) == Some(&b':') {
            path[..1].make_ascii_uppercase();
        }
        Some(PathBuf::from(path))
    } else {
        Some(PathBuf::from(decoded.into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_paths_with_spaces_and_unicode() {
        let path = if cfg!(windows) {
            PathBuf::from(r"C:\Users\me\My Project\é.py")
        } else {
            PathBuf::from("/home/me/My Project/é.py")
        };
        let uri = path_to_uri(&path).unwrap();
        assert!(
            uri.as_str().contains("My%20Project/%C3%A9.py"),
            "{}",
            uri.as_str()
        );
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }

    #[cfg(windows)]
    #[test]
    fn encoded_lowercase_drive_letters_resolve_to_the_same_path() {
        let uri: Uri = "file:///c%3A/dev/py/main.py".parse().unwrap();
        assert_eq!(
            uri_to_path(&uri).unwrap(),
            PathBuf::from(r"C:\dev\py\main.py")
        );
    }

    #[test]
    fn non_file_uris_have_no_path() {
        let uri: Uri = "untitled:Untitled-1".parse().unwrap();
        assert_eq!(uri_to_path(&uri), None);
    }
}
