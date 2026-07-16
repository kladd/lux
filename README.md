# Lux

A terminal multiplexer designed for tmux muscle memory, but with a few
differentiating features so far.

- Window management: Lux sessions are similar to tmux sessions, but windows and
  panes are different. In Lux the layout is independent of active windows/panes.
  Each window has its own tabs. Cycling tabs does not disturb the layout.
- Agents: Lux detects Claude Code and reports its status in the tab bar: working, idle, done, blocked.
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

Sessions are saved automatically and restored when the server next
starts, resuming Claude Code sessions in their tabs (disable with
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
| `H` `J` `K` `L` | move the active tab into the split left/down/up/right |
| `r` then `h` `j` `k` `l` | resize split left/down/up/right (tap again within 500ms to keep resizing) |
| `m` then `h` `j` `k` `l` | swap the focused window with the adjacent window left/down/up/right |
| `i` | rotate (flip the orientation of) the enclosing split |
| `=` | rebalance every split to an even ratio |
| `z` | maximize/zoom the focused window |
| `o` | close every split but the focused one |
| `x` | close the focused window |
| `,` | rename the active tab |
| `[` | enter scroll mode (mouse or keys; `q`/`Esc` to exit) |
| `d` | detach from the session |
| `s` | open the session switcher |
| `g` | open the CLAUDECOM grid |
| `f` | open the fuzzy tab finder |
| `:` | open the ex command line |

Arrow keys work as alternates for `h`/`j`/`k`/`l` (and Shift-arrows for
`H`/`J`/`K`/`L`).

Ex commands (typed after `:`, with autocomplete):

- `:vs` — split side-by-side
- `:sp` — split stacked
- `:w <path>` — write the tab's entire content, scrollback included, to a
  file (a leading `~/` expands to your home directory; relative paths
  resolve against the server's working directory)
- `:new [name]` / `:new-session [name]` — create a session (auto-named
  without an argument) and attach to it; a name already in use does
  nothing

### Navigating sessions

Prefix+`s` opens the session switcher: a list of sessions with a live
preview. Move the highlight with `j`/`k`, the arrow keys, or readline-style
`Ctrl-n`/`Ctrl-p`; `Enter` attaches, `Esc` cancels.

Prefix+`f` opens the fuzzy tab finder: a popover floating over your
session that lists every tab across every session, narrowing as you type
a query, with a live preview of the highlighted match. Move the highlight
with `Ctrl-n`/`Ctrl-p` or the arrow keys; `Enter` jumps to the
highlighted tab's home session, window, and tab; `Esc` cancels.

### CLAUDECOM

While any tab runs Claude Code, the switcher pins a **CLAUDECOM** entry
at the top: a live grid with one tile per Claude Code tab across every
session, each showing its status text, home session name, tab name, and
its content resized to fit the tile. Prefix+`g` jumps straight to the
grid without opening the switcher.

In the grid: move the highlight with `h`/`j`/`k`/`l` or the arrow keys
(overflow rows scroll with it); `Enter` captures the highlighted tile for
typing into its tab in place (marked with a `capture` label — prefix+`g`
or prefix+`Esc` returns to grid navigation); `g` jumps to the highlighted
tab's home session, window, and tab; prefix+`s` and prefix+`f` open the
switcher or finder directly; `q`/`Esc` returns to the session you came
from.

## Configuration

Lux reads `$XDG_CONFIG_HOME/lux/config.toml` (falling back to
`~/.config/lux/config.toml`) at startup. A missing file is fine; a malformed
one falls back to defaults with an error printed to stderr. The keybinding
table itself is not configurable:

```toml
# ~/.config/lux/config.toml
prefix = "C-a"   # "C-" prefix means Ctrl is held (default: C-b)
restore = false  # skip restoring persisted sessions at startup
notify = false   # no desktop notifications for Claude Code tabs
```

The prefix key spec is a single character, optionally prefixed with `C-`
for Ctrl.
