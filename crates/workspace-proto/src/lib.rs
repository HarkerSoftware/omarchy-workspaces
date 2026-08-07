//! Wire protocol between the daemon and its clients (CLI, panel).
//!
//! Transport: a Unix socket at `$XDG_RUNTIME_DIR/omarchy-workspaces/daemon.sock`
//! carrying newline-delimited JSON (one message per line, always compact).
//!
//! - Client → daemon: [`RequestEnvelope`] — `{"v":1,"id":42,"method":"…","params":{…}}`
//! - Daemon → client: [`ResponseEnvelope`] — `{"v":1,"id":42,"ok":true,"result":…}`
//! - Daemon → subscriber: [`EventEnvelope`] — `{"v":1,"seq":9,"event":"…","data":…}`
//!
//! Unknown methods yield [`error_code::UNKNOWN_METHOD`]; a different protocol
//! major version yields [`error_code::UNSUPPORTED_VERSION`]. `seq` increments
//! per pushed event so clients can detect gaps and re-query the snapshot.

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use workspace_core::DomainEvent;
use workspace_core::model::Slug;
use workspace_core::world::{MonitorInfo, TrackedWindow, WorkspaceInfo};

/// Protocol major version; the daemon rejects envelopes with a different major.
pub const PROTOCOL_VERSION: u32 = 1;

/// Well-known error codes carried in [`ErrorBody::code`].
pub mod error_code {
    /// The request could not be parsed at all.
    pub const BAD_REQUEST: &str = "BAD_REQUEST";
    /// The method name is not known to this daemon.
    pub const UNKNOWN_METHOD: &str = "UNKNOWN_METHOD";
    /// The envelope's protocol version is unsupported.
    pub const UNSUPPORTED_VERSION: &str = "UNSUPPORTED_VERSION";
    /// The referenced entity does not exist.
    pub const NOT_FOUND: &str = "NOT_FOUND";
    /// The request is valid but cannot be applied in the current state.
    pub const CONFLICT: &str = "CONFLICT";
    /// The daemon hit an internal error; details in `message`.
    pub const INTERNAL: &str = "INTERNAL";
    /// Hyprland rejected or failed a dispatch we issued for this request.
    pub const HYPRLAND: &str = "HYPRLAND";
    /// The query matched several entities; candidates in `data`.
    pub const AMBIGUOUS: &str = "AMBIGUOUS";
}

/// A request from a client.
///
/// Serialized with `method`/`params` at the top level of the envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
#[non_exhaustive]
pub enum Request {
    /// Daemon liveness/version/summary info.
    #[serde(rename = "daemon.status")]
    DaemonStatus,
    /// Full state dump for client bootstrap.
    #[serde(rename = "state.snapshot")]
    StateSnapshot,
    /// Start pushing events over this connection.
    #[serde(rename = "subscribe")]
    Subscribe {
        /// Topics to receive (`windows`, `workspaces`, `projects`, `daemon`);
        /// `None` means all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        topics: Option<Vec<String>>,
    },
    /// Create a project.
    #[serde(rename = "project.create")]
    ProjectCreate {
        /// Display name ("Web Development").
        name: String,
        /// Explicit slug; derived from the name when omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slug: Option<String>,
    },
    /// Delete a project. Takes an exact slug — never fuzzy.
    #[serde(rename = "project.delete")]
    ProjectDelete {
        /// Exact project slug.
        slug: String,
    },
    /// Rename a project's display name (slug unchanged).
    #[serde(rename = "project.rename")]
    ProjectRename {
        /// Exact project slug.
        slug: String,
        /// New display name.
        name: String,
    },
    /// Switch to a project (fuzzy query allowed).
    #[serde(rename = "project.switch")]
    ProjectSwitch {
        /// Slug, prefix, or fuzzy query.
        project: String,
    },
    /// List all projects.
    #[serde(rename = "project.list")]
    ProjectList,
    /// Capture a project's current windows as declarative app slots and
    /// persist its file.
    #[serde(rename = "project.save")]
    ProjectSave {
        /// Project query; defaults to the active project.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    /// Restore a project: adopt matching windows and launch what is missing.
    #[serde(rename = "project.restore")]
    ProjectRestore {
        /// Project query; defaults to the active project.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        /// Return the plan without executing it.
        #[serde(default)]
        dry_run: bool,
    },
    /// Dry-run the rules engine against a window.
    #[serde(rename = "rules.test")]
    RulesTest {
        /// Window address; defaults to the focused window.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
    },
    /// Re-read config.toml and rules.toml from disk; invalid files leave the
    /// running configuration untouched.
    #[serde(rename = "config.reload")]
    ConfigReload,
    /// Create a group inside a project.
    #[serde(rename = "group.create")]
    GroupCreate {
        /// Project query.
        project: String,
        /// Group display name.
        name: String,
        /// Explicit slug; derived from the name when omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slug: Option<String>,
    },
    /// Put a window into a group.
    #[serde(rename = "group.add")]
    GroupAdd {
        /// Project query.
        project: String,
        /// Group slug.
        group: String,
        /// Canonical window address.
        address: String,
    },
    /// Take a window out of its group (it stays in the project).
    #[serde(rename = "group.remove")]
    GroupRemove {
        /// Project query.
        project: String,
        /// Group slug.
        group: String,
        /// Canonical window address.
        address: String,
    },
    /// Park a group's windows on its parking workspace.
    #[serde(rename = "group.hide")]
    GroupHide {
        /// Project query.
        project: String,
        /// Group slug.
        group: String,
    },
    /// Bring a group's windows back to the project workspace.
    #[serde(rename = "group.show")]
    GroupShow {
        /// Project query.
        project: String,
        /// Group slug.
        group: String,
    },
    /// Show a group (if hidden) and focus one of its windows.
    #[serde(rename = "group.focus")]
    GroupFocus {
        /// Project query.
        project: String,
        /// Group slug.
        group: String,
    },
    /// Move a group (definition and windows) to another project.
    #[serde(rename = "group.move")]
    GroupMove {
        /// Source project query.
        project: String,
        /// Group slug.
        group: String,
        /// Destination project query.
        to: String,
    },
    /// Assign a window to a project (and optionally a group) manually.
    #[serde(rename = "window.assign")]
    WindowAssign {
        /// Canonical window address.
        address: String,
        /// Project slug, prefix, or fuzzy query.
        project: String,
        /// Group slug within the project.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
    },
}

/// Client → daemon message envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// Protocol major version.
    pub v: u32,
    /// Client-chosen id echoed in the response.
    pub id: u64,
    /// The request itself (`method` + `params`).
    #[serde(flatten)]
    pub request: Request,
}

/// Error payload in a failed response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Machine-readable code from [`error_code`].
    pub code: String,
    /// Human-readable description.
    pub message: String,
    /// Optional structured details (e.g. fuzzy-match candidates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Daemon → client response envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    /// Protocol major version.
    pub v: u32,
    /// Echo of the request id.
    pub id: u64,
    /// Whether the request succeeded.
    pub ok: bool,
    /// Success payload (shape depends on the method).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Failure payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl ResponseEnvelope {
    /// Build a success response, serializing `result`.
    pub fn success<T: Serialize>(id: u64, result: &T) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            ok: true,
            result: Some(serde_json::to_value(result).expect("result serializes")),
            error: None,
        }
    }

    /// Build a failure response.
    pub fn failure(id: u64, code: &str, message: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            ok: false,
            result: None,
            error: Some(ErrorBody {
                code: code.to_owned(),
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Daemon → subscriber pushed event envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Protocol major version.
    pub v: u32,
    /// Monotonic event counter (gap detection).
    pub seq: u64,
    /// The event (`event` + `data`).
    #[serde(flatten)]
    pub data: DomainEvent,
}

/// Any single line a client can receive from the daemon.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    /// A reply to one of our requests.
    Response(ResponseEnvelope),
    /// A pushed event (only after `subscribe`).
    Event(EventEnvelope),
}

/// Result of `daemon.status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Daemon semver.
    pub version: String,
    /// Seconds since the daemon started.
    pub uptime_s: u64,
    /// Whether the Hyprland event socket is connected.
    pub hypr_connected: bool,
    /// Slug of the active project, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_project: Option<Slug>,
    /// Number of tracked windows.
    pub windows: usize,
    /// Number of known projects.
    pub projects: usize,
}

/// One project in `project.list` results and events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectSummary {
    /// Project slug.
    pub slug: Slug,
    /// Display name.
    pub name: String,
    /// Whether this is the active project.
    pub active: bool,
    /// Number of windows currently assigned to it.
    pub windows: usize,
    /// Group slugs defined in the project.
    #[serde(default)]
    pub groups: Vec<Slug>,
    /// The Hyprland workspace name backing the project.
    pub workspace: String,
}

/// Result of `state.snapshot`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// All projects, in creation order.
    #[serde(default)]
    pub projects: Vec<ProjectSummary>,
    /// All tracked windows, sorted by address for deterministic output.
    pub windows: Vec<TrackedWindow>,
    /// All live workspaces, sorted by id.
    pub workspaces: Vec<WorkspaceInfo>,
    /// All live monitors.
    pub monitors: Vec<MonitorInfo>,
    /// Address of the focused window, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_window: Option<String>,
    /// Whether the Hyprland event socket is connected.
    pub hypr_connected: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_wire_shape() {
        let envelope = RequestEnvelope {
            v: 1,
            id: 42,
            request: Request::Subscribe {
                topics: Some(vec!["windows".into()]),
            },
        };
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["method"], "subscribe");
        assert_eq!(json["params"]["topics"][0], "windows");

        let parsed: RequestEnvelope =
            serde_json::from_str(r#"{"v":1,"id":7,"method":"daemon.status"}"#).unwrap();
        assert_eq!(parsed.request, Request::DaemonStatus);
    }

    #[test]
    fn server_message_disambiguates() {
        let response: ServerMessage =
            serde_json::from_str(r#"{"v":1,"id":1,"ok":true,"result":{}}"#).unwrap();
        assert!(matches!(response, ServerMessage::Response(_)));

        let event: ServerMessage = serde_json::from_str(
            r#"{"v":1,"seq":9,"event":"window.closed","data":{"address":"0xa"}}"#,
        )
        .unwrap();
        match event {
            ServerMessage::Event(e) => {
                assert_eq!(e.seq, 9);
                assert_eq!(
                    e.data,
                    DomainEvent::WindowClosed {
                        address: "0xa".into()
                    }
                );
            }
            other => panic!("expected event, got {other:?}"),
        }
    }

    #[test]
    fn unknown_method_fails_parse_but_envelope_id_recoverable() {
        // The daemon parses the envelope loosely first to recover the id, then
        // the typed request; this test pins the loose-parse contract.
        let raw = r#"{"v":1,"id":5,"method":"future.method","params":{}}"#;
        assert!(serde_json::from_str::<RequestEnvelope>(raw).is_err());
        let loose: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(loose["id"], 5);
    }
}
