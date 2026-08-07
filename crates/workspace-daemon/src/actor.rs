//! The single-writer state actor.
//!
//! Every mutation — Hyprland events, client requests, config reloads — flows
//! through one mpsc channel into this actor, which owns the [`World`]
//! exclusively. After each command it publishes a fresh immutable snapshot to
//! a `watch` channel (reads never block the actor) and emits domain events on
//! a `broadcast` bus (IPC push, autosave, future notifiers).

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use workspace_core::DomainEvent;
use workspace_core::config::Config;
use workspace_core::world::{MonitorInfo, TrackedWindow, WindowFacts, WorkspaceInfo, World};
use workspace_hypr::HyprEvent;
use workspace_proto::{DaemonStatus, ErrorBody, EventEnvelope, Request, Snapshot, error_code};

/// Outcome of a request processed by the actor.
pub type RequestResult = Result<serde_json::Value, ErrorBody>;

/// Commands consumed by the actor. The only way to mutate state.
#[derive(Debug)]
pub enum Command {
    /// Full state dump after (re)connecting to Hyprland.
    Hydrate {
        /// All live windows.
        windows: Vec<TrackedWindow>,
        /// All live workspaces.
        workspaces: Vec<WorkspaceInfo>,
        /// All live monitors.
        monitors: Vec<MonitorInfo>,
        /// Focused window address, if any.
        focused_window: Option<String>,
    },
    /// Event-socket connectivity changed.
    HyprConnection(bool),
    /// A parsed Hyprland event.
    Hypr(HyprEvent),
    /// Enriched facts for a window (follow-up fetch after `openwindow`).
    WindowFacts {
        /// Canonical window address.
        address: String,
        /// Hyprland's stable id, when reported.
        stable_id: Option<String>,
        /// Fresh facts.
        facts: WindowFacts,
    },
    /// A client request needing an answer.
    Request {
        /// The request.
        request: Request,
        /// Where to send the outcome.
        resp: oneshot::Sender<RequestResult>,
    },
    /// Graceful shutdown: emit `daemon.shutting_down` and stop.
    Shutdown,
}

/// Channels the rest of the daemon uses to talk to the actor.
#[derive(Clone, Debug)]
pub struct ActorHandles {
    /// Command sender (the single mutation path).
    pub commands: mpsc::Sender<Command>,
    /// Event bus for subscribers.
    pub bus: broadcast::Sender<Arc<EventEnvelope>>,
    /// Always-current state snapshot.
    pub snapshot: watch::Receiver<Arc<Snapshot>>,
}

/// Spawn the actor task and return its channels.
pub fn spawn(config: Config) -> ActorHandles {
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let (bus_tx, _) = broadcast::channel(256);
    let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(Snapshot::default()));
    let actor = StateActor {
        world: World::default(),
        _config: config,
        started: Instant::now(),
        seq: 0,
        bus: bus_tx.clone(),
        snapshot_tx,
    };
    tokio::spawn(actor.run(cmd_rx));
    ActorHandles {
        commands: cmd_tx,
        bus: bus_tx,
        snapshot: snapshot_rx,
    }
}

struct StateActor {
    world: World,
    _config: Config,
    started: Instant,
    seq: u64,
    bus: broadcast::Sender<Arc<EventEnvelope>>,
    snapshot_tx: watch::Sender<Arc<Snapshot>>,
}

impl StateActor {
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        while let Some(command) = commands.recv().await {
            let shutdown = matches!(command, Command::Shutdown);
            self.handle(command);
            self.publish_snapshot();
            if shutdown {
                break;
            }
        }
        tracing::debug!("state actor stopped");
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Hydrate {
                windows,
                workspaces,
                monitors,
                focused_window,
            } => {
                tracing::info!(
                    windows = windows.len(),
                    workspaces = workspaces.len(),
                    monitors = monitors.len(),
                    "hydrated from hyprland"
                );
                self.world
                    .hydrate(windows, workspaces, monitors, focused_window);
            }
            Command::HyprConnection(up) => {
                self.world.hypr_connected = up;
                self.emit(DomainEvent::HyprConnection { up });
            }
            Command::Hypr(event) => self.handle_hypr_event(event),
            Command::WindowFacts {
                address,
                stable_id,
                facts,
            } => {
                self.world.upsert_window(&address, stable_id, facts);
            }
            Command::Request { request, resp } => {
                let result = self.handle_request(request);
                let _ = resp.send(result);
            }
            Command::Shutdown => {
                self.emit(DomainEvent::ShuttingDown);
            }
        }
    }

    fn handle_hypr_event(&mut self, event: HyprEvent) {
        match event {
            HyprEvent::OpenWindow {
                address,
                workspace,
                class,
                title,
            } => {
                let address = address.as_str().to_owned();
                let workspace_id = self.workspace_id_by_name(&workspace);
                let facts = WindowFacts {
                    class: class.clone(),
                    title: title.clone(),
                    initial_class: class.clone(),
                    initial_title: title.clone(),
                    workspace: workspace.clone(),
                    workspace_id,
                    ..Default::default()
                };
                self.world.upsert_window(&address, None, facts);
                self.emit(DomainEvent::WindowOpened {
                    address,
                    class,
                    title,
                    workspace,
                });
            }
            HyprEvent::CloseWindow { address } => {
                let address = address.as_str().to_owned();
                if self.world.windows.contains_key(&address) {
                    self.world.remove_window(&address);
                    self.emit(DomainEvent::WindowClosed { address });
                }
            }
            HyprEvent::MoveWindow {
                address,
                workspace_id,
                workspace,
            } => {
                let address = address.as_str().to_owned();
                self.world
                    .set_window_workspace(&address, workspace_id, &workspace);
                self.emit(DomainEvent::WindowMoved { address, workspace });
            }
            HyprEvent::WindowTitle { address, title } => {
                let address = address.as_str().to_owned();
                self.world.set_title(&address, &title);
                self.emit(DomainEvent::WindowTitleChanged { address, title });
            }
            HyprEvent::ActiveWindow { address } => {
                let address = address.map(|a| a.as_str().to_owned());
                self.world.set_focus(address.clone());
                self.emit(DomainEvent::WindowFocused { address });
            }
            HyprEvent::ChangeFloatingMode { address, floating } => {
                self.world.set_floating(address.as_str(), floating);
            }
            HyprEvent::Pin { address, pinned } => {
                if let Some(window) = self.world.windows.get_mut(address.as_str()) {
                    window.facts.pinned = pinned;
                }
            }
            HyprEvent::Workspace { id, name } => {
                self.world.focused_workspace = Some(name.clone());
                if !self.world.workspaces.contains_key(&id) {
                    self.world.upsert_workspace(id, &name, "");
                }
                self.emit(DomainEvent::WorkspaceChanged { id, name });
            }
            HyprEvent::CreateWorkspace { id, name } => {
                self.world.upsert_workspace(id, &name, "");
            }
            HyprEvent::DestroyWorkspace { id, .. } => {
                self.world.remove_workspace(id);
            }
            HyprEvent::RenameWorkspace { id, name } => {
                self.world.rename_workspace(id, &name);
            }
            HyprEvent::MoveWorkspace { id, name, monitor } => {
                self.world.upsert_workspace(id, &name, &monitor);
            }
            HyprEvent::FocusedMonitor { monitor, .. } => {
                for m in &mut self.world.monitors {
                    m.focused = m.name == monitor;
                }
            }
            HyprEvent::MonitorAdded { id, name } => {
                if !self.world.monitors.iter().any(|m| m.id == id) {
                    self.world.monitors.push(MonitorInfo {
                        id,
                        name,
                        focused: false,
                    });
                }
            }
            HyprEvent::MonitorRemoved { id, .. } => {
                self.world.monitors.retain(|m| m.id != id);
            }
            HyprEvent::Fullscreen { active } => {
                if let Some(address) = self.world.focused_window.clone()
                    && let Some(window) = self.world.windows.get_mut(&address)
                {
                    window.facts.fullscreen = if active { 2 } else { 0 };
                }
            }
            HyprEvent::ConfigReloaded | HyprEvent::Urgent { .. } => {}
            HyprEvent::Unknown { name, data } => {
                tracing::trace!(name, data, "ignoring unknown hyprland event");
            }
            _ => {}
        }
    }

    fn handle_request(&mut self, request: Request) -> RequestResult {
        match request {
            Request::DaemonStatus => {
                let status = DaemonStatus {
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    uptime_s: self.started.elapsed().as_secs(),
                    hypr_connected: self.world.hypr_connected,
                    active_project: None,
                    windows: self.world.windows.len(),
                    projects: 0,
                };
                Ok(serde_json::to_value(status).expect("status serializes"))
            }
            Request::StateSnapshot => {
                Ok(serde_json::to_value(self.snapshot()).expect("snapshot serializes"))
            }
            // `subscribe` is handled per-connection by the server.
            Request::Subscribe { .. } => Err(ErrorBody {
                code: error_code::BAD_REQUEST.to_owned(),
                message: "subscribe is connection-scoped; the server handles it".to_owned(),
                data: None,
            }),
            #[allow(unreachable_patterns)] // Request is #[non_exhaustive]
            _ => Err(ErrorBody {
                code: error_code::UNKNOWN_METHOD.to_owned(),
                message: "method not implemented by this daemon".to_owned(),
                data: None,
            }),
        }
    }

    fn snapshot(&self) -> Snapshot {
        let mut windows: Vec<_> = self.world.windows.values().cloned().collect();
        windows.sort_by(|a, b| a.address.cmp(&b.address));
        let mut workspaces: Vec<_> = self.world.workspaces.values().cloned().collect();
        workspaces.sort_by_key(|w| w.id);
        Snapshot {
            windows,
            workspaces,
            monitors: self.world.monitors.clone(),
            focused_window: self.world.focused_window.clone(),
            hypr_connected: self.world.hypr_connected,
        }
    }

    fn publish_snapshot(&self) {
        let _ = self.snapshot_tx.send(Arc::new(self.snapshot()));
    }

    fn emit(&mut self, event: DomainEvent) {
        self.seq += 1;
        // Send fails only when no subscriber exists, which is fine.
        let _ = self.bus.send(Arc::new(EventEnvelope {
            v: workspace_proto::PROTOCOL_VERSION,
            seq: self.seq,
            data: event,
        }));
    }

    fn workspace_id_by_name(&self, name: &str) -> i64 {
        self.world
            .workspaces
            .values()
            .find(|w| w.name == name)
            .map(|w| w.id)
            .unwrap_or_default()
    }
}
