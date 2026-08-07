//! The Hyprland side of the daemon: event pump consumption, re-hydration on
//! (re)connect, and enrichment of `openwindow` events with full client facts.
//!
//! Hydration ordering: the event pump connects the event socket *before* this
//! task dumps state over the control socket. Events arriving during the dump
//! wait in the channel and are applied afterwards; the actor's idempotent
//! upserts make the replay harmless.

use std::path::PathBuf;

use tokio::sync::mpsc;
use workspace_core::world::{MonitorInfo, TrackedWindow, WindowFacts, WorkspaceInfo};
use workspace_hypr::events::{StreamItem, run_event_pump};
use workspace_hypr::{Client, HyprCtl, HyprEvent, HyprPaths};

use crate::actor::Command;

/// Convert one Hyprland client into our tracked-window form, resolving the
/// executable from `/proc/<pid>/exe` (best-effort; needs same-user access).
pub fn tracked_from_client(client: &Client) -> TrackedWindow {
    TrackedWindow {
        address: client.address.as_str().to_owned(),
        stable_id: client.stable_id.clone(),
        facts: WindowFacts {
            class: client.class.clone(),
            title: client.title.clone(),
            initial_class: client.initial_class.clone(),
            initial_title: client.initial_title.clone(),
            executable: resolve_executable(client.pid),
            pid: client.pid,
            workspace_id: client.workspace.id,
            workspace: client.workspace.name.clone(),
            monitor: client.monitor,
            at: client.at,
            size: client.size,
            floating: client.floating,
            pinned: client.pinned,
            fullscreen: client.fullscreen,
        },
        assignment: None,
        assigned_by: None,
    }
}

fn resolve_executable(pid: i32) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// Run forever: consume the event pump, keep the actor hydrated and fed.
/// Returns when the actor's command channel closes.
pub async fn run(paths: HyprPaths, commands: mpsc::Sender<Command>) {
    let ctl = HyprCtl::new(paths.clone());
    let (event_tx, mut events) = mpsc::channel(1024);
    tokio::spawn(run_event_pump(paths, event_tx));

    while let Some(item) = events.recv().await {
        match item {
            StreamItem::Connected => {
                match hydrate(&ctl).await {
                    Ok(command) => {
                        if commands.send(command).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "hydration failed; state may be stale until next reconnect");
                    }
                }
                if commands.send(Command::HyprConnection(true)).await.is_err() {
                    return;
                }
            }
            StreamItem::Disconnected => {
                if commands.send(Command::HyprConnection(false)).await.is_err() {
                    return;
                }
            }
            StreamItem::Event(event) => {
                let enrich = if let HyprEvent::OpenWindow { address, .. } = &event {
                    Some(address.clone())
                } else {
                    None
                };
                if commands.send(Command::Hypr(event)).await.is_err() {
                    return;
                }
                // The openwindow event carries no pid/geometry; follow up with
                // a clients fetch and feed the full facts to the actor.
                if let Some(address) = enrich {
                    match ctl.clients().await {
                        Ok(clients) => {
                            if let Some(client) = clients.iter().find(|c| c.address == address) {
                                let tracked = tracked_from_client(client);
                                let command = Command::WindowFacts {
                                    address: tracked.address.clone(),
                                    stable_id: tracked.stable_id.clone(),
                                    facts: tracked.facts,
                                };
                                if commands.send(command).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            tracing::debug!(%error, "openwindow enrichment fetch failed");
                        }
                    }
                }
            }
        }
    }
}

async fn hydrate(ctl: &HyprCtl) -> Result<Command, workspace_hypr::HyprError> {
    let clients = ctl.clients().await?;
    let workspaces = ctl.workspaces().await?;
    let monitors = ctl.monitors().await?;
    let focused_window = ctl
        .active_window()
        .await?
        .map(|c| c.address.as_str().to_owned());

    Ok(Command::Hydrate {
        windows: clients.iter().map(tracked_from_client).collect(),
        workspaces: workspaces
            .into_iter()
            .map(|w| WorkspaceInfo {
                id: w.id,
                name: w.name,
                monitor: w.monitor,
            })
            .collect(),
        monitors: monitors
            .into_iter()
            .map(|m| MonitorInfo {
                id: m.id,
                name: m.name,
                focused: m.focused,
            })
            .collect(),
        focused_window,
    })
}
