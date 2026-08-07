//! File persistence: project TOML files, runtime crash-recovery snapshot,
//! atomic writes, schema versioning and migrations, export/import.
//!
//! Layout:
//! - `~/.config/omarchy-workspaces/{config.toml, rules.toml}`
//! - `~/.local/state/omarchy-workspaces/{projects/<slug>.toml, runtime.json, logs/}`

#![warn(missing_docs)]
