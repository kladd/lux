# News

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
