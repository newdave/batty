---
title: Enforce version bumps before code merges and validate release tags
status: in-progress
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
pass. Live status reporting, required-check enforcement, and the installed 0.12.1
binary remain rollout checks.
