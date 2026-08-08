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

## Install on Omarchy

**1. Add the package repository** (prebuilt binaries; updates arrive
with every `omarchy update` / `pacman -Syu`). Append to
`/etc/pacman.conf`:

```ini
[harkersoftware]
SigLevel = Optional TrustAll
Server = https://raw.githubusercontent.com/HarkerSoftware/arch-repo/main/$arch
```

**2. Install and start the daemon:**

```sh
sudo pacman -Sy omarchy-workspaces
systemctl --user enable --now omarchy-workspaces
```

**3. Enable the sidebar** (adds the autostart entry and layer rules,
and starts it now):

```sh
workspace panel enable
```

**4. First project** — click **+** in the sidebar (or
`workspace create "My Project"`), open your apps on that workspace,
arrange them, then right-click the project → **Save windows**. From
then on the project reopens itself — windows, folders, browser tabs,
and layout — whenever you click it, even after a reboot.

**Optional — number hotkeys**: add to `~/.config/hypr/bindings.conf`
(N = position in the sidebar):

```
bindd = ALT, 1, Project workspace 1, exec, workspace switch --index 1
```

`workspace doctor` checks the installation end to end when anything
seems off.

### Other install methods

**Binary installer** (no repo setup, no automatic updates):

```sh
curl -fsSL https://github.com/HarkerSoftware/omarchy-workspaces/raw/main/packaging/install.sh | bash
```

**From source** (needs stable Rust; the panel needs `gtk4` +
`gtk4-layer-shell`, already present on Omarchy):

```sh
cargo build --release
install -Dm755 target/release/{workspace,workspace-daemon,workspace-panel} -t ~/.local/bin/
cp contrib/omarchy-workspaces.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now omarchy-workspaces
```

## What restore does

Restore relaunches missing apps in dependency order (with their working
directories, VS Code folders, chromium profile and tabs), adopts
matching live windows already on the project workspace, re-applies
floating geometry and fullscreen state, and rebuilds the captured tiled
arrangement best-effort (swap/re-split/resize passes — exotic
hand-built split trees may land approximately).

## Development

```sh
./contrib/check.sh   # fmt + clippy -D warnings + all tests
```

**Cutting a release** — move the `[Unreleased]` CHANGELOG items under
the new version heading, then:

```sh
./packaging/release.sh minor   # or: patch | major | X.Y.Z
```

One command: gates on checks and the CHANGELOG entry, bumps
`Cargo.toml` + `PKGBUILD`, commits, tags, pushes, waits for the GitHub
release build, and publishes to the pacman repository.

The daemon runs entirely against a scripted fake Hyprland in tests; see
`crates/workspace-hypr/src/fake.rs` and `crates/workspace-daemon/tests/`.
Architecture notes in [docs/architecture.md](docs/architecture.md), wire
protocol in [docs/protocol.md](docs/protocol.md).

## License

MIT — see [LICENSE](LICENSE).
