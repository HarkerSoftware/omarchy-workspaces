//! Restore-plan execution: adopt existing windows, launch missing apps in
//! dependency waves, correlate new windows back to their slots, and stream
//! progress events.
//!
//! Window apps launch through `dispatch exec [workspace name:<ws> silent] …`
//! so Hyprland places them on the project workspace natively; `service` slots
//! (databases, docker) spawn directly and are started but not supervised.
//! Correlation matches `window.opened` events against slot identities by
//! class (case-insensitive); each window satisfies at most one slot. Failures
//! and timeouts skip dependent slots and are reported, never hung on.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, broadcast, mpsc};
use workspace_core::DomainEvent;
use workspace_core::model::{LaunchSpec, ProjectId, Readiness};
use workspace_core::restore::{LaunchStep, RestorePlan};
use workspace_hypr::{Dispatch, HyprCtl, WindowAddress, WsTarget};
use workspace_proto::EventEnvelope;

use crate::actor::Command;

/// Everything the executor needs; assembled by the actor.
pub struct RestoreContext {
    /// The plan to execute.
    pub plan: RestorePlan,
    /// The owning project.
    pub project_id: ProjectId,
    /// Hyprland control client.
    pub ctl: HyprCtl,
    /// Actor command channel (correlation + progress emission).
    pub commands: mpsc::Sender<Command>,
    /// Bus sender to derive per-step event subscriptions from.
    pub bus: broadcast::Sender<Arc<EventEnvelope>>,
    /// Timeout applied when a spec has none.
    pub default_timeout: Duration,
}

impl std::fmt::Debug for RestoreContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestoreContext")
            .field("project", &self.plan.project)
            .finish()
    }
}

/// POSIX single-quote escaping.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Build the shell command line: env assignments, cd, command, args.
fn build_shell_command(spec: &LaunchSpec) -> String {
    let mut parts = Vec::new();
    if let Some(workdir) = &spec.workdir {
        parts.push(format!("cd {} &&", shell_quote(&expand_home(workdir))));
    }
    for (key, value) in &spec.env {
        parts.push(format!("{key}={}", shell_quote(value)));
    }
    parts.push(spec.command.clone());
    for arg in &spec.args {
        parts.push(shell_quote(arg));
    }
    parts.join(" ")
}

fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return format!("{}{rest}", home.to_string_lossy());
    }
    path.to_owned()
}

async fn emit(commands: &mpsc::Sender<Command>, event: DomainEvent) {
    let _ = commands.send(Command::RestoreEvent(event)).await;
}

/// Execute the plan. Runs as its own task; reports through the actor.
pub async fn execute(ctx: RestoreContext) {
    let project = ctx.plan.project.as_str().to_owned();
    let total = ctx.plan.launch_count();
    let mut completed = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let claimed: Arc<Mutex<HashSet<String>>> = Arc::default();

    // 1. Adopt existing windows: assign, move, and place them.
    let mut moves = Vec::new();
    for adopt in &ctx.plan.adopt {
        let _ = ctx
            .commands
            .send(Command::CorrelateRestore {
                address: adopt.address.clone(),
                slot_id: adopt.slot_id,
                project_id: ctx.project_id,
                group: adopt.group.clone(),
            })
            .await;
        claimed.lock().await.insert(adopt.address.clone());
        if adopt.needs_move {
            moves.push(Dispatch::MoveToWorkspaceSilent {
                target: WsTarget::Name(ctx.plan.workspace.clone()),
                address: WindowAddress::new(&adopt.address),
            });
        }
        moves.extend(placement_dispatches(&adopt.address, &adopt.placement));
    }
    if let Err(error) = ctx.ctl.dispatch_batch(&moves).await {
        tracing::warn!(%error, "adoption dispatches failed");
    }

    // 2. Launch missing slots wave by wave.
    for wave in &ctx.plan.waves {
        let mut handles = Vec::new();
        for step in wave {
            // Skip steps whose dependencies failed.
            if step.after.iter().any(|dep| failed.contains(dep)) {
                emit(
                    &ctx.commands,
                    DomainEvent::RestoreProgress {
                        project: project.clone(),
                        slot: step.label.clone(),
                        state: "skipped".into(),
                        completed,
                        total,
                    },
                )
                .await;
                failed.push(step.label.clone());
                continue;
            }
            handles.push((
                step.label.clone(),
                tokio::spawn(run_step(
                    step.clone(),
                    ctx.plan.workspace.clone(),
                    ctx.project_id,
                    ctx.ctl.clone(),
                    ctx.commands.clone(),
                    ctx.bus.subscribe(),
                    Arc::clone(&claimed),
                    ctx.default_timeout,
                    project.clone(),
                    completed,
                    total,
                )),
            ));
        }
        for (label, handle) in handles {
            match handle.await {
                Ok(true) => completed += 1,
                Ok(false) => failed.push(label),
                Err(_) => failed.push(label),
            }
        }
    }

    // 3. Focus the project workspace.
    if let Err(error) = ctx
        .ctl
        .dispatch(&Dispatch::Workspace(WsTarget::Name(
            ctx.plan.workspace.clone(),
        )))
        .await
    {
        tracing::warn!(%error, "final workspace focus failed");
    }

    emit(
        &ctx.commands,
        DomainEvent::RestoreFinished {
            project,
            adopted: ctx.plan.adopt.len(),
            launched: completed,
            failed,
        },
    )
    .await;
}

/// Dispatches to float/position/size a window per its slot placement.
pub fn placement_dispatches(
    address: &str,
    placement: &workspace_core::model::Placement,
) -> Vec<Dispatch> {
    let mut dispatches = Vec::new();
    if placement.floating {
        let address = WindowAddress::new(address);
        dispatches.push(Dispatch::SetFloating(address.clone()));
        if let Some((x, y)) = placement.position {
            dispatches.push(Dispatch::MoveWindowPixelExact {
                address: address.clone(),
                x,
                y,
            });
        }
        if let Some((w, h)) = placement.size {
            dispatches.push(Dispatch::ResizeWindowPixelExact { address, w, h });
        }
    }
    dispatches
}

/// Launch one slot and wait for readiness. Returns whether it became ready.
#[allow(clippy::too_many_arguments)] // internal plumbing, called from one place
async fn run_step(
    step: LaunchStep,
    workspace: String,
    project_id: ProjectId,
    ctl: HyprCtl,
    commands: mpsc::Sender<Command>,
    mut bus: broadcast::Receiver<Arc<EventEnvelope>>,
    claimed: Arc<Mutex<HashSet<String>>>,
    default_timeout: Duration,
    project: String,
    completed: usize,
    total: usize,
) -> bool {
    let progress = |state: &str| {
        let commands = commands.clone();
        let project = project.clone();
        let slot = step.label.clone();
        let state = state.to_owned();
        async move {
            emit(
                &commands,
                DomainEvent::RestoreProgress {
                    project,
                    slot,
                    state,
                    completed,
                    total,
                },
            )
            .await;
        }
    };
    progress("launching").await;

    let command_line = build_shell_command(&step.spec);
    if step.spec.service {
        // Services are user processes we start but do not manage.
        let spawned = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command_line)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Err(error) = spawned {
            tracing::warn!(%error, slot = step.label, "service spawn failed");
            progress("failed").await;
            return false;
        }
    } else {
        let dispatch = Dispatch::Exec {
            rules: vec![format!("workspace name:{workspace} silent")],
            command: command_line,
        };
        if let Err(error) = ctl.dispatch(&dispatch).await {
            tracing::warn!(%error, slot = step.label, "exec dispatch failed");
            progress("failed").await;
            return false;
        }
    }

    let timeout = step
        .spec
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(default_timeout);
    let ready = match &step.readiness {
        Readiness::Delay => true,
        Readiness::Command { cmd, interval_ms } => {
            wait_probe(cmd, Duration::from_millis(*interval_ms), timeout).await
        }
        Readiness::Window => {
            wait_window(&step, project_id, &commands, &mut bus, &claimed, timeout).await
        }
    };

    if !ready {
        progress("timeout").await;
        return false;
    }
    if step.spec.startup_delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(step.spec.startup_delay_ms)).await;
    }
    progress("ready").await;
    true
}

/// Poll a readiness probe command until it succeeds or the timeout passes.
async fn wait_probe(cmd: &str, interval: Duration, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let status = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .status()
            .await;
        if matches!(status, Ok(status) if status.success()) {
            return true;
        }
        if tokio::time::Instant::now() + interval > deadline {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Wait for an unclaimed window matching the slot's identity, claim it, and
/// correlate it through the actor (which also applies placement).
async fn wait_window(
    step: &LaunchStep,
    project_id: ProjectId,
    commands: &mpsc::Sender<Command>,
    bus: &mut broadcast::Receiver<Arc<EventEnvelope>>,
    claimed: &Arc<Mutex<HashSet<String>>>,
    timeout: Duration,
) -> bool {
    let wanted_class = step
        .identity
        .class
        .clone()
        .or(step.identity.initial_class.clone());
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let event = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return false,
            event = bus.recv() => event,
        };
        let envelope = match event {
            Ok(envelope) => envelope,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return false,
        };
        let DomainEvent::WindowOpened { address, class, .. } = &envelope.data else {
            continue;
        };
        let matches = match &wanted_class {
            Some(wanted) => class.eq_ignore_ascii_case(wanted),
            // No class in the identity: first unclaimed window wins.
            None => true,
        };
        if !matches {
            continue;
        }
        {
            let mut claimed = claimed.lock().await;
            if !claimed.insert(address.clone()) {
                continue; // already satisfied another slot
            }
        }
        let _ = commands
            .send(Command::CorrelateRestore {
                address: address.clone(),
                slot_id: step.slot_id,
                project_id,
                group: step.group.clone(),
            })
            .await;
        return true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_command_building() {
        let spec = LaunchSpec {
            command: "wezterm".into(),
            args: vec!["start".into(), "--cwd".into(), "/tmp/it's here".into()],
            env: [("RUST_LOG".to_string(), "debug".to_string())].into(),
            workdir: Some("~/Projects/api".into()),
            ..Default::default()
        };
        let cmd = build_shell_command(&spec);
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            cmd,
            format!(
                "cd '{home}/Projects/api' && RUST_LOG='debug' wezterm 'start' '--cwd' '/tmp/it'\\''s here'"
            )
        );
    }
}
