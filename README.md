# omarchy-workspaces

A workspace/project manager that sits **on top of** Hyprland (Omarchy Linux):
define named projects — groups of windows like *Web Development* = Firefox +
VS Code + Terminal — and switch between them with automatic window restore,
app launching with dependency ordering, and intelligent focus. Includes
session persistence across reboots, a configurable rules engine, fuzzy
search, a modern CLI, and a GTK4 layer-shell sidebar that follows your
Omarchy theme.

Think *tmux sessions × VS Code workspaces × macOS Spaces*, as a native
Hyprland citizen: projects map onto Hyprland **named workspaces**
(`name:web-dev`), so numeric workspaces and your `SUPER+1..0` bindings are
never touched.

## Components

| Binary | Role |
|---|---|
| `workspace-daemon` | Tracks Hyprland via IPC (event-driven), owns all state, serves a Unix socket |
| `workspace` | CLI: projects, groups, rules, save/restore, search, doctor |
| `workspace-panel` | GTK4 layer-shell left rail: collapsed icon strip, hover-expands, themed |

## Quick start

```sh
# start the daemon (or install the systemd unit, see below)
workspace-daemon &

workspace create "Web Development"      # slug: web-development
workspace switch web                    # fuzzy — focuses name:web-development
workspace assign 0x55… web-development  # adopt a window (see `workspace windows`)
workspace save                          # capture the current windows as app slots
workspace restore web-development       # after reboot: relaunch + re-place everything
workspace panel enable                  # the left sidebar, themed, autostarts
workspace doctor                        # health checks when anything is off
```

Groups within a project:

```sh
workspace group create web-development Backend
workspace group add web-development backend 0x55…
workspace group hide web-development backend    # parks windows off-screen
workspace group show web-development backend
```

## Configuration

`~/.config/omarchy-workspaces/config.toml` (all optional; see
`contrib/example-config.toml`):

```toml
[general]
workspace_prefix = ""          # prefix for our Hyprland workspace names
rule_action = "move"           # assign | move | move-focus
restore_on_boot = ["web-development"]

[autosave]
enabled = true
debounce_ms = 2000
interval_s = 60
```

Rules in `~/.config/omarchy-workspaces/rules.toml` auto-assign windows as
they open (see `contrib/example-rules.toml`):

```toml
[[rules]]
name = "browsers-to-research"
project = "research"
[rules.match]
class = { equals = "firefox" }
title = { contains = "arXiv" }
```

Apply config changes with `workspace daemon reload`. Test rules with
`workspace rules test`.

Launch specs with dependency ordering live in the project files
(`~/.local/state/omarchy-workspaces/projects/<slug>.toml`):

```toml
[[project.apps]]
name = "postgres"
[project.apps.identity]
[project.apps.launch]
command = "docker start postgres"
service = true
readiness = { type = "command", cmd = "pg_isready -q" }

[[project.apps]]
name = "editor"
[project.apps.identity]
class = "Code"
[project.apps.launch]
command = "code ~/Projects/api"
after = ["postgres"]
```

## Install

**From source** (needs stable Rust; the panel needs `gtk4` +
`gtk4-layer-shell`, already present on Omarchy):

```sh
cargo build --release
install -Dm755 target/release/{workspace,workspace-daemon,workspace-panel} -t ~/.local/bin/
```

**Daemon autostart** — either the systemd user unit:

```sh
cp contrib/omarchy-workspaces.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now omarchy-workspaces
```

…or Omarchy-style, in `~/.config/hypr/autostart.conf`:

```
exec-once = uwsm-app -- workspace-daemon
```

AUR packages and a curl-able `install.sh` ship with the first tagged
release (see `packaging/`).

## What v1 does and does not restore

Restore relaunches missing apps in dependency order, adopts matching live
windows, and re-applies **floating** geometry and fullscreen state. The
tiled layout tree (dwindle splits) is *not* serialized — Hyprland has no
stable layout-dump API; tiled windows re-flow under the current layout.

## Development

```sh
./contrib/check.sh   # fmt + clippy -D warnings + all tests
```

The daemon runs entirely against a scripted fake Hyprland in tests; see
`crates/workspace-hypr/src/fake.rs` and `crates/workspace-daemon/tests/`.
Architecture notes in [docs/architecture.md](docs/architecture.md), wire
protocol in [docs/protocol.md](docs/protocol.md).

## License

MIT — see [LICENSE](LICENSE).
