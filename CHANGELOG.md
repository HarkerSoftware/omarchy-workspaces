# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/); versions follow semver.

## [Unreleased]

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
