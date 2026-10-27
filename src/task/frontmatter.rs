//! The single implementation of task-file frontmatter handling: split a task
//! file into YAML frontmatter and Markdown body, parse it, render it back and
//! mutate it. Everything that reads or rewrites task frontmatter goes through
//! here so edge cases (BOM, CRLF, empty frontmatter, `---` look-alikes) are
//! handled in exactly one place.

use anyhow::{Context, Result, bail};
use serde_yaml::Mapping;

/// Split `content` into `(frontmatter, body)`.
///
/// The opening and closing delimiters must each be a line consisting solely of
/// `---` (trailing whitespace and CRLF tolerated); lines such as `----` or
/// `---x` inside the frontmatter are not delimiters. A leading BOM and leading
/// whitespace are ignored. The returned frontmatter has no trailing newline.
pub(crate) fn split(content: &str) -> Result<(&str, &str)> {
    let trimmed = content.trim_start_matches('\u{feff}').trim_start();
    let mut lines = trimmed.split_inclusive('\n');
    let open = lines.next().unwrap_or("");
    if open.trim_end() != "---" {
        bail!("task file missing YAML frontmatter (no opening ---)");
    }

    let mut offset = open.len();
    for line in lines {
        if line.trim_end() == "---" {
            let frontmatter = &trimmed[open.len()..offset];
            let frontmatter = frontmatter.strip_suffix('\n').unwrap_or(frontmatter);
            let frontmatter = frontmatter.strip_suffix('\r').unwrap_or(frontmatter);
            return Ok((frontmatter, &trimmed[offset + line.len()..]));
        }
        offset += line.len();
    }
    bail!("task file missing closing --- for frontmatter")
}

/// Parse `content` into its frontmatter mapping and body. Empty frontmatter
/// yields an empty mapping.
pub(crate) fn parse(content: &str) -> Result<(Mapping, &str)> {
    let (frontmatter, body) = split(content)?;
    if frontmatter.trim().is_empty() {
        return Ok((Mapping::new(), body));
    }
    let mapping = serde_yaml::from_str(frontmatter).context("failed to parse task frontmatter")?;
    Ok((mapping, body))
}

/// Render a mapping and body back into task-file text.
pub(crate) fn render(mapping: &Mapping, body: &str) -> Result<String> {
    let rendered =
        serde_yaml::to_string(mapping).context("failed to serialize task frontmatter")?;
    let rendered = rendered.strip_prefix("---\n").unwrap_or(&rendered);

    let mut updated = String::from("---\n");
    updated.push_str(rendered);
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("---\n");
    updated.push_str(body);
    Ok(updated)
}

/// Parse `content`, apply `mutator` to the frontmatter mapping and render the
/// result. The body is preserved byte for byte.
pub(crate) fn update<F>(content: &str, mutator: F) -> Result<String>
where
    F: FnOnce(&mut Mapping),
{
    let (mut mapping, body) = parse(content)?;
    mutator(&mut mapping);
    render(&mapping, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::Value;

    #[test]
    fn splits_basic_document() {
        let (fm, body) = split("---\nid: 1\ntitle: t\n---\nbody\n").unwrap();
        assert_eq!(fm, "id: 1\ntitle: t");
        assert_eq!(body, "body\n");
    }

    #[test]
    fn tolerates_bom_leading_whitespace_and_crlf() {
        let (fm, body) = split("\u{feff}\n  \n---\r\nid: 1\r\n---\r\nbody\r\n").unwrap();
        assert_eq!(fm, "id: 1");
        assert_eq!(body, "body\r\n");
    }

    #[test]
    fn empty_frontmatter_and_empty_body() {
        let (fm, body) = split("---\n---\n").unwrap();
        assert_eq!((fm, body), ("", ""));
        let (mapping, _) = parse("---\n---\nx").unwrap();
        assert!(mapping.is_empty());
    }

    #[test]
    fn closing_delimiter_at_eof_without_newline() {
        let (fm, body) = split("---\nid: 1\n---").unwrap();
        assert_eq!((fm, body), ("id: 1", ""));
    }

    #[test]
    fn dashes_lookalikes_are_not_delimiters() {
        let (fm, body) = split("---\nnote: |\n  a\n----\n---x\nid: 2\n---\nbody").unwrap();
        assert_eq!(fm, "note: |\n  a\n----\n---x\nid: 2");
        assert_eq!(body, "body");
    }

    #[test]
    fn body_may_contain_horizontal_rules() {
        let (_, body) = split("---\nid: 1\n---\ntext\n---\nmore\n").unwrap();
        assert_eq!(body, "text\n---\nmore\n");
    }

    #[test]
    fn missing_delimiters_error() {
        assert!(
            split("no frontmatter")
                .unwrap_err()
                .to_string()
                .contains("no opening")
        );
        assert!(
            split("---\nid: 1\n")
                .unwrap_err()
                .to_string()
                .contains("closing")
        );
        assert!(split("").is_err());
    }

    #[test]
    fn update_preserves_body_and_rewrites_mapping() {
        let updated = update(
            "---\nid: 1\nstatus: todo\n---\nbody\n---\nkeep\n",
            |mapping| {
                mapping.insert(Value::from("status"), Value::from("done"));
            },
        )
        .unwrap();
        let (mapping, body) = parse(&updated).unwrap();
        assert_eq!(mapping.get("status").and_then(Value::as_str), Some("done"));
        assert_eq!(body, "body\n---\nkeep\n");
        assert!(updated.starts_with("---\n"));
    }

    #[test]
    fn render_round_trips() {
        let (mapping, body) = parse("---\nid: 3\ntags: [a, b]\n---\nhello\n").unwrap();
        let rendered = render(&mapping, body).unwrap();
        let (again, again_body) = parse(&rendered).unwrap();
        assert_eq!(mapping, again);
        assert_eq!(again_body, "hello\n");
    }
}
