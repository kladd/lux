# News

## 2026-09-03

- Configuration `rule-style` draws the tab bar rule as a dash (default) or
  a braille dot line.
- The tab bar rule's working shimmer runs at the status text's pace rather
  than the tab's output rate.
- `:config-open` edits the config file in a new `$EDITOR` tab (vim fallback),
  and `:config-reload` applies it to every running session.
- Updated dependencies.

## 2026-09-02

- Middle-clicking a tab indicator closes that tab.
- Switcher entries shimmer or breathe in the color of the session's most
  pressing agent tab: blocked, then done, then working.
- Configuration `osc-titles` (`none`, `agents`, or `all`; default `agents`)
  controls which tabs take their name from the program's preferred title.
- A tab that produces output while off screen gets a dot on its indicator;
  a bell turns it green, and selecting the tab clears it.
- The focused tab bar's rule fills like a progress bar when the active tab
  reports progress, and the working shimmer moves at the tab's output rate,
  standing still when output stops.
- `prefix+Y` copies the last completed command's output to the clipboard.
- Scroll mode shows a scrollbar along the content's right edge.
- Strikethrough, blink, hidden text, and colored underlines now render.
- Configuration `dim-unfocused` darkens every window but the focused one, tab
  bar and its rule animations included, and config `shadows` gives popovers a
  drop shadow; both default off.

## 2026-09-01

- Codex tabs no longer report blocked from prompts left in scrollback.
- Claude MCP dialogs now read as blocked.
- Text typed into Claude or Codex input boxes no longer affects a tab's agent
  status.

## 2026-08-27

- `prefix+y` yanks a tab and `prefix+P` pastes it into any session's
  focused window; `prefix+Escape` cancels.
- Hovering any clickable element shows a hand pointer: tab indicators, minimized
  titles, the pending-agent indicator, the menu icon, and switcher entries.

## 2026-08-24

- Tab names too long for the tab bar are truncated with an ellipsis;
  names that fit keep their full length.
- Fixed a Claude Code tab staying working after its background agents
  finished.

## 2026-08-20

- Tabs not renamed by hand now take their name from the program's OSC
  window title when it sets one, falling back to the process name.
- Drag selections now copy to the clipboard on release, with a status
  message reporting the characters copied (`copy-on-select = false` to
  opt out).
- `:w` confirms a successful write with a status message reporting the
  path and byte count.
- Fixed Claude Code working detection on 2.1.228+: the new title
  spinner, the on-screen spinner line, and background shells/agents
  now all count as working.

## 2026-08-13

- CLI verbs resolve like tmux: full names, aliases, and unambiguous
  prefixes (e.g. `att`, `new-s`, `list-sessions`).

## 2026-08-12

- Improvements to session resume
- Allow clipboard paste in text prompts

## 2026-08-11

- A status-line menu icon toggles the session switcher.
- Kiro CLI support
- Fixed a false blocked state when a typed Claude Code message starts
  with "1."
- `prefix+prefix` forwards a literal prefix key press to the focused
  tab, repeating on a quick second tap.
- `prefix+w` closes the focused window's active tab.
- Switcher entries can be selected by mouse click.

## 2026-08-02

- New config-gated auto mode (`automode = true`): the CLAUDECOM entry points
  present one done-or-blocked agent tab at a time instead of the grid,
  handing off to the next automatically, with a fallback screen listing
  the agents still working when nothing needs attention.

## 2026-07-25

- `prefix+Tab` now jumps to the next agent tab that is done or blocked,
  across every session.
- A detected agent's bracketed status now carries how long the agent has
  been in that state (e.g. `[working 4m12s]`).

## 2026-07-24

- Codex support

## 2026-07-23

- The session status line now shows a shimmering indicator in place of
  the hostname whenever a Claude Code tab the user isn't looking at is
  done or blocked — the first such tab in CLAUDECOM order — and clicking
  it jumps straight to that tab, restoring and maximizing its window if
  it had been minimized.

## 2026-07-21

- Each window's tab bar now carries clickable minimize (`−`),
  maximize/restore (`□`/`❐`), and exit (`×`) controls at its right edge:
  minimize parks the window (processes still running) as a clickable
  title in the session status line that restores it by splitting the
  focused window, maximize toggles the zoom `prefix+z` provides, and
  exit closes the window like `prefix+x`; hovering a control brightens
  it and shows a hand pointer (in terminals that support pointer
  shapes).

## 2026-07-19

- The focused window's tab bar rule now animates in its active tab's
  status color.
- Tabs running Codex are now detected and their state (working, blocked,
  idle) classified from Codex's window title and on-screen prompts.

## 2026-07-16

- Window boundaries can now be resized by dragging them with the mouse —
  grab a vertical separator or a lower window's tab bar row — and the
  mouse pointer shows a matching resize shape when hovering a draggable
  boundary (in terminals that support pointer shapes).

## 2026-07-13

- Added a fuzzy tab finder (`prefix+f`): a bordered popover floating over
  your session narrows every tab across every session by name as you
  type, with a live preview of the highlighted match; Enter jumps to that
  tab, Ctrl-p/Ctrl-n or the arrows move the highlight.
- CLAUDECOM tiles are now a fixed 24 rows tall, widen evenly to fill the
  screen, and carry borders colored (and animated) by each tab's status,
  with a double-line border marking the highlight.
- CLAUDECOM tiles and the finder's preview resize the shown tab to fit,
  so its content reflows legibly instead of showing a crop of the
  full-size layout; a tab snaps back to its real size when viewed in its
  home window.
- Enter on a CLAUDECOM tile captures it for typing into the tab in place,
  marked with a `capture` label; the prefix key always leads a command
  there — `prefix+g` or `prefix+Esc` returns to the grid — and never
  reaches the tab.
- Leaving CLAUDECOM: Escape/`q` returns to your session, `g` jumps to the
  highlighted tile's tab in its home session, and `prefix+s`/`prefix+f`
  open the switcher or finder directly, from navigation or capture mode.
- Added `:new`/`:new-session [name]` ex commands to create and attach a
  session from inside lux; a name already in use is silently ignored.

## 2026-07-12

- Added desktop notifications when a Claude Code tab in any session
  finishes or needs input, delivered to your terminal via OSC 9;
  disable with `notify = false` in the config.

## 2026-07-11

- Added CLAUDECOM, a live overview of every Claude Code tab across
  sessions, reachable from the switcher or `prefix+g`.
- `-s`/`-t` and bare `attach`/`new` now attach-or-create a session by name
  instead of erroring on a missing or duplicate one.
- `prefix+m` plus a direction key now swaps the focused window with its
  spatially adjacent neighbor, replacing the old split-mirroring behavior.
- Multi-line pastes are delivered as a single bracketed paste instead of a
  stream of keystrokes, fixing per-line submission, auto-indent mangling,
  and leaked marker fragments.
- Copies made by programs running inside lux (OSC 52, e.g. Claude Code's
  highlight-copy or helix's clipboard yank) now reach the system clipboard
  and the client terminal.
- Shift+click bypasses a program's mouse grab, so selection, yank, and
  right-click paste work inside mouse-aware programs like helix.
- Fixed session persistence resuming the same Claude Code session in every
  tab instead of each tab's own.

## 2026-07-10

- Added window maximize (`prefix+z`), rotate (`prefix+i`), and repeatable
  move-tab (`prefix+H`/`J`/`K`/`L`); arrow keys now work as alternates for
  every directional binding.
- README documents the full keybinding table.

## 2026-07-08

- Sessions persist automatically as JSON snapshots and are restored at
  server startup, resuming Claude Code sessions in their tabs.
- `prefix+,` renames the active tab, pinning the name against automatic
  renaming.
- `prefix+x` closes the focused window outright.

## 2026-07-07

- Keybinding configuration removed: the table is hardcoded, with only the
  prefix key configurable.

## 2026-07-06

- Chorded/nested keymaps with a helix-style key-hint popup listing the
  available bindings after the prefix (and at each submap level).

## 2026-07-05

- Split into a client/server architecture: sessions run in a daemon,
  clients attach and detach, and keystrokes flow over passed descriptors.
- Added a session switcher (`prefix+s`) with live previews, navigable with
  readline or vim-style keys; `prefix+p` cycles to the previous tab.
- Tabs display their foreground command's name, with animated status text
  for tabs detected as running Claude Code.
- Frames render inside synchronized updates (DEC 2026) so redraws never
  tear.
- Fixed line-feed handling: Ctrl-J stays distinct from Enter instead of
  both reaching the program as carriage return.
- Each window's tab bar is drawn as chrome separate from tab content,
  with assorted UI polish.
- Added the README.

## 2026-07-04

- Initial multiplexer: window splits, tabs, directional focus, and an
  embedded terminal engine, all in a single process.
- Mouse text selection with yank and paste wired to the system clipboard.
