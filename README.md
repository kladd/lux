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
lux -s <name>          # create a named session (fails if it exists)
lux new-session -s <name>
lux -t <name>          # attach to an existing session
lux attach -t <name>
lux ls                 # list sessions
lux kill-server        # stop the server and all sessions
```

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
| `:` | open the ex command line |

Arrow keys work as alternates for `h`/`j`/`k`/`l` (and Shift-arrows for
`H`/`J`/`K`/`L`).

Ex commands (typed after `:`, with autocomplete):

- `:vs` — split side-by-side
- `:sp` — split stacked
- `:w <path>` — write the tab's entire content, scrollback included, to a
  file (a leading `~/` expands to your home directory; relative paths
  resolve against the server's working directory)

### Navigating sessions

Prefix+`s` opens the session switcher: a list of sessions with a live
preview. Move the highlight with `j`/`k`, the arrow keys, or readline-style
`Ctrl-n`/`Ctrl-p`; `Enter` attaches, `Esc` cancels.

## Configuration

Lux reads `$XDG_CONFIG_HOME/lux/config.toml` (falling back to
`~/.config/lux/config.toml`) at startup. A missing file is fine; a malformed
one falls back to defaults with an error printed to stderr. The prefix key
is the only setting; the keybinding table itself is not configurable:

```toml
# ~/.config/lux/config.toml
prefix = "C-a"   # "C-" prefix means Ctrl is held
```

The key spec is a single character, optionally prefixed with `C-` for Ctrl.
