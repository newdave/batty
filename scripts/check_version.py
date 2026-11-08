"""Enforce committed release metadata without modifying contributor branches."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib


class VersionError(ValueError):
    pass


def git(*args):
    return subprocess.check_output(["git", *args], text=True, stderr=subprocess.PIPE)


def commit(ref):
    return git("rev-parse", "--verify", "--end-of-options", f"{ref}^{{commit}}").strip()


def read(ref, path):
    return git("show", f"{ref}:{path}")


def version_tuple(version):
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise VersionError(f"Use a stable MAJOR.MINOR.PATCH version, got {version!r}")
    return tuple(map(int, version.split(".")))


def metadata(head):
    package = tomllib.loads(read(head, "Cargo.toml"))["package"]
    version = package["version"]
    version_tuple(version)
    lock = tomllib.loads(read(head, "Cargo.lock"))
    entries = [entry for entry in lock.get("package", []) if entry["name"] == package["name"]]
    if len(entries) != 1 or entries[0]["version"] != version:
        raise VersionError("Cargo.lock package version must match Cargo.toml")
    return version


def changelog(head, version):
    content = read(head, "CHANGELOG.md")
    headings = list(re.finditer(rf"^## {re.escape(version)}(?:[ \t].*)?$", content, re.MULTILINE))
    prefix = f"## {version} — "
    if len(headings) != 1 or not headings[0].group().startswith(prefix) or not headings[0].group()[len(prefix):].strip():
        raise VersionError(f"CHANGELOG.md needs exactly one '## {version} — <date>' section")
    heading = headings[0]
    notes = re.split(r"^## ", content[heading.end():], maxsplit=1, flags=re.MULTILINE)[0]
    if not any(line.strip() and not line.lstrip().startswith(("#", "<!--")) for line in notes.splitlines()):
        raise VersionError(f"CHANGELOG.md section for {version} needs release notes")


def bump_level(event):
    labels = {label["name"] for label in event.get("pull_request", {}).get("labels", [])}
    selected = {name.removeprefix("semver:") for name in labels if name.startswith("semver:")}
    if selected - {"patch", "minor", "major"} or len(selected) > 1:
        raise VersionError("Choose at most one label: semver:patch, semver:minor, semver:major")
    return next(iter(selected), "patch")


def check(base, head, level):
    version = metadata(head)
    previous = tomllib.loads(read(base, "Cargo.toml"))["package"]["version"]
    major, minor, patch = version_tuple(previous)
    paths = git("diff", "--name-only", "--no-renames", "-z", base, head, "--").split("\0")
    code_changed = any(
        path in {"Cargo.toml", "Cargo.lock", "build.rs"}
        or path.startswith(("src/", "assets/", "scripts/", ".cargo/"))
        for path in paths
    )
    if not code_changed and version == previous:
        return "No code or build-input changes; no version bump required."
    expected = {
        "patch": (major, minor, patch + 1),
        "minor": (major, minor + 1, 0),
        "major": (major + 1, 0, 0),
    }[level]
    expected_text = ".".join(map(str, expected))
    if version_tuple(version) != expected:
        raise VersionError(f"{level} bump from {previous} requires {expected_text}, got {version}")
    if "CHANGELOG.md" not in paths:
        raise VersionError("Update CHANGELOG.md with this release's changes")
    changelog(head, version)
    return f"Version policy passed: {previous} -> {version} ({level})."


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--base", help="PR base commit")
    mode.add_argument("--tag", help="Release tag, exactly v<Cargo.toml version>")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--event", type=Path, help="GitHub event JSON containing PR labels")
    args = parser.parse_args()
    try:
        head = commit(args.head)
        if args.tag:
            version = metadata(head)
            if args.tag != f"v{version}":
                raise VersionError(f"Release tag must be v{version}, got {args.tag!r}")
            changelog(head, version)
            print(f"Release metadata passed: {args.tag}.")
        else:
            event = json.loads(args.event.read_text()) if args.event else {}
            print(check(commit(args.base), head, bump_level(event)))
    except (VersionError, OSError, KeyError, TypeError, tomllib.TOMLDecodeError,
            json.JSONDecodeError, subprocess.CalledProcessError) as error:
        print(f"Version policy failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
