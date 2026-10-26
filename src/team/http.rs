//! Shared blocking HTTP agent and secret redaction for chat integrations.
//!
//! Every Telegram/Discord request goes through [`agent`] so a stalled network
//! can never block the daemon poll loop indefinitely.

use std::sync::OnceLock;
use std::time::Duration;

/// Connection establishment timeout.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Time allowed to wait for response bytes. Any server-side long-poll must use
/// a wait strictly below this (Telegram `getUpdates` uses `timeout=0`).
pub const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Time allowed to write the request.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Process-wide `ureq::Agent` with bounded timeouts.
pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(READ_TIMEOUT)
            .timeout_write(WRITE_TIMEOUT)
            .build()
    })
}

/// Replace every occurrence of `secret` in `text` with `<redacted>`. ureq
/// includes the request URL in error text, and Telegram embeds the bot token in
/// the URL path; Discord error bodies may echo credentials.
pub fn redact_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, "<redacted>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_are_bounded_and_ordered() {
        assert!(CONNECT_TIMEOUT <= Duration::from_secs(5));
        assert!(READ_TIMEOUT <= Duration::from_secs(15));
        assert!(WRITE_TIMEOUT <= Duration::from_secs(10));
        // getUpdates long-poll timeout is 0s, always below the read timeout.
        assert!(Duration::from_secs(0) < READ_TIMEOUT);
    }

    #[test]
    fn agent_fails_fast_on_refused_connection() {
        let start = std::time::Instant::now();
        let _ = agent().get("http://127.0.0.1:1/").call();
        assert!(start.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn redact_secret_replaces_all_occurrences() {
        let out = redact_secret("GET /bot123:ABC/x failed for 123:ABC", "123:ABC");
        assert_eq!(out, "GET /bot<redacted>/x failed for <redacted>");
    }

    #[test]
    fn redact_secret_ignores_empty_secret() {
        assert_eq!(redact_secret("hello", ""), "hello");
    }
}
