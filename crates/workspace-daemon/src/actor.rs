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
use workspace_storage::runtime::{RuntimeAssignment, RuntimeState};

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
    /// Persist the runtime snapshot now (autosave debounce fires this).
    Persist,
    /// A restore run correlated a window to a slot; record the assignment
    /// and apply the slot's placement.
    CorrelateRestore {
        /// Canonical window address.
        address: String,
        /// The satisfied slot.
        slot_id: uuid::Uuid,
        /// The owning project.
        project_id: workspace_core::model::ProjectId,
        /// Group the slot belongs to.
        group: Option<Slug>,
    },
    /// Emit a restore progress/finished event on behalf of the executor.
    RestoreEvent(DomainEvent),
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
pub fn spawn(
    config: Config,
    ctl: HyprCtl,
    config_dir: Option<PathBuf>,
    state_dir: Option<PathBuf>,
) -> ActorHandles {
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
    let (projects, recovery) = match &state_dir {
        Some(dir) => {
            let (projects, errors) = workspace_storage::projects::load_projects(dir);
            for error in &errors {
                tracing::error!(%error, "skipping unreadable project file");
            }
            let runtime = workspace_storage::runtime::load_runtime(dir);
            tracing::info!(
                projects = projects.len(),
                recovered_assignments = runtime.assignments.len(),
                "state loaded"
            );
            (projects, runtime)
        }
        None => (Vec::new(), RuntimeState::default()),
    };
    let actor = StateActor {
        world: World::default(),
        projects,
        recovery,
        config,
        ctl,
        registry,
        rules,
        config_dir,
        state_dir,
        self_tx: cmd_tx.clone(),
        hydrated_once: false,
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
    /// Assignments recovered from runtime.json, applied during hydration.
    recovery: RuntimeState,
    config: Config,
    ctl: HyprCtl,
    registry: MatcherRegistry,
    rules: RuleSet,
    config_dir: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    /// Clone of our own command sender, for restore executors and boot tasks.
    self_tx: mpsc::Sender<Command>,
    /// Whether the first hydration has happened (gates restore_on_boot).
    hydrated_once: bool,
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
                self.apply_recovery();
                if !self.hydrated_once {
                    self.hydrated_once = true;
                    self.schedule_restore_on_boot();
                }
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
            Command::Persist => {
                self.persist_runtime();
            }
            Command::CorrelateRestore {
                address,
                slot_id,
                project_id,
                group,
            } => {
                let placement = self
                    .projects
                    .iter()
                    .find(|p| p.id == project_id)
                    .and_then(|p| p.apps.iter().find(|s| s.slot_id == slot_id))
                    .map(|s| s.placement.clone());
                if let Some(window) = self.world.windows.get_mut(&address) {
                    window.assignment = Some((project_id, group));
                    window.assigned_by = Some(AssignmentSource::Restore(slot_id));
                    self.persist_runtime();
                    // Apply floating placement off the actor loop.
                    if let Some(placement) = placement
                        && placement.floating
                    {
                        let ctl = self.ctl.clone();
                        let dispatches =
                            crate::launcher::placement_dispatches(&address, &placement);
                        tokio::spawn(async move {
                            if let Err(error) = ctl.dispatch_batch(&dispatches).await {
                                tracing::warn!(%error, "placement dispatches failed");
                            }
                        });
                    }
                }
            }
            Command::RestoreEvent(event) => {
                // Restore step (e) of the plan: once a restore run completes,
                // park the members of groups that are marked hidden.
                if let DomainEvent::RestoreFinished { project, .. } = &event {
                    self.park_hidden_groups(project.clone());
                }
                self.emit(event);
            }
            Command::Shutdown => {
                self.persist_runtime();
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
            self.persist_runtime();
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
        self.persist_runtime();
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
            Request::ProjectSave { project } => self.project_save(project),
            Request::ProjectRestore { project, dry_run } => self.project_restore(project, dry_run),
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
            Request::GroupCreate {
                project,
                name,
                slug,
            } => self.group_create(&project, name, slug),
            Request::GroupAdd {
                project,
                group,
                address,
            } => self.group_membership(&project, &group, &address, true),
            Request::GroupRemove {
                project,
                group,
                address,
            } => self.group_membership(&project, &group, &address, false),
            Request::GroupHide { project, group } => self.group_visibility(&project, &group, true),
            Request::GroupShow { project, group } => self.group_visibility(&project, &group, false),
            Request::GroupFocus { project, group } => self.group_focus(&project, &group),
            Request::GroupMove { project, group, to } => self.group_move(&project, &group, &to),
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

    // ---- persistence --------------------------------------------------------

    /// Recovery key for a window: stableId when known, else the address.
    fn recovery_key(window: &TrackedWindow) -> String {
        window
            .stable_id
            .clone()
            .unwrap_or_else(|| window.address.clone())
    }

    /// Re-apply assignments recovered from runtime.json to freshly hydrated
    /// windows (daemon restart mid-session).
    fn apply_recovery(&mut self) {
        if self.recovery.assignments.is_empty() && self.recovery.active_project.is_none() {
            return;
        }
        let mut recovered = 0usize;
        let addresses: Vec<String> = self.world.windows.keys().cloned().collect();
        for address in addresses {
            let window = &self.world.windows[&address];
            if window.assignment.is_some() {
                continue;
            }
            let key = Self::recovery_key(window);
            let Some(saved) = self.recovery.assignments.get(&key) else {
                continue;
            };
            let Some(project) = self.projects.iter().find(|p| p.slug == saved.project) else {
                continue;
            };
            let assigned_by = match saved.source.as_str() {
                "manual" => AssignmentSource::Manual,
                rule => AssignmentSource::Rule(rule.to_owned()),
            };
            let (project_id, group) = (project.id, saved.group.clone());
            let window = self.world.windows.get_mut(&address).expect("iterated");
            window.assignment = Some((project_id, group));
            window.assigned_by = Some(assigned_by);
            recovered += 1;
        }
        if let Some(slug) = self.recovery.active_project.clone()
            && self.world.active_project.is_none()
            && let Some(project) = self.projects.iter().find(|p| p.slug == slug)
        {
            self.world.active_project = Some(project.id);
        }
        if recovered > 0 {
            tracing::info!(recovered, "assignments recovered from runtime.json");
        }
    }

    /// Snapshot assignments + active project to runtime.json.
    fn persist_runtime(&self) {
        let Some(dir) = &self.state_dir else { return };
        let mut state = RuntimeState {
            active_project: self.world.active_project.and_then(|id| {
                self.projects
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.slug.clone())
            }),
            assignments: Default::default(),
        };
        for window in self.world.windows.values() {
            let Some((project_id, group)) = &window.assignment else {
                continue;
            };
            let Some(project) = self.projects.iter().find(|p| p.id == *project_id) else {
                continue;
            };
            let source = match &window.assigned_by {
                Some(AssignmentSource::Rule(rule)) => rule.clone(),
                Some(AssignmentSource::Restore(_)) => "restore".to_owned(),
                _ => "manual".to_owned(),
            };
            state.assignments.insert(
                Self::recovery_key(window),
                RuntimeAssignment {
                    project: project.slug.clone(),
                    group: group.clone(),
                    source,
                },
            );
        }
        if let Err(error) = workspace_storage::runtime::save_runtime(dir, &state) {
            tracing::warn!(%error, "cannot persist runtime.json");
        }
    }

    /// Write one project's file to disk.
    fn persist_project(&self, project: &Project) {
        let Some(dir) = &self.state_dir else { return };
        if let Err(error) = workspace_storage::projects::save_project(dir, project) {
            tracing::error!(%error, slug = %project.slug, "cannot persist project file");
        }
    }

    /// `project.save`: capture the project's currently assigned windows as
    /// declarative app slots and persist the file.
    fn project_save(&mut self, query: Option<String>) -> Outcome {
        let project_id = match query {
            Some(query) => match self.resolve(&query) {
                Ok(project) => project.id,
                Err(error) => return Outcome::Reply(Err(error)),
            },
            None => match self.world.active_project {
                Some(id) => id,
                None => {
                    return Outcome::Reply(Err(bad_request(
                        "no project given and none is active".to_owned(),
                    )));
                }
            },
        };
        let monitor_names: std::collections::HashMap<i64, String> = self
            .world
            .monitors
            .iter()
            .map(|m| (m.id, m.name.clone()))
            .collect();
        let apps: Vec<workspace_core::model::AppSlot> = self
            .world
            .windows
            .values()
            .filter(|w| {
                w.assignment
                    .as_ref()
                    .is_some_and(|(id, _)| *id == project_id)
            })
            .map(|w| workspace_core::model::AppSlot {
                slot_id: uuid::Uuid::new_v4(),
                name: None,
                launch: None,
                identity: workspace_core::model::WindowIdentity {
                    class: Some(w.facts.class.clone()),
                    initial_class: Some(w.facts.initial_class.clone()),
                    executable: w.facts.executable.clone(),
                    title_pattern: None,
                },
                group: w.assignment.as_ref().and_then(|(_, g)| g.clone()),
                placement: workspace_core::model::Placement {
                    floating: w.facts.floating,
                    position: w.facts.floating.then_some(w.facts.at),
                    size: w.facts.floating.then_some(w.facts.size),
                    fullscreen: w.facts.fullscreen,
                    monitor: monitor_names.get(&w.facts.monitor).cloned(),
                },
            })
            .collect();
        let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) else {
            return Outcome::Reply(Err(not_found("project vanished".to_owned())));
        };
        project.apps = apps;
        let saved = project.clone();
        self.persist_project(&saved);
        self.persist_runtime();
        let summary = self.summary(&saved);
        Outcome::Reply(Ok(serde_json::json!({
            "saved": saved.slug,
            "slots": saved.apps.len(),
            "project": json(&summary),
        })))
    }

    // ---- restore ------------------------------------------------------------

    /// Plan (and unless `dry_run`, execute) a project restore.
    fn project_restore(&mut self, query: Option<String>, dry_run: bool) -> Outcome {
        let project = match query {
            Some(query) => match self.resolve(&query) {
                Ok(project) => project.clone(),
                Err(error) => return Outcome::Reply(Err(error)),
            },
            None => {
                let Some(id) = self.world.active_project else {
                    return Outcome::Reply(Err(bad_request(
                        "no project given and none is active".to_owned(),
                    )));
                };
                match self.projects.iter().find(|p| p.id == id) {
                    Some(project) => project.clone(),
                    None => return Outcome::Reply(Err(not_found("project vanished".to_owned()))),
                }
            }
        };
        let candidates: Vec<&TrackedWindow> = self
            .world
            .windows
            .values()
            .filter(|w| match &w.assignment {
                None => true,
                Some((id, _)) => *id == project.id,
            })
            .collect();
        let plan = match workspace_core::restore::plan(
            &project,
            &candidates,
            &self.config.general.workspace_prefix,
        ) {
            Ok(plan) => plan,
            Err(error) => return Outcome::Reply(Err(bad_request(error.to_string()))),
        };
        if dry_run {
            return Outcome::Reply(Ok(json(&plan)));
        }
        let result = serde_json::json!({ "started": true, "plan": json(&plan) });
        let ctx = crate::launcher::RestoreContext {
            plan,
            project_id: project.id,
            ctl: self.ctl.clone(),
            commands: self.self_tx.clone(),
            bus: self.bus.clone(),
            default_timeout: std::time::Duration::from_millis(
                self.config.launcher.default_timeout_ms,
            ),
        };
        self.world.active_project = Some(project.id);
        self.persist_runtime();
        tokio::spawn(crate::launcher::execute(ctx));
        Outcome::Reply(Ok(result))
    }

    /// Kick off `restore_on_boot` projects a few seconds after first hydration.
    fn schedule_restore_on_boot(&self) {
        for slug in self.config.general.restore_on_boot.clone() {
            let commands = self.self_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let (tx, rx) = oneshot::channel();
                let _ = commands
                    .send(Command::Request {
                        request: Request::ProjectRestore {
                            project: Some(slug.as_str().to_owned()),
                            dry_run: false,
                        },
                        resp: tx,
                    })
                    .await;
                match rx.await {
                    Ok(Err(error)) => {
                        tracing::warn!(project = %slug, error = %error.message, "restore_on_boot failed")
                    }
                    Err(_) => tracing::warn!(project = %slug, "restore_on_boot dropped"),
                    Ok(Ok(_)) => {}
                }
            });
        }
    }

    // ---- groups -------------------------------------------------------------

    /// Park the members of every hidden group of `project_slug` (fire and
    /// forget; used after restore runs).
    fn park_hidden_groups(&self, project_slug: String) {
        let Some(project) = self
            .projects
            .iter()
            .find(|p| p.slug.as_str() == project_slug)
        else {
            return;
        };
        let mut dispatches = Vec::new();
        for group in project.groups.iter().filter(|g| g.hidden) {
            let parking = ws_names::group_workspace(
                &self.config.general.workspace_prefix,
                &project.slug,
                &group.slug,
            );
            for address in self.group_members(project.id, &group.slug) {
                dispatches.push(Dispatch::MoveToWorkspaceSilent {
                    target: WsTarget::Name(parking.clone()),
                    address: workspace_hypr::WindowAddress::new(&address),
                });
            }
        }
        if dispatches.is_empty() {
            return;
        }
        let ctl = self.ctl.clone();
        tokio::spawn(async move {
            if let Err(error) = ctl.dispatch_batch(&dispatches).await {
                tracing::warn!(%error, "parking hidden groups failed");
            }
        });
    }

    /// Resolve a (project query, group slug) pair to indices, with typed errors.
    fn find_group(&self, project_query: &str, group: &str) -> Result<(usize, Slug), ErrorBody> {
        let project = self.resolve(project_query)?;
        let project_index = self
            .projects
            .iter()
            .position(|p| p.id == project.id)
            .expect("resolved");
        let group_slug = Slug::parse(group).map_err(|e| bad_request(e.to_string()))?;
        if !self.projects[project_index]
            .groups
            .iter()
            .any(|g| g.slug == group_slug)
        {
            return Err(not_found(format!(
                "project '{}' has no group '{group_slug}'",
                self.projects[project_index].slug
            )));
        }
        Ok((project_index, group_slug))
    }

    /// Windows currently assigned to (project, group).
    fn group_members(
        &self,
        project_id: workspace_core::model::ProjectId,
        group: &Slug,
    ) -> Vec<String> {
        self.world
            .windows
            .values()
            .filter(|w| {
                w.assignment
                    .as_ref()
                    .is_some_and(|(id, g)| *id == project_id && g.as_ref() == Some(group))
            })
            .map(|w| w.address.clone())
            .collect()
    }

    fn group_create(&mut self, project_query: &str, name: String, slug: Option<String>) -> Outcome {
        let project_id = match self.resolve(project_query) {
            Ok(project) => project.id,
            Err(error) => return Outcome::Reply(Err(error)),
        };
        let group_slug = match slug
            .map(|s| Slug::parse(&s))
            .unwrap_or_else(|| Slug::from_display_name(&name))
        {
            Ok(slug) => slug,
            Err(error) => return Outcome::Reply(Err(bad_request(error.to_string()))),
        };
        let project = self
            .projects
            .iter_mut()
            .find(|p| p.id == project_id)
            .expect("resolved");
        if project.groups.iter().any(|g| g.slug == group_slug) {
            return Outcome::Reply(Err(conflict(format!(
                "group '{group_slug}' already exists in '{}'",
                project.slug
            ))));
        }
        project.groups.push(workspace_core::model::Group {
            slug: group_slug.clone(),
            name,
            hidden: false,
        });
        let saved = project.clone();
        let project_slug = saved.slug.as_str().to_owned();
        self.persist_project(&saved);
        self.emit(DomainEvent::GroupChanged {
            project: project_slug.clone(),
            group: group_slug.as_str().to_owned(),
            change: "created".into(),
        });
        Outcome::Reply(Ok(
            serde_json::json!({ "project": project_slug, "group": group_slug }),
        ))
    }

    /// Add a window to a group (`join = true`) or remove it (`join = false`).
    fn group_membership(
        &mut self,
        project_query: &str,
        group: &str,
        address: &str,
        join: bool,
    ) -> Outcome {
        let (project_index, group_slug) = match self.find_group(project_query, group) {
            Ok(found) => found,
            Err(error) => return Outcome::Reply(Err(error)),
        };
        if !self.world.windows.contains_key(address) {
            return Outcome::Reply(Err(not_found(format!("no window at address {address}"))));
        }
        let project = &self.projects[project_index];
        let (project_id, project_slug) = (project.id, project.slug.clone());
        let hidden = project
            .groups
            .iter()
            .find(|g| g.slug == group_slug)
            .expect("checked")
            .hidden;

        let window = self.world.windows.get_mut(address).expect("checked above");
        window.assignment = Some((project_id, join.then(|| group_slug.clone())));
        window.assigned_by = Some(AssignmentSource::Manual);
        self.persist_runtime();
        self.emit(DomainEvent::GroupChanged {
            project: project_slug.as_str().to_owned(),
            group: group_slug.as_str().to_owned(),
            change: "membership".into(),
        });

        // Joining a hidden group parks the window; anything else lands on the
        // project's primary workspace.
        let target = if join && hidden {
            ws_names::group_workspace(
                &self.config.general.workspace_prefix,
                &project_slug,
                &group_slug,
            )
        } else {
            self.ws_name(&project_slug)
        };
        Outcome::DispatchThen {
            dispatches: vec![Dispatch::MoveToWorkspaceSilent {
                target: WsTarget::Name(target),
                address: workspace_hypr::WindowAddress::new(address),
            }],
            result: serde_json::json!({
                "project": project_slug,
                "group": group_slug,
                "address": address,
                "member": join,
            }),
        }
    }

    /// Hide (park) or show a group's windows.
    fn group_visibility(&mut self, project_query: &str, group: &str, hide: bool) -> Outcome {
        let (project_index, group_slug) = match self.find_group(project_query, group) {
            Ok(found) => found,
            Err(error) => return Outcome::Reply(Err(error)),
        };
        let project = &mut self.projects[project_index];
        let (project_id, project_slug) = (project.id, project.slug.clone());
        project
            .groups
            .iter_mut()
            .find(|g| g.slug == group_slug)
            .expect("checked")
            .hidden = hide;
        let saved = project.clone();
        self.persist_project(&saved);

        let members = self.group_members(project_id, &group_slug);
        let target = if hide {
            ws_names::group_workspace(
                &self.config.general.workspace_prefix,
                &project_slug,
                &group_slug,
            )
        } else {
            self.ws_name(&project_slug)
        };
        let dispatches: Vec<Dispatch> = members
            .iter()
            .map(|address| Dispatch::MoveToWorkspaceSilent {
                target: WsTarget::Name(target.clone()),
                address: workspace_hypr::WindowAddress::new(address),
            })
            .collect();
        self.emit(DomainEvent::GroupChanged {
            project: project_slug.as_str().to_owned(),
            group: group_slug.as_str().to_owned(),
            change: if hide { "hidden" } else { "shown" }.into(),
        });
        Outcome::DispatchThen {
            dispatches,
            result: serde_json::json!({
                "project": project_slug,
                "group": group_slug,
                "hidden": hide,
                "windows": members.len(),
            }),
        }
    }

    /// Show a group if hidden, then focus one of its windows.
    fn group_focus(&mut self, project_query: &str, group: &str) -> Outcome {
        let (project_index, group_slug) = match self.find_group(project_query, group) {
            Ok(found) => found,
            Err(error) => return Outcome::Reply(Err(error)),
        };
        let project = &mut self.projects[project_index];
        let (project_id, project_slug) = (project.id, project.slug.clone());
        let was_hidden = {
            let group = project
                .groups
                .iter_mut()
                .find(|g| g.slug == group_slug)
                .expect("checked");
            std::mem::replace(&mut group.hidden, false)
        };
        if was_hidden {
            let saved = project.clone();
            self.persist_project(&saved);
        }

        let members = self.group_members(project_id, &group_slug);
        let primary = self.ws_name(&project_slug);
        let mut dispatches = Vec::new();
        if was_hidden {
            dispatches.extend(
                members
                    .iter()
                    .map(|address| Dispatch::MoveToWorkspaceSilent {
                        target: WsTarget::Name(primary.clone()),
                        address: workspace_hypr::WindowAddress::new(address),
                    }),
            );
        }
        // Prefer the currently focused window if it is a member.
        let focus_target = members
            .iter()
            .find(|a| self.world.focused_window.as_deref() == Some(a.as_str()))
            .or_else(|| members.first());
        match focus_target {
            Some(address) => {
                dispatches.push(Dispatch::FocusWindow(workspace_hypr::WindowAddress::new(
                    address,
                )));
            }
            None => {
                // Empty group: just focus the project workspace.
                dispatches.push(Dispatch::Workspace(WsTarget::Name(primary)));
            }
        }
        if was_hidden {
            self.emit(DomainEvent::GroupChanged {
                project: project_slug.as_str().to_owned(),
                group: group_slug.as_str().to_owned(),
                change: "shown".into(),
            });
        }
        Outcome::DispatchThen {
            dispatches,
            result: serde_json::json!({
                "project": project_slug,
                "group": group_slug,
                "windows": members.len(),
            }),
        }
    }

    /// Move a group's definition and windows to another project.
    fn group_move(&mut self, project_query: &str, group: &str, to_query: &str) -> Outcome {
        let (source_index, group_slug) = match self.find_group(project_query, group) {
            Ok(found) => found,
            Err(error) => return Outcome::Reply(Err(error)),
        };
        let target_id = match self.resolve(to_query) {
            Ok(project) => project.id,
            Err(error) => return Outcome::Reply(Err(error)),
        };
        let source_id = self.projects[source_index].id;
        if source_id == target_id {
            return Outcome::Reply(Err(bad_request(
                "source and destination projects are the same".to_owned(),
            )));
        }
        let target_index = self
            .projects
            .iter()
            .position(|p| p.id == target_id)
            .expect("resolved");
        if self.projects[target_index]
            .groups
            .iter()
            .any(|g| g.slug == group_slug)
        {
            return Outcome::Reply(Err(conflict(format!(
                "project '{}' already has a group '{group_slug}'",
                self.projects[target_index].slug
            ))));
        }

        let group_def = {
            let source = &mut self.projects[source_index];
            let position = source
                .groups
                .iter()
                .position(|g| g.slug == group_slug)
                .expect("checked");
            source.groups.remove(position)
        };
        let hidden = group_def.hidden;
        self.projects[target_index].groups.push(group_def);
        let (source_project, target_project) = (
            self.projects[source_index].clone(),
            self.projects[target_index].clone(),
        );
        self.persist_project(&source_project);
        self.persist_project(&target_project);

        // Re-assign member windows and move them to the destination.
        let members = self.group_members(source_id, &group_slug);
        for address in &members {
            if let Some(window) = self.world.windows.get_mut(address) {
                window.assignment = Some((target_id, Some(group_slug.clone())));
            }
        }
        self.persist_runtime();
        let target_ws = if hidden {
            ws_names::group_workspace(
                &self.config.general.workspace_prefix,
                &target_project.slug,
                &group_slug,
            )
        } else {
            self.ws_name(&target_project.slug)
        };
        let dispatches: Vec<Dispatch> = members
            .iter()
            .map(|address| Dispatch::MoveToWorkspaceSilent {
                target: WsTarget::Name(target_ws.clone()),
                address: workspace_hypr::WindowAddress::new(address),
            })
            .collect();
        self.emit(DomainEvent::GroupChanged {
            project: target_project.slug.as_str().to_owned(),
            group: group_slug.as_str().to_owned(),
            change: "moved".into(),
        });
        Outcome::DispatchThen {
            dispatches,
            result: serde_json::json!({
                "group": group_slug,
                "from": source_project.slug,
                "to": target_project.slug,
                "windows": members.len(),
            }),
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
        self.persist_project(&project);
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
        if let Some(dir) = &self.state_dir
            && let Err(error) = workspace_storage::projects::delete_project(dir, &project.slug)
        {
            tracing::error!(%error, "cannot delete project file");
        }
        self.persist_runtime();
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
        let renamed = project.clone();
        let slug = renamed.slug.as_str().to_owned();
        self.persist_project(&renamed);
        let summary = self.summary(&renamed);
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
        self.persist_runtime();
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
        self.persist_runtime();
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
