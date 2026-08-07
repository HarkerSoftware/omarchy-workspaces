//! The Hyprland event stream (`.socket2.sock`): line parsing and a
//! reconnecting pump.
//!
//! Wire format: `EVENT>>DATA\n`, with comma-separated fields in `DATA`.
//! Fields that can contain commas (titles, commands) are always last, so
//! parsing uses `splitn` with known field counts. Where Hyprland offers `v2`
//! variants we consume those and ignore the legacy duplicates.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::model::WindowAddress;
use crate::socket::HyprPaths;

/// A parsed Hyprland event.
///
/// Only events the daemon reacts to are typed; everything else surfaces as
/// [`HyprEvent::Unknown`] so new Hyprland releases never break parsing.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum HyprEvent {
    /// A window was mapped. Carries no pid/geometry — callers should follow up
    /// with a `clients` fetch.
    OpenWindow {
        /// The new window.
        address: WindowAddress,
        /// Workspace name it opened on.
        workspace: String,
        /// Window class.
        class: String,
        /// Window title (may contain commas; parsed last).
        title: String,
    },
    /// A window was unmapped.
    CloseWindow {
        /// The closed window.
        address: WindowAddress,
    },
    /// A window moved to another workspace (`movewindowv2`).
    MoveWindow {
        /// The moved window.
        address: WindowAddress,
        /// Destination workspace id.
        workspace_id: i64,
        /// Destination workspace name.
        workspace: String,
    },
    /// A window's title changed (`windowtitlev2`).
    WindowTitle {
        /// The window.
        address: WindowAddress,
        /// The new title.
        title: String,
    },
    /// Keyboard focus moved to a window (`activewindowv2`); `None` when focus
    /// left all windows.
    ActiveWindow {
        /// Focused window, if any.
        address: Option<WindowAddress>,
    },
    /// A window's floating state changed.
    ChangeFloatingMode {
        /// The window.
        address: WindowAddress,
        /// Whether it now floats.
        floating: bool,
    },
    /// A window's pin state changed.
    Pin {
        /// The window.
        address: WindowAddress,
        /// Whether it is now pinned.
        pinned: bool,
    },
    /// The focused workspace changed (`workspacev2`).
    Workspace {
        /// Workspace id.
        id: i64,
        /// Workspace name.
        name: String,
    },
    /// A workspace was created (`createworkspacev2`).
    CreateWorkspace {
        /// Workspace id.
        id: i64,
        /// Workspace name.
        name: String,
    },
    /// A workspace was destroyed (`destroyworkspacev2`).
    DestroyWorkspace {
        /// Workspace id.
        id: i64,
        /// Workspace name.
        name: String,
    },
    /// A workspace moved to another monitor (`moveworkspacev2`).
    MoveWorkspace {
        /// Workspace id.
        id: i64,
        /// Workspace name.
        name: String,
        /// Destination output name.
        monitor: String,
    },
    /// A workspace was renamed.
    RenameWorkspace {
        /// Workspace id.
        id: i64,
        /// The new name.
        name: String,
    },
    /// Monitor focus changed (`focusedmonv2`).
    FocusedMonitor {
        /// Output name.
        monitor: String,
        /// Active workspace id on it.
        workspace_id: i64,
    },
    /// A monitor was connected (`monitoraddedv2`).
    MonitorAdded {
        /// Monitor id.
        id: i64,
        /// Output name.
        name: String,
    },
    /// A monitor was disconnected (`monitorremovedv2`).
    MonitorRemoved {
        /// Monitor id.
        id: i64,
        /// Output name.
        name: String,
    },
    /// The focused window changed fullscreen state. The event does not say
    /// which window; callers consult their focus state.
    Fullscreen {
        /// Whether a window is now fullscreen.
        active: bool,
    },
    /// Hyprland reloaded its configuration.
    ConfigReloaded,
    /// A window requested attention.
    Urgent {
        /// The window.
        address: WindowAddress,
    },
    /// Any event we do not model; kept for logging and forward compatibility.
    Unknown {
        /// Event name before `>>`.
        name: String,
        /// Raw payload after `>>`.
        data: String,
    },
}

impl HyprEvent {
    /// Parse one line of the event stream. Never fails: unrecognized or
    /// malformed lines become [`HyprEvent::Unknown`].
    pub fn parse(line: &str) -> Self {
        let Some((name, data)) = line.split_once(">>") else {
            return Self::Unknown {
                name: line.to_owned(),
                data: String::new(),
            };
        };
        Self::parse_parts(name, data).unwrap_or_else(|| Self::Unknown {
            name: name.to_owned(),
            data: data.to_owned(),
        })
    }

    /// `Some` when the event is recognized and well-formed.
    fn parse_parts(name: &str, data: &str) -> Option<Self> {
        Some(match name {
            "openwindow" => {
                let mut f = data.splitn(4, ',');
                Self::OpenWindow {
                    address: WindowAddress::new(f.next()?),
                    workspace: f.next()?.to_owned(),
                    class: f.next()?.to_owned(),
                    title: f.next()?.to_owned(),
                }
            }
            "closewindow" => Self::CloseWindow {
                address: WindowAddress::new(data),
            },
            "movewindowv2" => {
                let mut f = data.splitn(3, ',');
                Self::MoveWindow {
                    address: WindowAddress::new(f.next()?),
                    workspace_id: f.next()?.parse().ok()?,
                    workspace: f.next()?.to_owned(),
                }
            }
            "windowtitlev2" => {
                let (addr, title) = data.split_once(',')?;
                Self::WindowTitle {
                    address: WindowAddress::new(addr),
                    title: title.to_owned(),
                }
            }
            "activewindowv2" => Self::ActiveWindow {
                address: (!data.is_empty() && data != ",").then(|| WindowAddress::new(data)),
            },
            "changefloatingmode" => {
                let (addr, mode) = data.split_once(',')?;
                Self::ChangeFloatingMode {
                    address: WindowAddress::new(addr),
                    floating: mode.trim() == "1",
                }
            }
            "pin" => {
                let (addr, mode) = data.split_once(',')?;
                Self::Pin {
                    address: WindowAddress::new(addr),
                    pinned: mode.trim() == "1",
                }
            }
            "workspacev2" => {
                let (id, name) = data.split_once(',')?;
                Self::Workspace {
                    id: id.parse().ok()?,
                    name: name.to_owned(),
                }
            }
            "createworkspacev2" => {
                let (id, name) = data.split_once(',')?;
                Self::CreateWorkspace {
                    id: id.parse().ok()?,
                    name: name.to_owned(),
                }
            }
            "destroyworkspacev2" => {
                let (id, name) = data.split_once(',')?;
                Self::DestroyWorkspace {
                    id: id.parse().ok()?,
                    name: name.to_owned(),
                }
            }
            "moveworkspacev2" => {
                let mut f = data.splitn(3, ',');
                Self::MoveWorkspace {
                    id: f.next()?.parse().ok()?,
                    name: f.next()?.to_owned(),
                    monitor: f.next()?.to_owned(),
                }
            }
            "renameworkspace" => {
                let (id, name) = data.split_once(',')?;
                Self::RenameWorkspace {
                    id: id.parse().ok()?,
                    name: name.to_owned(),
                }
            }
            "focusedmonv2" => {
                let (monitor, ws) = data.split_once(',')?;
                Self::FocusedMonitor {
                    monitor: monitor.to_owned(),
                    workspace_id: ws.parse().ok()?,
                }
            }
            "monitoraddedv2" => {
                let mut f = data.splitn(3, ',');
                Self::MonitorAdded {
                    id: f.next()?.parse().ok()?,
                    name: f.next()?.to_owned(),
                }
            }
            "monitorremovedv2" => {
                let mut f = data.splitn(3, ',');
                Self::MonitorRemoved {
                    id: f.next()?.parse().ok()?,
                    name: f.next()?.to_owned(),
                }
            }
            "fullscreen" => Self::Fullscreen {
                active: data.trim() == "1",
            },
            "configreloaded" => Self::ConfigReloaded,
            "urgent" => Self::Urgent {
                address: WindowAddress::new(data),
            },
            _ => return None,
        })
    }
}

/// Items produced by [`run_event_pump`].
#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
    /// The event socket (re)connected. Callers should re-hydrate state.
    Connected,
    /// A parsed event.
    Event(HyprEvent),
    /// The event socket dropped; the pump will reconnect with backoff.
    Disconnected,
}

/// Exponential reconnect backoff: 250 ms doubling to a 5 s cap.
fn backoff_delay(attempt: u32) -> Duration {
    let base = Duration::from_millis(250);
    let capped = base.saturating_mul(2u32.saturating_pow(attempt.min(16)));
    capped.min(Duration::from_secs(5))
}

/// Connect to the event socket and pump parsed events into `tx`, reconnecting
/// with backoff forever. Returns when `tx`'s receiver is dropped.
pub async fn run_event_pump(paths: HyprPaths, tx: mpsc::Sender<StreamItem>) {
    let mut attempt: u32 = 0;
    loop {
        match UnixStream::connect(&paths.events).await {
            Ok(stream) => {
                attempt = 0;
                if tx.send(StreamItem::Connected).await.is_err() {
                    return;
                }
                let mut lines = BufReader::new(stream).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let event = HyprEvent::parse(&line);
                            if let HyprEvent::Unknown { name, data } = &event {
                                tracing::trace!(name, data, "unrecognized hyprland event");
                            }
                            if tx.send(StreamItem::Event(event)).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            tracing::debug!(%error, "event stream read failed");
                            break;
                        }
                    }
                }
                if tx.send(StreamItem::Disconnected).await.is_err() {
                    return;
                }
            }
            Err(error) => {
                tracing::debug!(%error, "event socket connect failed");
                if tx.is_closed() {
                    return;
                }
            }
        }
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(backoff_delay(attempt)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> WindowAddress {
        WindowAddress::new(s)
    }

    #[test]
    fn parses_typed_events() {
        let cases: Vec<(&str, HyprEvent)> = vec![
            (
                "openwindow>>80e63f30,web-dev,firefox,Rust — Mozilla Firefox",
                HyprEvent::OpenWindow {
                    address: addr("80e63f30"),
                    workspace: "web-dev".into(),
                    class: "firefox".into(),
                    title: "Rust — Mozilla Firefox".into(),
                },
            ),
            (
                // Commas in the title must survive: title is the last field.
                "openwindow>>80e63f30,1,Code,main.rs — a, b, and c",
                HyprEvent::OpenWindow {
                    address: addr("80e63f30"),
                    workspace: "1".into(),
                    class: "Code".into(),
                    title: "main.rs — a, b, and c".into(),
                },
            ),
            (
                "closewindow>>80e63f30",
                HyprEvent::CloseWindow {
                    address: addr("80e63f30"),
                },
            ),
            (
                "movewindowv2>>80e63f30,-1337,web-dev",
                HyprEvent::MoveWindow {
                    address: addr("80e63f30"),
                    workspace_id: -1337,
                    workspace: "web-dev".into(),
                },
            ),
            (
                "windowtitlev2>>80e63f30,foo, bar, baz",
                HyprEvent::WindowTitle {
                    address: addr("80e63f30"),
                    title: "foo, bar, baz".into(),
                },
            ),
            (
                "activewindowv2>>80e63f30",
                HyprEvent::ActiveWindow {
                    address: Some(addr("80e63f30")),
                },
            ),
            (
                "activewindowv2>>",
                HyprEvent::ActiveWindow { address: None },
            ),
            (
                "changefloatingmode>>80e63f30,1",
                HyprEvent::ChangeFloatingMode {
                    address: addr("80e63f30"),
                    floating: true,
                },
            ),
            (
                "workspacev2>>-1337,web-dev",
                HyprEvent::Workspace {
                    id: -1337,
                    name: "web-dev".into(),
                },
            ),
            (
                "createworkspacev2>>-1338,web-dev:backend",
                HyprEvent::CreateWorkspace {
                    id: -1338,
                    name: "web-dev:backend".into(),
                },
            ),
            (
                "destroyworkspacev2>>5,5",
                HyprEvent::DestroyWorkspace {
                    id: 5,
                    name: "5".into(),
                },
            ),
            (
                "moveworkspacev2>>-1337,web-dev,DP-2",
                HyprEvent::MoveWorkspace {
                    id: -1337,
                    name: "web-dev".into(),
                    monitor: "DP-2".into(),
                },
            ),
            (
                "renameworkspace>>3,coding",
                HyprEvent::RenameWorkspace {
                    id: 3,
                    name: "coding".into(),
                },
            ),
            (
                "focusedmonv2>>DP-1,6",
                HyprEvent::FocusedMonitor {
                    monitor: "DP-1".into(),
                    workspace_id: 6,
                },
            ),
            (
                "monitoraddedv2>>1,DP-2,Dell U2720Q",
                HyprEvent::MonitorAdded {
                    id: 1,
                    name: "DP-2".into(),
                },
            ),
            ("fullscreen>>1", HyprEvent::Fullscreen { active: true }),
            ("configreloaded>>", HyprEvent::ConfigReloaded),
            (
                "urgent>>80e63f30",
                HyprEvent::Urgent {
                    address: addr("80e63f30"),
                },
            ),
        ];
        for (line, want) in cases {
            assert_eq!(HyprEvent::parse(line), want, "line: {line}");
        }
    }

    #[test]
    fn unknown_and_malformed_events_never_fail() {
        assert_eq!(
            HyprEvent::parse("somefutureevent>>a,b,c"),
            HyprEvent::Unknown {
                name: "somefutureevent".into(),
                data: "a,b,c".into()
            }
        );
        // Malformed payload for a known event degrades to Unknown.
        assert_eq!(
            HyprEvent::parse("workspacev2>>not-a-number"),
            HyprEvent::Unknown {
                name: "workspacev2".into(),
                data: "not-a-number".into()
            }
        );
        // A line with no separator at all.
        assert_eq!(
            HyprEvent::parse("garbage"),
            HyprEvent::Unknown {
                name: "garbage".into(),
                data: String::new()
            }
        );
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_delay(1), Duration::from_millis(500));
        assert_eq!(backoff_delay(2), Duration::from_secs(1));
        assert_eq!(backoff_delay(10), Duration::from_secs(5));
        assert_eq!(backoff_delay(u32::MAX), Duration::from_secs(5));
    }
}
