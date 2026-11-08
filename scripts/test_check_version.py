"""Exercise the version gate through its public CLI and real temporary Git commits."""

import json
from pathlib import Path
import subprocess
import tempfile
import unittest


CHECKER = Path(__file__).with_name("check_version.py")


class VersionPolicyTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name).resolve()
        self.git("init", "-q")
        self.git("config", "user.name", "Version Test")
        self.git("config", "user.email", "version@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "core.hooksPath", "/dev/null")
        self.write_version("0.12.0")
        self.write("src/main.rs", "fn main() {}\n")
        self.base = self.commit()

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repo), *args], text=True).strip()

    def write(self, name, content):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def write_version(self, version, lock_version=None, notes="- A change.\n"):
        self.write("Cargo.toml", f'[package]\nname = "batty-cli"\nversion = "{version}"\n')
        self.write("Cargo.lock", f'version = 4\n[[package]]\nname = "batty-cli"\nversion = "{lock_version or version}"\n')
        self.write("CHANGELOG.md", f"# Changelog\n\n## {version} — 2026-11-09\n\n{notes}")

    def commit(self):
        self.git("add", "Cargo.toml", "Cargo.lock", "CHANGELOG.md", "src")
        extra = [p.name for p in self.repo.iterdir() if p.name not in {".git", "Cargo.toml", "Cargo.lock", "CHANGELOG.md", "src", "event.json"}]
        if extra:
            self.git("add", "--", *extra)
        self.git("commit", "-qm", "fixture", "--allow-empty")
        return self.git("rev-parse", "HEAD")

    def check(self, *, labels=(), tag=None):
        command = ["python3", str(CHECKER)]
        if tag is not None:
            command += ["--tag", tag]
        else:
            self.write("event.json", json.dumps({"pull_request": {"labels": [{"name": name} for name in labels]}}))
            command += ["--base", self.base, "--event", str(self.repo / "event.json")]
        return subprocess.run(command, cwd=self.repo, text=True, capture_output=True)

    def test_code_without_bump_fails(self):
        self.write("src/main.rs", "fn main() { println!(\"changed\"); }\n")
        self.commit()
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("0.12.1", result.stderr)

    def test_patch_is_default(self):
        self.write_version("0.12.1")
        self.commit()
        self.assertEqual(self.check().returncode, 0)

    def test_minor_and_major_need_matching_labels(self):
        for version, label in [("0.13.0", "semver:minor"), ("1.0.0", "semver:major")]:
            with self.subTest(version=version):
                self.write_version(version)
                self.commit()
                self.assertNotEqual(self.check().returncode, 0)
                self.assertEqual(self.check(labels=[label]).returncode, 0)

    def test_conflicting_or_misspelled_semver_labels_fail(self):
        self.write_version("0.12.1")
        self.commit()
        for labels in [("semver:patch", "semver:major"), ("semver:majer",)]:
            with self.subTest(labels=labels):
                self.assertNotEqual(self.check(labels=labels).returncode, 0)

    def test_documentation_and_test_only_changes_need_no_bump(self):
        self.write("docs/guide.md", "A guide.\n")
        self.write("tests/check.rs", "// A test.\n")
        self.commit()
        self.assertEqual(self.check().returncode, 0)

    def test_build_inputs_need_a_bump(self):
        for path in ["build.rs", "assets/prompt.md", "scripts/build.sh", ".cargo/flags.toml"]:
            with self.subTest(path=path):
                self.write(path, "changed\n")
                self.commit()
                self.assertNotEqual(self.check().returncode, 0)

    def test_lockfile_must_match_manifest(self):
        self.write_version("0.12.1", lock_version="0.12.0")
        self.commit()
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cargo.lock", result.stderr)

    def test_changelog_requires_new_version_and_nonempty_notes(self):
        self.write_version("0.12.1", notes="### Added\n\n")
        self.commit()
        self.assertNotEqual(self.check().returncode, 0)
        self.write("CHANGELOG.md", "## 0.12.0\n\n- Old notes.\n")
        self.commit()
        self.assertNotEqual(self.check().returncode, 0)

    def test_skip_and_downgrade_fail(self):
        for version in ["0.12.2", "0.11.9"]:
            with self.subTest(version=version):
                self.write_version(version)
                self.commit()
                self.assertNotEqual(self.check().returncode, 0)

    def test_release_tag_must_match_metadata(self):
        self.assertEqual(self.check(tag="v0.12.0").returncode, 0)
        self.assertNotEqual(self.check(tag="v0.12.1").returncode, 0)

    def test_release_heading_must_match_extractor_and_be_unique(self):
        for content in [
            "## 0.12.0\n\n- A change.\n",
            "## 0.12.0 notes\n\n- A change.\n",
            "## 0.12.0 — date\n\n- First.\n\n## 0.12.0 — date\n\n- Duplicate.\n",
        ]:
            with self.subTest(content=content):
                self.write("CHANGELOG.md", content)
                self.commit()
                self.assertNotEqual(self.check(tag="v0.12.0").returncode, 0)

    def test_candidate_checker_cannot_override_trusted_checker(self):
        self.write("scripts/check_version.py", 'print("always passes")\n')
        self.commit()
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("0.12.1", result.stderr)


if __name__ == "__main__":
    unittest.main()
