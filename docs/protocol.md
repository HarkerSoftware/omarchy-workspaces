# Wire protocol

Transport: Unix socket at `$XDG_RUNTIME_DIR/omarchy-workspaces/daemon.sock`,
newline-delimited JSON (one compact message per line). Debug with:

```sh
echo '{"v":1,"id":1,"method":"daemon.status"}' | socat - UNIX:$XDG_RUNTIME_DIR/omarchy-workspaces/daemon.sock
```

## Envelopes

```jsonc
// client → daemon
{"v":1, "id":42, "method":"project.switch", "params":{"project":"web-dev"}}
// daemon → client (response; id echoes the request)
{"v":1, "id":42, "ok":true,  "result":{…}}
{"v":1, "id":42, "ok":false, "error":{"code":"NOT_FOUND","message":"…","data":{…}}}
// daemon → subscriber (after `subscribe`; seq is monotonic for gap detection)
{"v":1, "seq":9107, "event":"window.opened", "data":{…}}
```

`v` is the protocol major; mismatches are rejected with
`UNSUPPORTED_VERSION`. Unknown methods return `UNKNOWN_METHOD`. Error codes:
`BAD_REQUEST`, `UNKNOWN_METHOD`, `UNSUPPORTED_VERSION`, `NOT_FOUND`,
`CONFLICT`, `AMBIGUOUS` (candidates in `data`), `HYPRLAND`, `INTERNAL`.

## Methods

| Method | Params | Result |
|---|---|---|
| `daemon.status` | — | version, uptime, hypr connection, active project, counts |
| `state.snapshot` | — | projects, windows, workspaces, monitors, focus |
| `subscribe` | `topics?: [windows,workspaces,projects,restore,daemon]` | ack; events follow |
| `project.create` | `name`, `slug?` | ProjectSummary |
| `project.delete` | `slug` (exact, never fuzzy) | `{deleted}` |
| `project.rename` | `slug`, `name` | ProjectSummary |
| `project.switch` | `project` (fuzzy) | ProjectSummary |
| `project.list` | — | `[ProjectSummary]` |
| `project.save` | `project?` (default active) | `{saved, slots}` |
| `project.restore` | `project?`, `dry_run?` | plan (dry run) or `{started, plan}` + `restore.*` events |
| `project.duplicate` | `project`, `name` | ProjectSummary |
| `project.export` | `project` | `{slug, toml}` |
| `project.import` | `toml`, `force?` | ProjectSummary (re-slugged on collision) |
| `window.assign` | `address`, `project`, `group?` | `{assigned, project}` |
| `group.create/add/remove/hide/show/focus/move` | project/group/address args | operation summary |
| `rules.test` | `address?` (default focused) | window facts + matching rules |
| `config.reload` | — | `{reloaded, rules}` |
| `search` | `query` | `{results: [{kind, slug/address, label, score}]}` |

## Events

`window.opened/closed/moved/title/focused`, `rule.matched`,
`workspace.changed`, `project.created/deleted/renamed/switched`,
`group.changed`, `restore.progress` (`state`: launching/ready/timeout/
failed/skipped), `restore.finished`, `daemon.hypr_connection`,
`daemon.shutting_down`.

Fuzzy resolution everywhere: exact slug > unique prefix > decisive nucleo
score; ambiguous queries return `AMBIGUOUS` with candidates rather than
guessing. Destructive operations (`project.delete`) take exact slugs only.
