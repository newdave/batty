# tmux layouts for non-experts

Batty runs each team member in tmux. This page compares the ways to lay
members out, explains the accessibility settings Batty applies to its own
session, and says which default to use and why.

Batty only touches its own session (`batty-<team>`). Your `~/.tmux.conf` and
your other tmux sessions are never changed.

## The problem with tiled panes

Putting every member in one window works for a handful of agents. With the
default template (architect, manager, 2 engineers, researcher, docs-writer,
qa-tester) a 220x50 terminal splits into 7+ panes only a few lines tall. Text
wraps, the agent's prompt scrolls out of view, and a newcomer has to know tmux
keys (prefix, pane switching, zoom, copy mode, detach) to do anything.

## Options

### (a) Tiled panes (`panes`)

```text
+-----------+-----------+-----------+
| architect | eng-1-1   | eng-1-2   |
+-----------+-----------+-----------+
| manager   | research  | docs      |
+-----------+-----------+-----------+
```

- Pro: see everyone at once. Good for 2-4 members on a large screen.
- Con: each pane shrinks as the team grows; unreadable past about 4.
- Needs: any tmux.

### (b) One window per member plus a dashboard (`windows`)

```text
 batty | [1:dashboard] 2:architect 3:manager 4:eng-1-1 5:eng-1-2 ...
+------------------------------------------------------------------+
| (the selected member fills the whole window)                     |
+------------------------------------------------------------------+
 Click a tab or F2-F10 to jump | F12 zoom | F1 help | Ctrl-b d detach
```

- Pro: every member gets the full terminal. The status bar is a clickable
  tab list, which needs no tmux knowledge. Window 1 is a live `batty status`
  dashboard, so you still see the whole team at a glance.
- Con: you only see one member at a time.
- Needs: tmux 2.9+ for the two-line status bar, 3.0+ for F-keys, 3.2+ for the
  help popup. Older versions fall back to a one-line bar and `display-message`.

### (c) Focus layout (`focus`)

```text
+-------------------------------+-----------+
|                               | eng-1-1   |
|  architect (big)              +-----------+
|                               | eng-1-2   |
|                               +-----------+
|                               | research  |
+-------------------------------+-----------+
```

- Pro: one readable pane plus a glance at the others. F2-F10 swap a member
  into the big pane, F11 cycles.
- Con: the narrow pane is too small for more than a few lines; with many
  members the side column is crowded.
- Needs: tmux 3.1+ for the 65% main pane width; older versions use tmux's
  default width.

### (d) Grouped windows (one window per role group)

One window for leadership (architect + manager), one for engineers, one for
support roles, each tiled inside.

- Pro: fewer tabs.
- Con: engineer groups of 4+ are small again, and the grouping is one more
  thing to configure. Not implemented; use `layout.zones` with `panes` if you
  want a hand-built version.

## Comparison

| Option | Readable at 8+ members | Needs tmux knowledge | See all at once |
| --- | --- | --- | --- |
| panes | no | medium | yes |
| windows | yes | low (click tabs) | via dashboard |
| focus | partly | low (F-keys) | partly |
| grouped | partly | low | partly |

## Recommendation

> **Current default:** `panes` with `keys: false`, matching upstream. `auto`
> and the F-keys are opt-in until the batty key table stops disabling tmux's
> mouse and root key bindings. The recommendation below is the intended
> default once that is fixed.

Recommended default, `auto`:

- Tile panes only when they stay usable: at most 4 agent panes (including the
  orchestrator pane) and a terminal that gives each pane at least 50 columns
  by 12 rows in a two-column grid. The size comes from the terminal running
  `batty start` (220x50 when there is no terminal).
- Otherwise use `windows`.
- If `layout.zones` is set and `mode` is omitted, tiled panes are kept. A
  hand-written layout is an explicit choice.

`windows` is the right default for beginners: it needs only the mouse, never
shrinks below the terminal size, and the dashboard replaces the "see
everything" benefit of tiling.

## Accessibility settings Batty applies

All are session- or window-scoped.

| Setting | Why |
| --- | --- |
| `mouse on` | click tabs and panes, wheel scrolls history |
| two-line status bar | clickable window tabs, plus a permanent key hint |
| `base-index 1`, `pane-base-index 1` | tabs match the keyboard (1 is first) |
| `renumber-windows on` | no gaps after a window closes |
| `aggressive-resize on`, `window-size latest` | fits the window to the client you are using |
| `history-limit 20000` | scrollback survives long agent output |
| `automatic-rename off`, `allow-rename off` | agents cannot rename tabs |
| F1 | help popup with keys and batty commands (`Ctrl-b ?` stays tmux's own list) |
| F2-F10 | jump to the dashboard/members |
| F12 | zoom the current pane to full screen |
| F11 (focus mode) | cycle which member is in the big pane |

Key bindings live in a key table named after the session (for example
`batty-myteam`), selected with the session-scoped `key-table` option. They are
never added to the root or prefix tables, so other tmux sessions on the same
server are unaffected. The prefix key (`Ctrl-b`) and every key without a Batty
binding behave normally, because tmux falls back to the usual tables (checked
on tmux 3.8). The table is cleared and rebuilt on each `batty start`, so
restarts never accumulate bindings. Set `layout.keys: false` to skip the key
bindings and the `key-table` option entirely (tabs and mouse still work).

Colours follow the Okabe-Ito colour-blind-safe set (blue and orange on a
dark bar). Meaning is never carried by colour alone: the active tab is
wrapped in brackets, the active pane title starts with `>`, and pane titles
spell out the state (`idle`, `working`, `PAUSED`).

The key hint line reads:

```text
Click a tab or F2-F10 jump to a member | F12 zoom | F1 help | scroll with mouse | Ctrl-b d to detach
```

Minimum sizes: panes below 50x12 are considered unreadable. If you force
`mode: panes` with many members, expect small panes; use F12 to zoom.

## Configuration

```yaml
layout:
  mode: panes  # panes (default) | auto | windows | focus
  keys: false  # true installs F-key bindings (batty-only key table; disables mouse bindings for now)
```

`zones` (see the config reference) still applies to `panes` mode. Existing
configs without `mode` keep working.

## Jumping from the shell

```bash
batty attach            # attach to the session
batty attach eng-1-1    # attach and select that member's window/pane
```

## Version requirements

| Feature | tmux |
| --- | --- |
| tiled panes, windows, mouse, tabs | any recent version |
| two-line status bar (hint line) | 2.9 |
| F-key bindings, `Ctrl-b ?` help | 3.0 |
| `window-size latest` | 3.1 |
| focus layout 65% main pane | 3.1 |
| help popup | 3.2 (falls back to `display-message`) |

Batty detects the version with `tmux -V` and skips features that are not
available; unavailable options never stop startup.

## Not covered

- Alt+number keys: many macOS terminals send Alt as an accent character, so
  F-keys are used instead. On laptops, F-keys may need the Fn key.
- Panes created by hand in an existing window do not inherit the window
  options (pane titles, aggressive-resize). Use `batty attach` and the tabs.
