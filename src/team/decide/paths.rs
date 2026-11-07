//! Per-path question builders, input builders and answer mappers. Everything
//! here is pure. Inputs passed to the builders must already be redacted.

use serde_json::Value;

use super::{Answer, Fallback, Question};
use crate::shim::classifier::ScreenVerdict;

/// Hard cap on the redacted input, in characters (about 24K tokens of the
/// model's 32K context).
pub const MAX_INPUT_CHARS: usize = 96_000;
/// Screens send only the last lines of the pane.
pub const SCREEN_TAIL_LINES: usize = 100;
/// Rubric levels, lowest first.
pub const RUBRIC_LEVELS: [&str; 3] = ["not met", "partly met", "met"];
/// Stall reasons the model may choose from.
pub const STALL_REASONS: [&str; 5] = [
    "waiting_on_input",
    "blocked_on_dependency",
    "looping",
    "idle_without_work",
    "other",
];

/// Which end of an over-long input survives the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Head,
    Tail,
}

/// Cap `text` at [`MAX_INPUT_CHARS`] characters on a character boundary.
pub fn cap_input(text: &str, keep: Keep) -> String {
    cap_input_to(text, MAX_INPUT_CHARS, keep)
}

fn cap_input_to(text: &str, limit: usize, keep: Keep) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    match keep {
        Keep::Head => text.chars().take(limit).collect(),
        Keep::Tail => text.chars().skip(total - limit).collect(),
    }
}

/// Last [`SCREEN_TAIL_LINES`] lines of a pane capture, then capped keeping the
/// tail.
pub fn screen_input(redacted_screen: &str) -> String {
    let lines: Vec<&str> = redacted_screen.lines().collect();
    let start = lines.len().saturating_sub(SCREEN_TAIL_LINES);
    cap_input(&lines[start..].join("\n"), Keep::Tail)
}

fn predicate(name: &str, instructions: &str) -> Question {
    Question::Predicate {
        name: name.into(),
        instructions: instructions.into(),
    }
}

fn choice(name: &str, instructions: &str, choices: &[(&str, &str)]) -> Question {
    Question::Choice {
        name: name.into(),
        instructions: instructions.into(),
        choices: choices
            .iter()
            .map(|(value, description)| ((*value).to_string(), Some((*description).to_string())))
            .collect(),
    }
}

// ---------------------------------------------------------------- outcomes

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenOutcome {
    Idle,
    Working,
    ContextExhausted,
    Unknown,
}

impl From<ScreenOutcome> for ScreenVerdict {
    fn from(outcome: ScreenOutcome) -> Self {
        match outcome {
            ScreenOutcome::Idle => ScreenVerdict::AgentIdle,
            ScreenOutcome::Working => ScreenVerdict::AgentWorking,
            ScreenOutcome::ContextExhausted => ScreenVerdict::ContextExhausted,
            ScreenOutcome::Unknown => ScreenVerdict::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageOutcome {
    Actionable,
    StatusReply,
    Noise,
    Escalation,
}

impl TriageOutcome {
    /// Stable label for events and logs.
    pub fn label(self) -> &'static str {
        match self {
            TriageOutcome::Actionable => "actionable",
            TriageOutcome::StatusReply => "status_reply",
            TriageOutcome::Noise => "noise",
            TriageOutcome::Escalation => "escalation",
        }
    }
}

/// A completion is vetoed when `!done` or `narration_only`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionOutcome {
    pub done: bool,
    pub narration_only: bool,
}

impl CompletionOutcome {
    pub fn vetoes(&self) -> bool {
        !self.done || self.narration_only
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RubricOutcome {
    pub criteria_met: bool,
    /// `(criterion, score)`; the score runs from 0 (not met) to 2 (met).
    pub scores: Vec<(String, f64)>,
}

/// Label for a rubric score, rounding to the nearest level.
pub fn rubric_label(score: f64) -> &'static str {
    let index = score.round().clamp(0.0, (RUBRIC_LEVELS.len() - 1) as f64) as usize;
    RUBRIC_LEVELS[index]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallOutcome {
    pub stuck: bool,
    /// One of [`STALL_REASONS`], only when `stuck`.
    pub reason: Option<String>,
}

// ---------------------------------------------------------------- confidence

/// Reject a refusal, an unrecognised answer, or an answer whose confidence is
/// below `min_confidence`. A predicate's confidence is how far its
/// probability is from a coin flip, `max(p, 1 - p)`. A choice or score with no
/// reported confidence is accepted.
pub fn apply_confidence(answer: &Answer, min_confidence: f64) -> Result<&Answer, Fallback> {
    let confidence = match answer {
        Answer::Refusal { .. } => return Err(Fallback::Refusal),
        Answer::Unknown => return Err(Fallback::InvalidAnswer),
        Answer::Predicate { probability, .. } => {
            if !(0.0..=1.0).contains(probability) {
                return Err(Fallback::InvalidAnswer);
            }
            Some(probability.max(1.0 - probability))
        }
        Answer::Choice { confidence, .. } | Answer::Score { confidence, .. } => *confidence,
    };
    match confidence {
        Some(confidence) if confidence < min_confidence => Err(Fallback::LowConfidence),
        _ => Ok(answer),
    }
}

/// The answer to question `name`: matched by name, else by position when that
/// answer carries no name.
fn find_answer<'a>(
    answers: &'a [Answer],
    name: &str,
    index: usize,
) -> Result<&'a Answer, Fallback> {
    if let Some(answer) = answers.iter().find(|a| a.name() == Some(name)) {
        return Ok(answer);
    }
    match answers.get(index) {
        Some(answer) if answer.name().is_none() => Ok(answer),
        _ => Err(Fallback::InvalidAnswer),
    }
}

fn choice_value(answer: &Answer, allowed: &[&str]) -> Result<String, Fallback> {
    let Answer::Choice { choice, .. } = answer else {
        return Err(Fallback::InvalidAnswer);
    };
    match choice {
        Value::String(value) if allowed.contains(&value.as_str()) => Ok(value.clone()),
        _ => Err(Fallback::InvalidAnswer),
    }
}

fn predicate_value(answer: &Answer) -> Result<bool, Fallback> {
    match answer {
        Answer::Predicate { probability, .. } => Ok(*probability >= 0.5),
        _ => Err(Fallback::InvalidAnswer),
    }
}

// ---------------------------------------------------------------- screen

const SCREEN_VALUES: [&str; 4] = ["idle", "working", "context_exhausted", "unknown"];

pub fn screen_questions() -> Vec<Question> {
    vec![choice(
        "state",
        "What is the terminal coding agent doing right now?",
        &[
            ("idle", "At its input prompt, waiting for a message"),
            ("working", "Actively processing or producing output"),
            (
                "context_exhausted",
                "Reports that the conversation or context is too large",
            ),
            ("unknown", "Cannot tell from the screen"),
        ],
    )]
}

pub fn map_screen(answers: &[Answer], min_confidence: f64) -> Result<ScreenOutcome, Fallback> {
    let answer = apply_confidence(find_answer(answers, "state", 0)?, min_confidence)?;
    Ok(match choice_value(answer, &SCREEN_VALUES)?.as_str() {
        "idle" => ScreenOutcome::Idle,
        "working" => ScreenOutcome::Working,
        "context_exhausted" => ScreenOutcome::ContextExhausted,
        _ => ScreenOutcome::Unknown,
    })
}

// ---------------------------------------------------------------- triage

const TRIAGE_VALUES: [&str; 4] = ["actionable", "status_reply", "noise", "escalation"];

pub fn triage_questions() -> Vec<Question> {
    vec![choice(
        "kind",
        "Classify this message sent to a supervising agent.",
        &[
            (
                "actionable",
                "Asks for or reports something that needs action",
            ),
            (
                "status_reply",
                "A routine status or acknowledgement that needs no action",
            ),
            ("noise", "Chatter with no information or request"),
            (
                "escalation",
                "Urgent: a blocker, failure or decision that needs attention now",
            ),
        ],
    )]
}

pub fn triage_input(redacted_body: &str) -> String {
    cap_input(redacted_body, Keep::Head)
}

pub fn map_triage(answers: &[Answer], min_confidence: f64) -> Result<TriageOutcome, Fallback> {
    let answer = apply_confidence(find_answer(answers, "kind", 0)?, min_confidence)?;
    Ok(match choice_value(answer, &TRIAGE_VALUES)?.as_str() {
        "actionable" => TriageOutcome::Actionable,
        "status_reply" => TriageOutcome::StatusReply,
        "noise" => TriageOutcome::Noise,
        _ => TriageOutcome::Escalation,
    })
}

// ---------------------------------------------------------------- completion

pub fn completion_questions() -> Vec<Question> {
    vec![
        predicate(
            "done",
            "Did the engineer actually complete the task described, with real code changes that satisfy it?",
        ),
        predicate(
            "narration_only",
            "Does the engineer's final message only narrate or promise work instead of showing finished work?",
        ),
    ]
}

/// Task text, the engineer's final response and a diff summary (stat, file
/// list, first hunks; never the full diff), each section capped so the whole
/// input stays under [`MAX_INPUT_CHARS`].
pub fn completion_input(task: &str, response: &str, diff_summary: &str) -> String {
    let section = MAX_INPUT_CHARS / 3 - 64;
    let part = |text: &str| cap_input_to(text, section, Keep::Head);
    cap_input(
        &format!(
            "TASK:\n{}\n\nENGINEER RESPONSE:\n{}\n\nDIFF SUMMARY:\n{}",
            part(task),
            part(response),
            part(diff_summary)
        ),
        Keep::Head,
    )
}

pub fn map_completion(
    answers: &[Answer],
    min_confidence: f64,
) -> Result<CompletionOutcome, Fallback> {
    let done = apply_confidence(find_answer(answers, "done", 0)?, min_confidence)?;
    let narration = apply_confidence(find_answer(answers, "narration_only", 1)?, min_confidence)?;
    Ok(CompletionOutcome {
        done: predicate_value(done)?,
        narration_only: predicate_value(narration)?,
    })
}

// ---------------------------------------------------------------- rubric

fn criterion_name(index: usize) -> String {
    format!("criterion_{}", index + 1)
}

/// One score question per criterion plus the `criteria_met` predicate. Callers
/// pass at most `criteria::MAX_CRITERIA`; extra criteria are ignored.
pub fn rubric_questions(criteria: &[String]) -> Vec<Question> {
    let mut questions: Vec<Question> = criteria
        .iter()
        .take(super::criteria::MAX_CRITERIA)
        .enumerate()
        .map(|(index, criterion)| Question::Score {
            name: criterion_name(index),
            instructions: format!("How well does the change meet this criterion: {criterion}"),
            levels: RUBRIC_LEVELS.iter().map(|l| (*l).to_string()).collect(),
        })
        .collect();
    questions.push(predicate(
        "criteria_met",
        "Does the change meet every acceptance criterion?",
    ));
    questions
}

pub fn rubric_input(task: &str, diff_summary: &str) -> String {
    let half = MAX_INPUT_CHARS / 2 - 64;
    cap_input(
        &format!(
            "TASK:\n{}\n\nDIFF SUMMARY:\n{}",
            cap_input_to(task, half, Keep::Head),
            cap_input_to(diff_summary, half, Keep::Head)
        ),
        Keep::Head,
    )
}

pub fn map_rubric(
    answers: &[Answer],
    criteria: &[String],
    min_confidence: f64,
) -> Result<RubricOutcome, Fallback> {
    let criteria: Vec<&String> = criteria
        .iter()
        .take(super::criteria::MAX_CRITERIA)
        .collect();
    let mut scores = Vec::with_capacity(criteria.len());
    for (index, criterion) in criteria.iter().enumerate() {
        let answer = apply_confidence(
            find_answer(answers, &criterion_name(index), index)?,
            min_confidence,
        )?;
        let Answer::Score { score, .. } = answer else {
            return Err(Fallback::InvalidAnswer);
        };
        if !score.is_finite() {
            return Err(Fallback::InvalidAnswer);
        }
        scores.push(((*criterion).clone(), *score));
    }
    let met = apply_confidence(
        find_answer(answers, "criteria_met", criteria.len())?,
        min_confidence,
    )?;
    Ok(RubricOutcome {
        criteria_met: predicate_value(met)?,
        scores,
    })
}

// ---------------------------------------------------------------- stall

pub fn stall_questions() -> Vec<Question> {
    vec![
        predicate(
            "stuck",
            "Is this supervising agent stuck, making no useful progress?",
        ),
        choice(
            "reason",
            "If it is stuck, what is the most likely reason?",
            &[
                ("waiting_on_input", "Waiting for a reply or approval"),
                (
                    "blocked_on_dependency",
                    "Blocked on another task or resource",
                ),
                ("looping", "Repeating the same actions without progress"),
                ("idle_without_work", "Idle with nothing to do"),
                ("other", "Some other reason"),
            ],
        ),
    ]
}

pub fn stall_input(redacted_output: &str) -> String {
    cap_input(redacted_output, Keep::Tail)
}

/// `stuck` must be a confident answer. The reason is best effort: a refused,
/// unsure or unrecognised reason leaves it `None` without failing the verdict.
pub fn map_stall(answers: &[Answer], min_confidence: f64) -> Result<StallOutcome, Fallback> {
    let stuck = predicate_value(apply_confidence(
        find_answer(answers, "stuck", 0)?,
        min_confidence,
    )?)?;
    let reason = if stuck {
        find_answer(answers, "reason", 1)
            .and_then(|answer| apply_confidence(answer, min_confidence))
            .and_then(|answer| choice_value(answer, &STALL_REASONS))
            .ok()
    } else {
        None
    };
    Ok(StallOutcome { stuck, reason })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn choice_answer(name: &str, value: &str, confidence: Option<f64>) -> Answer {
        Answer::Choice {
            name: Some(name.into()),
            choice: json!(value),
            confidence,
        }
    }

    fn predicate_answer(name: &str, probability: f64) -> Answer {
        Answer::Predicate {
            name: Some(name.into()),
            probability,
        }
    }

    fn score_answer(name: &str, score: f64, confidence: Option<f64>) -> Answer {
        Answer::Score {
            name: Some(name.into()),
            score,
            confidence,
        }
    }

    #[test]
    fn screen_maps_each_value_one_to_one() {
        for (value, outcome, verdict) in [
            ("idle", ScreenOutcome::Idle, ScreenVerdict::AgentIdle),
            (
                "working",
                ScreenOutcome::Working,
                ScreenVerdict::AgentWorking,
            ),
            (
                "context_exhausted",
                ScreenOutcome::ContextExhausted,
                ScreenVerdict::ContextExhausted,
            ),
            ("unknown", ScreenOutcome::Unknown, ScreenVerdict::Unknown),
        ] {
            let answers = [choice_answer("state", value, Some(0.8))];
            assert_eq!(map_screen(&answers, 0.6), Ok(outcome));
            assert_eq!(ScreenVerdict::from(outcome), verdict);
        }
    }

    #[test]
    fn screen_rejects_unknown_values_and_wrong_types() {
        let bad = [choice_answer("state", "sleeping", Some(0.9))];
        assert_eq!(map_screen(&bad, 0.6), Err(Fallback::InvalidAnswer));
        let wrong = [predicate_answer("state", 0.9)];
        assert_eq!(map_screen(&wrong, 0.6), Err(Fallback::InvalidAnswer));
        let non_string = [Answer::Choice {
            name: Some("state".into()),
            choice: json!(3),
            confidence: Some(0.9),
        }];
        assert_eq!(map_screen(&non_string, 0.6), Err(Fallback::InvalidAnswer));
        assert_eq!(map_screen(&[], 0.6), Err(Fallback::InvalidAnswer));
    }

    #[test]
    fn answers_match_by_position_when_unnamed() {
        let answers = [Answer::Choice {
            name: None,
            choice: json!("idle"),
            confidence: None,
        }];
        assert_eq!(map_screen(&answers, 0.6), Ok(ScreenOutcome::Idle));
    }

    #[test]
    fn triage_maps_each_value() {
        for (value, outcome) in [
            ("actionable", TriageOutcome::Actionable),
            ("status_reply", TriageOutcome::StatusReply),
            ("noise", TriageOutcome::Noise),
            ("escalation", TriageOutcome::Escalation),
        ] {
            let answers = [choice_answer("kind", value, Some(0.7))];
            assert_eq!(map_triage(&answers, 0.6), Ok(outcome));
            assert_eq!(outcome.label(), value);
        }
    }

    #[test]
    fn mappers_reject_refusal_unknown_and_low_confidence() {
        let refusal = [Answer::Refusal {
            name: Some("kind".into()),
        }];
        assert_eq!(map_triage(&refusal, 0.6), Err(Fallback::Refusal));
        assert_eq!(
            map_triage(&[Answer::Unknown], 0.6),
            Err(Fallback::InvalidAnswer)
        );
        let low = [choice_answer("kind", "noise", Some(0.5))];
        assert_eq!(map_triage(&low, 0.6), Err(Fallback::LowConfidence));
        let at_floor = [choice_answer("kind", "noise", Some(0.6))];
        assert_eq!(map_triage(&at_floor, 0.6), Ok(TriageOutcome::Noise));
    }

    #[test]
    fn apply_confidence_handles_predicates_and_missing_confidence() {
        let sure = predicate_answer("p", 0.05);
        assert!(apply_confidence(&sure, 0.6).is_ok());
        let coin_flip = predicate_answer("p", 0.55);
        assert_eq!(
            apply_confidence(&coin_flip, 0.6),
            Err(Fallback::LowConfidence)
        );
        let out_of_range = predicate_answer("p", 1.5);
        assert_eq!(
            apply_confidence(&out_of_range, 0.6),
            Err(Fallback::InvalidAnswer)
        );
        let no_confidence = choice_answer("c", "x", None);
        assert!(apply_confidence(&no_confidence, 0.6).is_ok());
    }

    #[test]
    fn completion_maps_predicates() {
        let answers = [
            predicate_answer("done", 0.95),
            predicate_answer("narration_only", 0.02),
        ];
        let outcome = map_completion(&answers, 0.6).unwrap();
        assert_eq!(
            outcome,
            CompletionOutcome {
                done: true,
                narration_only: false
            }
        );
        assert!(!outcome.vetoes());

        let narration = [
            predicate_answer("done", 0.9),
            predicate_answer("narration_only", 0.9),
        ];
        assert!(map_completion(&narration, 0.6).unwrap().vetoes());
        let not_done = [
            predicate_answer("done", 0.1),
            predicate_answer("narration_only", 0.1),
        ];
        assert!(map_completion(&not_done, 0.6).unwrap().vetoes());
    }

    #[test]
    fn completion_falls_back_on_any_bad_answer() {
        let refused = [
            predicate_answer("done", 0.9),
            Answer::Refusal {
                name: Some("narration_only".into()),
            },
        ];
        assert_eq!(map_completion(&refused, 0.6), Err(Fallback::Refusal));
        let unsure = [
            predicate_answer("done", 0.9),
            predicate_answer("narration_only", 0.5),
        ];
        assert_eq!(map_completion(&unsure, 0.6), Err(Fallback::LowConfidence));
        assert_eq!(
            map_completion(&[predicate_answer("done", 0.9)], 0.6),
            Err(Fallback::InvalidAnswer)
        );
    }

    #[test]
    fn rubric_questions_use_three_levels_and_a_summary_predicate() {
        let criteria = vec!["adds a flag".to_string(), "documents it".to_string()];
        let questions = rubric_questions(&criteria);
        assert_eq!(questions.len(), 3);
        match &questions[0] {
            Question::Score { name, levels, .. } => {
                assert_eq!(name, "criterion_1");
                assert_eq!(levels, &["not met", "partly met", "met"]);
            }
            other => panic!("expected score, got {other:?}"),
        }
        assert!(
            matches!(&questions[2], Question::Predicate { name, .. } if name == "criteria_met")
        );
    }

    #[test]
    fn rubric_maps_scores_and_summary() {
        let criteria = vec!["adds a flag".to_string(), "documents it".to_string()];
        let answers = [
            score_answer("criterion_1", 1.9, Some(0.8)),
            score_answer("criterion_2", 0.2, None),
            predicate_answer("criteria_met", 0.1),
        ];
        let outcome = map_rubric(&answers, &criteria, 0.6).unwrap();
        assert!(!outcome.criteria_met);
        assert_eq!(
            outcome.scores,
            vec![
                ("adds a flag".to_string(), 1.9),
                ("documents it".to_string(), 0.2)
            ]
        );
        assert_eq!(rubric_label(1.9), "met");
        assert_eq!(rubric_label(0.2), "not met");
        assert_eq!(rubric_label(1.0), "partly met");
        assert_eq!(rubric_label(-3.0), "not met");
        assert_eq!(rubric_label(9.0), "met");
    }

    #[test]
    fn rubric_rejects_bad_answers() {
        let criteria = vec!["one".to_string()];
        let low = [
            score_answer("criterion_1", 1.0, Some(0.3)),
            predicate_answer("criteria_met", 0.9),
        ];
        assert_eq!(
            map_rubric(&low, &criteria, 0.6),
            Err(Fallback::LowConfidence)
        );
        let wrong_type = [
            predicate_answer("criterion_1", 0.9),
            predicate_answer("criteria_met", 0.9),
        ];
        assert_eq!(
            map_rubric(&wrong_type, &criteria, 0.6),
            Err(Fallback::InvalidAnswer)
        );
        let nan = [
            score_answer("criterion_1", f64::NAN, None),
            predicate_answer("criteria_met", 0.9),
        ];
        assert_eq!(
            map_rubric(&nan, &criteria, 0.6),
            Err(Fallback::InvalidAnswer)
        );
        let missing_summary = [score_answer("criterion_1", 1.0, None)];
        assert_eq!(
            map_rubric(&missing_summary, &criteria, 0.6),
            Err(Fallback::InvalidAnswer)
        );
    }

    #[test]
    fn stall_maps_stuck_and_reason() {
        let answers = [
            predicate_answer("stuck", 0.9),
            choice_answer("reason", "looping", Some(0.8)),
        ];
        assert_eq!(
            map_stall(&answers, 0.6),
            Ok(StallOutcome {
                stuck: true,
                reason: Some("looping".into())
            })
        );
        let calm = [
            predicate_answer("stuck", 0.05),
            choice_answer("reason", "looping", Some(0.8)),
        ];
        assert_eq!(
            map_stall(&calm, 0.6),
            Ok(StallOutcome {
                stuck: false,
                reason: None
            })
        );
    }

    #[test]
    fn stall_reason_is_best_effort() {
        let unsure_reason = [
            predicate_answer("stuck", 0.9),
            choice_answer("reason", "looping", Some(0.2)),
        ];
        let outcome = map_stall(&unsure_reason, 0.6).unwrap();
        assert!(outcome.stuck);
        assert_eq!(outcome.reason, None);
        let bad_reason = [
            predicate_answer("stuck", 0.9),
            choice_answer("reason", "gremlins", Some(0.9)),
        ];
        assert_eq!(map_stall(&bad_reason, 0.6).unwrap().reason, None);
        let refused_stuck = [Answer::Refusal {
            name: Some("stuck".into()),
        }];
        assert_eq!(map_stall(&refused_stuck, 0.6), Err(Fallback::Refusal));
    }

    #[test]
    fn question_sets_stay_within_the_request_limit() {
        for questions in [
            screen_questions(),
            triage_questions(),
            completion_questions(),
            stall_questions(),
        ] {
            assert!(questions.len() <= super::super::client::MAX_QUESTIONS);
        }
        let many: Vec<String> = (0..30).map(|i| format!("c{i}")).collect();
        // 10 criteria plus the summary predicate.
        assert_eq!(rubric_questions(&many).len(), 11);
    }

    #[test]
    fn cap_keeps_head_or_tail_on_char_boundaries() {
        let text = format!("{}{}", "a".repeat(MAX_INPUT_CHARS), "é".repeat(10));
        let head = cap_input(&text, Keep::Head);
        assert_eq!(head.chars().count(), MAX_INPUT_CHARS);
        assert!(head.chars().all(|c| c == 'a'));
        let tail = cap_input(&text, Keep::Tail);
        assert_eq!(tail.chars().count(), MAX_INPUT_CHARS);
        assert!(tail.ends_with("éééééééééé"));
        assert_eq!(cap_input("short", Keep::Head), "short");
    }

    #[test]
    fn screen_input_keeps_the_last_hundred_lines() {
        let screen: String = (0..250).map(|i| format!("line {i}\n")).collect();
        let input = screen_input(&screen);
        assert_eq!(input.lines().count(), SCREEN_TAIL_LINES);
        assert!(input.starts_with("line 150"));
        assert!(input.ends_with("line 249"));
    }

    #[test]
    fn long_screen_keeps_the_tail_under_the_cap() {
        let screen = format!("{}\nTHE END", "x".repeat(200_000));
        let input = screen_input(&screen);
        assert!(input.chars().count() <= MAX_INPUT_CHARS);
        assert!(input.ends_with("THE END"));
    }

    #[test]
    fn completion_and_rubric_inputs_respect_the_cap() {
        let big = "y".repeat(200_000);
        let completion = completion_input(&big, &big, &big);
        assert!(completion.chars().count() <= MAX_INPUT_CHARS);
        assert!(completion.contains("DIFF SUMMARY:"));
        let rubric = rubric_input(&big, &big);
        assert!(rubric.chars().count() <= MAX_INPUT_CHARS);
        assert!(rubric.contains("DIFF SUMMARY:"));
        assert!(triage_input(&big).chars().count() <= MAX_INPUT_CHARS);
        assert!(stall_input(&big).chars().count() <= MAX_INPUT_CHARS);
    }
}
