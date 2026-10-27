//! Pure builders for the tmux accessibility layer of a batty session.
//!
//! Everything here returns tmux argv vectors (or strings) so it can be unit
//! tested without a tmux server. All options are session- or window-scoped;
//! nothing touches `~/.tmux.conf` or global options.
//!
//! Key bindings are server-wide in tmux. Batty binds its F-keys in the root
//! table, and each binding dispatches on a session option
//! (`@batty_key_F2`, ...): a session that set the option runs its own action,
//! every other session gets the key passed through to the pane. No custom
//! `key-table` is used, so tmux's mouse bindings and the user's own `bind -n`
//! keys keep working, and several batty sessions never overwrite each other.

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
///
/// Reads the controlling terminal (`/dev/tty`) first, so a start with
/// redirected stdin still sees the real size; then stdin; then
/// [`DEFAULT_COLS`]x[`DEFAULT_ROWS`].
pub fn terminal_size() -> (u16, u16) {
    pick_size(
        || {
            std::fs::File::open("/dev/tty")
                .ok()
                .and_then(|tty| stty_size(std::process::Stdio::from(tty)))
        },
        || stty_size(std::process::Stdio::inherit()),
    )
}

/// First size found: controlling terminal, then stdin, then the default.
pub fn pick_size(
    tty: impl FnOnce() -> Option<(u16, u16)>,
    stdin: impl FnOnce() -> Option<(u16, u16)>,
) -> (u16, u16) {
    tty().or_else(stdin).unwrap_or((DEFAULT_COLS, DEFAULT_ROWS))
}

/// Run `stty size` with the given stdin; `None` when it is not a terminal.
fn stty_size(stdin: std::process::Stdio) -> Option<(u16, u16)> {
    std::process::Command::new("stty")
        .arg("size")
        .stdin(stdin)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| parse_stty_size(&String::from_utf8_lossy(&o.stdout)))
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

impl ResolvedMode {
    /// Value stored in the `@batty_layout` session option.
    pub fn as_str(self) -> &'static str {
        match self {
            ResolvedMode::Panes => "panes",
            ResolvedMode::Windows => "windows",
            ResolvedMode::Focus => "focus",
        }
    }

    /// Parse the `@batty_layout` session option (unknown means panes).
    pub fn from_option(value: &str) -> ResolvedMode {
        match value.trim() {
            "windows" => ResolvedMode::Windows,
            "focus" => ResolvedMode::Focus,
            _ => ResolvedMode::Panes,
        }
    }
}

/// Session option recording the resolved layout, read by runtime member adds.
pub const LAYOUT_OPTION: &str = "@batty_layout";

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

/// A window name for `member` that is not already in `existing`.
pub fn unique_window_name(member: &str, existing: &[String]) -> String {
    let base = window_name(member);
    let taken = |name: &str| name == "dashboard" || existing.iter().any(|e| e == name);
    let mut candidate = base.clone();
    let mut n = 2;
    while taken(&candidate) {
        candidate = format!("{base}-{n}");
        n += 1;
    }
    candidate
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

/// Batty keys need `run-shell -C` and `display-popup` (tmux 3.2+).
pub fn keys_supported(version: Option<TmuxVersion>) -> bool {
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
///
/// `status_format0` is the server's global `status-format[0]`. Setting
/// `status-format[1]` on the session gives it its own array, which would
/// otherwise leave line 0 (the window tabs) empty, so line 0 is copied in.
pub fn session_option_commands(
    session: &str,
    version: Option<TmuxVersion>,
    mode: ResolvedMode,
    keys: bool,
    status_format0: Option<&str>,
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
        s("display-time", "4000"),
        s(LAYOUT_OPTION, mode.as_str()),
    ];
    let hint = hint_line(version, mode, keys);
    let two_lines = version.is_some_and(|v| v.at_least(2, 9));
    if let Some(line0) = status_format0.filter(|f| two_lines && !f.trim().is_empty()) {
        cmds.push(s("status", "2"));
        cmds.push(s("status-format[0]", line0));
        cmds.push(s("status-right", " #{session_name} "));
        cmds.push(s(
            "status-format[1]",
            &format!("#[align=centre,fg=colour250] {hint} "),
        ));
    } else {
        // No two-line status: keep tmux's own line and put the hint right.
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
        w("window-status-format", " #I:#W "),
        w(
            "window-status-current-format",
            "#[bg=colour208,fg=colour16,bold] [#I:#W] #[default]",
        ),
        w("window-status-separator", ""),
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

/// F-key for jump slot `i` (0-based): F2..F10.
pub fn jump_key(i: usize) -> String {
    format!("F{}", i + 2)
}

/// Every key batty may bind in the root table.
pub fn owned_keys() -> Vec<String> {
    (1..=12).map(|n| format!("F{n}")).collect()
}

/// Session option holding `key`'s action for one session.
pub fn key_option(key: &str) -> String {
    format!("{}{key}", crate::tmux::BATTY_KEY_OPTION_PREFIX)
}

/// The shared root-table binding for `key`.
///
/// It runs the action stored in the current session's `@batty_key_<key>`
/// option, and sends the key on to the pane when that option is unset (any
/// non-batty session, or a key this session does not use).
pub fn root_binding(key: &str) -> Vec<String> {
    let opt = key_option(key);
    vec![
        "bind-key".into(),
        "-T".into(),
        "root".into(),
        key.into(),
        "if-shell".into(),
        "-F".into(),
        format!("#{{{opt}}}"),
        format!("run-shell -C '#{{{opt}}}'"),
        format!("send-keys {key}"),
    ]
}

/// Per-session key actions as `(key, tmux command)` pairs.
pub fn key_actions(
    jumps: &[JumpTarget],
    mode: ResolvedMode,
    help_file: &Path,
) -> Vec<(String, String)> {
    let mut actions = vec![
        ("F1".to_string(), help_action(help_file)),
        ("F12".to_string(), "resize-pane -Z".to_string()),
    ];
    for (i, target) in jumps.iter().take(MAX_JUMP_KEYS).enumerate() {
        let action = match (target, mode) {
            (JumpTarget::Window(n), _) => format!("select-window -t :{n}"),
            (JumpTarget::Pane(id), ResolvedMode::Focus) => {
                format!("swap-pane -s {id} -t :.1 ; select-pane -t :.1")
            }
            (JumpTarget::Pane(id), _) => format!("select-pane -t {id}"),
        };
        actions.push((jump_key(i), action));
    }
    if mode == ResolvedMode::Focus {
        actions.push((
            "F11".to_string(),
            "rotate-window -U ; select-pane -t :.1".to_string(),
        ));
    }
    actions
}

/// Commands that install batty's keys for one session.
///
/// Sets the session's `@batty_keys` marker and one `@batty_key_<key>` option
/// per key it uses (unsetting the rest, so a refresh drops stale jumps), then
/// binds each owned key in the root table. `root_listing` is the current
/// `list-keys -T root` output: a key the user already bound there is left
/// alone and is simply not a batty key. Returns nothing when keys are
/// unsupported (tmux older than 3.2).
pub fn key_binding_commands(
    session: &str,
    jumps: &[JumpTarget],
    mode: ResolvedMode,
    help_file: &Path,
    version: Option<TmuxVersion>,
    root_listing: &str,
) -> Vec<Vec<String>> {
    use crate::tmux::{RootKeyOwner, root_key_owner};
    if !keys_supported(version) {
        return Vec::new();
    }
    let actions = key_actions(jumps, mode, help_file);
    let mut cmds = vec![set(session, "-q", crate::tmux::BATTY_KEYS_OPTION, "1")];
    for key in owned_keys() {
        let option = key_option(&key);
        match actions.iter().find(|(k, _)| *k == key) {
            Some((_, action)) => cmds.push(set(session, "-q", &option, action)),
            None => cmds.push(vec![
                "set-option".into(),
                "-qu".into(),
                "-t".into(),
                session.into(),
                option,
            ]),
        }
    }
    for key in owned_keys() {
        if root_key_owner(root_listing, &key) != RootKeyOwner::Other {
            cmds.push(root_binding(&key));
        }
    }
    cmds
}

fn help_action(help_file: &Path) -> String {
    let file = sh_quote(&help_file.to_string_lossy());
    let script = format!("cat {file}; printf '\\nPress Enter to close'; read _");
    format!(
        "display-popup -E -w 80 -h 30 {}",
        tmux_quote(&format!("sh -c {}", sh_quote(&script)))
    )
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
    fn layout_defaults_to_auto_with_keys() {
        let layout: crate::team::config::LayoutConfig =
            serde_yaml::from_str("zones:\n- name: architect\n  width_pct: 100\n").unwrap();
        assert_eq!(layout.mode, LayoutMode::Auto);
        assert_eq!(
            crate::team::config::LayoutConfig::default().mode,
            LayoutMode::Auto
        );
        assert_eq!(layout.zones.len(), 1);
        assert!(layout.keys);
        assert!(crate::team::config::LayoutConfig::default().keys);
        let off: crate::team::config::LayoutConfig = serde_yaml::from_str("keys: false\n").unwrap();
        assert!(!off.keys);
        // Existing zones keep their tiled layout under `auto`.
        assert_eq!(
            resolve_mode(layout.mode, 9, !layout.zones.is_empty(), 80, 24),
            ResolvedMode::Panes
        );
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
        let new =
            session_option_commands("batty-x", v(3, 4), ResolvedMode::Windows, true, Some("F0"));
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

        // Line 0 (window tabs) is kept when line 1 becomes the hint.
        assert!(has(&new, "status-format[0]", "F0"));
        let unknown =
            session_option_commands("batty-x", v(3, 4), ResolvedMode::Windows, true, None);
        assert!(!unknown.iter().any(|c| c[4].starts_with("status-format")));
        assert!(
            unknown
                .iter()
                .any(|c| c[4] == "status-right" && c[5].contains("Click a tab"))
        );
        // N2: no custom key table, which would hide root and mouse bindings.
        assert!(!new.iter().any(|c| c[4] == "key-table"));
        assert!(has(&new, LAYOUT_OPTION, "windows"));
        let nokeys =
            session_option_commands("batty-x", v(3, 4), ResolvedMode::Windows, false, Some("F0"));
        assert!(
            nokeys
                .iter()
                .any(|c| c[4] == "status-format[1]" && !c[5].contains("F1"))
        );
        let old =
            session_option_commands("batty-x", v(2, 6), ResolvedMode::Windows, true, Some("F0"));
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
        // Tab formats are window options: set on every window, not just one.
        assert!(cmds.iter().any(|c| c[4] == "window-status-format"));
    }

    #[test]
    fn pane_border_names_state_without_relying_on_colour() {
        assert!(PANE_BORDER_FORMAT.contains("@batty_role"));
        assert!(PANE_BORDER_FORMAT.contains("@batty_status"));
        assert!(PANE_BORDER_FORMAT.contains("pane_active"));
    }

    fn option_value<'a>(cmds: &'a [Vec<String>], name: &str) -> Option<&'a str> {
        cmds.iter()
            .find(|c| c[0] == "set-option" && c[1] == "-q" && c[4] == name)
            .map(|c| c[5].as_str())
    }

    #[test]
    fn key_bindings_dispatch_from_root_on_session_options() {
        let jumps = vec![JumpTarget::Window(1), JumpTarget::Window(2)];
        let cmds = key_binding_commands(
            "batty-x",
            &jumps,
            ResolvedMode::Windows,
            Path::new("/p/help.txt"),
            v(3, 4),
            "",
        );
        // Actions live in the session's own options.
        assert_eq!(option_value(&cmds, "@batty_keys"), Some("1"));
        assert_eq!(
            option_value(&cmds, "@batty_key_F2"),
            Some("select-window -t :1")
        );
        assert_eq!(
            option_value(&cmds, "@batty_key_F3"),
            Some("select-window -t :2")
        );
        assert_eq!(
            option_value(&cmds, "@batty_key_F12"),
            Some("resize-pane -Z")
        );
        assert!(
            option_value(&cmds, "@batty_key_F1")
                .unwrap()
                .starts_with("display-popup")
        );
        // Unused keys are unset so a refresh drops stale jumps.
        assert!(
            cmds.iter()
                .any(|c| c[1] == "-qu" && c[4] == "@batty_key_F4")
        );
        assert!(option_value(&cmds, "@batty_key_F4").is_none());
        // Every set targets this session only.
        assert!(
            cmds.iter()
                .filter(|c| c[0] == "set-option")
                .all(|c| c[2] == "-t" && c[3] == "batty-x")
        );
        // Root bindings are identical for every session and pass the key on.
        let binds: Vec<_> = cmds.iter().filter(|c| c[0] == "bind-key").collect();
        assert_eq!(binds.len(), 12);
        assert!(binds.iter().all(|c| c[1] == "-T" && c[2] == "root"));
        let f2 = binds.iter().find(|c| c[3] == "F2").unwrap();
        assert_eq!(
            f2[4..],
            [
                "if-shell",
                "-F",
                "#{@batty_key_F2}",
                "run-shell -C '#{@batty_key_F2}'",
                "send-keys F2"
            ]
        );
        assert!(!cmds.iter().flatten().any(|a| a == "key-table" || a == "?"));
        assert!(!cmds.iter().any(|c| c[0] == "unbind-key"));
    }

    #[test]
    fn key_bindings_leave_user_root_keys_alone() {
        let listing = "bind-key -T root F5 display-message mine\n\
                       bind-key -T root F2 if-shell -F \"#{@batty_key_F2}\" x y\n";
        let cmds = key_binding_commands(
            "s",
            &[JumpTarget::Window(1)],
            ResolvedMode::Windows,
            Path::new("/h"),
            v(3, 4),
            listing,
        );
        let bound: Vec<&str> = cmds
            .iter()
            .filter(|c| c[0] == "bind-key")
            .map(|c| c[3].as_str())
            .collect();
        assert!(!bound.contains(&"F5"), "user's F5 is not overwritten");
        assert!(bound.contains(&"F2"), "batty's own binding is refreshed");
        assert_eq!(bound.len(), 11);
    }

    #[test]
    fn keys_need_tmux_3_2() {
        let f = |ver| key_binding_commands("s", &[], ResolvedMode::Panes, Path::new("/h"), ver, "");
        assert!(f(v(3, 1)).is_empty());
        assert!(f(None).is_empty());
        let cmds = f(v(3, 2));
        assert!(
            option_value(&cmds, "@batty_key_F1")
                .unwrap()
                .starts_with("display-popup")
        );
        assert!(!hint_line(v(3, 1), ResolvedMode::Panes, true).contains("F1"));
        assert!(hint_line(v(3, 2), ResolvedMode::Panes, true).contains("F1"));
    }

    #[test]
    fn pane_and_focus_jumps() {
        let jumps = vec![JumpTarget::Pane("%7".into())];
        let f = |mode| key_binding_commands("s", &jumps, mode, Path::new("/h"), v(3, 4), "");
        let panes = f(ResolvedMode::Panes);
        assert_eq!(
            option_value(&panes, "@batty_key_F2"),
            Some("select-pane -t %7")
        );
        assert!(option_value(&panes, "@batty_key_F11").is_none());
        let focus = f(ResolvedMode::Focus);
        assert!(
            option_value(&focus, "@batty_key_F2")
                .unwrap()
                .starts_with("swap-pane -s %7")
        );
        assert!(
            option_value(&focus, "@batty_key_F11")
                .unwrap()
                .starts_with("rotate-window")
        );
    }

    #[test]
    fn help_popup_quotes_the_path() {
        let actions = key_actions(&[], ResolvedMode::Panes, Path::new("/p/it's #1/help.txt"));
        let f1 = &actions.iter().find(|(k, _)| k == "F1").unwrap().1;
        assert!(f1.starts_with("display-popup -E"));
        assert!(
            f1.contains("##1"),
            "# is escaped for format expansion: {f1}"
        );
    }

    #[test]
    fn resolved_mode_round_trips_through_session_option() {
        for mode in [
            ResolvedMode::Panes,
            ResolvedMode::Windows,
            ResolvedMode::Focus,
        ] {
            assert_eq!(ResolvedMode::from_option(mode.as_str()), mode);
        }
        assert_eq!(ResolvedMode::from_option(""), ResolvedMode::Panes);
        assert_eq!(
            ResolvedMode::from_option("windows\n"),
            ResolvedMode::Windows
        );
    }

    #[test]
    fn unique_window_name_avoids_existing_windows() {
        let existing = vec!["dashboard".to_string(), "eng-1".into(), "eng-1-2".into()];
        assert_eq!(unique_window_name("eng-1", &existing), "eng-1-3");
        assert_eq!(unique_window_name("eng.2", &existing), "eng-2");
        assert_eq!(unique_window_name("dashboard", &[]), "dashboard-2");
    }

    #[test]
    fn terminal_size_prefers_tty_then_stdin_then_default() {
        assert_eq!(pick_size(|| Some((100, 30)), || Some((80, 24))), (100, 30));
        assert_eq!(pick_size(|| None, || Some((80, 24))), (80, 24));
        assert_eq!(pick_size(|| None, || None), (DEFAULT_COLS, DEFAULT_ROWS));
        // The stdin probe only runs when the tty probe fails.
        let mut called = false;
        pick_size(
            || Some((1, 1)),
            || {
                called = true;
                None
            },
        );
        assert!(!called);
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
