//! Live desktop state: every tracked window, workspace, and monitor.
//!
//! `World` is the daemon's in-memory model of Hyprland, maintained by
//! idempotent upserts so that a full re-hydration dump and a replayed event
//! stream converge to the same state regardless of interleaving.
//!
//! Window keys are canonical address strings (`0x…`) produced by the IPC
//! layer; this crate deliberately stores them as plain strings so it stays
//! independent of the Hyprland transport.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{ProjectId, Slug};

/// Everything the daemon knows about one live window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowFacts {
    /// Current window class.
    pub class: String,
    /// Current title.
    pub title: String,
    /// Class at map time.
    pub initial_class: String,
    /// Title at map time.
    pub initial_title: String,
    /// Executable path resolved from `/proc/<pid>/exe`, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    /// Process id (not unique per window).
    pub pid: i32,
    /// Workspace id the window is on.
    pub workspace_id: i64,
    /// Workspace name the window is on.
    pub workspace: String,
    /// Monitor id.
    pub monitor: i64,
    /// Top-left corner.
    pub at: (i32, i32),
    /// Size in pixels.
    pub size: (i32, i32),
    /// Whether the window floats.
    pub floating: bool,
    /// Whether the window is pinned.
    pub pinned: bool,
    /// Fullscreen mode (0/1/2).
    pub fullscreen: u8,
}

/// How a window came to be assigned to a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentSource {
    /// A rule matched (rule name recorded).
    Rule(String),
    /// The user assigned it explicitly. Never overridden by rules.
    Manual,
    /// Correlated to a restore slot.
    Restore(uuid::Uuid),
}

/// A live window plus our annotations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackedWindow {
    /// Canonical window address (`0x…`), the per-session key.
    pub address: String,
    /// Hyprland's session-stable window id, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    /// Current facts about the window.
    pub facts: WindowFacts,
    /// Project/group assignment, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment: Option<(ProjectId, Option<Slug>)>,
    /// Where the assignment came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_by: Option<AssignmentSource>,
}

/// One live workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// Workspace id (negative for named).
    pub id: i64,
    /// Workspace name.
    pub name: String,
    /// Output name it is on.
    pub monitor: String,
}

/// One live monitor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// Monitor id.
    pub id: i64,
    /// Output name.
    pub name: String,
    /// Whether it has focus.
    pub focused: bool,
}

/// The daemon's complete live-state model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct World {
    /// Tracked windows keyed by canonical address.
    pub windows: HashMap<String, TrackedWindow>,
    /// Live workspaces keyed by id.
    pub workspaces: HashMap<i64, WorkspaceInfo>,
    /// Live monitors.
    pub monitors: Vec<MonitorInfo>,
    /// Address of the focused window, if any.
    pub focused_window: Option<String>,
    /// Name of the focused workspace, if known.
    pub focused_workspace: Option<String>,
    /// The active project, if any.
    pub active_project: Option<ProjectId>,
    /// Whether the Hyprland event socket is currently connected.
    pub hypr_connected: bool,
}

impl World {
    /// Replace all compositor-derived state from a full dump, preserving
    /// assignments of windows that still exist.
    pub fn hydrate(
        &mut self,
        windows: Vec<TrackedWindow>,
        workspaces: Vec<WorkspaceInfo>,
        monitors: Vec<MonitorInfo>,
        focused_window: Option<String>,
    ) {
        let mut next: HashMap<String, TrackedWindow> = windows
            .into_iter()
            .map(|w| (w.address.clone(), w))
            .collect();
        for (address, incoming) in next.iter_mut() {
            if let Some(previous) = self.windows.get(address) {
                incoming.assignment = previous.assignment.clone();
                incoming.assigned_by = previous.assigned_by.clone();
            }
        }
        self.windows = next;
        self.workspaces = workspaces.into_iter().map(|w| (w.id, w)).collect();
        self.monitors = monitors;
        self.focused_window = focused_window;
    }

    /// Insert or update a window with fresh facts (idempotent upsert).
    pub fn upsert_window(&mut self, address: &str, stable_id: Option<String>, facts: WindowFacts) {
        match self.windows.get_mut(address) {
            Some(window) => {
                window.facts = facts;
                if stable_id.is_some() {
                    window.stable_id = stable_id;
                }
            }
            None => {
                self.windows.insert(
                    address.to_owned(),
                    TrackedWindow {
                        address: address.to_owned(),
                        stable_id,
                        facts,
                        assignment: None,
                        assigned_by: None,
                    },
                );
            }
        }
    }

    /// Remove a window; unknown addresses are a no-op (event replay safety).
    pub fn remove_window(&mut self, address: &str) {
        self.windows.remove(address);
        if self.focused_window.as_deref() == Some(address) {
            self.focused_window = None;
        }
    }

    /// Update a window's title; unknown addresses are a no-op.
    pub fn set_title(&mut self, address: &str, title: &str) {
        if let Some(window) = self.windows.get_mut(address) {
            window.facts.title = title.to_owned();
        }
    }

    /// Update a window's floating state; unknown addresses are a no-op.
    pub fn set_floating(&mut self, address: &str, floating: bool) {
        if let Some(window) = self.windows.get_mut(address) {
            window.facts.floating = floating;
        }
    }

    /// Move a window to a workspace; unknown addresses are a no-op.
    pub fn set_window_workspace(&mut self, address: &str, workspace_id: i64, workspace: &str) {
        if let Some(window) = self.windows.get_mut(address) {
            window.facts.workspace_id = workspace_id;
            window.facts.workspace = workspace.to_owned();
        }
    }

    /// Record the focused window (or none).
    pub fn set_focus(&mut self, address: Option<String>) {
        self.focused_window = address;
    }

    /// Insert or update a workspace.
    pub fn upsert_workspace(&mut self, id: i64, name: &str, monitor: &str) {
        self.workspaces.insert(
            id,
            WorkspaceInfo {
                id,
                name: name.to_owned(),
                monitor: monitor.to_owned(),
            },
        );
    }

    /// Remove a workspace; unknown ids are a no-op.
    pub fn remove_workspace(&mut self, id: i64) {
        self.workspaces.remove(&id);
    }

    /// Rename a workspace; unknown ids are a no-op.
    pub fn rename_workspace(&mut self, id: i64, name: &str) {
        if let Some(workspace) = self.workspaces.get_mut(&id) {
            workspace.name = name.to_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(class: &str) -> WindowFacts {
        WindowFacts {
            class: class.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn upserts_are_idempotent() {
        let mut world = World::default();
        world.upsert_window("0xa", Some("s1".into()), facts("firefox"));
        world.upsert_window("0xa", None, facts("firefox"));
        assert_eq!(world.windows.len(), 1);
        // A later upsert without stable_id must not erase the known one.
        assert_eq!(world.windows["0xa"].stable_id.as_deref(), Some("s1"));

        // Events for unknown windows are no-ops, not errors.
        world.remove_window("0xdead");
        world.set_title("0xdead", "gone");
        world.set_floating("0xdead", true);
        world.set_window_workspace("0xdead", 1, "1");
        assert_eq!(world.windows.len(), 1);
    }

    #[test]
    fn hydrate_preserves_assignments() {
        let mut world = World::default();
        world.upsert_window("0xa", None, facts("firefox"));
        let project = ProjectId::new();
        world.windows.get_mut("0xa").unwrap().assignment = Some((project, None));
        world.windows.get_mut("0xa").unwrap().assigned_by = Some(AssignmentSource::Manual);

        // Re-hydration (e.g. after reconnect) replaces facts but keeps our
        // annotations for windows that still exist.
        world.hydrate(
            vec![
                TrackedWindow {
                    address: "0xa".into(),
                    stable_id: None,
                    facts: facts("firefox"),
                    assignment: None,
                    assigned_by: None,
                },
                TrackedWindow {
                    address: "0xb".into(),
                    stable_id: None,
                    facts: facts("kitty"),
                    assignment: None,
                    assigned_by: None,
                },
            ],
            vec![],
            vec![],
            Some("0xb".into()),
        );
        assert_eq!(world.windows.len(), 2);
        assert_eq!(world.windows["0xa"].assignment, Some((project, None)));
        assert_eq!(world.windows["0xb"].assignment, None);
        assert_eq!(world.focused_window.as_deref(), Some("0xb"));
    }

    #[test]
    fn focus_clears_when_window_closes() {
        let mut world = World::default();
        world.upsert_window("0xa", None, facts("firefox"));
        world.set_focus(Some("0xa".into()));
        world.remove_window("0xa");
        assert_eq!(world.focused_window, None);
    }

    #[test]
    fn workspace_lifecycle() {
        let mut world = World::default();
        world.upsert_workspace(-1337, "web-dev", "DP-1");
        world.rename_workspace(-1337, "webdev");
        assert_eq!(world.workspaces[&-1337].name, "webdev");
        world.remove_workspace(-1337);
        world.remove_workspace(-1337); // replay-safe
        assert!(world.workspaces.is_empty());
    }
}
