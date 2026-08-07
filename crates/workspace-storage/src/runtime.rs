//! `runtime.json`: session-scoped crash recovery.
//!
//! Records the active project and every window assignment so a daemon restart
//! mid-session recovers annotations without re-running rules. Keys prefer
//! Hyprland's `stableId` (survives daemon restarts within a compositor
//! session) and fall back to the window address. Never used across reboots —
//! cross-boot matching is `WindowIdentity`'s job.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use workspace_core::model::Slug;

use crate::StorageError;
use crate::atomic::atomic_write;

/// One recovered assignment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeAssignment {
    /// Project slug (ids are not stable across files; slugs resolve on load).
    pub project: Slug,
    /// Group slug, if assigned to a group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<Slug>,
    /// Provenance: `manual`, `restore`, or the rule name.
    pub source: String,
}

/// The whole runtime snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeState {
    /// Slug of the active project, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_project: Option<Slug>,
    /// Assignments keyed by `stableId` (preferred) or window address.
    #[serde(default)]
    pub assignments: HashMap<String, RuntimeAssignment>,
}

fn runtime_path(state_dir: &Path) -> PathBuf {
    state_dir.join("runtime.json")
}

/// Write the runtime snapshot atomically.
pub fn save_runtime(state_dir: &Path, state: &RuntimeState) -> Result<(), StorageError> {
    std::fs::create_dir_all(state_dir)?;
    let bytes = serde_json::to_vec(state).map_err(StorageError::Json)?;
    atomic_write(&runtime_path(state_dir), &bytes)?;
    Ok(())
}

/// Load the runtime snapshot; a missing or corrupt file yields the default
/// (recovery is best-effort by design).
pub fn load_runtime(state_dir: &Path) -> RuntimeState {
    match std::fs::read(runtime_path(state_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => RuntimeState::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_corrupt_tolerance() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = RuntimeState {
            active_project: Some(Slug::parse("web-dev").unwrap()),
            assignments: HashMap::new(),
        };
        state.assignments.insert(
            "stable123".into(),
            RuntimeAssignment {
                project: Slug::parse("web-dev").unwrap(),
                group: None,
                source: "manual".into(),
            },
        );
        save_runtime(dir.path(), &state).unwrap();
        assert_eq!(load_runtime(dir.path()), state);

        std::fs::write(runtime_path(dir.path()), b"garbage").unwrap();
        assert_eq!(load_runtime(dir.path()), RuntimeState::default());

        // Missing file is default too.
        assert_eq!(
            load_runtime(&dir.path().join("nowhere")),
            RuntimeState::default()
        );
    }
}
