//! Typed models for Hyprland's JSON output, tolerant of schema drift.
//!
//! Hyprland adds and reshapes fields between releases, so every struct keeps
//! serde's default unknown-field tolerance, marks non-essential fields
//! `#[serde(default)]`, and captures anything unrecognized in an `extra` map
//! for forward compatibility.

use serde::{Deserialize, Deserializer, Serialize};

/// A window address, canonicalized to lowercase `0x…` hex.
///
/// `hyprctl -j clients` reports addresses as `"0x556bf8bdd060"` while event
/// lines report them without the `0x` prefix; both normalize to the same
/// canonical form here. This is the unique per-session window key (pids are
/// shared between windows of single-process apps).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct WindowAddress(String);

impl WindowAddress {
    /// Normalize an address from either JSON (`0x…`) or event (`…`) form.
    pub fn new(raw: &str) -> Self {
        let hex = raw.trim().trim_start_matches("0x").to_ascii_lowercase();
        Self(format!("0x{hex}"))
    }

    /// The canonical `0x…` form.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The form used in dispatcher arguments, e.g. `address:0x556bf8bdd060`.
    pub fn dispatch_arg(&self) -> String {
        format!("address:{}", self.0)
    }
}

impl std::fmt::Display for WindowAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `pad` honors width/alignment flags (needed for table output).
        f.pad(&self.0)
    }
}

impl<'de> Deserialize<'de> for WindowAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::new(&raw))
    }
}

/// The `{id, name}` workspace reference embedded in clients and monitors.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRef {
    /// Workspace id; negative for named workspaces.
    pub id: i64,
    /// Workspace name; numeric string for numeric workspaces.
    #[serde(default)]
    pub name: String,
}

/// Accept a fullscreen value that Hyprland has represented as both a bool and
/// an int across releases; normalize to the 0.56 int semantics (0/1/2).
fn fullscreen_mode<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Int(u8),
        Bool(bool),
    }
    Ok(match Raw::deserialize(deserializer)? {
        Raw::Int(v) => v,
        Raw::Bool(true) => 2,
        Raw::Bool(false) => 0,
    })
}

/// One window, as reported by `j/clients` and `j/activewindow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    /// Unique per-session window key.
    pub address: WindowAddress,
    /// Whether the surface is currently mapped.
    #[serde(default)]
    pub mapped: bool,
    /// Whether Hyprland considers the window hidden.
    #[serde(default)]
    pub hidden: bool,
    /// Top-left corner `[x, y]`.
    #[serde(default)]
    pub at: (i32, i32),
    /// Size `[w, h]`.
    #[serde(default)]
    pub size: (i32, i32),
    /// Workspace the window is on.
    #[serde(default)]
    pub workspace: WorkspaceRef,
    /// Whether the window floats.
    #[serde(default)]
    pub floating: bool,
    /// Whether the window is pinned to all workspaces.
    #[serde(default)]
    pub pinned: bool,
    /// Fullscreen mode: 0 = none, 1 = maximize, 2 = fullscreen.
    #[serde(default, deserialize_with = "fullscreen_mode")]
    pub fullscreen: u8,
    /// Monitor id the window is on; -1 when unmapped.
    #[serde(default)]
    pub monitor: i64,
    /// Current window class.
    #[serde(default)]
    pub class: String,
    /// Current window title.
    #[serde(default)]
    pub title: String,
    /// Class at map time; survives runtime class changes.
    #[serde(default)]
    pub initial_class: String,
    /// Title at map time.
    #[serde(default)]
    pub initial_title: String,
    /// Process id; NOT unique per window.
    #[serde(default)]
    pub pid: i32,
    /// Whether the window runs under XWayland.
    #[serde(default)]
    pub xwayland: bool,
    /// Addresses of windows sharing a Hyprland tab group with this one.
    #[serde(default)]
    pub grouped: Vec<WindowAddress>,
    /// Position in the global focus history (0 = focused).
    #[serde(rename = "focusHistoryID", default = "neg_one")]
    pub focus_history_id: i32,
    /// Persistent per-window id (Hyprland ≥ 0.5x); session-scoped for us.
    #[serde(default)]
    pub stable_id: Option<String>,
    /// Unrecognized fields, preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn neg_one() -> i32 {
    -1
}

/// One workspace, as reported by `j/workspaces`. Field names in this reply are
/// lowercase rather than camelCase, hence the per-field renames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// Workspace id; negative for named workspaces, sparse in practice.
    pub id: i64,
    /// Workspace name.
    #[serde(default)]
    pub name: String,
    /// Output name the workspace is on (e.g. `DP-1`).
    #[serde(default)]
    pub monitor: String,
    /// Monitor id.
    #[serde(rename = "monitorID", default = "neg_one_i64")]
    pub monitor_id: i64,
    /// Number of windows on the workspace.
    #[serde(default)]
    pub windows: u32,
    /// Whether any window on it is fullscreen.
    #[serde(rename = "hasfullscreen", default)]
    pub has_fullscreen: bool,
    /// Address of the last focused window on the workspace.
    #[serde(rename = "lastwindow", default)]
    pub last_window: String,
    /// Whether the workspace is persistent.
    #[serde(rename = "ispersistent", default)]
    pub is_persistent: bool,
    /// Unrecognized fields, preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn neg_one_i64() -> i64 {
    -1
}

/// One monitor, as reported by `j/monitors`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    /// Monitor id.
    pub id: i64,
    /// Output name (e.g. `DP-1`).
    #[serde(default)]
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Whether this monitor has pointer focus.
    #[serde(default)]
    pub focused: bool,
    /// The active regular workspace.
    #[serde(default)]
    pub active_workspace: WorkspaceRef,
    /// The active special workspace (id 0 when none).
    #[serde(default)]
    pub special_workspace: WorkspaceRef,
    /// Position of the monitor in layout space.
    #[serde(default)]
    pub x: i32,
    /// Position of the monitor in layout space.
    #[serde(default)]
    pub y: i32,
    /// Mode width in pixels.
    #[serde(default)]
    pub width: u32,
    /// Mode height in pixels.
    #[serde(default)]
    pub height: u32,
    /// Output scale factor.
    #[serde(default)]
    pub scale: f64,
    /// Unrecognized fields, preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_address_normalizes_both_forms() {
        let from_json = WindowAddress::new("0x556BF8BDD060");
        let from_event = WindowAddress::new("556bf8bdd060");
        assert_eq!(from_json, from_event);
        assert_eq!(from_json.as_str(), "0x556bf8bdd060");
        assert_eq!(from_json.dispatch_arg(), "address:0x556bf8bdd060");
    }

    #[test]
    fn client_parses_live_0_56_shape() {
        // Trimmed from real `hyprctl -j clients` output on Hyprland 0.56,
        // including fields we do not model (tags, xdgTag, …).
        let json = r#"{
            "address": "0x556bf8bdd060",
            "mapped": true, "hidden": false,
            "at": [12, 65], "size": [3416, 1363],
            "workspace": {"id": 6, "name": "6"},
            "floating": false, "pinned": false,
            "fullscreen": 0, "fullscreenClient": 0,
            "monitor": 0,
            "class": "Code", "title": "model.rs — omarchy, workspaces",
            "initialClass": "Code", "initialTitle": "Visual Studio Code",
            "pid": 72605, "xwayland": false,
            "grouped": ["0x556bf8bdd060", "0x556bf8aa0000"],
            "tags": ["default-opacity*"],
            "focusHistoryID": 0,
            "inhibitingIdle": false,
            "xdgTag": "", "contentType": "none",
            "stableId": "180011c6"
        }"#;
        let c: Client = serde_json::from_str(json).unwrap();
        assert_eq!(c.address.as_str(), "0x556bf8bdd060");
        assert_eq!(c.at, (12, 65));
        assert_eq!(c.size, (3416, 1363));
        assert_eq!(c.workspace.id, 6);
        assert_eq!(c.initial_class, "Code");
        assert_eq!(c.stable_id.as_deref(), Some("180011c6"));
        assert_eq!(c.grouped.len(), 2);
        // Unknown fields land in extra, not on the floor.
        assert!(c.extra.contains_key("tags"));
    }

    #[test]
    fn client_tolerates_older_schema() {
        // Bool fullscreen, no stableId, missing optional fields.
        let json = r#"{
            "address": "0x1",
            "at": [0,0], "size": [1,1],
            "workspace": {"id": 1, "name": "1"},
            "fullscreen": true,
            "class": "foo", "title": "bar",
            "pid": 1
        }"#;
        let c: Client = serde_json::from_str(json).unwrap();
        assert_eq!(c.fullscreen, 2);
        assert_eq!(c.stable_id, None);
        assert_eq!(c.focus_history_id, -1);
    }

    #[test]
    fn workspace_parses_live_shape() {
        let json = r#"{
            "id": -1337, "name": "web-dev", "monitor": "DP-1", "monitorID": 0,
            "windows": 3, "hasfullscreen": false,
            "lastwindow": "0x556bf8bdd060", "lastwindowtitle": "Firefox",
            "ispersistent": false, "tiledLayout": "dwindle"
        }"#;
        let w: Workspace = serde_json::from_str(json).unwrap();
        assert_eq!(w.id, -1337);
        assert_eq!(w.name, "web-dev");
        assert!(!w.has_fullscreen);
        assert!(w.extra.contains_key("tiledLayout"));
    }

    #[test]
    fn monitor_parses_live_shape() {
        let json = r#"{
            "id": 0, "name": "DP-1", "description": "LG ULTRAGEAR+",
            "focused": true,
            "activeWorkspace": {"id": 6, "name": "6"},
            "specialWorkspace": {"id": 0, "name": ""},
            "x": 0, "y": 0, "width": 3440, "height": 1440,
            "scale": 1.0, "reserved": [0, 26, 0, 0]
        }"#;
        let m: Monitor = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "DP-1");
        assert_eq!(m.active_workspace.id, 6);
        assert_eq!((m.width, m.height), (3440, 1440));
    }
}
