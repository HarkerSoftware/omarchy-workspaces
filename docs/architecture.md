# Architecture

Three processes, one protocol:

```
┌────────────┐   Hyprland IPC    ┌──────────────────┐   NDJSON unix socket   ┌────────────┐
│  Hyprland  │ ◄───────────────► │ workspace-daemon │ ◄────────────────────► │ workspace  │ (CLI)
│ .socket(2) │  events+dispatch  │   (state owner)  │  requests+event push   ├────────────┤
└────────────┘                   └──────────────────┘                        │ panel, …   │
                                                                             └────────────┘
```

## Crates

- **workspace-core** — pure domain logic: model (projects, groups, app
  slots, window identity), rules engine, restore planner, launch toposort,
  fuzzy search, config schema. No tokio, no IO; everything unit-testable.
- **workspace-hypr** — async Hyprland IPC. Hand-rolled tokio `UnixStream`
  against the wire protocol; schema-tolerant serde models; reconnecting
  event pump; a scripted fake server behind the `fake` feature.
- **workspace-proto** — the daemon⇄client wire types (shared by CLI and
  panel).
- **workspace-storage** — atomic writes, versioned project TOML files with
  a migration registry, `runtime.json` crash recovery, XDG paths.
- **workspace-daemon** — composition: state actor, hypr pump, IPC server,
  restore executor, autosave.
- **workspace-cli** / **workspace-panel** — protocol clients.

## The state actor

All mutation flows through one mpsc channel into a single-writer actor that
owns the `World` (live windows/workspaces/monitors) and the project list.
Reads come from an always-current `watch` snapshot; subscribers get events
from a `broadcast` bus. Requests needing Hyprland dispatches mutate state
immediately and execute the dispatch in a spawned task — the actor loop
never awaits the compositor.

## Hydration & event replay

The event socket connects *before* the state dump, so events arriving
during the dump wait in the channel and replay afterwards. Every state
application is an idempotent upsert keyed by window address, which makes
the replay harmless. `openwindow` events carry no pid/geometry; a follow-up
`clients` fetch enriches the window (and only then do rules run).

## Named-workspace mapping

- Project primary workspace: `name:<slug>` (optional configured prefix).
- Group parking workspace: `name:<slug>:<group>` — hidden groups' windows
  are parked there via `movetoworkspacesilent`; plain named workspaces, no
  special-workspace overlay quirks.
- The daemon claims only names that match a known project; `create`
  refuses slugs colliding with foreign named workspaces.

## Restore

`core::restore::plan` is pure: scored identity matching (executable 100,
class 50, initial class 40, title 10; threshold 50; greedy one-window-one-
slot) diffs desired slots against live windows into adopt steps, launch
waves (Kahn toposort over `after` dependencies), and extras. The executor
launches window apps via `dispatch exec [workspace name:<ws> silent] …` so
Hyprland places them natively, spawns `service` slots directly, correlates
`window.opened` events back to slots, skips dependent subtrees on
timeout, and streams `restore.progress` events.

## Identity across restarts

- Within a compositor session: window address (unique) and Hyprland's
  `stableId` (used as the `runtime.json` recovery key so a daemon restart
  keeps assignments).
- Across reboots: `WindowIdentity` scoring only. `stableId` is never
  trusted across boots.
