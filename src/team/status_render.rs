//! Human-readable rendering for `batty status`.
//!
//! Everything here is a pure function of already-collected data, a terminal
//! width, and a colour flag, so the output can be unit tested without a
//! terminal, tmux, or the filesystem.

use super::daemon::MainSmokeState;
use super::status::{AgentHealthSummary, StatusTaskEntry, TeamStatusRow, WatchdogStatus};

const DEFAULT_WIDTH: usize = 100;
const MIN_WIDTH: usize = 40;
const COLUMN_GAP: usize = 2;
const MAX_ATTENTION_ITEMS: usize = 5;

/// Borrowed snapshot of everything the human renderer needs.
pub(crate) struct StatusView<'a> {
    pub(crate) team: &'a str,
    pub(crate) session: &'a str,
    pub(crate) session_running: bool,
    pub(crate) paused: bool,
    pub(crate) main_smoke: Option<&'a MainSmokeState>,
    pub(crate) watchdog: &'a WatchdogStatus,
    pub(crate) publish_handoff: Option<&'a crate::release::ReleasePublishHandoff>,
    pub(crate) active_tasks: &'a [StatusTaskEntry],
    pub(crate) review_queue: &'a [StatusTaskEntry],
    pub(crate) rows: &'a [TeamStatusRow],
    /// Task id of a merge left in post-merge verification by a daemon that
    /// is no longer running.
    pub(crate) stale_in_flight_merge: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RenderOptions {
    pub(crate) width: usize,
    pub(crate) color: bool,
    pub(crate) wide: bool,
    /// Print member rows at full natural width, with no fitting or truncation
    /// (`--wide` when stdout is not a terminal, so pipes get complete rows).
    pub(crate) unfitted: bool,
}

/// One thing the operator should look at, in plain language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttentionItem {
    pub(crate) member: Option<String>,
    pub(crate) text: String,
    pub(crate) command: Option<String>,
}

// ---------------------------------------------------------------------------
// Terminal detection
// ---------------------------------------------------------------------------

/// Pick the render width: `COLUMNS` if valid, else the terminal size, else 100.
pub(crate) fn resolve_width(columns_env: Option<&str>, tty_cols: Option<usize>) -> usize {
    let from_env = columns_env
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|cols| *cols > 0);
    from_env
        .or(tty_cols.filter(|cols| *cols > 0))
        .unwrap_or(DEFAULT_WIDTH)
        .max(MIN_WIDTH)
}

/// Colour only on a TTY, with `NO_COLOR` unset/empty and no `--no-color`.
pub(crate) fn resolve_color(no_color_flag: bool, no_color_env: Option<&str>, is_tty: bool) -> bool {
    !no_color_flag && is_tty && no_color_env.is_none_or(str::is_empty)
}

/// (columns, rows) of the terminal attached to stdout, if any.
#[cfg(unix)]
pub(crate) fn tty_size() -> Option<(usize, usize)> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes a winsize into the valid pointer we pass.
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ as _, &mut size) };
    (rc == 0 && size.ws_col > 0).then_some((size.ws_col as usize, size.ws_row as usize))
}

#[cfg(not(unix))]
pub(crate) fn tty_size() -> Option<(usize, usize)> {
    None
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// Replace control characters so task titles cannot inject terminal escapes.
fn sanitize(input: &str) -> String {
    input
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Truncate to at most `max` characters, ending in `…` when shortened.
pub(crate) fn truncate(input: &str, max: usize) -> String {
    let count = input.chars().count();
    if count <= max {
        return input.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = input.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn format_elapsed(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => {
            let (h, m) = (secs / 3600, (secs % 3600) / 60);
            if m == 0 {
                format!("{h}h")
            } else {
                format!("{h}h{m:02}m")
            }
        }
        _ => format!("{}d", secs / 86_400),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Plain,
    Bold,
    Dim,
    Green,
    Yellow,
    Red,
}

fn paint(text: &str, tone: Tone, color: bool) -> String {
    let code = match (color, tone) {
        (false, _) | (_, Tone::Plain) => return text.to_string(),
        (_, Tone::Bold) => "1",
        (_, Tone::Dim) => "2",
        (_, Tone::Green) => "32",
        (_, Tone::Yellow) => "33",
        (_, Tone::Red) => "31",
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

fn is_blank(value: &str) -> bool {
    let value = value.trim();
    value.is_empty() || value == "-"
}

// ---------------------------------------------------------------------------
// Plain-language member facts
// ---------------------------------------------------------------------------

fn state_label(state: &str) -> (&'static str, String, Tone) {
    match state {
        "working" => ("●", "working".into(), Tone::Green),
        "reviewing" => ("●", "reviewing".into(), Tone::Green),
        "triaging" => ("●", "triaging".into(), Tone::Green),
        "idle" => ("○", "idle".into(), Tone::Dim),
        "done" => ("✓", "done".into(), Tone::Dim),
        "starting" => ("◌", "starting".into(), Tone::Yellow),
        "stopped" => ("■", "stopped".into(), Tone::Yellow),
        "crashed" => ("✖", "crashed".into(), Tone::Red),
        "user" => ("", "you".into(), Tone::Dim),
        other => ("?", sanitize(other), Tone::Yellow),
    }
}

fn task_label(view: &StatusView<'_>, id: u32) -> String {
    let found = view
        .active_tasks
        .iter()
        .chain(view.review_queue.iter())
        .find(|task| task.id == id);
    match found {
        Some(task) => format!("#{id} {}", sanitize(&task.title)),
        None => format!("#{id}"),
    }
}

fn more_suffix(rest: usize) -> String {
    if rest == 0 {
        String::new()
    } else {
        format!(" (+{rest} more)")
    }
}

fn working_on(view: &StatusView<'_>, row: &TeamStatusRow) -> String {
    if let Some((first, rest)) = row.active_owned_tasks.split_first() {
        return format!("{}{}", task_label(view, *first), more_suffix(rest.len()));
    }
    if let Some((first, rest)) = row.review_owned_tasks.split_first() {
        return format!(
            "review {}{}",
            task_label(view, *first),
            more_suffix(rest.len())
        );
    }
    "-".to_string()
}

fn has_branch_problem(row: &TeamStatusRow) -> bool {
    row.signal
        .as_deref()
        .is_some_and(|s| s.contains("branch mismatch") || s.contains("branch recovery"))
}

fn member_has_attention_signal(row: &TeamStatusRow) -> bool {
    row.state == "crashed"
        || row.health.has_supervisory_warning()
        || !row.health.backend_health.is_healthy()
        || row.health.delivery_failure_count > 0
        || has_branch_problem(row)
}

/// The most important thing to say about a member, in plain words.
fn note_for(row: &TeamStatusRow) -> String {
    if let Some(signal) = row.signal.as_deref().filter(|s| !is_blank(s)) {
        return sanitize(signal);
    }
    if !row.health.backend_health.is_healthy() {
        return backend_note(&row.health);
    }
    if let Some(summary) = row.health.stall_summary.as_deref() {
        return sanitize(summary);
    }
    if row.health.delivery_failure_count > 0 {
        return format!(
            "{} message delivery failure(s)",
            row.health.delivery_failure_count
        );
    }
    if row.health.restart_count > 0 {
        return format!("restarted {}x", row.health.restart_count);
    }
    if row.pending_inbox > 0 {
        return format!("{} unread message(s)", row.pending_inbox);
    }
    String::new()
}

fn backend_note(health: &AgentHealthSummary) -> String {
    use crate::agent::BackendHealth::*;
    match health.backend_health {
        Healthy => String::new(),
        Degraded => "agent backend is returning errors".into(),
        Unreachable => "agent program not found".into(),
        QuotaExhausted => "agent quota exhausted".into(),
        AuthRequired => "agent needs a fresh login".into(),
        ConfigurationRequired => "agent backend settings need repair".into(),
    }
}

// ---------------------------------------------------------------------------
// Attention list
// ---------------------------------------------------------------------------

pub(crate) fn attention_items(view: &StatusView<'_>) -> Vec<AttentionItem> {
    let mut items = Vec::new();
    let mut push = |member: Option<&str>, text: String, command: Option<String>| {
        items.push(AttentionItem {
            member: member.map(str::to_string),
            text,
            command,
        });
    };

    if !view.session_running {
        push(
            None,
            "The team is not running.".into(),
            Some("batty start".into()),
        );
    } else if view.paused {
        push(
            None,
            "Nudges and standups are paused.".into(),
            Some("batty resume".into()),
        );
    }

    if let Some(task_id) = view.stale_in_flight_merge {
        push(
            None,
            format!(
                "The merge of #{task_id} is stuck in post-merge verification with no daemon running; it blocks `batty merge` (`batty start` resumes it)."
            ),
            Some("batty merge --force-clear-stale".into()),
        );
    }

    if let Some(smoke) = view.main_smoke.filter(|s| s.broken) {
        let commit = smoke.broken_commit.as_deref().unwrap_or("unknown commit");
        push(
            None,
            format!("main is failing its smoke test (broken by {commit})."),
            Some("batty doctor".into()),
        );
    }

    if view.session_running {
        for row in view.rows.iter().filter(|r| r.role_type != "User") {
            let name = row.name.as_str();
            let on_task = row
                .active_owned_tasks
                .first()
                .map(|id| format!(" on #{id}"))
                .unwrap_or_default();
            if row.state == "crashed" {
                push(
                    Some(name),
                    format!("{name} has crashed{on_task}."),
                    Some("batty doctor".into()),
                );
            }
            if row.health.has_supervisory_warning() {
                let for_how_long = row
                    .health
                    .task_elapsed_secs
                    .map(|secs| format!(" for {}", format_elapsed(secs)))
                    .unwrap_or_default();
                let why = row
                    .health
                    .stall_summary
                    .as_deref()
                    .map(|s| format!(": {}", sanitize(s)))
                    .unwrap_or_default();
                push(
                    Some(name),
                    format!("{name} looks stalled{for_how_long}{on_task}{why}"),
                    Some(format!("batty attach {name}")),
                );
            }
            if !row.health.backend_health.is_healthy() {
                let command = matches!(
                    row.health.backend_health,
                    crate::agent::BackendHealth::ConfigurationRequired
                )
                .then(|| format!("batty backend-reset {name}"));
                push(
                    Some(name),
                    format!("{name}: {}.", backend_note(&row.health)),
                    command,
                );
            }
            if row.health.delivery_failure_count > 0 {
                push(
                    Some(name),
                    format!(
                        "{name} had {} message delivery failure(s).",
                        row.health.delivery_failure_count
                    ),
                    Some(format!("batty inbox {name}")),
                );
            }
            if has_branch_problem(row) {
                push(
                    Some(name),
                    format!("{name}: {}", sanitize(row.signal.as_deref().unwrap_or(""))),
                    Some("batty worktree --health".into()),
                );
            }
        }
    }

    for task in view.active_tasks.iter().chain(view.review_queue.iter()) {
        if task.test_summary.is_some() {
            let owner = task.claimed_by.as_deref().unwrap_or("nobody");
            push(
                task.claimed_by.as_deref(),
                format!(
                    "#{} {} has failing tests (owner: {owner}).",
                    task.id,
                    truncate(&sanitize(&task.title), 40)
                ),
                Some("batty board".into()),
            );
        }
    }
    for task in view.review_queue.iter().filter(|t| t.blocked_on.is_some()) {
        let next = task
            .next_action
            .as_deref()
            .unwrap_or("resolve the blocker, then retry review");
        push(
            None,
            format!(
                "Review of #{} is blocked. Next: {}",
                task.id,
                truncate(&sanitize(next), 80)
            ),
            None,
        );
    }
    if let Some(handoff) = view.publish_handoff.filter(|h| h.status == "blocked") {
        push(
            None,
            format!(
                "Release publish is blocked: {}",
                truncate(&sanitize(&handoff.blocked_reasons.join("; ")), 80)
            ),
            None,
        );
    }
    items
}

// ---------------------------------------------------------------------------
// Summary block
// ---------------------------------------------------------------------------

pub(crate) fn render_summary(view: &StatusView<'_>, opts: &RenderOptions) -> String {
    let color = opts.color;
    let mut out = String::new();

    let (session_word, tone, hint) = if view.session_running {
        ("running", Tone::Green, "view it with `batty attach`")
    } else {
        ("stopped", Tone::Red, "start it with `batty start`")
    };
    out.push_str(&format!(
        "{} · session {} ({hint})\n",
        paint(&format!("Team {}", sanitize(view.team)), Tone::Bold, color),
        paint(session_word, tone, color),
    ));
    if opts.wide {
        out.push_str(&format!("Session: {}\n", sanitize(view.session)));
        out.push_str(&format!(
            "Watchdog: {}\n",
            super::status::format_watchdog_summary(view.watchdog)
        ));
        if let Some(smoke) = view.main_smoke {
            out.push_str(&format!(
                "Main smoke: {}\n",
                super::status::format_main_smoke_summary(smoke)
            ));
        }
        if let Some(handoff) = view.publish_handoff {
            out.push_str(&format!(
                "Publish handoff: {}\n",
                super::status::format_publish_handoff_summary(handoff)
            ));
        }
    }

    let items = attention_items(view);
    let mut working = 0;
    let mut idle = 0;
    let mut other = 0;
    let mut attention_members: std::collections::BTreeSet<String> = items
        .iter()
        .filter_map(|item| item.member.clone())
        .collect();
    for row in view.rows.iter().filter(|r| r.role_type != "User") {
        match row.state.as_str() {
            "working" | "reviewing" | "triaging" => working += 1,
            "idle" | "done" => idle += 1,
            _ => other += 1,
        }
        if view.session_running && member_has_attention_signal(row) {
            attention_members.insert(row.name.clone());
        }
    }
    let mut counts = vec![format!("{working} working"), format!("{idle} idle")];
    if other > 0 {
        counts.push(format!("{other} starting/stopped"));
    }
    let needs = attention_members.len();
    counts.push(paint(
        &format!("{needs} need attention"),
        if needs > 0 { Tone::Red } else { Tone::Dim },
        color,
    ));
    out.push_str(&format!("Members: {}\n", counts.join(" · ")));

    if items.is_empty() {
        out.push_str(&paint("Nothing needs your attention.", Tone::Green, color));
        out.push('\n');
        return out;
    }
    out.push_str(&paint("Needs attention:", Tone::Bold, color));
    out.push('\n');
    let shown = if opts.wide {
        items.len()
    } else {
        items.len().min(MAX_ATTENTION_ITEMS)
    };
    for item in &items[..shown] {
        let one_line = match &item.command {
            Some(cmd) => format!("  ! {} -> run `{cmd}`", item.text),
            None => format!("  ! {}", item.text),
        };
        if one_line.chars().count() <= opts.width {
            out.push_str(&paint(&one_line, Tone::Yellow, color));
            out.push('\n');
        } else {
            // Keep the suggested command visible by moving it to its own line.
            let text = truncate(&format!("  ! {}", item.text), opts.width);
            out.push_str(&paint(&text, Tone::Yellow, color));
            out.push('\n');
            if let Some(cmd) = &item.command {
                out.push_str(&format!("      -> run `{cmd}`\n"));
            }
        }
    }
    if items.len() > shown {
        out.push_str(&format!(
            "  ... and {} more (use `batty status --wide`)\n",
            items.len() - shown
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Members table
// ---------------------------------------------------------------------------

struct Column {
    header: &'static str,
    cells: Vec<(String, Tone)>,
    min: usize,
    flex: bool,
    right: bool,
}

fn nonzero(n: usize) -> String {
    if n == 0 { String::new() } else { n.to_string() }
}

fn build_columns(view: &StatusView<'_>, wide: bool) -> Vec<Column> {
    let rows = view.rows;
    let plain = |f: &dyn Fn(&TeamStatusRow) -> String| -> Vec<(String, Tone)> {
        rows.iter().map(|r| (f(r), Tone::Plain)).collect()
    };
    let col = |header, cells, min, flex, right| Column {
        header,
        cells,
        min,
        flex,
        right,
    };
    let states: Vec<(String, Tone)> = rows
        .iter()
        .map(|r| {
            let (symbol, word, tone) = state_label(&r.state);
            let text = if symbol.is_empty() {
                word
            } else {
                format!("{symbol} {word}")
            };
            (text, tone)
        })
        .collect();

    let mut cols = vec![
        col("MEMBER", plain(&|r| sanitize(&r.name)), 8, true, false),
        col("ROLE", plain(&|r| sanitize(&r.role)), 6, true, false),
    ];
    if wide {
        cols.push(col(
            "AGENT",
            plain(&|r| r.agent.clone().unwrap_or_default()),
            5,
            true,
            false,
        ));
        cols.push(col("STATE", states, 7, false, false));
        cols.push(col(
            "INBOX",
            plain(&|r| nonzero(r.pending_inbox)),
            5,
            false,
            true,
        ));
        cols.push(col(
            "TRIAGE",
            plain(&|r| nonzero(r.triage_backlog)),
            6,
            false,
            true,
        ));
        cols.push(col(
            "STALE",
            plain(&|r| {
                r.worktree_staleness
                    .map(|n| n.to_string())
                    .unwrap_or_default()
            }),
            5,
            false,
            true,
        ));
        cols.push(col(
            "ACTIVE",
            plain(&|r| super::status::format_owned_tasks_summary(&r.active_owned_tasks)),
            6,
            true,
            false,
        ));
        cols.push(col(
            "REVIEW",
            plain(&|r| super::status::format_owned_tasks_summary(&r.review_owned_tasks)),
            6,
            true,
            false,
        ));
        cols.push(col("ETA", plain(&|r| r.eta.clone()), 3, true, false));
        cols.push(col(
            "HEALTH",
            plain(&|r| r.health_summary.clone()),
            6,
            true,
            false,
        ));
        cols.push(col(
            "SIGNAL",
            plain(&|r| r.signal.as_deref().map(sanitize).unwrap_or_default()),
            10,
            true,
            false,
        ));
        cols.push(col(
            "REPORTS TO",
            plain(&|r| r.reports_to.clone().unwrap_or_default()),
            8,
            true,
            false,
        ));
    } else {
        cols.push(col("STATE", states, 7, false, false));
        cols.push(col(
            "WORKING ON",
            plain(&|r| working_on(view, r)),
            10,
            true,
            false,
        ));
        cols.push(col(
            "FOR",
            plain(&|r| {
                if r.active_owned_tasks.is_empty() {
                    String::new()
                } else {
                    r.health
                        .task_elapsed_secs
                        .map(format_elapsed)
                        .unwrap_or_default()
                }
            }),
            3,
            false,
            true,
        ));
        let notes = rows
            .iter()
            .map(|r| {
                let tone = if member_has_attention_signal(r) {
                    Tone::Yellow
                } else {
                    Tone::Plain
                };
                (note_for(r), tone)
            })
            .collect();
        cols.push(col("NOTE", notes, 8, true, false));
    }

    // Hide columns that are empty for every row (MEMBER/ROLE/STATE stay).
    cols.retain(|c| {
        matches!(c.header, "MEMBER" | "ROLE" | "STATE")
            || c.cells.iter().any(|(text, _)| !is_blank(text))
    });
    for c in &mut cols {
        for (text, _) in &mut c.cells {
            if is_blank(text) {
                *text = "-".to_string();
            }
        }
    }
    cols
}

fn fit_widths(cols: &[Column], width: usize) -> Vec<usize> {
    let mut widths: Vec<usize> = cols
        .iter()
        .map(|c| {
            c.cells
                .iter()
                .map(|(t, _)| t.chars().count())
                .chain([c.header.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let gaps = COLUMN_GAP * cols.len().saturating_sub(1);
    while widths.iter().sum::<usize>() + gaps > width {
        // Shrink the widest flexible column that still has room above its minimum.
        let candidate = (0..cols.len())
            .filter(|&i| cols[i].flex && widths[i] > cols[i].min)
            .max_by_key(|&i| widths[i]);
        match candidate {
            Some(i) => widths[i] -= 1,
            None => break,
        }
    }
    widths
}

pub(crate) fn render_members(view: &StatusView<'_>, opts: &RenderOptions) -> String {
    if view.rows.is_empty() {
        return "No team members configured.\n".to_string();
    }
    let cols = build_columns(view, opts.wide);
    let widths = fit_widths(
        &cols,
        if opts.unfitted {
            usize::MAX
        } else {
            opts.width
        },
    );
    let gap = " ".repeat(COLUMN_GAP);
    let last = cols.len() - 1;
    let pad = |text: &str, w: usize, right: bool, is_last: bool| {
        let text = truncate(text, w);
        let fill = if is_last && !right {
            String::new()
        } else {
            " ".repeat(w.saturating_sub(text.chars().count()))
        };
        if right {
            format!("{fill}{text}")
        } else {
            format!("{text}{fill}")
        }
    };

    let mut out = String::new();
    let header: Vec<String> = cols
        .iter()
        .zip(&widths)
        .enumerate()
        .map(|(i, (c, &w))| {
            paint(
                &pad(c.header, w, c.right, i == last),
                Tone::Bold,
                opts.color,
            )
        })
        .collect();
    out.push_str(&header.join(&gap));
    out.push('\n');
    for i in 0..view.rows.len() {
        let line: Vec<String> = cols
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(j, (c, &w))| {
                let (text, tone) = &c.cells[i];
                paint(&pad(text, w, c.right, j == last), *tone, opts.color)
            })
            .collect();
        out.push_str(&line.join(&gap));
        out.push('\n');
    }
    out
}

pub(crate) fn render_hint(opts: &RenderOptions, watching: bool) -> String {
    let text = if watching {
        "Ctrl-C to exit · `--wide` all columns · `batty attach` view team"
    } else {
        "`--wide` all columns · `--watch` live refresh · `batty attach` view team"
    };
    paint(&truncate(text, opts.width), Tone::Dim, opts.color)
}

/// Summary, members table and hint, as one block of text (test helper).
#[cfg(test)]
pub(crate) fn render_overview(
    view: &StatusView<'_>,
    opts: &RenderOptions,
    watching: bool,
) -> String {
    let mut out = render_summary(view, opts);
    out.push('\n');
    out.push_str(&render_members(view, opts));
    out.push('\n');
    out.push_str(&render_hint(opts, watching));
    out.push('\n');
    out
}

/// Truncate every line of plain (escape-free) text to `width` characters.
pub(crate) fn truncate_lines(text: &str, width: usize) -> String {
    text.lines()
        .map(|line| truncate(line, width))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, role: &str, role_type: &str, state: &str) -> TeamStatusRow {
        TeamStatusRow {
            name: name.into(),
            role: role.into(),
            role_type: role_type.into(),
            agent: Some("claude".into()),
            reports_to: None,
            state: state.into(),
            pending_inbox: 0,
            triage_backlog: 0,
            active_owned_tasks: vec![],
            review_owned_tasks: vec![],
            signal: None,
            runtime_label: None,
            worktree_staleness: None,
            health: AgentHealthSummary::default(),
            health_summary: "-".into(),
            eta: "-".into(),
        }
    }

    fn task(id: u32, title: &str) -> StatusTaskEntry {
        StatusTaskEntry {
            id,
            title: title.into(),
            status: "in-progress".into(),
            priority: "high".into(),
            claimed_by: None,
            review_owner: None,
            blocked_on: None,
            branch: None,
            worktree_path: None,
            commit: None,
            branch_mismatch: None,
            next_action: None,
            test_summary: None,
            latest_artifact: None,
            failed_test_state: None,
        }
    }

    struct Fixture {
        rows: Vec<TeamStatusRow>,
        tasks: Vec<StatusTaskEntry>,
        watchdog: WatchdogStatus,
    }

    fn fixture() -> Fixture {
        let mut architect = row("architect", "architect", "Architect", "idle");
        architect.pending_inbox = 2;
        let mut manager = row("manager-1", "manager", "Manager", "working");
        manager.reports_to = Some("architect".into());
        let mut eng1 = row("eng-1-1", "engineer", "Engineer", "working");
        eng1.reports_to = Some("manager-1".into());
        eng1.active_owned_tasks = vec![40];
        eng1.health.task_elapsed_secs = Some(540);
        let mut eng2 = row("eng-1-2", "engineer", "Engineer", "working");
        eng2.reports_to = Some("manager-1".into());
        eng2.active_owned_tasks = vec![41];
        eng2.health.task_elapsed_secs = Some(1500);
        eng2.health.stall_reason = Some("stalled".into());
        eng2.health.stall_summary = Some("no output for 25m".into());
        let eng3 = row("eng-1-3", "engineer", "Engineer", "idle");
        let user = row("human", "user", "User", "user");
        Fixture {
            rows: vec![user, architect, manager, eng1, eng2, eng3],
            tasks: vec![
                task(40, "Add retry logic to the Discord bridge"),
                task(41, "Fix flaky shim classifier test on slow machines"),
            ],
            watchdog: WatchdogStatus::default(),
        }
    }

    fn view<'a>(f: &'a Fixture, running: bool) -> StatusView<'a> {
        StatusView {
            team: "demo",
            session: "batty-demo",
            session_running: running,
            paused: false,
            main_smoke: None,
            watchdog: &f.watchdog,
            publish_handoff: None,
            active_tasks: &f.tasks,
            review_queue: &[],
            rows: &f.rows,
            stale_in_flight_merge: None,
        }
    }

    fn opts(width: usize) -> RenderOptions {
        RenderOptions {
            width,
            color: false,
            wide: false,
            unfitted: false,
        }
    }

    fn max_width(text: &str) -> usize {
        text.lines().map(|l| l.chars().count()).max().unwrap_or(0)
    }

    #[test]
    fn overview_fits_width_at_80_120_200() {
        let f = fixture();
        for width in [80, 120, 200] {
            let out = render_overview(&view(&f, true), &opts(width), false);
            assert!(max_width(&out) <= width, "width {width}:\n{out}");
            assert!(!out.contains('\x1b'));
        }
    }

    #[test]
    fn overview_80_snapshot() {
        let f = fixture();
        let out = render_overview(&view(&f, true), &opts(80), false);
        let expected = "\
Team demo · session running (view it with `batty attach`)
Members: 3 working · 2 idle · 1 need attention
Needs attention:
  ! eng-1-2 looks stalled for 25m on #41: no output for 25m
      -> run `batty attach eng-1-2`

MEMBER     ROLE       STATE      WORKING ON             FOR  NOTE
human      user       you        -                        -  -
architect  architect  ○ idle     -                        -  2 unread message(s)
manager-1  manager    ● working  -                        -  -
eng-1-1    engineer   ● working  #40 Add retry logic …   9m  -
eng-1-2    engineer   ● working  #41 Fix flaky shim c…  25m  no output for 25m
eng-1-3    engineer   ○ idle     -                        -  -

`--wide` all columns · `--watch` live refresh · `batty attach` view team
";
        assert_eq!(out, expected);
    }

    #[test]
    fn long_titles_truncate_with_ellipsis_not_wrap() {
        let f = fixture();
        let out = render_members(&view(&f, true), &opts(60));
        assert!(out.contains('…'));
        assert!(max_width(&out) <= 60, "{out}");
        assert_eq!(out.lines().count(), f.rows.len() + 1);
    }

    #[test]
    fn empty_columns_are_hidden() {
        let mut f = fixture();
        for r in &mut f.rows {
            r.health.task_elapsed_secs = None;
            r.pending_inbox = 0;
            r.health.stall_reason = None;
            r.health.stall_summary = None;
        }
        let out = render_members(&view(&f, true), &opts(120));
        assert!(!out.contains("FOR"), "{out}");
        assert!(!out.contains("NOTE"), "{out}");
        assert!(out.contains("WORKING ON"));
        let wide = render_members(
            &view(&f, true),
            &RenderOptions {
                wide: true,
                ..opts(200)
            },
        );
        assert!(!wide.contains("TRIAGE"), "{wide}");
        assert!(!wide.contains("STALE"), "{wide}");
        assert!(wide.contains("REPORTS TO"));
        assert!(wide.contains("ACTIVE"));
    }

    #[test]
    fn wide_mode_fits_and_keeps_columns() {
        let mut f = fixture();
        f.rows[4].signal = Some("nudged".into());
        let o = RenderOptions {
            wide: true,
            ..opts(120)
        };
        let out = render_members(&view(&f, true), &o);
        assert!(max_width(&out) <= 120, "{out}");
        for header in ["MEMBER", "ROLE", "AGENT", "STATE", "INBOX", "SIGNAL"] {
            assert!(out.contains(header), "{header} missing:\n{out}");
        }
    }

    #[test]
    fn wide_unfitted_prints_full_rows_without_truncation() {
        let mut f = fixture();
        f.rows[4].signal = Some("nudged-with-a-very-long-signal-name-that-would-be-cut".into());
        let narrow = RenderOptions {
            wide: true,
            ..opts(40)
        };
        let fitted = render_members(&view(&f, true), &narrow);
        assert!(!fitted.contains("nudged-with-a-very-long-signal-name-that-would-be-cut"));
        let full = render_members(
            &view(&f, true),
            &RenderOptions {
                unfitted: true,
                ..narrow
            },
        );
        assert!(
            full.contains("nudged-with-a-very-long-signal-name-that-would-be-cut"),
            "{full}"
        );
    }

    #[test]
    fn colour_is_opt_in_and_state_words_survive() {
        let f = fixture();
        let coloured = render_overview(
            &view(&f, true),
            &RenderOptions {
                color: true,
                ..opts(100)
            },
            false,
        );
        assert!(coloured.contains("\x1b["));
        let stripped = strip_ansi(&coloured);
        let plain = render_overview(&view(&f, true), &opts(100), false);
        assert_eq!(stripped, plain);
        assert!(plain.contains("working"));
        assert!(plain.contains("idle"));
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for n in chars.by_ref() {
                    if n == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn attention_lists_stall_with_existing_command() {
        let f = fixture();
        let items = attention_items(&view(&f, true));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].member.as_deref(), Some("eng-1-2"));
        assert!(items[0].text.contains("stalled for 25m on #41"));
        assert_eq!(items[0].command.as_deref(), Some("batty attach eng-1-2"));
    }

    #[test]
    fn stopped_session_points_at_start() {
        let f = fixture();
        let out = render_summary(&view(&f, false), &opts(100));
        assert!(out.contains("session stopped (start it with `batty start`)"));
        assert!(out.contains("run `batty start`"));
    }

    #[test]
    fn paused_and_crash_and_backend_items() {
        let mut f = fixture();
        f.rows[3].state = "crashed".into();
        f.rows[5].health.backend_health = crate::agent::BackendHealth::ConfigurationRequired;
        let mut v = view(&f, true);
        v.paused = true;
        let items = attention_items(&v);
        let cmds: Vec<_> = items.iter().filter_map(|i| i.command.as_deref()).collect();
        assert!(cmds.contains(&"batty resume"));
        assert!(cmds.contains(&"batty doctor"));
        assert!(cmds.contains(&"batty backend-reset eng-1-3"));
    }

    #[test]
    fn stale_in_flight_merge_is_an_attention_item() {
        let f = fixture();
        let mut v = view(&f, false);
        v.stale_in_flight_merge = Some(42);
        let items = attention_items(&v);
        let item = items
            .iter()
            .find(|i| i.command.as_deref() == Some("batty merge --force-clear-stale"))
            .expect("stale merge must be listed");
        assert!(item.text.contains("#42"), "{}", item.text);
        let out = render_summary(&v, &opts(200));
        assert!(out.contains("--force-clear-stale"), "{out}");
    }

    #[test]
    fn healthy_team_says_nothing_needs_attention() {
        let mut f = fixture();
        f.rows[4].health = AgentHealthSummary::default();
        let out = render_summary(&view(&f, true), &opts(100));
        assert!(out.contains("Nothing needs your attention."));
        assert!(out.contains("0 need attention"));
    }

    #[test]
    fn titles_cannot_inject_escapes() {
        let mut f = fixture();
        f.tasks[0].title = "evil\x1b[31m title\n".into();
        let out = render_members(&view(&f, true), &opts(120));
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn width_and_colour_resolution() {
        assert_eq!(resolve_width(Some("132"), Some(80)), 132);
        assert_eq!(resolve_width(Some("junk"), Some(80)), 80);
        assert_eq!(resolve_width(None, None), 100);
        assert_eq!(resolve_width(Some("10"), None), MIN_WIDTH);
        assert!(resolve_color(false, None, true));
        assert!(resolve_color(false, Some(""), true));
        assert!(!resolve_color(false, Some("1"), true));
        assert!(!resolve_color(true, None, true));
        assert!(!resolve_color(false, None, false));
    }

    #[test]
    fn truncate_handles_edges() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "ab…");
        assert_eq!(truncate("abcd", 0), "");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
    }

    #[test]
    fn elapsed_formatting() {
        assert_eq!(format_elapsed(5), "5s");
        assert_eq!(format_elapsed(1500), "25m");
        assert_eq!(format_elapsed(3600), "1h");
        assert_eq!(format_elapsed(5400), "1h30m");
        assert_eq!(format_elapsed(172_800), "2d");
    }

    #[test]
    fn json_report_serialisation_is_unchanged() {
        let f = fixture();
        let report = crate::team::status::build_team_status_json_report(
            crate::team::status::TeamStatusJsonReportInput {
                team: "demo".into(),
                session: "batty-demo".into(),
                session_running: true,
                paused: false,
                main_smoke: None,
                watchdog: f.watchdog.clone(),
                workflow_metrics: None,
                publish_handoff: None,
                active_tasks: f.tasks.clone(),
                review_queue: vec![],
                optional_subsystems: None,
                engineer_profiles: None,
                members: f.rows.clone(),
            },
        );
        let actual = serde_json::to_string_pretty(&report).unwrap();
        let golden = include_str!("fixtures/status_json_report.json");
        assert_eq!(actual.trim_end(), golden.trim_end());
    }
}
