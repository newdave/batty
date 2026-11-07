//! Jev supervisory decisions: redaction, Gateway client, budget, change gate
//! and per-path question builders. Everything here is pure or blocking-IO and
//! never touches the daemon; the daemon runs these as background jobs.

pub mod redact;
