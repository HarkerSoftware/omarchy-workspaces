//! Domain events: what the daemon announces on its internal bus and pushes to
//! subscribed IPC clients.
//!
//! The serde representation is the wire format (`{"event": "window.opened",
//! "data": {…}}` after envelope flattening), so event names are part of the
//! protocol contract and must stay stable.

use serde::{Deserialize, Serialize};

/// An event on the daemon's internal bus, also pushed to subscribed clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
#[non_exhaustive]
pub enum DomainEvent {
    /// A window appeared.
    #[serde(rename = "window.opened")]
    WindowOpened {
        /// Canonical window address.
        address: String,
        /// Window class.
        class: String,
        /// Window title.
        title: String,
        /// Workspace name it opened on.
        workspace: String,
    },
    /// A window closed.
    #[serde(rename = "window.closed")]
    WindowClosed {
        /// Canonical window address.
        address: String,
    },
    /// A window moved to another workspace.
    #[serde(rename = "window.moved")]
    WindowMoved {
        /// Canonical window address.
        address: String,
        /// Destination workspace name.
        workspace: String,
    },
    /// A window's title changed.
    #[serde(rename = "window.title")]
    WindowTitleChanged {
        /// Canonical window address.
        address: String,
        /// The new title.
        title: String,
    },
    /// Window focus changed (`None` = no window focused).
    #[serde(rename = "window.focused")]
    WindowFocused {
        /// Canonical address of the focused window, if any.
        address: Option<String>,
    },
    /// The focused workspace changed.
    #[serde(rename = "workspace.changed")]
    WorkspaceChanged {
        /// Workspace id.
        id: i64,
        /// Workspace name.
        name: String,
    },
    /// A rule matched a newly opened window.
    #[serde(rename = "rule.matched")]
    RuleMatched {
        /// The rule name.
        rule: String,
        /// Canonical window address.
        address: String,
        /// Target project slug.
        project: String,
    },
    /// A project was created.
    #[serde(rename = "project.created")]
    ProjectCreated {
        /// Project slug.
        slug: String,
        /// Display name.
        name: String,
    },
    /// A project was deleted.
    #[serde(rename = "project.deleted")]
    ProjectDeleted {
        /// Project slug.
        slug: String,
    },
    /// A project was renamed (slug unchanged).
    #[serde(rename = "project.renamed")]
    ProjectRenamed {
        /// Project slug.
        slug: String,
        /// New display name.
        name: String,
    },
    /// Projects were manually reordered.
    #[serde(rename = "project.reordered")]
    ProjectsReordered {
        /// All project slugs in their new order.
        order: Vec<String>,
    },
    /// A project was closed: its windows were asked to close.
    #[serde(rename = "project.closed")]
    ProjectClosed {
        /// Project slug.
        slug: String,
        /// Number of windows asked to close.
        windows: usize,
    },
    /// The active project changed (`None` = no project active).
    #[serde(rename = "project.switched")]
    ProjectSwitched {
        /// Slug of the now-active project, if any.
        slug: Option<String>,
    },
    /// A group was created, hidden, shown, moved, or its membership changed.
    #[serde(rename = "group.changed")]
    GroupChanged {
        /// Owning project slug.
        project: String,
        /// Group slug.
        group: String,
        /// What happened: `created` | `hidden` | `shown` | `moved` | `membership`.
        change: String,
    },
    /// One restore step changed state.
    #[serde(rename = "restore.progress")]
    RestoreProgress {
        /// Project slug.
        project: String,
        /// Slot label.
        slot: String,
        /// `launching` | `ready` | `timeout` | `failed` | `skipped`.
        state: String,
        /// Slots completed so far.
        completed: usize,
        /// Total slots to launch.
        total: usize,
    },
    /// A restore run finished.
    #[serde(rename = "restore.finished")]
    RestoreFinished {
        /// Project slug.
        project: String,
        /// Existing windows adopted.
        adopted: usize,
        /// Slots launched to readiness.
        launched: usize,
        /// Labels of slots that failed, timed out, or were skipped.
        failed: Vec<String>,
    },
    /// The Hyprland event socket connected or dropped.
    #[serde(rename = "daemon.hypr_connection")]
    HyprConnection {
        /// Whether the connection is up.
        up: bool,
    },
    /// The daemon is exiting.
    #[serde(rename = "daemon.shutting_down")]
    ShuttingDown,
}

impl DomainEvent {
    /// Subscription topic this event belongs to.
    pub fn topic(&self) -> &'static str {
        match self {
            Self::WindowOpened { .. }
            | Self::WindowClosed { .. }
            | Self::WindowMoved { .. }
            | Self::WindowTitleChanged { .. }
            | Self::WindowFocused { .. }
            | Self::RuleMatched { .. } => "windows",
            Self::WorkspaceChanged { .. } => "workspaces",
            Self::ProjectCreated { .. }
            | Self::ProjectDeleted { .. }
            | Self::ProjectRenamed { .. }
            | Self::ProjectClosed { .. }
            | Self::ProjectsReordered { .. }
            | Self::ProjectSwitched { .. }
            | Self::GroupChanged { .. } => "projects",
            Self::RestoreProgress { .. } | Self::RestoreFinished { .. } => "restore",
            Self::HyprConnection { .. } | Self::ShuttingDown => "daemon",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_is_stable() {
        let event = DomainEvent::WindowOpened {
            address: "0xa".into(),
            class: "firefox".into(),
            title: "Rust".into(),
            workspace: "web-dev".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event"], "window.opened");
        assert_eq!(json["data"]["class"], "firefox");
        assert_eq!(event.topic(), "windows");

        let json = serde_json::to_value(DomainEvent::ShuttingDown).unwrap();
        assert_eq!(json["event"], "daemon.shutting_down");
    }
}
