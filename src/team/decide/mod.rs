//! Jev supervisory decisions: redaction, Gateway client, budget, change gate
//! and per-path question builders. Everything here is pure or blocking-IO and
//! never touches the daemon; the daemon runs these as background jobs.

pub mod budget;
pub mod client;
pub mod gate;
pub mod redact;

use std::fmt;

use serde::Deserialize;

/// One typed question about a block of state text.
#[derive(Debug, Clone, PartialEq)]
pub enum Question {
    Predicate {
        name: String,
        instructions: String,
    },
    Choice {
        name: String,
        instructions: String,
        /// `(value, optional description)`.
        choices: Vec<(String, Option<String>)>,
    },
    Score {
        name: String,
        instructions: String,
        /// Ordered labels, lowest first (2 to 10 levels).
        levels: Vec<String>,
    },
}

impl Question {
    pub fn name(&self) -> &str {
        match self {
            Question::Predicate { name, .. }
            | Question::Choice { name, .. }
            | Question::Score { name, .. } => name,
        }
    }
}

/// One answer, in question order. Optional fields mirror the Gateway schema.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Predicate {
        name: Option<String>,
        probability: f64,
    },
    Choice {
        name: Option<String>,
        choice: serde_json::Value,
        confidence: Option<f64>,
    },
    Score {
        name: Option<String>,
        score: f64,
        confidence: Option<f64>,
    },
    Refusal {
        name: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

impl Answer {
    pub fn name(&self) -> Option<&str> {
        match self {
            Answer::Predicate { name, .. }
            | Answer::Choice { name, .. }
            | Answer::Score { name, .. }
            | Answer::Refusal { name } => name.as_deref(),
            Answer::Unknown => None,
        }
    }
}

/// A parsed Gateway reply.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionResponse {
    pub answers: Vec<Answer>,
    /// USD cost reported by the Gateway (`provider_metadata.gateway.cost`).
    pub cost_usd: f64,
    pub input_tokens: u64,
}

/// Why a decision fell back to the heuristic. The label is the only text that
/// ever reaches an event: never raw content, URLs or keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    Disabled,
    NoKey,
    Budget,
    Timeout,
    /// HTTP status, or 0 when the request failed before a status arrived.
    Http(u16),
    InvalidAnswer,
    Refusal,
    LowConfidence,
}

impl fmt::Display for Fallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fallback::Disabled => f.write_str("disabled"),
            Fallback::NoKey => f.write_str("no_key"),
            Fallback::Budget => f.write_str("budget"),
            Fallback::Timeout => f.write_str("timeout"),
            Fallback::Http(code) => write!(f, "http_{code}"),
            Fallback::InvalidAnswer => f.write_str("invalid_answer"),
            Fallback::Refusal => f.write_str("refusal"),
            Fallback::LowConfidence => f.write_str("low_confidence"),
        }
    }
}

impl std::error::Error for Fallback {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_labels_match_the_event_vocabulary() {
        assert_eq!(Fallback::Disabled.to_string(), "disabled");
        assert_eq!(Fallback::NoKey.to_string(), "no_key");
        assert_eq!(Fallback::Budget.to_string(), "budget");
        assert_eq!(Fallback::Timeout.to_string(), "timeout");
        assert_eq!(Fallback::Http(429).to_string(), "http_429");
        assert_eq!(Fallback::InvalidAnswer.to_string(), "invalid_answer");
        assert_eq!(Fallback::Refusal.to_string(), "refusal");
        assert_eq!(Fallback::LowConfidence.to_string(), "low_confidence");
    }

    #[test]
    fn answer_deserializes_each_kind() {
        let predicate: Answer =
            serde_json::from_str(r#"{"type":"predicate","name":"done","probability":0.9}"#)
                .unwrap();
        assert_eq!(predicate.name(), Some("done"));
        let choice: Answer = serde_json::from_str(
            r#"{"type":"choice","name":"kind","choice":"noise","confidence":0.7}"#,
        )
        .unwrap();
        assert!(matches!(choice, Answer::Choice { confidence: Some(c), .. } if c == 0.7));
        let refusal: Answer = serde_json::from_str(r#"{"type":"refusal"}"#).unwrap();
        assert_eq!(refusal, Answer::Refusal { name: None });
        let unknown: Answer = serde_json::from_str(r#"{"type":"mystery","x":1}"#).unwrap();
        assert_eq!(unknown, Answer::Unknown);
    }

    #[test]
    fn question_name_reads_every_variant() {
        let q = Question::Score {
            name: "s".into(),
            instructions: "i".into(),
            levels: vec!["a".into(), "b".into()],
        };
        assert_eq!(q.name(), "s");
    }
}
