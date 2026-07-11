# News

## 2026-07-11

- `-s` and `-t` now both attach to the named session if it exists and
  create it otherwise, instead of erroring on a collision or a missing
  session.
- Bare `attach`/`attach-session` attaches to the most recently attached
  session, or creates a new auto-named one if nothing has been attached
  to yet.
- Bare `new`/`new-session` creates an auto-named session instead of
  erroring.
- CLAUDECOM grid tiles are now a fixed 60×24 instead of stretching to
  fill the screen.
- Each CLAUDECOM tile's header now shows the tab name after the session
  name (`session:tab`).

- `prefix+g` opens the CLAUDECOM grid directly, without going through the
  switcher.
- The CLAUDECOM grid's highlighted tile is now marked by a border around
  the tile instead of a reversed header row.
- `q` exits the CLAUDECOM grid back to the switcher, alongside Escape.
- The session switcher now pins a "CLAUDECOM" entry whenever
  any tab runs Claude Code: a live grid of every Claude Code tab across
  all sessions, navigated with h/j/k/l or arrows, Enter jumping to the
  highlighted tab's home session and Escape returning to the switcher.
- `prefix+m` plus a direction key now swaps the focused window with its
  spatially adjacent neighbor, replacing the old split-mirroring behavior.
- Multi-line pastes are delivered as a single bracketed paste instead of a
  stream of keystrokes, so they no longer submit per line, trip
  auto-indent, or leak `[201~` marker fragments.
- Copies made by programs running inside lux (OSC 52, e.g. Claude Code's
  highlight-copy or helix's clipboard yank) now reach the system clipboard
  and the client terminal.
- Shift+click bypasses a program's mouse grab, so selection, yank, and
  right-click paste work inside mouse-aware programs like helix.
- Each Claude Code tab now persists its own session id, fixing restores
  that resumed the same session in every tab and lost the others.

## 2026-07-10

- `prefix+z` maximizes the focused window to the whole content area and
  back, leaving the layout untouched.
- `prefix+i` rotates the split enclosing the focused window between
  side-by-side and stacked.
- `prefix+m` entered a mirror submap reversing a split's children
  (replaced by window swap the next day).
- Move-tab rebound from a submap to direct `prefix+H`/`J`/`K`/`L`, with a
  500ms repeat window for chained moves.
- Arrow keys (and Shift-arrows for move-tab) accepted as alternates for
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
- `prefix+s` opens a session switcher with previews; `prefix+p` cycles to
  the previous tab.
- The session switcher navigates with readline-style keys as well as
  vim-style ones.
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
