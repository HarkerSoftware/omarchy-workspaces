//! The single-writer state actor.
//!
//! Every mutation — Hyprland events, client requests, config reloads — flows
//! through one mpsc channel into this actor, which owns the [`World`] and the
//! project list exclusively. After each command it publishes a fresh immutable
//! snapshot to a `watch` channel (reads never block the actor) and emits
//! domain events on a `broadcast` bus (IPC push, autosave, future notifiers).
//!
//! Requests that require Hyprland dispatches (switching, assigning) mutate
//! state immediately, then execute the dispatches in a spawned task so the
//! actor loop never awaits the compositor; the requester's response reports
//! dispatch failures.

use std::sync::Arc;
use std::time::Instant;

use std::path::PathBuf;

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use workspace_core::config::{Config, RuleAction};
use workspace_core::model::{Project, Slug};
use workspace_core::rules::{MatcherRegistry, RuleSet};
use workspace_core::search::{self, Resolution};
use workspace_core::world::{
    AssignmentSource, MonitorInfo, TrackedWindow, WindowFacts, WorkspaceInfo, World,
};
use workspace_core::{DomainEvent, ws_names};
use workspace_hypr::{Dispatch, HyprCtl, HyprEvent, WsTarget};
use workspace_proto::{
    DaemonStatus, ErrorBody, EventEnvelope, ProjectSummary, Request, Snapshot, error_code,
};

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

/// Load and compile `rules.toml` from the config dir. Missing file = empty
/// set; invalid file = error string naming every problem.
pub fn load_rules(
    config_dir: Option<&PathBuf>,
    registry: &MatcherRegistry,
) -> Result<RuleSet, String> {
    let Some(path) = config_dir.map(|dir| dir.join("rules.toml")) else {
        return Ok(RuleSet::default());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RuleSet::default());
        }
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    RuleSet::parse(&text, registry).map_err(|errors| {
        errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    })
}

/// Spawn the actor task and return its channels.
pub fn spawn(config: Config, ctl: HyprCtl, config_dir: Option<PathBuf>) -> ActorHandles {
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let (bus_tx, _) = broadcast::channel(256);
    let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(Snapshot::default()));
    let registry = MatcherRegistry::builtin();
    let rules = match load_rules(config_dir.as_ref(), &registry) {
        Ok(rules) => {
            tracing::info!(rules = rules.len(), "rules loaded");
            rules
        }
        Err(error) => {
            tracing::error!(%error, "invalid rules.toml; starting with no rules");
            RuleSet::default()
        }
    };
    let actor = StateActor {
        world: World::default(),
        projects: Vec::new(),
        config,
        ctl,
        registry,
        rules,
        config_dir,
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

/// How a request concludes: an immediate reply, or a reply gated on Hyprland
/// dispatches executed off the actor loop.
enum Outcome {
    Reply(RequestResult),
    DispatchThen {
        dispatches: Vec<Dispatch>,
        result: serde_json::Value,
    },
}

struct StateActor {
    world: World,
    projects: Vec<Project>,
    config: Config,
    ctl: HyprCtl,
    registry: MatcherRegistry,
    rules: RuleSet,
    config_dir: Option<PathBuf>,
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
                self.apply_rules(&address);
            }
            Command::Request { request, resp } => match self.handle_request(request) {
                Outcome::Reply(result) => {
                    let _ = resp.send(result);
                }
                Outcome::DispatchThen { dispatches, result } => {
                    let ctl = self.ctl.clone();
                    tokio::spawn(async move {
                        let outcome = match ctl.dispatch_batch(&dispatches).await {
                            Ok(()) => Ok(result),
                            Err(error) => Err(ErrorBody {
                                code: error_code::HYPRLAND.to_owned(),
                                message: error.to_string(),
                                data: None,
                            }),
                        };
                        let _ = resp.send(outcome);
                    });
                }
            },
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
                self.sync_active_project_with_workspace(&name);
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

    /// Keep `active_project` in sync when the user switches workspaces by any
    /// means (keybind, waybar, our own dispatch).
    fn sync_active_project_with_workspace(&mut self, workspace_name: &str) {
        let prefix = self.config.general.workspace_prefix.clone();
        let new_active = match ws_names::parse(&prefix, workspace_name) {
            ws_names::ParsedName::Project(slug)
            | ws_names::ParsedName::Group { project: slug, .. } => {
                self.projects.iter().find(|p| p.slug == slug).map(|p| p.id)
            }
            ws_names::ParsedName::Foreign => None,
        };
        if self.world.active_project != new_active {
            self.world.active_project = new_active;
            let slug = new_active.and_then(|id| {
                self.projects
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.slug.as_str().to_owned())
            });
            self.emit(DomainEvent::ProjectSwitched { slug });
        }
    }

    /// Evaluate rules for a freshly enriched window and act per config.
    /// Never overrides an existing assignment (Manual > Restore > Rule).
    fn apply_rules(&mut self, address: &str) {
        let Some(window) = self.world.windows.get(address) else {
            return;
        };
        if window.assignment.is_some() {
            return;
        }
        let facts = window.facts.clone();
        let (rule_name, project_slug, group) = {
            let matched = self.rules.matches(&facts);
            let Some(rule) = matched.first() else { return };
            (rule.name.clone(), rule.project.clone(), rule.group.clone())
        };
        let Some(project) = self.projects.iter().find(|p| p.slug == project_slug) else {
            tracing::warn!(
                rule = rule_name,
                project = %project_slug,
                "rule matched but its target project does not exist"
            );
            return;
        };
        let project_id = project.id;
        let ws = self.ws_name(&project_slug);

        let window = self.world.windows.get_mut(address).expect("checked above");
        window.assignment = Some((project_id, group));
        window.assigned_by = Some(AssignmentSource::Rule(rule_name.clone()));
        tracing::info!(rule = rule_name, window = address, project = %project_slug, "rule assigned window");
        self.emit(DomainEvent::RuleMatched {
            rule: rule_name,
            address: address.to_owned(),
            project: project_slug.as_str().to_owned(),
        });

        let dispatch = match self.config.general.rule_action {
            RuleAction::Assign => None,
            RuleAction::Move => Some(Dispatch::MoveToWorkspaceSilent {
                target: WsTarget::Name(ws),
                address: workspace_hypr::WindowAddress::new(address),
            }),
            RuleAction::MoveFocus => Some(Dispatch::MoveToWorkspace {
                target: WsTarget::Name(ws),
                address: workspace_hypr::WindowAddress::new(address),
            }),
        };
        if let Some(dispatch) = dispatch {
            let ctl = self.ctl.clone();
            tokio::spawn(async move {
                if let Err(error) = ctl.dispatch(&dispatch).await {
                    tracing::warn!(%error, "rule move dispatch failed");
                }
            });
        }
    }

    fn handle_request(&mut self, request: Request) -> Outcome {
        match request {
            Request::DaemonStatus => Outcome::Reply(Ok(json(&self.status()))),
            Request::StateSnapshot => Outcome::Reply(Ok(json(&self.snapshot()))),
            Request::RulesTest { address } => self.rules_test(address),
            Request::ConfigReload => self.config_reload(),
            Request::ProjectCreate { name, slug } => self.project_create(name, slug),
            Request::ProjectDelete { slug } => self.project_delete(&slug),
            Request::ProjectRename { slug, name } => self.project_rename(&slug, name),
            Request::ProjectSwitch { project } => self.project_switch(&project),
            Request::ProjectList => {
                let list: Vec<ProjectSummary> =
                    self.projects.iter().map(|p| self.summary(p)).collect();
                Outcome::Reply(Ok(json(&list)))
            }
            Request::WindowAssign {
                address,
                project,
                group,
            } => self.window_assign(&address, &project, group),
            // `subscribe` is handled per-connection by the server.
            Request::Subscribe { .. } => Outcome::Reply(Err(ErrorBody {
                code: error_code::BAD_REQUEST.to_owned(),
                message: "subscribe is connection-scoped; the server handles it".to_owned(),
                data: None,
            })),
            #[allow(unreachable_patterns)] // Request is #[non_exhaustive]
            _ => Outcome::Reply(Err(ErrorBody {
                code: error_code::UNKNOWN_METHOD.to_owned(),
                message: "method not implemented by this daemon".to_owned(),
                data: None,
            })),
        }
    }

    // ---- rules & config -----------------------------------------------------

    fn rules_test(&mut self, address: Option<String>) -> Outcome {
        let address = match address.or_else(|| self.world.focused_window.clone()) {
            Some(address) => address,
            None => {
                return Outcome::Reply(Err(bad_request(
                    "no address given and no window is focused".to_owned(),
                )));
            }
        };
        let Some(window) = self.world.windows.get(&address) else {
            return Outcome::Reply(Err(not_found(format!("no window at address {address}"))));
        };
        let matches: Vec<serde_json::Value> = self
            .rules
            .matches(&window.facts)
            .iter()
            .map(|rule| {
                serde_json::json!({
                    "rule": rule.name,
                    "project": rule.project,
                    "group": rule.group,
                    "stop": rule.stop,
                })
            })
            .collect();
        Outcome::Reply(Ok(serde_json::json!({
            "address": address,
            "class": window.facts.class,
            "title": window.facts.title,
            "executable": window.facts.executable,
            "matches": matches,
        })))
    }

    fn config_reload(&mut self) -> Outcome {
        let Some(dir) = self.config_dir.clone() else {
            return Outcome::Reply(Err(bad_request(
                "daemon was started without a config directory".to_owned(),
            )));
        };
        let config_path = dir.join("config.toml");
        let config = match std::fs::read_to_string(&config_path) {
            Ok(text) => match Config::parse(&text) {
                Ok(config) => config,
                Err(error) => {
                    return Outcome::Reply(Err(bad_request(format!(
                        "{} invalid — keeping current config: {error}",
                        config_path.display()
                    ))));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(error) => {
                return Outcome::Reply(Err(bad_request(format!(
                    "cannot read {}: {error}",
                    config_path.display()
                ))));
            }
        };
        let rules = match load_rules(Some(&dir), &self.registry) {
            Ok(rules) => rules,
            Err(error) => {
                return Outcome::Reply(Err(bad_request(format!(
                    "rules.toml invalid — keeping current rules: {error}"
                ))));
            }
        };
        let rule_count = rules.len();
        self.config = config;
        self.rules = rules;
        tracing::info!(rules = rule_count, "configuration reloaded");
        Outcome::Reply(Ok(
            serde_json::json!({ "reloaded": true, "rules": rule_count }),
        ))
    }

    // ---- project operations -------------------------------------------------

    fn project_create(&mut self, name: String, slug: Option<String>) -> Outcome {
        let slug = match slug {
            Some(raw) => match Slug::parse(&raw) {
                Ok(slug) => slug,
                Err(error) => return Outcome::Reply(Err(bad_request(error.to_string()))),
            },
            None => match Slug::from_display_name(&name) {
                Ok(slug) => slug,
                Err(error) => return Outcome::Reply(Err(bad_request(error.to_string()))),
            },
        };
        if self.projects.iter().any(|p| p.slug == slug) {
            return Outcome::Reply(Err(conflict(format!("project '{slug}' already exists"))));
        }
        let ws = self.ws_name(&slug);
        // Refuse to claim a named workspace the user created independently.
        if self.world.workspaces.values().any(|w| w.name == ws) {
            return Outcome::Reply(Err(conflict(format!(
                "a workspace named '{ws}' already exists in Hyprland and does not belong to \
                 omarchy-workspaces; pick another slug or set general.workspace_prefix"
            ))));
        }
        let project = Project {
            id: workspace_core::model::ProjectId::new(),
            slug: slug.clone(),
            name: name.clone(),
            groups: Vec::new(),
            apps: Vec::new(),
            monitor: None,
        };
        let summary = self.summary(&project);
        self.projects.push(project);
        self.emit(DomainEvent::ProjectCreated {
            slug: slug.as_str().to_owned(),
            name,
        });
        Outcome::Reply(Ok(json(&summary)))
    }

    fn project_delete(&mut self, slug: &str) -> Outcome {
        let Some(index) = self.projects.iter().position(|p| p.slug.as_str() == slug) else {
            return Outcome::Reply(Err(not_found(format!(
                "no project with slug '{slug}' (delete requires the exact slug)"
            ))));
        };
        let project = self.projects.remove(index);
        for window in self.world.windows.values_mut() {
            if window
                .assignment
                .as_ref()
                .is_some_and(|(id, _)| *id == project.id)
            {
                window.assignment = None;
                window.assigned_by = None;
            }
        }
        if self.world.active_project == Some(project.id) {
            self.world.active_project = None;
            self.emit(DomainEvent::ProjectSwitched { slug: None });
        }
        self.emit(DomainEvent::ProjectDeleted {
            slug: project.slug.as_str().to_owned(),
        });
        Outcome::Reply(Ok(serde_json::json!({ "deleted": project.slug })))
    }

    fn project_rename(&mut self, slug: &str, name: String) -> Outcome {
        let Some(project) = self.projects.iter_mut().find(|p| p.slug.as_str() == slug) else {
            return Outcome::Reply(Err(not_found(format!("no project with slug '{slug}'"))));
        };
        project.name = name.clone();
        let slug = project.slug.as_str().to_owned();
        let summary = {
            let project = self
                .projects
                .iter()
                .find(|p| p.slug.as_str() == slug)
                .expect("just renamed");
            self.summary(project)
        };
        self.emit(DomainEvent::ProjectRenamed { slug, name });
        Outcome::Reply(Ok(json(&summary)))
    }

    fn project_switch(&mut self, query: &str) -> Outcome {
        let (project_id, slug, monitor) = match self.resolve(query) {
            Ok(project) => (project.id, project.slug.clone(), project.monitor.clone()),
            Err(error) => return Outcome::Reply(Err(error)),
        };
        let ws = self.ws_name(&slug);
        let mut dispatches = Vec::new();
        if self.config.switch.move_workspace_to_preferred_monitor
            && let Some(monitor) = monitor
            && self.world.monitors.iter().any(|m| m.name == monitor)
            && self.world.workspaces.values().any(|w| w.name == ws)
        {
            dispatches.push(Dispatch::MoveWorkspaceToMonitor {
                workspace: WsTarget::Name(ws.clone()),
                monitor,
            });
        }
        dispatches.push(Dispatch::Workspace(WsTarget::Name(ws)));

        self.world.active_project = Some(project_id);
        self.emit(DomainEvent::ProjectSwitched {
            slug: Some(slug.as_str().to_owned()),
        });
        let project = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .expect("resolved above");
        let result = json(&self.summary(project));
        Outcome::DispatchThen { dispatches, result }
    }

    fn window_assign(&mut self, address: &str, query: &str, group: Option<String>) -> Outcome {
        if !self.world.windows.contains_key(address) {
            return Outcome::Reply(Err(not_found(format!("no window at address {address}"))));
        }
        let (project_id, slug, groups) = match self.resolve(query) {
            Ok(project) => (project.id, project.slug.clone(), project.groups.clone()),
            Err(error) => return Outcome::Reply(Err(error)),
        };
        let group_slug = match group {
            None => None,
            Some(raw) => match Slug::parse(&raw) {
                Ok(g) if groups.iter().any(|existing| existing.slug == g) => Some(g),
                Ok(g) => {
                    return Outcome::Reply(Err(not_found(format!(
                        "project '{slug}' has no group '{g}'"
                    ))));
                }
                Err(error) => return Outcome::Reply(Err(bad_request(error.to_string()))),
            },
        };
        let ws = self.ws_name(&slug);
        let window = self.world.windows.get_mut(address).expect("checked above");
        window.assignment = Some((project_id, group_slug));
        window.assigned_by = Some(AssignmentSource::Manual);
        let target_address = workspace_hypr::WindowAddress::new(address);
        Outcome::DispatchThen {
            dispatches: vec![Dispatch::MoveToWorkspaceSilent {
                target: WsTarget::Name(ws),
                address: target_address,
            }],
            result: serde_json::json!({ "assigned": address, "project": slug }),
        }
    }

    // ---- helpers ------------------------------------------------------------

    fn resolve(&self, query: &str) -> Result<&Project, ErrorBody> {
        match search::resolve(query, &self.projects) {
            Resolution::Match(project) => Ok(project),
            Resolution::Ambiguous(candidates) => Err(ErrorBody {
                code: error_code::AMBIGUOUS.to_owned(),
                message: format!("'{query}' matches several projects"),
                data: Some(serde_json::json!({
                    "candidates": candidates
                        .iter()
                        .map(|p| p.slug.as_str())
                        .collect::<Vec<_>>()
                })),
            }),
            Resolution::NotFound => Err(ErrorBody {
                code: error_code::NOT_FOUND.to_owned(),
                message: format!("no project matching '{query}'"),
                data: Some(serde_json::json!({
                    "candidates": self
                        .projects
                        .iter()
                        .map(|p| p.slug.as_str())
                        .collect::<Vec<_>>()
                })),
            }),
        }
    }

    fn ws_name(&self, slug: &Slug) -> String {
        ws_names::project_workspace(&self.config.general.workspace_prefix, slug)
    }

    fn summary(&self, project: &Project) -> ProjectSummary {
        ProjectSummary {
            slug: project.slug.clone(),
            name: project.name.clone(),
            active: self.world.active_project == Some(project.id),
            windows: self
                .world
                .windows
                .values()
                .filter(|w| {
                    w.assignment
                        .as_ref()
                        .is_some_and(|(id, _)| *id == project.id)
                })
                .count(),
            groups: project.groups.iter().map(|g| g.slug.clone()).collect(),
            workspace: self.ws_name(&project.slug),
        }
    }

    fn status(&self) -> DaemonStatus {
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            uptime_s: self.started.elapsed().as_secs(),
            hypr_connected: self.world.hypr_connected,
            active_project: self.world.active_project.and_then(|id| {
                self.projects
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.slug.clone())
            }),
            windows: self.world.windows.len(),
            projects: self.projects.len(),
        }
    }

    fn snapshot(&self) -> Snapshot {
        let mut windows: Vec<_> = self.world.windows.values().cloned().collect();
        windows.sort_by(|a, b| a.address.cmp(&b.address));
        let mut workspaces: Vec<_> = self.world.workspaces.values().cloned().collect();
        workspaces.sort_by_key(|w| w.id);
        Snapshot {
            projects: self.projects.iter().map(|p| self.summary(p)).collect(),
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

fn json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).expect("serializes")
}

fn bad_request(message: String) -> ErrorBody {
    ErrorBody {
        code: error_code::BAD_REQUEST.to_owned(),
        message,
        data: None,
    }
}

fn not_found(message: String) -> ErrorBody {
    ErrorBody {
        code: error_code::NOT_FOUND.to_owned(),
        message,
        data: None,
    }
}

fn conflict(message: String) -> ErrorBody {
    ErrorBody {
        code: error_code::CONFLICT.to_owned(),
        message,
        data: None,
    }
}
