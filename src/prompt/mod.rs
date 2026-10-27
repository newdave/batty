//! ANSI stripping for agent PTY output.

use regex::Regex;

/// Strip ANSI escape sequences from PTY output.
pub fn strip_ansi(input: &str) -> String {
    // Matches CSI sequences (ESC [ ... final byte), OSC sequences (ESC ] ... ST),
    // and simple two-byte escapes (ESC + one char).
    static ANSI_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[^\[\]]").unwrap()
    });
    ANSI_RE.replace_all(input, "").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ANSI stripping ──

    #[test]
    fn strip_ansi_removes_csi() {
        let input = "\x1b[31mERROR\x1b[0m: something broke";
        assert_eq!(strip_ansi(input), "ERROR: something broke");
    }

    #[test]
    fn strip_ansi_removes_osc() {
        let input = "\x1b]0;title\x07some text";
        assert_eq!(strip_ansi(input), "some text");
    }

    #[test]
    fn strip_ansi_passthrough_clean_text() {
        let input = "just normal text";
        assert_eq!(strip_ansi(input), "just normal text");
    }
}
