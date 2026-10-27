//! The single implementation of task-file frontmatter handling: split a task
//! file into YAML frontmatter and Markdown body, parse it, render it back and
//! mutate it. Everything that reads or rewrites task frontmatter goes through
//! here so edge cases (BOM, CRLF, empty frontmatter, `---` look-alikes) are
//! handled in exactly one place.

use anyhow::{Context, Result, bail};
use serde_yaml::Mapping;

/// Whether `line` is a closing delimiter: `---` optionally followed by
/// whitespace and a `#` comment (the form upstream accepted), i.e.
/// `^---\s*(#.*)?$`. `----` and `---text` are not delimiters.
fn is_closing_delimiter(line: &str) -> bool {
    let line = line.trim_end_matches(['\n', '\r']);
    match line.strip_prefix("---") {
        Some(rest) => {
            let rest = rest.trim_start();
            rest.is_empty() || rest.starts_with('#')
        }
        None => false,
    }
}

/// Split `content` into `(frontmatter, body)`.
///
/// The opening delimiter must be a line consisting solely of `---` (trailing
/// whitespace and CRLF tolerated). The closing delimiter is `---` optionally
/// followed by whitespace and a `#` comment; lines such as `----` or `---x`
/// inside the frontmatter are not delimiters. A leading BOM and leading
/// whitespace are ignored. The returned frontmatter has no trailing newline.
/// Errors name the 1-based line number they refer to.
pub(crate) fn split(content: &str) -> Result<(&str, &str)> {
    let without_bom = content.trim_start_matches('\u{feff}');
    let trimmed = without_bom.trim_start();
    // 1-based line of the first non-blank line, for diagnostics.
    let first_line = 1 + without_bom[..without_bom.len() - trimmed.len()]
        .matches('\n')
        .count();
    let mut lines = trimmed.split_inclusive('\n');
    let open = lines.next().unwrap_or("");
    if open.trim_end() != "---" {
        bail!("task file missing YAML frontmatter (no opening --- at line {first_line})");
    }

    let mut offset = open.len();
    let mut line_no = first_line;
    let mut lookalike: Option<(usize, &str)> = None;
    for line in lines {
        line_no += 1;
        if is_closing_delimiter(line) {
            let frontmatter = &trimmed[open.len()..offset];
            let frontmatter = frontmatter.strip_suffix('\n').unwrap_or(frontmatter);
            let frontmatter = frontmatter.strip_suffix('\r').unwrap_or(frontmatter);
            return Ok((frontmatter, &trimmed[offset + line.len()..]));
        }
        if lookalike.is_none() && line.starts_with("---") {
            lookalike = Some((line_no, line.trim_end()));
        }
        offset += line.len();
    }
    match lookalike {
        Some((n, text)) => bail!(
            "task file missing closing --- for frontmatter (opened at line {first_line}; \
             line {n} `{text}` is not a valid closing delimiter, use `---` alone or `--- # comment`)"
        ),
        None => bail!(
            "task file missing closing --- for frontmatter (opened at line {first_line}, \
             reached end of file at line {line_no})"
        ),
    }
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
    fn closing_delimiter_tolerates_trailing_space_and_comment() {
        for closer in ["--- # note", "---  ", "---#x", "---\t# tab comment"] {
            let doc = format!("---\nid: 1\n{closer}\nbody\n");
            let (fm, body) = split(&doc).unwrap_or_else(|e| panic!("{closer:?}: {e}"));
            assert_eq!(fm, "id: 1", "{closer:?}");
            assert_eq!(body, "body\n", "{closer:?}");
        }
        let (fm, body) = split("---\r\nid: 1\r\n--- # note\r\nbody\r\n").unwrap();
        assert_eq!(fm, "id: 1");
        assert_eq!(body, "body\r\n");
    }

    #[test]
    fn four_dashes_closer_is_rejected_with_line_number() {
        let err = split("---\nid: 1\n----\nbody\n").unwrap_err().to_string();
        assert!(err.contains("closing"), "{err}");
        assert!(err.contains("line 3"), "{err}");
        assert!(err.contains("----"), "{err}");
        let err = split("---\nid: 1\n---text\n").unwrap_err().to_string();
        assert!(err.contains("line 3"), "{err}");
    }

    #[test]
    fn errors_report_line_numbers_after_leading_blank_lines() {
        let err = split("\n\nnot frontmatter\n").unwrap_err().to_string();
        assert!(err.contains("line 3"), "{err}");
        let err = split("\n---\nid: 1\n").unwrap_err().to_string();
        assert!(err.contains("opened at line 2"), "{err}");
        assert!(err.contains("end of file at line 3"), "{err}");
    }

    /// The splitter upstream 5637f78 used (`task::split_frontmatter`).
    fn upstream_split(content: &str) -> Option<(&str, &str)> {
        let trimmed = content.trim_start();
        if !trimmed.starts_with("---") {
            return None;
        }
        let after_open = &trimmed[3..];
        let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);
        let close_pos = after_open.find("\n---")?;
        let body = &after_open[close_pos + 4..];
        Some((
            &after_open[..close_pos],
            body.strip_prefix('\n').unwrap_or(body),
        ))
    }

    fn collect_md(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_md(&path, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }

    #[test]
    fn corpus_of_repo_task_files_parses_wherever_upstream_parsed() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        for dir in ["tests/fixtures", ".batty"] {
            let dir = root.join(dir);
            if dir.is_dir() {
                collect_md(&dir, &mut files);
            }
        }
        // Only task files: those living under a `tasks` directory.
        files.retain(|p| p.components().any(|c| c.as_os_str() == "tasks"));
        assert!(!files.is_empty(), "expected task fixtures in the repo");
        for path in files {
            let content = std::fs::read_to_string(&path).unwrap();
            let Some((up_fm, _)) = upstream_split(&content) else {
                continue;
            };
            let (fm, _) = split(&content)
                .unwrap_or_else(|e| panic!("{} parsed upstream but not now: {e}", path.display()));
            assert_eq!(
                fm.trim_end(),
                up_fm.trim_end_matches('\r').trim_end(),
                "{}",
                path.display()
            );
        }
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
