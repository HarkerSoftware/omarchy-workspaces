# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/); versions follow semver.

## [Unreleased]

## [0.3.0] - 2026-08-08

### Added
- Periodic update checks: the daemon polls GitHub releases daily
  (`[updates]` config section) and surfaces newer versions as a panel
  badge, desktop notification, `workspace status` line, and event.
- One-command releases (`packaging/release.sh`) and a fully automated
  GitHub release pipeline that also publishes the pacman repository.
- Omarchy install walkthrough in the README (pacman repo, panel enable,
  first project, hotkeys).

### Fixed
- Packaged systemd unit pointed at `~/.local/bin`; pacman installs run
  from `/usr/bin` (0.2.0-2).
- Panel exclusive zone now tracks the rail's real collapsed width, so
  tiled windows no longer slide under the sidebar.

## [0.2.0] - 2026-08-07

### Added
- Faithful save/restore: real launch command capture (working
  directories, VS Code folders, chromium profiles and tabs via SNSS),
  tiled-layout restoration, physical workspace membership, project
  close, auto-restore on switch, per-slot launch settings UI, state
  rings, drag-to-reorder, ALT+1..9 hotkey support.

## [0.1.0] - 2026-08-07

### Added
- Daemon with event-driven Hyprland tracking (schema-tolerant IPC, buffered
  hydration, reconnect with backoff) and a versioned NDJSON control socket.
- Projects on Hyprland named workspaces: create/delete/rename/switch
  (fuzzy)/duplicate/export/import; manual window assignment.
- Window groups with hide/show/focus/move via parking workspaces.
- Rules engine (`rules.toml`): class/initial_class/title/executable matchers
  with equals/contains/regex, priorities, pluggable matcher registry.
- Session persistence: per-project TOML files (versioned, migration-ready),
  `runtime.json` crash recovery keyed by Hyprland's stableId, debounced
  autosave, `workspace save`.
- Restore: scored window↔slot matching, dependency-ordered launch waves
  (`after`, services, window/delay/command readiness), floating-geometry
  placement, `--dry-run`, live progress, `restore_on_boot`.
- GTK4 layer-shell sidebar: 48px icon rail, hover-expand overlay, Omarchy
  theme colors with live reload, `workspace panel enable/disable/status`.
- `workspace doctor`, `workspace search`, `--json` output, systemd user
  unit, CI + release workflows, PKGBUILD, install.sh.
