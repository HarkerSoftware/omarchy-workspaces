//! Restore planning: pure functions that diff a project's declarative slots
//! against live windows and produce an executable plan.
//!
//! Matching is scored: executable path 100, class 50, initial class 40,
//! title pattern 10 — a window claims a slot only at a total of 50 or more,
//! and each window satisfies at most one slot (greedy, best score first).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::launch::{self, LaunchPlanError};
use crate::model::{Project, Readiness, WindowIdentity};
use crate::world::TrackedWindow;
use crate::ws_names;

/// Score a live window against a slot identity. `None` when below threshold.
pub fn identity_score(identity: &WindowIdentity, window: &TrackedWindow) -> Option<u32> {
    let mut score = 0;
    if let Some(exe) = &identity.executable
        && window.facts.executable.as_deref() == Some(exe.as_path())
    {
        score += 100;
    }
    if let Some(class) = &identity.class
        && window.facts.class.eq_ignore_ascii_case(class)
    {
        score += 50;
    }
    if let Some(initial) = &identity.initial_class
        && window.facts.initial_class.eq_ignore_ascii_case(initial)
    {
        score += 40;
    }
    if let Some(pattern) = &identity.title_pattern
        && regex::Regex::new(pattern)
            .map(|re| re.is_match(&window.facts.title))
            .unwrap_or(false)
    {
        score += 10;
    }
    (score >= 50).then_some(score)
}

/// Result of diffing desired slots against live windows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reconciliation {
    /// Slot → live window address pairs.
    pub matched: Vec<(Uuid, String)>,
    /// Slots with no live window.
    pub missing: Vec<Uuid>,
    /// Live windows assigned to the project but matching no slot.
    pub extra: Vec<String>,
}

/// Greedy best-score matching between a project's slots and candidate
/// windows. `candidates` should be the windows plausibly belonging to the
/// project (assigned to it, or unassigned).
pub fn reconcile(project: &Project, candidates: &[&TrackedWindow]) -> Reconciliation {
    let mut pairs: Vec<(u32, usize, usize)> = Vec::new(); // (score, slot idx, window idx)
    for (si, slot) in project.apps.iter().enumerate() {
        for (wi, window) in candidates.iter().enumerate() {
            if let Some(score) = identity_score(&slot.identity, window) {
                pairs.push((score, si, wi));
            }
        }
    }
    pairs.sort_by_key(|(score, si, wi)| (std::cmp::Reverse(*score), *si, *wi));

    let mut slot_taken = vec![false; project.apps.len()];
    let mut window_taken = vec![false; candidates.len()];
    let mut matched = Vec::new();
    for (_, si, wi) in pairs {
        if slot_taken[si] || window_taken[wi] {
            continue;
        }
        slot_taken[si] = true;
        window_taken[wi] = true;
        matched.push((project.apps[si].slot_id, candidates[wi].address.clone()));
    }

    let missing = project
        .apps
        .iter()
        .filter(|slot| !matched.iter().any(|(id, _)| *id == slot.slot_id))
        .map(|slot| slot.slot_id)
        .collect();
    let extra = candidates
        .iter()
        .filter(|w| {
            // Only windows already assigned to this project count as extras.
            w.assignment
                .as_ref()
                .is_some_and(|(id, _)| *id == project.id)
                && !matched.iter().any(|(_, addr)| *addr == w.address)
        })
        .map(|w| w.address.clone())
        .collect();
    Reconciliation {
        matched,
        missing,
        extra,
    }
}

/// One slot to launch, with everything the executor needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchStep {
    /// The slot being launched.
    pub slot_id: Uuid,
    /// Display label for progress output.
    pub label: String,
    /// Full launch specification.
    pub spec: crate::model::LaunchSpec,
    /// Identity used to correlate the resulting window.
    pub identity: WindowIdentity,
    /// Desired placement once the window appears.
    pub placement: crate::model::Placement,
    /// Group slug the window belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<crate::model::Slug>,
    /// Labels of the slots this one waits for (for skip propagation).
    #[serde(default)]
    pub after: Vec<String>,
    /// Effective readiness (copied from the spec for executor convenience).
    pub readiness: Readiness,
}

/// A window that already exists and just needs adopting into the project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdoptStep {
    /// The slot satisfied by this window.
    pub slot_id: Uuid,
    /// Display label.
    pub label: String,
    /// The live window's address.
    pub address: String,
    /// Whether the window must move to the project workspace.
    pub needs_move: bool,
    /// Desired placement to (re)apply.
    pub placement: crate::model::Placement,
    /// Group slug the window belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<crate::model::Slug>,
}

/// The full executable restore plan. Serializable for `--dry-run`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestorePlan {
    /// Project slug.
    pub project: crate::model::Slug,
    /// The Hyprland workspace name backing the project.
    pub workspace: String,
    /// Existing windows to adopt.
    pub adopt: Vec<AdoptStep>,
    /// Missing slots to launch, in dependency waves.
    pub waves: Vec<Vec<LaunchStep>>,
    /// Windows assigned to the project but matching no slot (left alone).
    pub extra: Vec<String>,
}

impl RestorePlan {
    /// Total number of slots to launch.
    pub fn launch_count(&self) -> usize {
        self.waves.iter().map(Vec::len).sum()
    }
}

/// Errors from restore planning.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// The dependency graph is invalid.
    #[error(transparent)]
    Launch(#[from] LaunchPlanError),
}

/// Build the plan for restoring `project`.
///
/// `candidates` are live windows that may satisfy slots (assigned to the
/// project or unassigned); `workspace_prefix` is the configured prefix.
pub fn plan(
    project: &Project,
    candidates: &[&TrackedWindow],
    workspace_prefix: &str,
) -> Result<RestorePlan, PlanError> {
    let workspace = ws_names::project_workspace(workspace_prefix, &project.slug);
    let reconciliation = reconcile(project, candidates);

    let adopt = reconciliation
        .matched
        .iter()
        .filter_map(|(slot_id, address)| {
            let slot = project.apps.iter().find(|s| s.slot_id == *slot_id)?;
            let window = candidates.iter().find(|w| w.address == *address)?;
            Some(AdoptStep {
                slot_id: *slot_id,
                label: slot.label(),
                address: address.clone(),
                needs_move: window.facts.workspace != workspace,
                placement: slot.placement.clone(),
                group: slot.group.clone(),
            })
        })
        .collect();

    let missing_slots: Vec<&crate::model::AppSlot> = project
        .apps
        .iter()
        .filter(|slot| reconciliation.missing.contains(&slot.slot_id))
        .collect();
    // Waves are computed over the whole project so dependencies on already-
    // running slots are honored, then filtered down to the missing ones.
    let all_slots: Vec<&crate::model::AppSlot> = project.apps.iter().collect();
    let all_waves = launch::waves(&all_slots)?;
    let waves: Vec<Vec<LaunchStep>> = all_waves
        .into_iter()
        .map(|wave| {
            wave.into_iter()
                .filter_map(|slot_id| {
                    let slot = missing_slots.iter().find(|s| s.slot_id == slot_id)?;
                    let spec = slot.launch.clone()?;
                    Some(LaunchStep {
                        slot_id,
                        label: slot.label(),
                        identity: slot.identity.clone(),
                        placement: slot.placement.clone(),
                        group: slot.group.clone(),
                        after: spec.after.clone(),
                        readiness: spec.readiness.clone(),
                        spec,
                    })
                })
                .collect::<Vec<_>>()
        })
        .filter(|wave| !wave.is_empty())
        .collect();

    Ok(RestorePlan {
        project: project.slug.clone(),
        workspace,
        adopt,
        waves,
        extra: reconciliation.extra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AppSlot, LaunchSpec, Placement, Slug};
    use crate::world::{TrackedWindow, WindowFacts};

    fn window(address: &str, class: &str, exe: Option<&str>) -> TrackedWindow {
        TrackedWindow {
            address: address.to_owned(),
            stable_id: None,
            facts: WindowFacts {
                class: class.to_owned(),
                initial_class: class.to_owned(),
                executable: exe.map(Into::into),
                workspace: "1".to_owned(),
                ..Default::default()
            },
            assignment: None,
            assigned_by: None,
        }
    }

    fn slot(name: &str, class: &str, after: &[&str]) -> AppSlot {
        AppSlot {
            slot_id: uuid::Uuid::new_v4(),
            name: Some(name.to_owned()),
            identity: WindowIdentity {
                class: Some(class.to_owned()),
                ..Default::default()
            },
            launch: Some(LaunchSpec {
                command: class.to_owned(),
                after: after.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            }),
            group: None,
            placement: Placement::default(),
        }
    }

    fn project(slots: Vec<AppSlot>) -> Project {
        let mut p = Project::new("Test").unwrap();
        p.slug = Slug::parse("test").unwrap();
        p.apps = slots;
        p
    }

    #[test]
    fn scoring_thresholds() {
        let identity = WindowIdentity {
            class: Some("firefox".into()),
            ..Default::default()
        };
        assert_eq!(
            identity_score(&identity, &window("0xa", "Firefox", None)),
            Some(50)
        );
        assert_eq!(
            identity_score(&identity, &window("0xa", "kitty", None)),
            None
        );

        // Title alone can never claim a slot (10 < 50).
        let weak = WindowIdentity {
            title_pattern: Some(".*".into()),
            ..Default::default()
        };
        assert_eq!(identity_score(&weak, &window("0xa", "any", None)), None);

        // Executable outranks everything.
        let by_exe = WindowIdentity {
            executable: Some("/usr/bin/kitty".into()),
            ..Default::default()
        };
        assert_eq!(
            identity_score(&by_exe, &window("0xa", "x", Some("/usr/bin/kitty"))),
            Some(100)
        );
    }

    #[test]
    fn greedy_matching_each_window_claims_one_slot() {
        // Two identical terminal slots, one live terminal: one matched, one missing.
        let p = project(vec![slot("t1", "kitty", &[]), slot("t2", "kitty", &[])]);
        let w = window("0xa", "kitty", None);
        let r = reconcile(&p, &[&w]);
        assert_eq!(r.matched.len(), 1);
        assert_eq!(r.missing.len(), 1);
    }

    #[test]
    fn plan_adopts_and_launches_in_waves() {
        let mut slots = vec![
            slot("postgres", "pg", &[]),
            slot("editor", "code", &["postgres"]),
            slot("browser", "firefox", &["editor"]),
        ];
        slots[0].launch.as_mut().unwrap().service = true;
        let p = project(slots);
        // firefox already runs; pg and code are missing.
        let firefox = window("0xf", "firefox", None);
        let plan = plan(&p, &[&firefox], "").unwrap();

        assert_eq!(plan.workspace, "test");
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].label, "browser");
        assert!(plan.adopt[0].needs_move); // it sits on workspace "1"
        assert_eq!(plan.launch_count(), 2);
        assert_eq!(plan.waves.len(), 2);
        assert_eq!(plan.waves[0][0].label, "postgres");
        assert_eq!(plan.waves[1][0].label, "editor");

        // Round-trips through JSON for --dry-run.
        let json = serde_json::to_string(&plan).unwrap();
        let back: RestorePlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plan);
    }
}
