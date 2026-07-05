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
| `H` `J` `K` `L` | resize split left/down/up/right |
| `o` | close every split but the focused one |
| `[` | enter scroll mode (mouse or keys; `q`/`Esc` to exit) |
| `d` | detach from the session |
| `s` | open the session switcher |
| `:` | open the ex command line |

Ex commands (typed after `:`, with autocomplete):

- `:vs` — split side-by-side
- `:sp` — split stacked
- `:w <path>` — write the visible scrollback to a file

### Navigating sessions

Prefix+`s` opens the session switcher: a list of sessions with a live
preview. Move the highlight with `j`/`k`, the arrow keys, or readline-style
`Ctrl-n`/`Ctrl-p`; `Enter` attaches, `Esc` cancels.

## Configuration

Lux reads `$XDG_CONFIG_HOME/lux/config.toml` (falling back to
`~/.config/lux/config.toml`) at startup. A missing file is fine; a malformed
one falls back to defaults with an error printed to stderr. You can override
the prefix key and any subset of the default keybindings:

```toml
# ~/.config/lux/config.toml
prefix = "C-a"

[keys]
split-side-by-side = "v"   # rebind % -> v
resize-left = "C-y"        # "C-" prefix means Ctrl is held
```

Key specs are a single character, optionally prefixed with `C-` for Ctrl.
Rebinding a command to a key already in use displaces whatever default held
that key.
