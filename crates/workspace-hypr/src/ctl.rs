//! Request/response client for Hyprland's control socket.
//!
//! Protocol (what `hyprctl` speaks): connect, write the request string
//! (`j/clients`, `dispatch workspace 2`, `[[BATCH]]…`), read the reply until
//! Hyprland closes the connection. One connection per request.

use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::HyprError;
use crate::model::{Client, Monitor, WindowAddress, Workspace};
use crate::socket::HyprPaths;

/// A workspace target in dispatcher grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsTarget {
    /// Numeric workspace id.
    Id(i64),
    /// Named workspace (`name:<name>` on the wire).
    Name(String),
    /// The previously focused workspace.
    Previous,
}

impl std::fmt::Display for WsTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Id(id) => write!(f, "{id}"),
            Self::Name(name) => write!(f, "name:{name}"),
            Self::Previous => f.write_str("previous"),
        }
    }
}

/// A cardinal direction for directional dispatchers (`swapwindow`,
/// `movewindow`, `movefocus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDir {
    /// Left.
    Left,
    /// Right.
    Right,
    /// Up.
    Up,
    /// Down.
    Down,
}

impl std::fmt::Display for MoveDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Left => "l",
            Self::Right => "r",
            Self::Up => "u",
            Self::Down => "d",
        })
    }
}

/// A typed Hyprland dispatcher. Rendered to the wire string by [`Dispatch::render`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    /// Focus a workspace.
    Workspace(WsTarget),
    /// Move a window to a workspace without following it.
    MoveToWorkspaceSilent {
        /// Destination workspace.
        target: WsTarget,
        /// Window to move.
        address: WindowAddress,
    },
    /// Move a window to a workspace and follow it.
    MoveToWorkspace {
        /// Destination workspace.
        target: WsTarget,
        /// Window to move.
        address: WindowAddress,
    },
    /// Focus a specific window.
    FocusWindow(WindowAddress),
    /// Ask a window's client to close (graceful, like clicking the X).
    CloseWindow(WindowAddress),
    /// Swap the focused window with its neighbor in a direction (tiled).
    SwapWindow(MoveDir),
    /// Move the focused window in a direction; toward an empty edge this
    /// re-splits the layout in that orientation (side-by-side ⇄ stacked).
    MoveWindowDir(MoveDir),
    /// Toggle the focused window's split orientation (dwindle; requires
    /// settled focus — batching it with `focuswindow` races).
    ToggleSplit,
    /// Resize the focused window to an exact size; on tiled windows this
    /// adjusts the surrounding split ratios.
    ResizeActiveExact {
        /// Width in pixels.
        w: i32,
        /// Height in pixels.
        h: i32,
    },
    /// Launch a command, optionally with exec rules like `workspace name:x silent`.
    Exec {
        /// Exec rules placed in `[…]` before the command.
        rules: Vec<String>,
        /// The command line to run.
        command: String,
    },
    /// Set a window floating.
    SetFloating(WindowAddress),
    /// Set a window tiled.
    SetTiled(WindowAddress),
    /// Move a floating window to an exact position.
    MoveWindowPixelExact {
        /// Window to move.
        address: WindowAddress,
        /// Absolute x.
        x: i32,
        /// Absolute y.
        y: i32,
    },
    /// Resize a floating window to an exact size.
    ResizeWindowPixelExact {
        /// Window to resize.
        address: WindowAddress,
        /// Width in pixels.
        w: i32,
        /// Height in pixels.
        h: i32,
    },
    /// Move a workspace to a monitor by output name.
    MoveWorkspaceToMonitor {
        /// Workspace to move.
        workspace: WsTarget,
        /// Destination output name.
        monitor: String,
    },
    /// An escape hatch for dispatchers not yet modeled.
    Raw(String),
}

impl Dispatch {
    /// Render to the dispatcher argument string (without the `dispatch ` prefix).
    pub fn render(&self) -> String {
        match self {
            Self::Workspace(t) => format!("workspace {t}"),
            Self::MoveToWorkspaceSilent { target, address } => {
                format!("movetoworkspacesilent {target},{}", address.dispatch_arg())
            }
            Self::MoveToWorkspace { target, address } => {
                format!("movetoworkspace {target},{}", address.dispatch_arg())
            }
            Self::FocusWindow(address) => format!("focuswindow {}", address.dispatch_arg()),
            Self::CloseWindow(address) => format!("closewindow {}", address.dispatch_arg()),
            Self::SwapWindow(dir) => format!("swapwindow {dir}"),
            Self::MoveWindowDir(dir) => format!("movewindow {dir}"),
            Self::ToggleSplit => "layoutmsg togglesplit".to_owned(),
            Self::ResizeActiveExact { w, h } => format!("resizeactive exact {w} {h}"),
            Self::Exec { rules, command } => {
                if rules.is_empty() {
                    format!("exec {command}")
                } else {
                    format!("exec [{}] {command}", rules.join("; "))
                }
            }
            Self::SetFloating(address) => format!("setfloating {}", address.dispatch_arg()),
            Self::SetTiled(address) => format!("settiled {}", address.dispatch_arg()),
            Self::MoveWindowPixelExact { address, x, y } => {
                format!("movewindowpixel exact {x} {y},{}", address.dispatch_arg())
            }
            Self::ResizeWindowPixelExact { address, w, h } => {
                format!("resizewindowpixel exact {w} {h},{}", address.dispatch_arg())
            }
            Self::MoveWorkspaceToMonitor { workspace, monitor } => {
                format!("moveworkspacetomonitor {workspace} {monitor}")
            }
            Self::Raw(s) => s.clone(),
        }
    }
}

/// Client for the Hyprland control socket.
#[derive(Debug, Clone)]
pub struct HyprCtl {
    paths: HyprPaths,
}

impl HyprCtl {
    /// Create a client for the given socket paths.
    pub fn new(paths: HyprPaths) -> Self {
        Self { paths }
    }

    /// Send a raw request string and return the raw reply bytes.
    pub async fn raw_request(&self, request: &str) -> Result<Vec<u8>, HyprError> {
        let mut stream = UnixStream::connect(&self.paths.ctl).await?;
        stream.write_all(request.as_bytes()).await?;
        let mut reply = Vec::with_capacity(4096);
        stream.read_to_end(&mut reply).await?;
        Ok(reply)
    }

    async fn json_request<T: DeserializeOwned>(&self, request: &str) -> Result<T, HyprError> {
        let reply = self.raw_request(request).await?;
        serde_json::from_slice(&reply).map_err(|source| HyprError::Decode {
            request: request.to_owned(),
            source,
        })
    }

    /// All windows.
    pub async fn clients(&self) -> Result<Vec<Client>, HyprError> {
        self.json_request("j/clients").await
    }

    /// All workspaces.
    pub async fn workspaces(&self) -> Result<Vec<Workspace>, HyprError> {
        self.json_request("j/workspaces").await
    }

    /// All monitors.
    pub async fn monitors(&self) -> Result<Vec<Monitor>, HyprError> {
        self.json_request("j/monitors").await
    }

    /// The focused window, if any. Hyprland replies with an empty object when
    /// no window is focused, which fails to decode as a `Client`.
    pub async fn active_window(&self) -> Result<Option<Client>, HyprError> {
        let reply = self.raw_request("j/activewindow").await?;
        Ok(serde_json::from_slice(&reply).ok())
    }

    /// Hyprland version info, as raw JSON (used by `doctor`).
    pub async fn version(&self) -> Result<serde_json::Value, HyprError> {
        self.json_request("j/version").await
    }

    /// Execute one dispatcher.
    pub async fn dispatch(&self, dispatch: &Dispatch) -> Result<(), HyprError> {
        let request = format!("dispatch {}", dispatch.render());
        let reply = self.raw_request(&request).await?;
        Self::check_ok(&request, &reply)
    }

    /// Execute several dispatchers atomically via `[[BATCH]]`.
    pub async fn dispatch_batch(&self, dispatches: &[Dispatch]) -> Result<(), HyprError> {
        if dispatches.is_empty() {
            return Ok(());
        }
        let body = dispatches
            .iter()
            .map(|d| format!("dispatch {}", d.render()))
            .collect::<Vec<_>>()
            .join("; ");
        let request = format!("[[BATCH]]{body}");
        let reply = self.raw_request(&request).await?;
        Self::check_ok(&request, &reply)
    }

    /// Hyprland replies `ok` (batches: one `ok` per command) on success and an
    /// error description otherwise.
    fn check_ok(request: &str, reply: &[u8]) -> Result<(), HyprError> {
        let text = String::from_utf8_lossy(reply);
        let all_ok = text
            .split(['\n', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .all(|s| s == "ok");
        if all_ok {
            Ok(())
        } else {
            Err(HyprError::DispatchFailed {
                request: request.to_owned(),
                reply: text.into_owned(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> WindowAddress {
        WindowAddress::new("0x556bf8bdd060")
    }

    #[test]
    fn dispatch_rendering() {
        let cases = [
            (
                Dispatch::Workspace(WsTarget::Name("web-dev".into())),
                "workspace name:web-dev",
            ),
            (Dispatch::Workspace(WsTarget::Id(3)), "workspace 3"),
            (
                Dispatch::Workspace(WsTarget::Previous),
                "workspace previous",
            ),
            (
                Dispatch::MoveToWorkspaceSilent {
                    target: WsTarget::Name("web-dev:backend".into()),
                    address: addr(),
                },
                "movetoworkspacesilent name:web-dev:backend,address:0x556bf8bdd060",
            ),
            (
                Dispatch::FocusWindow(addr()),
                "focuswindow address:0x556bf8bdd060",
            ),
            (
                Dispatch::Exec {
                    rules: vec!["workspace name:web-dev silent".into()],
                    command: "firefox".into(),
                },
                "exec [workspace name:web-dev silent] firefox",
            ),
            (
                Dispatch::Exec {
                    rules: vec![],
                    command: "kitty".into(),
                },
                "exec kitty",
            ),
            (
                Dispatch::MoveWindowPixelExact {
                    address: addr(),
                    x: 100,
                    y: 200,
                },
                "movewindowpixel exact 100 200,address:0x556bf8bdd060",
            ),
            (
                Dispatch::MoveWorkspaceToMonitor {
                    workspace: WsTarget::Name("ml".into()),
                    monitor: "DP-1".into(),
                },
                "moveworkspacetomonitor name:ml DP-1",
            ),
        ];
        for (dispatch, want) in cases {
            assert_eq!(dispatch.render(), want);
        }
    }

    #[test]
    fn check_ok_accepts_single_and_batch() {
        assert!(HyprCtl::check_ok("r", b"ok").is_ok());
        assert!(HyprCtl::check_ok("r", b"ok\nok\nok").is_ok());
        let err = HyprCtl::check_ok("r", b"Invalid dispatcher").unwrap_err();
        assert!(matches!(err, HyprError::DispatchFailed { .. }));
    }
}
