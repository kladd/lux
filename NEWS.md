# News

## 2026-07-12

- Added desktop notifications when a Claude Code tab in any session
  finishes or needs input, delivered to your terminal via OSC 9;
  disable with `notify = false` in the config.

## 2026-07-11

- `-s`/`-t` and bare `attach`/`new` now attach-or-create a session by name
  instead of erroring on a missing or duplicate one.
- Added CLAUDECOM, a live overview of every Claude Code tab across
  sessions, reachable from the switcher or `prefix+g`.
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
