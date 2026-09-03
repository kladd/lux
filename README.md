# Lux

A terminal multiplexer designed for tmux muscle memory, but with a few
differentiating features.

- Window management: Lux sessions are similar to tmux sessions, but windows and
  panes are different. In Lux the layout is independent of active windows/panes.
  Each window has its own tabs. Cycling tabs does not disturb the layout,
  and a tab can be yanked and pasted into another window, even in another
  session.
- Agents: Lux detects Claude Code, Codex, and Kiro CLI and reports their
  status in the tab bar: working, idle, done, blocked.
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
lux ls                 # list sessions
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

### Navigating sessions

Prefix+`s` opens the session switcher: a list of sessions with a live
preview. Move the highlight with `j`/`k`, the arrow keys, or readline-style
`Ctrl-n`/`Ctrl-p`; `Enter` (or clicking an entry) attaches, `Esc` cancels.
Clicking the `☢` icon at the left of the status bar opens it too; while
the switcher is open the icon shows as `○`, and clicking it exits.

Prefix+`f` opens the fuzzy tab finder: a popover over your session
listing every tab across every session, narrowing as you type a query,
with a live preview of the highlighted match. Move the highlight with
`Ctrl-n`/`Ctrl-p` or the arrow keys; `Enter` jumps to the highlighted
tab's home session, window, and tab; `Esc` cancels.

### CLAUDECOM

While any tab runs Claude Code, the switcher pins a **CLAUDECOM** entry
at the top: a live grid with one tile per Claude Code tab across every
session, each showing status text, home session name, tab name, and
content resized to fit the tile. Prefix+`g` jumps straight to the grid
without opening the switcher.

In the grid: move the highlight with `h`/`j`/`k`/`l` or the arrow keys
(overflow rows scroll with it); `Enter` captures the highlighted tile for
typing into its tab in place (marked with a `capture` label — prefix+`g`
or prefix+`Esc` returns to grid navigation); `g` jumps to the highlighted
tab's home session, window, and tab; prefix+`s` and prefix+`f` open the
switcher or finder directly; `q`/`Esc` returns to the session you came
from.

### Auto mode

With `automode = true` (see Configuration), both of CLAUDECOM's entry
points — prefix+`g` and selecting CLAUDECOM from the switcher — open auto
mode instead of the grid. Auto mode attaches you to one agent tab that's
done or blocked at a time. Once that tab starts working again or goes
away, it hands off automatically to the next such tab, in the same order
the grid uses. Prefix+`Tab` skips to the next one manually. When no tab
needs attention, it shows a blank screen — "Claude doesn't need you right
now" — with a list of tabs still working underneath.

## Configuration

Lux reads `$XDG_CONFIG_HOME/lux/config.toml` (falling back to
`~/.config/lux/config.toml`) at startup. A missing file is fine; a malformed
one falls back to defaults and prints an error to stderr. The keybinding
table itself is not configurable:

```toml
# ~/.config/lux/config.toml
prefix = "C-a"   # "C-" prefix means Ctrl is held (default: C-b)
restore = false  # skip restoring persisted sessions at startup
notify = false   # no desktop notifications for Claude Code tabs
automode = true  # CLAUDECOM opens auto mode instead of the grid
copy-on-select = false   # selections yank only on right-click
osc-titles = "all"       # which tabs are named by the program's OSC title
palette = "default"      # the interface color set
dim-unfocused = true     # darken every window but the focused one
shadows = true           # popovers cast a shadow on the content beneath
```

The prefix key spec is a single character, optionally prefixed with `C-`
for Ctrl.

`osc-titles` is `none`, `agents` (the default), or `all`. A tab not renamed
by hand is named after its foreground process; where the option allows it,
a window title the program sets with OSC 0/2 replaces that name. Agent tabs
use the title by default; a Claude Code session name outranks it.

`palette` names the color set lux draws its own chrome in: agent status,
tab bars, the status line, selections, and popovers. Only `default` exists
so far. Terminal content always keeps your terminal's own colors.

`dim-unfocused` and `shadows` are off by default. Both darken cells, and a
cell in the terminal's default colors is darkened from the palette's
stand-ins (light grey on black), so they look best on a dark terminal.
