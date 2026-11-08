---
title: Use Caveman skills in Codex agents without duplicating plugin content
status: done
priority: normal
---

Extend the existing Codex provider overlay with installed Caveman skills for
terse replies, broad discovery, review comments, and commit messages. Preserve
technical payloads, completion evidence, explicit style overrides, and normal
prose in persisted artifacts. Do not implicitly compress files, send repository
content to another provider, install plugins, or change global configuration.

Acceptance:

- Both Codex backend aliases receive the guidance; other providers do not.
- The provider prompt remains below the existing 1,600-byte regression bound.
- Existing interactive and SDK launch coverage continues to pass.
- Validate a frozen source snapshot in an isolated checkout, with focused tests,
  `cargo test`, `make lint`, and independent review.

Validation:

- The provider regression failed on the missing `caveman` instruction before the
  change; all three focused regressions pass afterward.
- The frozen feature snapshot at `/private/tmp/batty-caveman-check.B4tmlr`
  passes `cargo test`: 4,360 library tests, 154 binary tests, and nine contract
  tests passed; 154 library tests were ignored. No failures.
- `make lint` passes; independent review returned APPROVE.
- Initial sandbox validation denied loopback HTTP fixtures. An unsandboxed
  control passed. A separate run with `TMPDIR=/tmp` triggered the board's
  deliberate symlink rejection. The final full run used canonical
  `TMPDIR=/private/tmp` outside the sandbox and passed, without source changes.
- All four feature source/documentation files in the working checkout match the
  verified snapshot. Unrelated ongoing completion repairs were excluded.

No binary was installed or restarted. This change does not repair queue
processing or prove token savings in a live provider session.
