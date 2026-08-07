# omarchy-workspaces

A workspace/project manager that sits **on top of** Hyprland (Omarchy Linux):
define named projects — groups of windows like *Web Development* = Firefox +
VS Code + Terminal — and switch between them with automatic window restore,
app launching, and intelligent focus. Includes session persistence across
reboots, a configurable rules engine, fuzzy search, a modern CLI, and a GTK4
layer-shell sidebar.

> **Status: early development.** The architecture is in place; features land
> milestone by milestone. Nothing here is ready for daily use yet.

## Components

| Binary | Role |
|---|---|
| `workspace-daemon` | Tracks Hyprland via IPC, owns all state, serves a Unix socket |
| `workspace` | CLI: create/switch/save/restore projects, groups, rules, doctor |
| `workspace-panel` | GTK4 layer-shell left icon rail for switching projects |

Projects map onto Hyprland **named workspaces** (`name:web-dev`), so the
numeric workspaces and your `SUPER+1..0` bindings are never touched.

## Building

Requires stable Rust (Arch: `pacman -S rust`). The panel additionally needs
`gtk4` and `gtk4-layer-shell` (already present on Omarchy).

```sh
cargo build --release
./contrib/check.sh   # fmt + clippy + tests
```

## License

MIT — see [LICENSE](LICENSE).
