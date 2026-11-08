# Fail cleanly and retry kanban-md release downloads

Status: ready for review

The fork's existing main CI run failed four Linux jobs while installing
kanban-md: an HTTP error body reached tar, which reported that the input was not
gzip data. Release assets for the pinned version exist. Streaming a download
without checking the HTTP response hides the underlying error and prevents a
reliable retry.

All four installation steps now download to a runner-local temporary file with
HTTP failure detection and bounded retries before extracting the completed
archive. The pinned version and installed executable remain unchanged.

Acceptance:

- Every kanban-md installation checks HTTP status and retries transient errors.
- A failed download cannot reach extraction.
- Fork CI passes dependency installation on the published repair branch.

Local validation parsed the workflow successfully. Hosted installation checks remain pending the repair PR.
