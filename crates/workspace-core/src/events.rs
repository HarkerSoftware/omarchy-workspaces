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
            | Self::WindowFocused { .. } => "windows",
            Self::WorkspaceChanged { .. } => "workspaces",
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
