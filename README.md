# Lux

A terminal multiplexer designed for tmux muscle memory, but with a few
differentiating features.

- Window management: Lux sessions are similar to tmux sessions, but windows and
  panes are different. In Lux the layout is independent of active windows/panes.
  Each window has its own tabs. Cycling tabs does not disturb the layout,
  and a tab can be yanked and pasted into another window, even in another
  session.
- Agents: Lux detects Claude Code, Codex, and Kiro CLI and reports their
  status in the tab bar: working, waiting (the turn is over but background
  shells, agents, or MCP tasks still run), idle, done, blocked. Detection
  sees through symlinked or wrapped installs and launchers run via node,
  bun, python, or a shell.
- vim/helix style: prefix+`:` opens a command line with autocomplete, and
  commands like `:vs`/`:sp` mirror vim's split bindings.

## Installation

```sh
git clone <this-repo>
cd lux
cargo install --path .
```

## CLI reference

### Starting a session

```sh
lux                    # create and attach to a new session
lux -s <name>          # attach to a session by name, creating it if needed
lux new-session -s <name>
lux -t <name>          # same; -t is kept for tmux muscle memory
lux attach -t <name>
lux attach             # reattach to the most recently attached session
lux ls                 # list sessions, including name@alias for connected hosts
lux kill-server        # stop the server and all sessions
```

Sessions save automatically and restore when the server next starts,
resuming Claude Code sessions in their tabs (disable with
`restore = false`, see Configuration).

### Navigating and manipulating windows

All window commands start with the prefix key (default `Ctrl-b`):

| Key | Action |
| --- | --- |
| `%` | split side-by-side |
| `"` | split stacked |
| `c` | new tab |
| `n` / `p` | next / previous tab |
| `0`-`9` | jump to tab by index |
| `h` `j` `k` `l` | focus split left/down/up/right |
| `H` `J` `K` `L` | move active tab into split left/down/up/right (tap again within 500ms to keep moving) |
| `r` then `h` `j` `k` `l` | resize split left/down/up/right (tap again within 500ms to keep resizing) |
| `m` then `h` `j` `k` `l` | swap focused window with adjacent window left/down/up/right |
| `i` | flip the enclosing split's orientation |
| `=` | rebalance every split to an even ratio |
| `z` | maximize/zoom the focused window |
| `o` | close every split but the focused one |
| `w` | close the active tab |
| `x` | close the focused window |
| `,` | rename the active tab |
| `y` | yank the active tab (`*` marks it in the tab bar) |
| `P` | paste the yanked tab into the focused window, even in another session |
| `Esc` | cancel a pending yank |
| `Y` | copy the last completed command's output to the clipboard (needs the shell's OSC 133 integration) |
| `[` | enter scroll mode (mouse or keys; `q`/`Esc` to exit; a scrollbar on the right edge shows where you are) |
| `/` | search the scrollback: enters scroll mode and opens a `/` prompt (plain text, case-sensitive); `Enter` jumps to the nearest match above the view and highlights every match, `n`/`N` step to older/newer matches, and `/` inside scroll mode searches again |
| `d` | detach from the session |
| `s` | open the session switcher |
| `g` | open the CLAUDECOM grid |
| `f` | open the fuzzy tab finder |
| `Tab` | jump to the next done or blocked agent tab, across every session (wraps) |
| `:` | open the ex command line |
| the prefix key again | send a literal prefix keypress to the tab (tap again within 500ms to send another) |

Arrow keys work as alternates for `h`/`j`/`k`/`l` (and Shift-arrows for
`H`/`J`/`K`/`L`).

Clicking a tab indicator selects that tab; middle-clicking it closes the
tab, like prefix+`w`.

In terminals that support pointer shapes, clickable chrome — tab
indicators, window controls, minimized window titles, the status bar's
menu icon and agent indicator, and switcher entries — shows a hand
pointer on hover, and draggable split boundaries a resize pointer.

Ex commands (typed after `:`, with autocomplete):

- `:vs` — split side-by-side
- `:sp` — split stacked
- `:w <path>` — write the tab's entire content, scrollback included, to a
  file (a leading `~/` expands to your home directory; relative paths
  resolve against the server's working directory)
- `:new [name]` / `:new-session [name]` — create a session (auto-named
  without an argument) and attach to it; a name already in use does
  nothing
- `:rename-session <name>` — rename the current session
- `:kill-session [name]` — kill the named session, or the current one
  without an argument
- `:config-open` — open the config file in a new tab running `$EDITOR`
- `:config-reload` — re-read the config file and apply it to every session
- `:config-set <key> <value>` — write one key to the config file (creating
  it if needed) and reload; an unknown key or invalid value leaves the file
  unchanged

### Navigating sessions

Prefix+`s` opens the session switcher: a list of sessions with a live
preview. Move the highlight with `j`/`k`, the arrow keys, or readline-style
`Ctrl-n`/`Ctrl-p`; `Enter` (or clicking an entry) attaches, `Esc` cancels.
`n` prompts for a name and creates a new session, attaching to it (leave
the name empty to auto-name it; a taken name returns to the switcher).
Clicking the `☢` icon at the left of the status bar opens it too; while
the switcher is open the icon shows as `○`, and clicking it exits.
Sessions on a connected host (see `:connect`) follow this host's, grouped
under a heading naming the host.

With `sidebar = true`, the session list stays visible at the left instead.
Prefix+`s` moves focus into it, where the same keys move the highlight,
`Enter` attaches, and `Esc` returns focus to your window. Clicking an entry
attaches at any time.

Prefix+`f` opens the fuzzy tab finder: a popover over your session
listing every tab across every session, grouped under a heading per
session, narrowing as you type a query, with a live preview of the
highlighted match. Move the highlight with
`Ctrl-n`/`Ctrl-p` or the arrow keys; `Enter` jumps to the highlighted
tab's home session, window, and tab; `Esc` cancels.

### CLAUDECOM

Prefix+`g` opens **CLAUDECOM**: a live grid with one tile per agent tab
across every session, each showing status text, home session name, tab
name, and content resized to fit the tile.

In the grid: move the highlight with `h`/`j`/`k`/`l` or the arrow keys
(overflow rows scroll with it); `Enter` captures the highlighted tile for
typing into its tab in place (marked with a `capture` label — prefix+`g`
or prefix+`Esc` returns to grid navigation); `g` jumps to the highlighted
tab's home session, window, and tab; prefix+`s` and prefix+`f` open the
switcher or finder directly; `q`/`Esc` returns to the session you came
from.

With `grid-mouse = true`, the mouse works there too: the highlight
follows the pointer, a click captures the tile under it, a click on the
captured tile or on empty space releases it, and a double click jumps to
the tile's tab. The wheel scrolls the captured tab while the pointer is
over its tile, and moves the highlight by a row when nothing is captured.

### Auto mode

With `automode = true` (see Configuration), prefix+`g` opens auto mode
instead of the grid. Auto mode attaches you to one agent tab that's
done or blocked at a time. Once that tab starts working again or goes
away, it hands off automatically to the next such tab, in the same order
the grid uses. Prefix+`Tab` skips to the next one manually. When no tab
needs attention, it shows a blank screen — "Claude doesn't need you right
now" — with a list of tabs still working or waiting underneath.

## Configuration

Lux reads `$XDG_CONFIG_HOME/lux/config.toml` (falling back to
`~/.config/lux/config.toml`) at startup and again on `:config-reload` or
`:config-set`. A
missing file is fine. A malformed one prints an error to stderr: at startup
lux falls back to defaults, and on reload every session keeps the settings
it has. The keybinding table itself is not configurable:

```toml
# ~/.config/lux/config.toml
prefix = "C-a"   # "C-" prefix means Ctrl is held (default: C-b)
restore = false  # skip restoring persisted sessions at startup
notify = false   # no desktop notifications for Claude Code tabs
automode = true  # CLAUDECOM opens auto mode instead of the grid
copy-on-select = false   # selections yank only on right-click
osc-titles = "all"       # which tabs are named by the program's OSC title
rule-style = "dots"      # draw tab bar rules as braille dots
palette = "default"      # the interface color set
dim-unfocused = false    # leave unfocused windows at full brightness
shadows = true           # popovers cast a shadow on the content beneath
layout-transitions = false  # snap maximize into place
attach-transition = false   # draw the first frame after attaching at once
attach-style = "coalesce"   # how the first frame after attaching appears
sidebar = true              # keep the session list visible at the left
grid-mouse = true           # clicks and the wheel work in the CLAUDECOM grid
```

The prefix key spec is a single character, optionally prefixed with `C-`
for Ctrl.

`osc-titles` is `none`, `agents` (the default), or `all`. A tab not renamed
by hand is named after its foreground process; where the option allows it,
a window title the program sets with OSC 0/2 replaces that name. Agent tabs
use the title by default; a Claude Code session name outranks it.

`rule-style` is `rule` (the default) or `dots` and picks the glyph a
window's tab bar rule is drawn with: a thin dash, or a two-dot braille
line. Either one shimmers, breathes, fills with progress, and takes the
status color the same way.

`palette` names the color set lux draws its own chrome in: agent status,
tab bars, the status line, selections, and popovers. Only `default` exists
so far. Terminal content always keeps your terminal's own colors.

`dim-unfocused` darkens every window but the focused one and is on by
default; a window losing focus fades to that shade rather than snapping
to it. `shadows` is off by default. Both darken cells. Lux
asks your terminal for its default and ANSI colors when you attach, so a
dimmed cell keeps its hue; a terminal that doesn't answer is darkened from
the palette's stand-ins (light grey on black) instead.

`layout-transitions` is on by default: maximizing animates the window
between its place in the layout and the full area. Set it to `false` to
snap instead.

`attach-transition` is "rain" by default. Set it to `false`
to turn it off..
