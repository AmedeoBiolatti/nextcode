"""Cache reuse must never hide a source change from Cargo."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest

SPEC = importlib.util.spec_from_file_location(
    "ci_source_cache", Path(__file__).resolve().parents[1] / "ci_source_cache.py"
)
CACHE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CACHE)


class SourceCacheTests(unittest.TestCase):
    def test_only_identical_tracked_files_get_their_old_timestamps(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            names = ["unchanged.rs", "changed.rs", "deleted.rs"]
            for name in names:
                (root / name).write_text("original", encoding="utf-8")
            subprocess.run(["git", "-C", str(root), "add", "."], check=True)
            manifest = root / "cache.json"
            old = 1_600_000_000_000_000_000
            new = old + 10_000_000_000
            for name in names:
                os.utime(root / name, ns=(old, old))
            self.assertEqual(CACHE.snapshot(root, manifest), 3)
            for name in names:
                os.utime(root / name, ns=(new, new))
            (root / "changed.rs").write_text("modified", encoding="utf-8")
            os.utime(root / "changed.rs", ns=(new, new))
            (root / "deleted.rs").unlink()
            (root / "added.rs").write_text("new", encoding="utf-8")
            subprocess.run(["git", "-C", str(root), "add", "added.rs"], check=True)
            os.utime(root / "added.rs", ns=(new, new))
            self.assertEqual(CACHE.restore(root, manifest), 1)
            self.assertEqual((root / "unchanged.rs").stat().st_mtime_ns, old)
            self.assertGreater((root / "changed.rs").stat().st_mtime_ns, new)
            self.assertGreater((root / "added.rs").stat().st_mtime_ns, new)
            self.assertEqual((root / "changed.rs").read_text(), "modified")
            self.assertFalse((root / "deleted.rs").exists())

    def test_missing_manifest_is_a_cold_build(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            self.assertEqual(CACHE.restore(root, root / "missing.json"), 0)

    def test_cargo_reuses_identical_sources_but_rebuilds_changed_sources(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text(
                '[package]\nname="cached_fixture"\nversion="0.1.0"\nedition="2024"\n',
                encoding="utf-8",
            )
            source = root / "src/main.rs"
            source.write_text('fn main() { println!("before"); }', encoding="utf-8")
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            subprocess.run(["git", "-C", str(root), "add", "Cargo.toml", "src"], check=True)

            def run():
                return subprocess.run(
                    ["cargo", "run", "--offline"], cwd=root,
                    env=dict(os.environ, CARGO_TERM_COLOR="never", CARGO_TARGET_DIR=str(root / "target")),
                    capture_output=True, text=True, check=True,
                )

            self.assertEqual(run().stdout.strip(), "before")
            manifest = root / "cache.json"
            CACHE.snapshot(root, manifest)
            now = time.time_ns()
            for _, path in CACHE.tracked_files(root):
                os.utime(path, ns=(now, now))
            CACHE.restore(root, manifest)
            warm = run()
            self.assertEqual(warm.stdout.strip(), "before")
            self.assertNotIn("Compiling cached_fixture", warm.stderr)
            source.write_text('fn main() { println!("after!"); }', encoding="utf-8")
            # Even a changed source backdated before the cached build must
            # rebuild. Equal lengths also guard against size-only checks.
            os.utime(source, ns=(now - 10_000_000_000, now - 10_000_000_000))
            CACHE.restore(root, manifest)
            changed = run()
            self.assertEqual(changed.stdout.strip(), "after!")
            self.assertIn("Compiling cached_fixture", changed.stderr)


if __name__ == "__main__":
    unittest.main()
