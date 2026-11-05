# Lux

A terminal multiplexer for tmux muscle memory, with a few differences:

- The layout is independent of what's in it. Each window has its own tabs,
  cycling tabs leaves the layout alone, and a tab can be yanked and pasted
  into another window, even in another session.
- Lux detects Claude Code, Codex, and Kiro CLI and shows each one's status
  in the tab bar: working, waiting, idle, done, or blocked.
- Prefix+`:` opens a vim-style command line with completion.

## Installation

```sh
git clone <this-repo>
cd lux
cargo install --path .
```

## Usage

```sh
lux                    # create and attach to a new session
lux -s <name>          # attach to a session, creating it if needed
lux attach             # reattach to the last session
lux ls                 # list sessions
lux kill-server        # stop the server and all sessions
```

Sessions are saved and restored when the server restarts.

### Keys

All keys follow the prefix (default `Ctrl-b`):

| Key | Action |
| --- | --- |
| `%` / `"` | split side-by-side / stacked |
| `c` | new tab |
| `n` / `p` / `0`-`9` | next / previous / numbered tab |
| `h` `j` `k` `l` | focus split |
| `H` `J` `K` `L` | move tab into adjacent split |
| `r` + `hjkl` | resize split |
| `m` + `hjkl` | swap with adjacent window |
| `z` | zoom the focused window |
| `w` / `x` | close tab / window |
| `,` | rename tab |
| `y` / `P` | yank tab / paste it into the focused window |
| `[` / `/` | scroll mode / search scrollback |
| `s` | session switcher |
| `f` | fuzzy tab finder |
| `Tab` | next done or blocked agent tab |
| `:` | command line |
| `d` | detach |

Arrow keys work in place of `hjkl`.

Commands include `:vs`, `:sp`, `:w <path>`, `:new [name]`,
`:rename-session <name>`, `:kill-session [name]`, `:config-open`,
`:config-reload`, and `:config-set <key> <value>`.

## Configuration

Lux reads `~/.config/lux/config.toml` (or `$XDG_CONFIG_HOME/lux/config.toml`).
Every option is optional:

```toml
prefix = "C-a"              # default C-b
restore = false             # don't restore sessions at startup
notify = false              # no desktop notifications
copy-on-select = false      # yank selections only on right-click
osc-titles = "all"          # none, agents (default), or all
rule-style = "dots"         # rule (default) or dots
dim-unfocused = false       # don't darken unfocused windows
shadows = true              # popovers cast shadows
layout-transitions = false  # snap zoom instead of animating
attach-style = "none"       # glitch (default), rain, coalesce, crt,
                            # typewriter, decrypt, burn, or none
sidebar = true              # keep the session list on the left
pet = false                 # hide the agent status pet
```
