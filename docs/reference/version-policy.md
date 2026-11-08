# Version policy

Every PR that changes Batty code or build inputs must prepare a new stable
`MAJOR.MINOR.PATCH` version before merging. Update the package version in both
`Cargo.toml` and `Cargo.lock`, and add release notes under exactly one heading
of the form `## <version> — <date>` in `CHANGELOG.md`.

The default is one patch increment from the current base branch, for example
`0.12.0` to `0.12.1`. Apply `semver:minor` for `0.13.0` or `semver:major` for
`1.0.0`. `semver:patch` explicitly selects the default. Conflicting or misspelled
SemVer labels fail the check. Rebase or update stale branches when main advances.

The gate covers `src/`, `assets/`, `scripts/`, `.cargo/`, `Cargo.toml`,
`Cargo.lock`, and `build.rs`. Documentation-only and test-only PRs do not need a
bump; package and lockfile versions must still agree. CI-only edits outside
those paths do not change the installed binary and may retain its version.

Run the same check locally against committed changes:

```sh
python3 -m unittest discover -s scripts -p test_check_version.py
python3 scripts/check_version.py --base origin/main --head HEAD
python3 scripts/check_version.py --base origin/main --head HEAD --event /path/to/pr-event.json
python3 scripts/check_version.py --tag v0.12.1
```

GitHub runs candidate checker tests in a read-only PR job. Enforcement runs the
trusted base branch's checker, fetches candidate Git objects without checking
them out, and reads candidate manifests and release notes as data. Its token can
only read repository contents and write commit statuses. It publishes the
`Version Policy` status on the PR head; a PR cannot replace its own enforcement
script. Label changes trigger a fresh check.

Commit messages containing `[skip ci]` can skip ordinary PR tests but cannot
skip this trusted-base enforcement workflow. The version status remains required.

Main must require the `Version Policy` status and up-to-date branches. This
turns a failing check into a merge block. The first installation of this trusted
workflow needs a reviewed bootstrap merge; subsequent PRs use main's policy.

The existing release workflow validates the tag, package versions, and release
notes before any platform build. No workflow added here rewrites PR branches,
creates release tags, publishes packages, or installs binaries automatically.
Never install changed code under a previously shipped version number.
