//! Secret redaction applied before any text is hashed, sent to the Gateway,
//! logged or stored in an event. Matches become `[REDACTED:<kind>]`.

use std::sync::OnceLock;

use regex::{Captures, Regex};

struct Rule {
    re: Regex,
    /// Regex expansion template. Rules that must keep surrounding syntax
    /// (the variable name of an env assignment, `Bearer `) capture it in
    /// group 1 and re-emit it.
    template: String,
    /// Optional veto on a match; `false` keeps the matched text.
    accept: Option<fn(&str) -> bool>,
}

fn rule(kind: &str, pattern: &str) -> Rule {
    Rule {
        re: Regex::new(pattern).expect("redaction pattern compiles"),
        template: format!("[REDACTED:{kind}]"),
        accept: None,
    }
}

fn rule_keep_prefix(kind: &str, pattern: &str) -> Rule {
    Rule {
        re: Regex::new(pattern).expect("redaction pattern compiles"),
        template: format!("${{1}}[REDACTED:{kind}]"),
        accept: None,
    }
}

/// A base64-looking run only counts as a secret when it mixes upper case,
/// lower case and digits. That keeps long snake_case identifiers, paths and
/// lower-case hex digests (40-char git SHAs) intact.
fn mixed_alphanumeric(run: &str) -> bool {
    run.chars().any(|c| c.is_ascii_uppercase())
        && run.chars().any(|c| c.is_ascii_lowercase())
        && run.chars().any(|c| c.is_ascii_digit())
}

fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let mut base64 = rule("base64", r"[A-Za-z0-9+/_-]{40,}={0,2}");
        base64.accept = Some(mixed_alphanumeric);
        vec![
            // Multi-line and structured secrets first so later rules never
            // see their interior.
            rule(
                "private_key",
                r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----(?:.*?-----END [A-Z ]*PRIVATE KEY-----|[A-Za-z0-9+/=\s]*)",
            ),
            Rule {
                re: Regex::new(r"://[^\s/:@]+:[^\s/@]+@").expect("redaction pattern compiles"),
                template: "://[REDACTED:url_credentials]@".to_string(),
                accept: None,
            },
            rule(
                "jwt",
                r"\beyJ[A-Za-z0-9_-]{5,}\.eyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}",
            ),
            rule("anthropic_key", r"\bsk-ant-[A-Za-z0-9_-]{20,}"),
            rule("openai_key", r"\bsk-[A-Za-z0-9_-]{20,}"),
            rule("github_token", r"\bgh[pousr]_[A-Za-z0-9]{36,}"),
            rule("github_token", r"\bgithub_pat_[A-Za-z0-9_]{22,}"),
            rule("slack_token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}"),
            rule("aws_access_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
            rule_keep_prefix(
                "aws_secret",
                r#"(?i)(aws_secret_access_key[ \t]*[=:][ \t]*)["']?[A-Za-z0-9/+=]{20,}["']?"#,
            ),
            rule("google_key", r"\bAIza[0-9A-Za-z_-]{35}"),
            rule_keep_prefix("bearer", r"(?i)(\bBearer[ \t]+)[A-Za-z0-9._~+/=-]{16,}"),
            rule_keep_prefix(
                "env_secret",
                r"(?m)^([ \t]*(?:export[ \t]+)?[A-Z0-9_]*(?:KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL)[A-Z0-9_]*[ \t]*=[ \t]*)\S+",
            ),
            rule("hex_secret", r"\b[0-9A-Fa-f]{48,}\b"),
            base64,
        ]
    })
}

/// Replace every recognised secret in `text` with `[REDACTED:<kind>]`.
/// Idempotent: redacting already-redacted text changes nothing.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for rule in rules() {
        out = rule
            .re
            .replace_all(&out, |caps: &Captures| {
                let whole = caps.get(0).map_or("", |m| m.as_str());
                if rule.accept.is_some_and(|accept| !accept(whole)) {
                    return whole.to_string();
                }
                let mut replaced = String::new();
                caps.expand(&rule.template, &mut replaced);
                replaced
            })
            .into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures are assembled at runtime so secret scanners do not flag the
    /// source file.
    fn join(parts: &[&str]) -> String {
        parts.concat()
    }

    fn assert_redacted(text: &str, kind: &str, secret: &str) {
        let out = redact(text);
        assert!(!out.contains(secret), "secret leaked: {out}");
        assert!(
            out.contains(&format!("[REDACTED:{kind}]")),
            "expected kind {kind} in: {out}"
        );
    }

    #[test]
    fn redacts_anthropic_key() {
        let key = join(&["sk-", "ant-api03-", "AbCdEfGhIjKlMnOpQrStUv"]);
        assert_redacted(&format!("key {key} here"), "anthropic_key", &key);
    }

    #[test]
    fn redacts_openai_key() {
        let key = join(&["sk-", "proj_AbCdEfGhIjKlMnOpQrStUvWx"]);
        let out = redact(&format!("OPENAI={key}!"));
        assert!(!out.contains(&key));
        assert!(out.contains("[REDACTED:"));
        assert_redacted(&format!("use {key} now"), "openai_key", &key);
    }

    #[test]
    fn redacts_github_tokens() {
        let classic = join(&["gh", "p_", &"a1B2".repeat(9)]);
        assert_redacted(&format!("token={classic}"), "github_token", &classic);
        let fine = join(&["github", "_pat_", "11ABCDEFG0abcdefghijkl_mnopqrstuvwxyz"]);
        assert_redacted(&format!("auth {fine} end"), "github_token", &fine);
    }

    #[test]
    fn redacts_slack_token() {
        let tok = join(&["xo", "xb-", "1234567890-abcdefghijklmn"]);
        assert_redacted(&format!("slack {tok}."), "slack_token", &tok);
    }

    #[test]
    fn redacts_aws_keys() {
        let id = join(&["AKIA", "IOSFODNN7EXAMPLE"]);
        assert_redacted(&format!("id {id} x"), "aws_access_key", &id);
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let out = redact(&format!("aws_secret_access_key = {secret}"));
        assert!(!out.contains(secret));
        assert!(out.starts_with("aws_secret_access_key = [REDACTED:aws_secret]"));
    }

    #[test]
    fn redacts_google_key() {
        let key = join(&["AI", "zaSyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q"]);
        assert_redacted(&format!("g {key} g"), "google_key", &key);
    }

    #[test]
    fn redacts_jwt() {
        let jwt = join(&[
            "ey",
            "JhbGciOiJIUzI1NiJ9.",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0.",
            "dBjftJeZ4CVPmB92K27uhbUJU1p1r",
        ]);
        assert_redacted(&format!("jwt: {jwt}"), "jwt", &jwt);
    }

    #[test]
    fn redacts_private_key_block() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nabc123\n-----END RSA PRIVATE KEY-----";
        let out = redact(&format!("before\n{pem}\nafter"));
        assert_eq!(out, "before\n[REDACTED:private_key]\nafter");
    }

    #[test]
    fn redacts_truncated_private_key_block() {
        let out = redact("x\n-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\nkqhkiG9w0B");
        assert!(out.contains("[REDACTED:private_key]"));
        assert!(!out.contains("MIIEvQ"));
    }

    #[test]
    fn redacts_url_credentials() {
        let out = redact("clone https://user:hunter2@example.com/repo.git now");
        assert_eq!(
            out,
            "clone https://[REDACTED:url_credentials]@example.com/repo.git now"
        );
    }

    #[test]
    fn keeps_urls_without_credentials() {
        let text = "see https://example.com:8080/path and ssh://git@host/repo";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn redacts_bearer_token() {
        let out = redact("Authorization: Bearer abcdef1234567890XYZ");
        assert_eq!(out, "Authorization: Bearer [REDACTED:bearer]");
    }

    #[test]
    fn redacts_env_style_secrets() {
        let out = redact("export API_TOKEN=abc123\nDB_PASSWORD = s3cr3t\nplain=1");
        assert_eq!(
            out,
            "export API_TOKEN=[REDACTED:env_secret]\nDB_PASSWORD = [REDACTED:env_secret]\nplain=1"
        );
    }

    #[test]
    fn env_rule_needs_uppercase_assignment() {
        let text = "const MAX_KEY_LEN: usize = 5;\nlet key = 3;";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn redacts_long_hex_run() {
        let hex = "0123456789abcdef".repeat(4);
        assert_redacted(&format!("digest {hex} end"), "hex_secret", &hex);
    }

    #[test]
    fn redacts_long_base64_run() {
        let b64 = "Zm9vYmFyQmF6UXV4MTIzNDU2Nzg5MEFCQ0RFRkdISUo=";
        assert_redacted(&format!("blob {b64} end"), "base64", b64);
    }

    #[test]
    fn keeps_git_shas_uuids_and_ordinary_code() {
        let text = "commit a94a8fe5ccb19ba61c4c0873d391e987982fbbd3 id 123e4567-e89b-12d3-a456-426614174000\n\
                    fn handle_engineer_completion_and_wait_for_review() { let x = foo(1, \"bar\"); }\n\
                    /Users/dhill/Projects/batty/src/team/decide/redact.rs risk-assessment\n";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn redaction_is_idempotent() {
        let key = join(&["sk-", "ant-api03-", "AbCdEfGhIjKlMnOpQrStUv"]);
        let text = format!(
            "{key}\nexport SECRET_KEY=abc\nBearer abcdef1234567890XYZ\nhttps://u:p@h.io/\n{}\n-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----",
            "0123456789abcdef".repeat(4)
        );
        let once = redact(&text);
        assert_eq!(redact(&once), once);
    }

    #[test]
    fn empty_and_plain_text_pass_through() {
        assert_eq!(redact(""), "");
        assert_eq!(redact("hello world"), "hello world");
    }
}
