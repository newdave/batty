//! Acceptance-criteria parser for the review rubric.

/// The rubric scores at most this many criteria (one question each).
pub const MAX_CRITERIA: usize = 10;
/// Longest criterion text sent to the model, in characters.
const MAX_CRITERION_CHARS: usize = 300;

const HEADING: &str = "## Acceptance Criteria";

/// Strip a list marker (`-`, `*`, `+`, `1.`, `1)`) and an optional checkbox.
fn strip_marker(line: &str) -> Option<&str> {
    let rest = if let Some(rest) = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
    {
        rest
    } else {
        let digits = line.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let after = &line[digits..];
        after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))?
    };
    let rest = rest.trim_start();
    let rest = rest
        .strip_prefix("[ ]")
        .or_else(|| rest.strip_prefix("[x]"))
        .or_else(|| rest.strip_prefix("[X]"))
        .map_or(rest, str::trim_start);
    Some(rest)
}

/// Bullet lines under the `## Acceptance Criteria` heading, up to the next
/// heading. Only unindented bullets count; indented lines elaborate a
/// criterion. At most [`MAX_CRITERIA`] are returned.
pub fn parse_acceptance_criteria(task_body: &str) -> Vec<String> {
    let mut criteria = Vec::new();
    let mut in_section = false;
    for line in task_body.lines() {
        let trimmed_end = line.trim_end();
        if trimmed_end.starts_with('#') {
            in_section = trimmed_end.trim() == HEADING;
            continue;
        }
        if !in_section {
            continue;
        }
        let Some(text) = strip_marker(trimmed_end) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        criteria.push(text.chars().take(MAX_CRITERION_CHARS).collect());
        if criteria.len() == MAX_CRITERIA {
            break;
        }
    }
    criteria
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bullets_under_the_heading() {
        let body = "---\nid: 1\n---\n\n# Title\n\nIntro\n\n## Acceptance Criteria\n\n- First thing\n- Second thing\n\n## Notes\n\n- not a criterion\n";
        assert_eq!(
            parse_acceptance_criteria(body),
            vec!["First thing", "Second thing"]
        );
    }

    #[test]
    fn caps_at_ten_criteria() {
        let mut body = String::from("## Acceptance Criteria\n");
        for i in 0..12 {
            body.push_str(&format!("- criterion {i}\n"));
        }
        let criteria = parse_acceptance_criteria(&body);
        assert_eq!(criteria.len(), 10);
        assert_eq!(criteria[9], "criterion 9");
    }

    #[test]
    fn missing_section_gives_no_criteria() {
        assert!(parse_acceptance_criteria("# Title\n\n- a bullet\n").is_empty());
        assert!(parse_acceptance_criteria("").is_empty());
    }

    #[test]
    fn handles_checkboxes_numbers_and_star_bullets() {
        let body = "## Acceptance Criteria\n- [ ] open box\n- [x] done box\n* star\n1. numbered\n2) paren\n";
        assert_eq!(
            parse_acceptance_criteria(body),
            vec!["open box", "done box", "star", "numbered", "paren"]
        );
    }

    #[test]
    fn ignores_indented_lines_prose_and_empty_bullets() {
        let body = "## Acceptance Criteria\nSome prose.\n- real\n  - nested detail\n- \n-\n";
        assert_eq!(parse_acceptance_criteria(body), vec!["real"]);
    }

    #[test]
    fn section_ends_at_the_next_heading_and_can_reappear() {
        let body =
            "## Acceptance Criteria\n- one\n### Sub\n- skipped\n## Acceptance Criteria\n- two\n";
        assert_eq!(parse_acceptance_criteria(body), vec!["one", "two"]);
    }

    #[test]
    fn long_criteria_are_truncated() {
        let body = format!("## Acceptance Criteria\n- {}\n", "x".repeat(500));
        assert_eq!(parse_acceptance_criteria(&body)[0].chars().count(), 300);
    }
}
