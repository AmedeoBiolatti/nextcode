"""Preserve Cargo freshness across checkouts, only for byte-identical sources.

The manifest travels in the same cache as target/. Changed/new files get
fresh timestamps so Cargo rebuilds them and their dependants.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess


def tracked_files(root):
    names = subprocess.check_output(["git", "-C", str(root), "ls-files", "-z"])
    for name in names.split(b"\0"):
        if name:
            relative = os.fsdecode(name)
            path = root / relative
            if path.is_file() and not path.is_symlink():
                yield relative, path


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def snapshot(root, manifest):
    files = {
        name: {"sha256": digest(path), "mtime_ns": path.stat().st_mtime_ns}
        for name, path in tracked_files(root)
    }
    manifest.parent.mkdir(parents=True, exist_ok=True)
    manifest.write_text(json.dumps({"version": 1, "files": files}), encoding="utf-8")
    return len(files)


def restore(root, manifest):
    data = (
        json.loads(manifest.read_text(encoding="utf-8"))
        if manifest.exists() else {"version": 1, "files": {}}
    )
    if data.get("version") != 1:
        raise ValueError("unsupported source cache manifest")
    restored = 0
    # Iterate the current index, never paths supplied by the cached manifest.
    for name, path in tracked_files(root):
        previous = data["files"].get(name)
        if previous and digest(path) == previous["sha256"]:
            mtime = previous["mtime_ns"]
            if not isinstance(mtime, int) or mtime < 0:
                raise ValueError("invalid source timestamp")
            os.utime(path, ns=(path.stat().st_atime_ns, mtime))
            restored += 1
        else:
            # A cache can be published after this job checked out its sources.
            # Use the restore time, not the potentially earlier checkout time.
            os.utime(path, None)
    return restored


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("restore", "snapshot"))
    args = parser.parse_args()
    root = Path.cwd()
    manifest = root / ".ci-cache/source-files.json"
    operation = restore if args.operation == "restore" else snapshot
    count = operation(root, manifest)
    print(f"Source cache {args.operation}: {count} tracked files")


if __name__ == "__main__":
    main()
