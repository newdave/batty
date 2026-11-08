---
title: Enforce version bumps before code merges and validate release tags
status: done
priority: high
---

Implementation plan:

- Add a tested, dependency-free checker for committed Cargo versions, changelog
  notes, patch-by-default PR bumps, and explicit minor/major labels.
- Run the checker on PR updates and label changes; validate tags before release
  builds. Keep candidate tests read-only; scope trusted enforcement's write
  access to commit status reporting.
- Bump this release to 0.12.1 and document the project release requirement.
- Verify the isolated checkout, obtain independent review, and publish the change.
- Require the version check on main while preserving existing branch safeguards,
  then build, sign, install, and verify the versioned binary.

Code and build-input PRs require one bump from the current base version.
Documentation-only and test-only changes do not require a bump. Conflicting
SemVer labels fail. This checker enforces a prepared bump; it does not grant a
bot permission to rewrite contributor branches or publish packages automatically.

Observed failure: the Superpowers/Caveman commits `6cf88a55` and `0ffd7702`
changed the installed binary without changing 0.12.0. Existing release automation
did not enforce a version bump on PRs. This task corrects that omission and
requires future code merges to declare their version change mechanically.

Review caught a trust-boundary gap in candidate-owned enforcement and a mismatch
between accepted changelog headings and the release extractor. The trusted-base
workflow and failing-then-passing heading regressions address those findings.

Local validation: 12 policy tests and 4,523 Rust tests pass, with 154 ignored.
`make lint`, `actionlint` on both changed workflows, and independent final review
pass.

Live rollout evidence:

- PR #29 merged the implementation and version bump as `6af79f52`.
- Probe commit `5820c024` replaced its own checker with an always-success script
  without bumping 0.12.1. Trusted enforcement rejected it: "patch bump from
  0.12.1 requires 0.12.2, got 0.12.1". GitHub run `42535397273` recorded the
  failure; main's protection then reported this candidate as BLOCKED.
- Probe commit `2673612b` restored the real checker and retained only a docs
  change. GitHub run `42535684126` reported Version Policy success. Both probe
  commits used `[skip ci]`; trusted enforcement still ran.
- Main now requires the Version Policy status and up-to-date branches, including
  administrators. PRs are required with zero mandatory human approvals. Main had
  no pre-existing branch protection to replace.
- The semver:patch, semver:minor, and semver:major labels were created.
- The committed, verified release was built and installed on this laptop.
  `~/.cargo/bin/batty --version` reports 0.12.1 and macOS signature verification
  passes. The prior binary is backed up in `.batty/install-backups/`.

No release tag was created and no package was published. Unrelated root-checkout
repairs remain untouched; no live daemon recovery is claimed.
