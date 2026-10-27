//! Pure builders for the tmux accessibility layer of a batty session.
//!
//! Everything here returns tmux argv vectors (or strings) so it can be unit
//! tested without a tmux server. All options are session- or window-scoped;
//! nothing touches `~/.tmux.conf` or global options. Key bindings are
//! server-wide in tmux, so each one is guarded to act only inside `batty-*`
//! sessions and to pass the key through everywhere else.

use std::path::Path;

use super::config::LayoutMode;

/// Minimum agent-pane size for the `auto` rule (columns x rows per pane).
pub const MIN_PANE_COLS: u16 = 50;
pub const MIN_PANE_ROWS: u16 = 12;
/// `auto` never tiles more than this many agent panes.
pub const AUTO_MAX_PANES: usize = 4;
/// Size assumed when no controlling terminal is available.
pub const DEFAULT_COLS: u16 = 220;
pub const DEFAULT_ROWS: u16 = 50;
/// Highest number of F-keys used for jump targets (F2..F10 -> 9 targets).
pub const MAX_JUMP_KEYS: usize = 9;
pub const HISTORY_LIMIT: u32 = 20_000;

/// The concrete layout used after resolving `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedMode {
    Panes,
    Windows,
    Focus,
}

/// Resolve `layout.mode` to a concrete layout.
///
/// `auto` tiles panes only when they stay usable: at most [`AUTO_MAX_PANES`]
/// panes, and the terminal gives each at least 50x12 cells in a 2-column
/// grid. Otherwise it uses one window per member. A hand-written
/// `layout.zones` list in `auto` mode is honoured as tiled panes, so existing
/// configs keep the layout their author designed.
pub fn resolve_mode(
    mode: LayoutMode,
    agent_panes: usize,
    has_zones: bool,
    cols: u16,
    rows: u16,
) -> ResolvedMode {
    match mode {
        LayoutMode::Panes => ResolvedMode::Panes,
        LayoutMode::Windows => ResolvedMode::Windows,
        LayoutMode::Focus => ResolvedMode::Focus,
        LayoutMode::Auto => {
            if has_zones || panes_stay_usable(agent_panes, cols, rows) {
                ResolvedMode::Panes
            } else {
                ResolvedMode::Windows
            }
        }
    }
}

/// True when `n` tiled panes would each keep a readable size.
pub fn panes_stay_usable(n: usize, cols: u16, rows: u16) -> bool {
    if n == 0 || n > AUTO_MAX_PANES {
        return false;
    }
    let grid_cols = n.min(2) as u16;
    let grid_rows = n.div_ceil(2) as u16;
    cols / grid_cols >= MIN_PANE_COLS && rows / grid_rows >= MIN_PANE_ROWS
}

/// Terminal size of the invoking terminal, or the session default.
pub fn terminal_size() -> (u16, u16) {
    std::process::Command::new("stty")
        .arg("size")
        .stdin(std::process::Stdio::inherit())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| parse_stty_size(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or((DEFAULT_COLS, DEFAULT_ROWS))
}

/// Parse `stty size` output (`rows cols`) into `(cols, rows)`.
pub fn parse_stty_size(output: &str) -> Option<(u16, u16)> {
    let mut it = output.split_whitespace();
    let rows: u16 = it.next()?.parse().ok()?;
    let cols: u16 = it.next()?.parse().ok()?;
    (rows > 0 && cols > 0).then_some((cols, rows))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TmuxVersion {
    pub major: u32,
    pub minor: u32,
}

impl TmuxVersion {
    pub fn at_least(self, major: u32, minor: u32) -> bool {
        (self.major, self.minor) >= (major, minor)
    }
}

/// Parse `tmux -V` output such as `tmux 3.4`, `tmux 3.3a`, `tmux next-3.5`.
pub fn parse_tmux_version(output: &str) -> Option<TmuxVersion> {
    let token = output.split_whitespace().nth(1)?;
    let token = token.trim_start_matches(|c: char| !c.is_ascii_digit());
    let mut parts = token.splitn(2, '.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor_raw = parts.next().unwrap_or("0");
    let digits: String = minor_raw.chars().take_while(char::is_ascii_digit).collect();
    let minor = digits.parse().unwrap_or(0);
    Some(TmuxVersion { major, minor })
}

/// Detect the installed tmux version (None when unknown).
pub fn detect_tmux_version() -> Option<TmuxVersion> {
    let out = crate::tmux::tmux_command().arg("-V").output().ok()?;
    parse_tmux_version(&String::from_utf8_lossy(&out.stdout))
}

/// Turn a member name into a safe tmux window name (no `.` or `:`).
pub fn window_name(member: &str) -> String {
    let cleaned: String = member
        .chars()
        .map(|c| {
            if c.is_whitespace() || c == '.' || c == ':' {
                '-'
            } else {
                c
            }
        })
        .collect();
    if cleaned.is_empty() {
        "member".to_string()
    } else {
        cleaned
    }
}

/// Unique window names for members in the given (hierarchy) order.
/// Duplicates get a `-2`, `-3`, ... suffix; `dashboard` is reserved.
pub fn window_names<'a>(members: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut used: Vec<String> = vec!["dashboard".to_string()];
    let mut out = Vec::new();
    for member in members {
        let base = window_name(member);
        let mut candidate = base.clone();
        let mut n = 2;
        while used.contains(&candidate) {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        used.push(candidate.clone());
        out.push(candidate);
    }
    out
}

/// Shell command for the dashboard window.
///
/// Uses `batty status --watch` when the binary supports it, otherwise a
/// five-second shell loop. Keep this the single place to switch.
pub fn dashboard_command(batty: &str, project_root: &str, supports_watch: bool) -> String {
    let b = sh_quote(batty);
    let r = sh_quote(project_root);
    if supports_watch {
        format!("cd {r} && exec {b} status --watch")
    } else {
        format!(
            "cd {r} && while true; do clear; {b} status; printf '\\n(refreshing every 5s; Ctrl-c stops)\\n'; sleep 5; done"
        )
    }
}

/// Whether this batty binary's `status` accepts `--watch`.
pub fn status_supports_watch(batty: &str) -> bool {
    std::process::Command::new(batty)
        .args(["status", "--help"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("--watch"))
        .unwrap_or(false)
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// What a jump key targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JumpTarget {
    /// Window index (windows mode).
    Window(usize),
    /// Pane id such as `%7` (panes and focus modes).
    Pane(String),
}

/// One-line key hint shown in the second status row.
pub fn hint_line(version: Option<TmuxVersion>, mode: ResolvedMode, keys: bool) -> String {
    let what = match mode {
        ResolvedMode::Windows => "Click a tab",
        _ => "Click a pane",
    };
    if keys && keys_supported(version) {
        let jump = if mode == ResolvedMode::Focus {
            "F2-F10 focus a member, F11 next"
        } else {
            "F2-F10 jump to a member"
        };
        format!("{what} or {jump} | F12 zoom | F1 help | scroll with mouse | Ctrl-b d to detach")
    } else {
        format!("{what} to switch | Ctrl-b z zoom | scroll with mouse | Ctrl-b d to detach")
    }
}

fn keys_supported(version: Option<TmuxVersion>) -> bool {
    version.is_some_and(|v| v.at_least(3, 0))
}

fn popup_supported(version: Option<TmuxVersion>) -> bool {
    version.is_some_and(|v| v.at_least(3, 2))
}

fn set(target: &str, scope: &str, name: &str, value: &str) -> Vec<String> {
    vec![
        "set-option".into(),
        scope.into(),
        "-t".into(),
        target.into(),
        name.into(),
        value.into(),
    ]
}

/// Pane titles: role, an active marker (not colour alone), then state words.
pub const PANE_BORDER_FORMAT: &str =
    "#{?pane_active,#[fg=colour39 bold]> ,  }#[bold]#{@batty_role}#[default] #{@batty_status} ";

/// Session-scoped option commands (`set-option -t <session> ...`).
pub fn session_option_commands(
    session: &str,
    version: Option<TmuxVersion>,
    mode: ResolvedMode,
    keys: bool,
) -> Vec<Vec<String>> {
    let s = |n: &str, v: &str| set(session, "-q", n, v);
    let mut cmds = vec![
        s("mouse", "on"),
        s("history-limit", &HISTORY_LIMIT.to_string()),
        s("base-index", "1"),
        s("renumber-windows", "on"),
        s("status", "on"),
        s("status-interval", "5"),
        s("status-style", "bg=colour17,fg=colour255"),
        s("status-left", " #[bold]batty#[default] | "),
        s("status-left-length", "24"),
        s("window-status-format", " #I:#W "),
        s(
            "window-status-current-format",
            "#[bg=colour208,fg=colour16,bold] [#I:#W] #[default]",
        ),
        s("window-status-separator", ""),
        s("display-time", "4000"),
    ];
    if keys && keys_supported(version) {
        // Session-scoped key table: other sessions on the server are unaffected.
        cmds.push(s("key-table", &key_table_name(session)));
    }
    let hint = hint_line(version, mode, keys);
    if version.is_some_and(|v| v.at_least(2, 9)) {
        cmds.push(s("status", "2"));
        cmds.push(s("status-right", " #{session_name} "));
        cmds.push(s(
            "status-format[1]",
            &format!("#[align=centre,fg=colour250] {hint} "),
        ));
    } else {
        cmds.push(s("status-right-length", "120"));
        cmds.push(s("status-right", &format!(" {hint} ")));
    }
    if version.is_some_and(|v| v.at_least(3, 1)) {
        cmds.push(s("window-size", "latest"));
    }
    cmds
}

/// Window-scoped option commands for one window target (`session:N`).
pub fn window_option_commands(target: &str) -> Vec<Vec<String>> {
    let w = |n: &str, v: &str| set(target, "-wq", n, v);
    vec![
        w("aggressive-resize", "on"),
        w("automatic-rename", "off"),
        w("allow-rename", "off"),
        w("pane-base-index", "1"),
        w("pane-border-status", "top"),
        w("pane-border-format", PANE_BORDER_FORMAT),
        w("pane-border-style", "fg=colour244"),
        w("pane-active-border-style", "fg=colour39,bold"),
    ]
}

/// Commands that make one window a focus layout (big pane + narrow list).
pub fn focus_layout_commands(target: &str, version: Option<TmuxVersion>) -> Vec<Vec<String>> {
    let mut cmds = Vec::new();
    if version.is_some_and(|v| v.at_least(3, 1)) {
        cmds.push(set(target, "-wq", "main-pane-width", "65%"));
    }
    cmds.push(vec![
        "select-layout".into(),
        "-t".into(),
        target.into(),
        "main-vertical".into(),
    ]);
    cmds
}

/// Name of the key table owned by one batty session (the session name).
///
/// A session whose `key-table` option names a custom table looks keys up
/// there first; the prefix key and any unbound key still behave normally
/// (verified on tmux 3.8). Using one table per session keeps pane-id targets
/// from different teams apart.
pub fn key_table_name(session: &str) -> String {
    session.to_string()
}

fn bind(table: &str, key: &str, action: &str) -> Vec<String> {
    vec![
        "bind-key".into(),
        "-T".into(),
        table.into(),
        key.into(),
        action.into(),
    ]
}

/// F-key for jump slot `i` (0-based): F2..F10.
pub fn jump_key(i: usize) -> String {
    format!("F{}", i + 2)
}

/// Key bindings in the session's own key table (never the root or prefix
/// tables). The table is cleared first so re-running start cannot accumulate
/// stale bindings. Returns nothing when keys are unsupported.
pub fn key_binding_commands(
    session: &str,
    jumps: &[JumpTarget],
    mode: ResolvedMode,
    help_file: &Path,
    version: Option<TmuxVersion>,
) -> Vec<Vec<String>> {
    if !keys_supported(version) {
        return Vec::new();
    }
    let table = key_table_name(session);
    let table = table.as_str();
    let mut cmds = vec![vec![
        "unbind-key".into(),
        "-a".into(),
        "-T".into(),
        table.into(),
    ]];
    cmds.push(bind(table, "F1", &help_action(help_file, version)));
    cmds.push(bind(table, "F12", "resize-pane -Z"));
    for (i, target) in jumps.iter().take(MAX_JUMP_KEYS).enumerate() {
        let key = jump_key(i);
        let action = match (target, mode) {
            (JumpTarget::Window(n), _) => format!("select-window -t :{n}"),
            (JumpTarget::Pane(id), ResolvedMode::Focus) => {
                format!("swap-pane -s {id} -t :.1 ; select-pane -t :.1")
            }
            (JumpTarget::Pane(id), _) => format!("select-pane -t {id}"),
        };
        cmds.push(bind(table, &key, &action));
    }
    if mode == ResolvedMode::Focus {
        cmds.push(bind(table, "F11", "rotate-window -U ; select-pane -t :.1"));
    }
    cmds
}

fn help_action(help_file: &Path, version: Option<TmuxVersion>) -> String {
    if popup_supported(version) {
        let file = sh_quote(&help_file.to_string_lossy());
        let script = format!("cat {file}; printf '\\nPress Enter to close'; read _");
        format!(
            "display-popup -E -w 80 -h 30 {}",
            tmux_quote(&format!("sh -c {}", sh_quote(&script)))
        )
    } else {
        "display-message -d 15000 'F2-F10 jump | F12 zoom | scroll: mouse wheel | detach: Ctrl-b d | commands: batty status, batty send, batty inbox'".to_string()
    }
}

fn tmux_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
            .replace('#', "##")
    )
}

/// Text of the F1 help popup.
pub fn help_text(mode: ResolvedMode, members: &[String]) -> String {
    let mut out = String::new();
    out.push_str("BATTY TMUX HELP\n===============\n\n");
    out.push_str("Moving around\n");
    out.push_str(match mode {
        ResolvedMode::Windows => {
            "  Click a tab in the bottom bar, or press F2-F10 to jump to a window.\n"
        }
        ResolvedMode::Panes => "  Click a pane, or press F2-F10 to jump to a member's pane.\n",
        ResolvedMode::Focus => {
            "  F2-F10 puts that member in the big pane; F11 cycles to the next.\n"
        }
    });
    out.push_str("  F12              zoom/unzoom the current pane (full screen)\n");
    out.push_str("  Mouse wheel      scroll back (press q or Esc to leave scrollback)\n");
    out.push_str("  Ctrl-b d         detach (agents keep running; `batty attach` returns)\n");
    out.push_str("  F1               this help\n");
    out.push_str("  Ctrl-b ?         tmux's own key list\n\n");
    out.push_str("Jump keys\n");
    for (i, m) in members.iter().take(MAX_JUMP_KEYS).enumerate() {
        out.push_str(&format!("  {:<4} {m}\n", jump_key(i)));
    }
    out.push_str("\nBatty commands (run in another terminal)\n");
    out.push_str("  batty status          team and task overview\n");
    out.push_str("  batty attach [NAME]   reconnect, optionally jumping to a member\n");
    out.push_str("  batty send NAME MSG   message a member\n");
    out.push_str("  batty inbox NAME      show a member's queue\n");
    out.push_str("  batty stop            stop the team\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u32, minor: u32) -> Option<TmuxVersion> {
        Some(TmuxVersion { major, minor })
    }

    #[test]
    fn auto_small_team_on_big_terminal_uses_panes() {
        assert_eq!(
            resolve_mode(LayoutMode::Auto, 4, false, 220, 50),
            ResolvedMode::Panes
        );
    }

    #[test]
    fn auto_many_members_uses_windows() {
        assert_eq!(
            resolve_mode(LayoutMode::Auto, 7, false, 400, 100),
            ResolvedMode::Windows
        );
    }

    #[test]
    fn auto_small_terminal_uses_windows() {
        assert_eq!(
            resolve_mode(LayoutMode::Auto, 3, false, 80, 24),
            ResolvedMode::Windows
        );
        assert!(panes_stay_usable(2, 100, 24));
        assert!(!panes_stay_usable(4, 100, 20));
        assert!(!panes_stay_usable(0, 220, 50));
    }

    #[test]
    fn auto_honours_explicit_zones() {
        assert_eq!(
            resolve_mode(LayoutMode::Auto, 9, true, 220, 50),
            ResolvedMode::Panes
        );
    }

    #[test]
    fn explicit_modes_are_not_overridden() {
        assert_eq!(
            resolve_mode(LayoutMode::Panes, 20, false, 80, 24),
            ResolvedMode::Panes
        );
        assert_eq!(
            resolve_mode(LayoutMode::Windows, 1, true, 400, 100),
            ResolvedMode::Windows
        );
        assert_eq!(
            resolve_mode(LayoutMode::Focus, 1, false, 80, 24),
            ResolvedMode::Focus
        );
    }

    #[test]
    fn layout_mode_defaults_to_auto_for_legacy_zones() {
        let layout: crate::team::config::LayoutConfig =
            serde_yaml::from_str("zones:\n- name: architect\n  width_pct: 100\n").unwrap();
        assert_eq!(layout.mode, LayoutMode::Auto);
        assert_eq!(layout.zones.len(), 1);
        assert!(layout.keys);
        let off: crate::team::config::LayoutConfig = serde_yaml::from_str("keys: false\n").unwrap();
        assert!(!off.keys);
    }

    #[test]
    fn layout_mode_parses_every_value_without_zones() {
        for (text, mode) in [
            ("auto", LayoutMode::Auto),
            ("panes", LayoutMode::Panes),
            ("windows", LayoutMode::Windows),
            ("focus", LayoutMode::Focus),
        ] {
            let layout: crate::team::config::LayoutConfig =
                serde_yaml::from_str(&format!("mode: {text}\n")).unwrap();
            assert_eq!(layout.mode, mode);
            assert!(layout.zones.is_empty());
        }
        assert!(serde_yaml::from_str::<crate::team::config::LayoutConfig>("mode: grid\n").is_err());
    }

    #[test]
    fn parses_stty_size() {
        assert_eq!(parse_stty_size("50 220\n"), Some((220, 50)));
        assert_eq!(parse_stty_size("0 0"), None);
        assert_eq!(parse_stty_size("x"), None);
    }

    #[test]
    fn parses_tmux_versions() {
        assert_eq!(parse_tmux_version("tmux 3.4\n"), v(3, 4));
        assert_eq!(parse_tmux_version("tmux 3.3a"), v(3, 3));
        assert_eq!(parse_tmux_version("tmux next-3.5"), v(3, 5));
        assert_eq!(parse_tmux_version("tmux 2"), v(2, 0));
        assert_eq!(parse_tmux_version("garbage"), None);
        assert!(v(3, 2).unwrap().at_least(3, 2));
        assert!(!v(2, 9).unwrap().at_least(3, 0));
    }

    #[test]
    fn window_names_are_safe_unique_and_ordered() {
        let names = window_names(["architect", "eng 1.2", "eng-1.2", "dashboard", "a:b"]);
        assert_eq!(
            names,
            vec!["architect", "eng-1-2", "eng-1-2-2", "dashboard-2", "a-b"]
        );
        assert_eq!(window_name(""), "member");
    }

    #[test]
    fn dashboard_command_switches_on_watch_support() {
        let watch = dashboard_command("/bin/batty", "/p/it's", true);
        assert!(watch.contains("status --watch"));
        assert!(watch.contains("'/p/it'\\''s'"));
        let looped = dashboard_command("/bin/batty", "/p", false);
        assert!(looped.contains("while true"));
        assert!(looped.contains("sleep 5"));
        assert!(!looped.contains("--watch"));
    }

    #[test]
    fn session_options_are_session_scoped_and_versioned() {
        let new = session_option_commands("batty-x", v(3, 4), ResolvedMode::Windows, true);
        assert!(new.iter().all(|c| c[2] == "-t" && c[3] == "batty-x"));
        assert!(new.iter().all(|c| c[0] == "set-option" && c[1] == "-q"));
        let has = |cmds: &Vec<Vec<String>>, name: &str, value: &str| {
            cmds.iter().any(|c| c[4] == name && c[5] == value)
        };
        assert!(has(&new, "mouse", "on"));
        assert!(has(&new, "base-index", "1"));
        assert!(has(&new, "renumber-windows", "on"));
        assert!(has(&new, "status", "2"));
        assert!(has(&new, "window-size", "latest"));
        assert!(has(&new, "history-limit", "20000"));
        assert!(
            new.iter()
                .any(|c| c[4] == "status-format[1]" && c[5].contains("F1 help"))
        );

        assert!(has(&new, "key-table", "batty-x"));
        let nokeys = session_option_commands("batty-x", v(3, 4), ResolvedMode::Windows, false);
        assert!(!nokeys.iter().any(|c| c[4] == "key-table"));
        assert!(
            nokeys
                .iter()
                .any(|c| c[4] == "status-format[1]" && !c[5].contains("F1"))
        );
        let old = session_option_commands("batty-x", v(2, 6), ResolvedMode::Windows, true);
        assert!(!old.iter().any(|c| c[4] == "key-table"));
        assert!(!has(&old, "status", "2"));
        assert!(!has(&old, "window-size", "latest"));
        assert!(
            old.iter()
                .any(|c| c[4] == "status-right" && c[5].contains("Click a tab"))
        );
    }

    #[test]
    fn window_options_use_window_scope() {
        let cmds = window_option_commands("batty-x:3");
        assert!(cmds.iter().all(|c| c[1] == "-wq" && c[3] == "batty-x:3"));
        assert!(
            cmds.iter()
                .any(|c| c[4] == "aggressive-resize" && c[5] == "on")
        );
        assert!(cmds.iter().any(|c| c[4] == "allow-rename" && c[5] == "off"));
    }

    #[test]
    fn pane_border_names_state_without_relying_on_colour() {
        assert!(PANE_BORDER_FORMAT.contains("@batty_role"));
        assert!(PANE_BORDER_FORMAT.contains("@batty_status"));
        assert!(PANE_BORDER_FORMAT.contains("pane_active"));
    }

    #[test]
    fn key_bindings_use_session_table_and_are_numbered() {
        let jumps = vec![JumpTarget::Window(1), JumpTarget::Window(2)];
        let cmds = key_binding_commands(
            "batty-x",
            &jumps,
            ResolvedMode::Windows,
            Path::new("/p/help.txt"),
            v(3, 4),
        );
        // Table is cleared first so restarts do not accumulate bindings.
        assert_eq!(cmds[0], vec!["unbind-key", "-a", "-T", "batty-x"]);
        // Never the root or prefix tables.
        assert!(
            cmds[1..]
                .iter()
                .all(|c| c[0] == "bind-key" && c[1] == "-T" && c[2] == "batty-x")
        );
        let f2 = cmds.iter().find(|c| c[3] == "F2").unwrap();
        assert_eq!(f2[4], "select-window -t :1");
        assert!(
            cmds.iter()
                .any(|c| c[3] == "F3" && c[4] == "select-window -t :2")
        );
        assert!(!cmds.iter().any(|c| c[3] == "F4"));
        let f12 = cmds.iter().find(|c| c[3] == "F12").unwrap();
        assert_eq!(f12[4], "resize-pane -Z");
        let f1 = cmds.iter().find(|c| c[3] == "F1").unwrap();
        assert!(f1[4].starts_with("display-popup"));
        assert!(!cmds.iter().any(|c| c.iter().any(|a| a == "?")));
    }

    #[test]
    fn help_falls_back_to_message_before_3_2() {
        let f = |ver| key_binding_commands("s", &[], ResolvedMode::Panes, Path::new("/h"), ver);
        let cmds = f(v(3, 0));
        let f1 = cmds.iter().find(|c| c[3] == "F1").unwrap();
        assert!(f1[4].starts_with("display-message"));
        assert!(f(v(2, 8)).is_empty());
        assert!(f(None).is_empty());
    }

    #[test]
    fn pane_and_focus_jumps() {
        let jumps = vec![JumpTarget::Pane("%7".into())];
        let f = |mode| key_binding_commands("s", &jumps, mode, Path::new("/h"), v(3, 4));
        let panes = f(ResolvedMode::Panes);
        assert!(
            panes
                .iter()
                .any(|c| c[3] == "F2" && c[4] == "select-pane -t %7")
        );
        assert!(!panes.iter().any(|c| c[3] == "F11"));
        let focus = f(ResolvedMode::Focus);
        assert!(
            focus
                .iter()
                .any(|c| c[3] == "F2" && c[4].starts_with("swap-pane -s %7"))
        );
        assert!(
            focus
                .iter()
                .any(|c| c[3] == "F11" && c[4].starts_with("rotate-window"))
        );
    }

    #[test]
    fn focus_layout_uses_main_vertical() {
        let cmds = focus_layout_commands("batty-x:1", v(3, 4));
        assert!(cmds.iter().any(|c| c.last().unwrap() == "main-vertical"));
        assert!(
            cmds.iter()
                .any(|c| c.get(4).is_some_and(|x| x == "main-pane-width"))
        );
        let old = focus_layout_commands("batty-x:1", v(3, 0));
        assert!(
            !old.iter()
                .any(|c| c.get(4).is_some_and(|x| x == "main-pane-width"))
        );
    }

    #[test]
    fn help_text_lists_keys_and_members() {
        let t = help_text(ResolvedMode::Windows, &["architect".into(), "eng-1".into()]);
        assert!(t.contains("F2   architect"));
        assert!(t.contains("F3   eng-1"));
        assert!(t.contains("Ctrl-b d"));
        assert!(t.contains("batty attach"));
    }

    #[test]
    fn hint_line_mentions_core_keys() {
        let off = hint_line(v(3, 4), ResolvedMode::Windows, false);
        assert!(!off.contains("F1") && off.contains("Ctrl-b d to detach"));
        let h = hint_line(v(3, 4), ResolvedMode::Windows, true);
        assert!(h.contains("Click a tab"));
        assert!(h.contains("F1 help"));
        assert!(h.contains("Ctrl-b d to detach"));
    }
}
