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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, broadcast, mpsc};
use uuid::Uuid;
use workspace_core::DomainEvent;
use workspace_core::model::{LaunchSpec, Placement, ProjectId, Readiness};
use workspace_core::restore::{LaunchStep, RestorePlan};
use workspace_hypr::{Dispatch, HyprCtl, MoveDir, WindowAddress, WsTarget};
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
    // Address → slot id of the windows satisfying slots this run; doubles as
    // the claim registry during correlation and feeds the final layout pass.
    let claimed: Arc<Mutex<HashMap<String, Uuid>>> = Arc::default();

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
        claimed
            .lock()
            .await
            .insert(adopt.address.clone(), adopt.slot_id);
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

    // 4. Swap tiled windows back into their captured arrangement.
    apply_tiled_layout(&ctx, &*claimed.lock().await).await;

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

/// A captured target: (center, size).
type TargetRect = ((i32, i32), (i32, i32));
/// A rectangle as (top-left, size).
type TargetRectAbs = ((i32, i32), (i32, i32));
/// A live tiled window: (address, top-left, size).
type LiveRect = (String, (i32, i32), (i32, i32));

/// Restore the captured spatial arrangement of the tiled windows.
///
/// Tiled windows cannot be placed at coordinates; the pass iterates with
/// four corrective moves — resize an anchored window whose size is wrong,
/// swap toward an existing spot, flip a wrong-axis split, or move a window
/// into its neighbor's cell — until everything is close enough or the
/// budget runs out. Windows with the same slot identity (three consoles)
/// are interchangeable, so their targets are re-dealt to the nearest ones
/// each iteration instead of demanding a specific pairing.
async fn apply_tiled_layout(ctx: &RestoreContext, correlated: &HashMap<String, Uuid>) {
    // Captured rects + identity group per satisfied tiled slot.
    let slots: HashMap<Uuid, (&Placement, String)> = ctx
        .plan
        .adopt
        .iter()
        .map(|a| (a.slot_id, (&a.placement, identity_group(&a.identity))))
        .chain(
            ctx.plan
                .waves
                .iter()
                .flatten()
                .map(|s| (s.slot_id, (&s.placement, identity_group(&s.identity)))),
        )
        .collect();
    let targets: Vec<(String, String, TargetRect)> = correlated
        .iter()
        .filter_map(|(address, slot_id)| {
            let (placement, group) = slots.get(slot_id)?;
            if placement.floating {
                return None;
            }
            let (x, y) = placement.position?;
            let (w, h) = placement.size?;
            Some((
                address.clone(),
                group.clone(),
                ((x + w / 2, y + h / 2), (w, h)),
            ))
        })
        .collect();
    if targets.len() < 2 {
        return; // nothing to arrange (or an old file without geometry)
    }

    let budget = targets.len() * targets.len() * 2 + targets.len() * 2;
    let mut converged = false;
    // Total misplacement must keep improving; a step that stops helping
    // (unsatisfiable proportions ping-ponging a split) ends the pass early
    // instead of thrashing the layout until the budget runs out.
    let mut best_total = i64::MAX;
    let mut stalled = 0;
    for _ in 0..budget {
        let Some(current) = workspace_rects(ctx).await else {
            return;
        };
        // Interchangeable windows: deal each identity group's targets to
        // its nearest members, so a console never chases a specific spot
        // another identical console already sits in.
        let desired = assign_targets(&targets, &current);
        let total: i64 = desired
            .iter()
            .filter_map(|(address, (want_center, _))| {
                let (_, at, size) = current.iter().find(|(a, _, _)| a == *address)?;
                Some(distance2(*want_center, center_of(*at, *size)))
            })
            .sum();
        if total < best_total {
            best_total = total;
            stalled = 0;
        } else {
            stalled += 1;
            if stalled >= 4 {
                // Positions may still be fine (only proportions were being
                // chased); count that as converged for the final sweep.
                converged = worst_misplaced(&desired, &current).is_none();
                break;
            }
        }
        let Some(action) = next_action(&desired, &current) else {
            converged = true;
            break;
        };
        let (address, dispatch) = match action {
            LayoutAction::Swap(address, dir) => (address, Dispatch::SwapWindow(dir)),
            LayoutAction::Reinsert(address, dir) => (address, Dispatch::MoveWindowDir(dir)),
            LayoutAction::ToggleSplit(address) => (address, Dispatch::ToggleSplit),
            LayoutAction::Resize(address, (w, h)) => {
                (address, Dispatch::ResizeActiveExact { w, h })
            }
        };
        // A failed step (no neighbor in that direction, transient focus
        // trouble) must not end the pass: state is re-read next iteration
        // and the budget bounds retries.
        if let Err(error) = focused_dispatch(ctx, address, dispatch).await {
            tracing::debug!(%error, "layout pass: step failed; continuing");
        }
        // Give the compositor a beat to apply the new geometry.
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    tracing::info!(
        project = %ctx.plan.project,
        windows = targets.len(),
        converged,
        "layout pass finished"
    );
    if !converged {
        // Resizing an unconverged layout makes it worse (windows get
        // crushed against splits that are still wrong) — leave it be.
        return;
    }

    // Positions are settled; one resize sweep restores split proportions
    // (uneven stacks like a 70/30 editor/terminal). Tiled `resizeactive`
    // shifts the splits around the window, so this runs once, not to a
    // fixpoint — later resizes rebalance earlier ones close enough.
    let Some(current) = workspace_rects(ctx).await else {
        return;
    };
    let desired = assign_targets(&targets, &current);
    for (address, _at, size) in &current {
        let Some(((_, _), (want_w, want_h))) = desired.get(address.as_str()) else {
            continue;
        };
        if (want_w - size.0).abs() <= 48 && (want_h - size.1).abs() <= 48 {
            continue;
        }
        let resize = Dispatch::ResizeActiveExact {
            w: *want_w,
            h: *want_h,
        };
        if let Err(error) = focused_dispatch(ctx, address, resize).await {
            tracing::debug!(%error, "layout pass: resize failed; continuing");
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
}

/// Focus a window, let the focus settle, then run a dispatcher that acts on
/// the focused window. Separate requests: batching both in one tick is what
/// made directional dispatchers act on the previously focused window.
async fn focused_dispatch(
    ctx: &RestoreContext,
    address: &str,
    dispatch: Dispatch,
) -> Result<(), workspace_hypr::HyprError> {
    ctx.ctl
        .dispatch(&Dispatch::FocusWindow(WindowAddress::new(address)))
        .await?;
    tokio::time::sleep(Duration::from_millis(30)).await;
    ctx.ctl.dispatch(&dispatch).await
}

/// The tiled windows currently on the plan's workspace.
async fn workspace_rects(ctx: &RestoreContext) -> Option<Vec<LiveRect>> {
    match ctx.ctl.clients().await {
        Ok(clients) => Some(
            clients
                .iter()
                .filter(|c| !c.floating && c.workspace.name == ctx.plan.workspace)
                .map(|c| (c.address.as_str().to_owned(), c.at, c.size))
                .collect(),
        ),
        Err(error) => {
            tracing::warn!(%error, "layout pass: cannot list clients");
            None
        }
    }
}

/// One corrective step for the layout pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutAction<'a> {
    /// Swap this window toward its captured spot.
    Swap(&'a str, MoveDir),
    /// Move this window (`movewindow`) toward its captured neighbor so it
    /// enters that window's cell; dwindle splits the cell on entry.
    Reinsert(&'a str, MoveDir),
    /// The window and its captured neighbor already share the right
    /// combined area but are split along the wrong axis; flip the split.
    ToggleSplit(&'a str),
    /// The window is anchored at its captured corner but has the wrong
    /// size; resize it (neighbors redistribute into the freed space).
    Resize(&'a str, (i32, i32)),
}

/// Key identifying interchangeable slots: same identity → any of the
/// group's windows can satisfy any of the group's spots.
fn identity_group(identity: &workspace_core::model::WindowIdentity) -> String {
    format!(
        "{}|{}|{}|{}",
        identity.class.as_deref().unwrap_or(""),
        identity.initial_class.as_deref().unwrap_or(""),
        identity
            .executable
            .as_ref()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default(),
        identity.title_pattern.as_deref().unwrap_or(""),
    )
}

/// Deal each identity group's captured spots to its nearest live windows
/// (greedy, closest pair first). Windows of unique identity keep their own
/// spot; interchangeable ones (identical consoles) get whichever spot is
/// closest, so the pass never forces two look-alikes to trade places.
fn assign_targets<'t>(
    targets: &'t [(String, String, TargetRect)],
    current: &[LiveRect],
) -> HashMap<&'t str, TargetRect> {
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, (_, group, _)) in targets.iter().enumerate() {
        groups.entry(group.as_str()).or_default().push(index);
    }
    let position = |address: &str| {
        current
            .iter()
            .find(|(a, _, _)| a == address)
            .map(|(_, at, size)| center_of(*at, *size))
    };
    let mut assigned: HashMap<&'t str, TargetRect> = HashMap::new();
    for members in groups.into_values() {
        // (distance, member index in `targets` for the window, index for
        // the spot) — sorted so the closest window-spot pair wins first.
        let mut pairs: Vec<(i64, usize, usize)> = Vec::new();
        for &window in &members {
            let Some(window_center) = position(&targets[window].0) else {
                continue;
            };
            for &spot in &members {
                let (spot_center, _) = targets[spot].2;
                pairs.push((distance2(window_center, spot_center), window, spot));
            }
        }
        pairs.sort();
        let mut window_taken = vec![false; targets.len()];
        let mut spot_taken = vec![false; targets.len()];
        for (_, window, spot) in pairs {
            if window_taken[window] || spot_taken[spot] {
                continue;
            }
            window_taken[window] = true;
            spot_taken[spot] = true;
            assigned.insert(targets[window].0.as_str(), targets[spot].2);
        }
    }
    assigned
}

/// The window farthest from its assigned captured center, beyond tolerance
/// (a quarter of the captured size — gaps and reserved edges shift things
/// by a handful of pixels). `None` when every window is close enough.
fn worst_misplaced<'d>(
    desired: &HashMap<&'d str, TargetRect>,
    current: &[LiveRect],
) -> Option<&'d str> {
    let mut worst: Option<(&'d str, i64)> = None;
    for (address, (want_center, (want_w, want_h))) in desired {
        let Some((_, at, size)) = current.iter().find(|(a, _, _)| a == address) else {
            continue;
        };
        let distance = distance2(*want_center, center_of(*at, *size));
        let tolerance = i64::from(want_w.min(want_h) / 4).pow(2);
        if distance > tolerance && worst.is_none_or(|(_, d)| distance > d) {
            worst = Some((address, distance));
        }
    }
    worst.map(|(address, _)| address)
}

fn dominant_direction(dx: i64, dy: i64) -> MoveDir {
    if dx.abs() >= dy.abs() {
        if dx < 0 { MoveDir::Left } else { MoveDir::Right }
    } else if dy < 0 {
        MoveDir::Up
    } else {
        MoveDir::Down
    }
}

fn center_of(at: (i32, i32), size: (i32, i32)) -> (i32, i32) {
    (at.0 + size.0 / 2, at.1 + size.1 / 2)
}

fn distance2(a: (i32, i32), b: (i32, i32)) -> i64 {
    let (dx, dy) = (i64::from(a.0 - b.0), i64::from(a.1 - b.1));
    dx * dx + dy * dy
}

/// Bounding box over rects given as (top-left, size): (top-left, size).
fn bounding_box(rects: &[TargetRectAbs]) -> TargetRectAbs {
    let left = rects.iter().map(|((x, _), _)| *x).min().unwrap_or(0);
    let top = rects.iter().map(|((_, y), _)| *y).min().unwrap_or(0);
    let right = rects.iter().map(|((x, _), (w, _))| x + w).max().unwrap_or(0);
    let bottom = rects.iter().map(|((_, y), (_, h))| y + h).max().unwrap_or(0);
    ((left, top), (right - left, bottom - top))
}

/// Pick the next corrective action for the window farthest from its captured
/// center (beyond tolerance):
/// - some current rectangle sits at the captured spot → swap toward it;
/// - it and its captured neighbor already occupy the right combined area,
///   split along the wrong axis → toggle the split;
/// - otherwise → move it toward the neighbor's *current* cell, entering it.
///
/// `None` when every window is close enough to where it was.
fn next_action<'d>(
    desired: &HashMap<&'d str, TargetRect>,
    current: &[LiveRect],
) -> Option<LayoutAction<'d>> {
    let rect_of = |address: &str| {
        current
            .iter()
            .find(|(a, _, _)| a == address)
            .map(|(_, at, size)| (*at, *size))
    };

    // Safe shrinks first: a window anchored at its captured corner that is
    // larger than captured (and needs no growth) resizes in place — the
    // "misplaced" center is a proportion problem, and shrinking frees space
    // for the others without ever crushing anyone. Growth is never forced
    // here: it can exceed what the topology allows and crush neighbors.
    let mut best_shrink: Option<(&'d str, (i32, i32), i64)> = None;
    for (address, ((want_cx, want_cy), (want_w, want_h))) in desired {
        let Some((at, size)) = rect_of(address) else {
            continue;
        };
        let want_at = (want_cx - want_w / 2, want_cy - want_h / 2);
        let tolerance = i64::from(want_w.min(want_h) / 4).pow(2);
        let needs_shrink = size.0 - want_w > 48 || size.1 - want_h > 48;
        let needs_growth = want_w - size.0 > 48 || want_h - size.1 > 48;
        if needs_shrink && !needs_growth && distance2(want_at, at) <= tolerance {
            let excess =
                i64::from(size.0) * i64::from(size.1) - i64::from(*want_w) * i64::from(*want_h);
            if best_shrink.is_none_or(|(_, _, e)| excess > e) {
                best_shrink = Some((address, (*want_w, *want_h), excess));
            }
        }
    }
    if let Some((address, size, _)) = best_shrink {
        return Some(LayoutAction::Resize(address, size));
    }

    let worst = worst_misplaced(desired, current);
    let address = worst?;
    let (want_center, (want_w, want_h)) = desired[address];
    let (at, size) = rect_of(address)?;
    let tolerance = i64::from(want_w.min(want_h) / 4).pow(2);

    // Swaps only permute rectangles: they can reach the captured spot only
    // if some other window's rectangle is already there.
    let target_exists = current.iter().any(|(other, o_at, o_size)| {
        other != address && distance2(want_center, center_of(*o_at, *o_size)) <= tolerance
    });
    if target_exists {
        let dx = i64::from(want_center.0 - center_of(at, size).0);
        let dy = i64::from(want_center.1 - center_of(at, size).1);
        return Some(LayoutAction::Swap(address, dominant_direction(dx, dy)));
    }

    // The captured neighbor: the window this one shares an edge (a split)
    // with in the captured layout.
    let (neighbor, (neighbor_center, neighbor_size)) = desired
        .iter()
        .filter(|(other, _)| **other != address)
        .min_by_key(|(_, (other_center, _))| distance2(*other_center, want_center))
        .map(|(a, r)| (*a, *r))?;
    let (neighbor_at, neighbor_live_size) = rect_of(neighbor)?;

    // If the pair's current combined area matches its captured combined
    // area, they are the right two windows in the right place, split along
    // the wrong axis — a toggle fixes it in place. Compared via bounding
    // boxes with the usual tolerance.
    let rect_from_center =
        |(cx, cy): (i32, i32), (w, h): (i32, i32)| ((cx - w / 2, cy - h / 2), (w, h));
    let want_bb = bounding_box(&[
        rect_from_center(want_center, (want_w, want_h)),
        rect_from_center(neighbor_center, neighbor_size),
    ]);
    let live_bb = bounding_box(&[(at, size), (neighbor_at, neighbor_live_size)]);
    let bb_tolerance = i64::from(want_bb.1.0.min(want_bb.1.1) / 4).pow(2);
    let bb_matches = distance2(center_of(want_bb.0, want_bb.1), center_of(live_bb.0, live_bb.1))
        <= bb_tolerance
        && distance2(want_bb.1, live_bb.1) <= bb_tolerance;
    if bb_matches {
        return Some(LayoutAction::ToggleSplit(address));
    }

    // Otherwise enter the neighbor's current cell: move toward where the
    // neighbor is *now*, not where the capture says it belongs.
    let dx = i64::from(center_of(neighbor_at, neighbor_live_size).0 - center_of(at, size).0);
    let dy = i64::from(center_of(neighbor_at, neighbor_live_size).1 - center_of(at, size).1);
    Some(LayoutAction::Reinsert(address, dominant_direction(dx, dy)))
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
    claimed: Arc<Mutex<HashMap<String, Uuid>>>,
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
    claimed: &Arc<Mutex<HashMap<String, Uuid>>>,
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
            if claimed.contains_key(address) {
                continue; // already satisfied another slot
            }
            claimed.insert(address.clone(), step.slot_id);
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

    #[allow(clippy::type_complexity)] // test helper mirroring fixture tuples
    fn live(rects: &[(&str, (i32, i32), (i32, i32))]) -> Vec<LiveRect> {
        rects
            .iter()
            .map(|(a, at, size)| (a.to_string(), *at, *size))
            .collect()
    }

    /// Simulate `swapwindow`: exchange the rectangles of the focused window
    /// and its nearest neighbor in the given direction.
    fn simulate_swap(current: &mut [LiveRect], address: &str, dir: MoveDir) {
        let from = current.iter().position(|(a, _, _)| a == address).unwrap();
        let (_, at, size) = current[from];
        let center = (at.0 + size.0 / 2, at.1 + size.1 / 2);
        let neighbor = current
            .iter()
            .enumerate()
            .filter(|(i, (_, n_at, n_size))| {
                let n_center = (n_at.0 + n_size.0 / 2, n_at.1 + n_size.1 / 2);
                *i != from
                    && match dir {
                        MoveDir::Left => n_center.0 < center.0,
                        MoveDir::Right => n_center.0 > center.0,
                        MoveDir::Up => n_center.1 < center.1,
                        MoveDir::Down => n_center.1 > center.1,
                    }
            })
            .min_by_key(|(_, (_, n_at, n_size))| {
                let n_center = (n_at.0 + n_size.0 / 2, n_at.1 + n_size.1 / 2);
                let (dx, dy) = (
                    i64::from(n_center.0 - center.0),
                    i64::from(n_center.1 - center.1),
                );
                dx * dx + dy * dy
            })
            .map(|(i, _)| i);
        if let Some(to) = neighbor {
            let (_, b_at, b_size) = current[to];
            current[to].1 = at;
            current[to].2 = size;
            current[from].1 = b_at;
            current[from].2 = b_size;
        }
    }

    #[test]
    fn layout_pass_converges_on_permuted_windows() {
        // Captured layout: editor fills the left half, browser top-right,
        // terminal bottom-right.
        let left = ((0, 0), (1720, 1440));
        let top_right = ((1720, 0), (1720, 720));
        let bottom_right = ((1720, 720), (1720, 720));
        let desired: HashMap<&str, TargetRect> = [
            ("editor", (center(left), left.1)),
            ("browser", (center(top_right), top_right.1)),
            ("term", (center(bottom_right), bottom_right.1)),
        ]
        .into_iter()
        .collect();

        // The windows reopened into the same tree, fully permuted.
        let mut current = live(&[
            ("term", left.0, left.1),
            ("editor", top_right.0, top_right.1),
            ("browser", bottom_right.0, bottom_right.1),
        ]);

        let mut swaps = 0;
        while let Some(action) = next_action(&desired, &current) {
            let LayoutAction::Swap(address, dir) = action else {
                panic!("same tree shape must never toggle: {action:?}");
            };
            simulate_swap(&mut current, address, dir);
            swaps += 1;
            assert!(swaps <= 18, "did not converge: {current:?}");
        }
        for (address, at, size) in &current {
            let (want_center, _) = desired[address.as_str()];
            assert_eq!((at.0 + size.0 / 2, at.1 + size.1 / 2), want_center);
        }
        assert!(swaps >= 2, "permutation needs at least two swaps");
    }

    #[test]
    fn orientation_mismatch_reinserts_along_the_captured_axis() {
        // Captured: stacked vertically. Live: dwindle opened them
        // side-by-side — no rectangle sits at either captured spot, so no
        // amount of swapping helps; a window must be re-inserted, and the
        // direction must be vertical (the captured relationship), never the
        // horizontal axis the current distance vector points along.
        let top = ((0, 0), (3440, 720));
        let bottom = ((0, 720), (3440, 720));
        let desired: HashMap<&str, TargetRect> = [
            ("editor", (center(top), top.1)),
            ("term", (center(bottom), bottom.1)),
        ]
        .into_iter()
        .collect();
        let side_by_side = live(&[
            ("editor", (0, 0), (1720, 1440)),
            ("term", (1720, 0), (1720, 1440)),
        ]);
        // The pair occupies the right combined area (the whole workspace):
        // this is a pure orientation problem — toggle, don't move.
        assert!(matches!(
            next_action(&desired, &side_by_side),
            Some(LayoutAction::ToggleSplit(_))
        ));

        // After the toggle the rectangles are stacked but swapped: now the
        // captured spots exist and a swap finishes the job.
        let mut stacked_swapped = live(&[("editor", bottom.0, bottom.1), ("term", top.0, top.1)]);
        let action = next_action(&desired, &stacked_swapped);
        let Some(LayoutAction::Swap(address, dir)) = action else {
            panic!("expected a swap, got {action:?}");
        };
        simulate_swap(&mut stacked_swapped, address, dir);
        assert_eq!(next_action(&desired, &stacked_swapped), None);
    }

    fn center(rect: ((i32, i32), (i32, i32))) -> (i32, i32) {
        ((rect.0).0 + (rect.1).0 / 2, (rect.0).1 + (rect.1).1 / 2)
    }

    #[test]
    fn three_columns_reinserts_into_the_neighbors_column() {
        // Captured: console above vscode in the left half, chromium right.
        // Restored: dwindle opened three columns. The misplaced stack
        // member must move INTO its neighbor's current cell (horizontal
        // direction), not toward its captured relationship (vertical) —
        // that was the bug that produced a full-width bottom strip.
        let console = ((0, 0), (1720, 720));
        let vscode = ((0, 720), (1720, 720));
        let chromium = ((1720, 0), (1720, 1440));
        let desired: HashMap<&str, TargetRect> = [
            ("console", (center(console), console.1)),
            ("vscode", (center(vscode), vscode.1)),
            ("chromium", (center(chromium), chromium.1)),
        ]
        .into_iter()
        .collect();
        let three_columns = live(&[
            ("console", (0, 0), (1146, 1440)),
            ("vscode", (1146, 0), (1146, 1440)),
            ("chromium", (2293, 0), (1147, 1440)),
        ]);
        match next_action(&desired, &three_columns) {
            // Whichever of the pair is judged worst, it must move
            // horizontally toward the other's column — never Up/Down.
            Some(LayoutAction::Reinsert("vscode", MoveDir::Left))
            | Some(LayoutAction::Reinsert("console", MoveDir::Right)) => {}
            other => panic!("expected a horizontal reinsert, got {other:?}"),
        }
    }

    #[test]
    fn anchored_window_with_wrong_size_is_resized_not_moved() {
        // Chromium sits exactly at its captured corner but twice as wide —
        // its center is off, yet the fix is a resize, never topology moves.
        let desired: HashMap<&str, TargetRect> = [
            ("chromium", ((477, 733), (834, 1390))),
            ("code", ((2589, 733), (1677, 1390))),
        ]
        .into_iter()
        .collect();
        let current = live(&[
            ("chromium", (60, 38), (1677, 1390)),
            ("code", (1751, 38), (1677, 1390)),
        ]);
        assert_eq!(
            next_action(&desired, &current),
            Some(LayoutAction::Resize("chromium", (834, 1390)))
        );
    }

    #[test]
    fn interchangeable_windows_take_their_nearest_spots() {
        let identity = |class: &str| workspace_core::model::WindowIdentity {
            class: Some(class.into()),
            ..Default::default()
        };
        // Three identical consoles; the slot↔window binding is reversed
        // relative to where they actually opened.
        let spot = |x, y, w, h| ((x + w / 2, y + h / 2), (w, h));
        let targets = vec![
            ("a".to_owned(), identity_group(&identity("Alacritty")), spot(0, 0, 800, 700)),
            ("b".to_owned(), identity_group(&identity("Alacritty")), spot(0, 700, 800, 700)),
            ("c".to_owned(), identity_group(&identity("Alacritty")), spot(800, 0, 800, 1400)),
            ("d".to_owned(), identity_group(&identity("code")), spot(1600, 0, 800, 1400)),
        ];
        // Live: consoles a/b/c sit in each other's captured spots; code is
        // in place. With re-dealt targets nothing needs to move at all.
        let current = live(&[
            ("a", (800, 0), (800, 1400)),
            ("b", (0, 0), (800, 700)),
            ("c", (0, 700), (800, 700)),
            ("d", (1600, 0), (800, 1400)),
        ]);
        let desired = assign_targets(&targets, &current);
        assert_eq!(desired["a"], spot(800, 0, 800, 1400));
        assert_eq!(desired["b"], spot(0, 0, 800, 700));
        assert_eq!(desired["c"], spot(0, 700, 800, 700));
        assert_eq!(desired["d"], spot(1600, 0, 800, 1400));
        assert_eq!(next_action(&desired, &current), None);

        // A unique-identity window (code) never trades spots with the
        // consoles even when misplaced.
        let swapped = live(&[
            ("a", (0, 0), (800, 700)),
            ("b", (0, 700), (800, 700)),
            ("c", (1600, 0), (800, 1400)),
            ("d", (800, 0), (800, 1400)),
        ]);
        let desired = assign_targets(&targets, &swapped);
        assert_eq!(desired["d"], spot(1600, 0, 800, 1400));
        assert_eq!(desired["c"], spot(800, 0, 800, 1400));
    }

    #[test]
    fn already_correct_layout_needs_no_swaps() {
        let rect = ((0, 0), (1720, 1440));
        let desired: HashMap<&str, TargetRect> =
            [("editor", (center(rect), rect.1))].into_iter().collect();
        let current = live(&[("editor", rect.0, rect.1)]);
        assert_eq!(next_action(&desired, &current), None);
        // A missing window (closed mid-restore) is simply skipped.
        assert_eq!(next_action(&desired, &[]), None);
    }

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
