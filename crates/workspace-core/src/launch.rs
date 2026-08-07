//! Dependency ordering for app launches: Kahn's topological sort into
//! concurrent waves.
//!
//! Slots in the same wave have no ordering constraints between them and
//! launch concurrently; wave N+1 starts only when every slot it depends on is
//! ready. Cycles and references to unknown slot names are validation errors.

use std::collections::HashMap;

use uuid::Uuid;

use crate::model::AppSlot;

/// Errors from dependency planning.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaunchPlanError {
    /// An `after` entry names a slot that does not exist.
    #[error("slot {slot:?} depends on unknown slot {dependency:?}")]
    UnknownDependency {
        /// The dependent slot's label.
        slot: String,
        /// The missing dependency name.
        dependency: String,
    },
    /// Two slots share a name.
    #[error("duplicate slot name {name:?}")]
    DuplicateName {
        /// The duplicated name.
        name: String,
    },
    /// The dependency graph has a cycle.
    #[error("dependency cycle involving slots: {slots}")]
    Cycle {
        /// Comma-separated labels of the slots stuck in the cycle.
        slots: String,
    },
}

/// Order `slots` into launch waves. Slot order within a wave follows input
/// order (deterministic plans).
pub fn waves(slots: &[&AppSlot]) -> Result<Vec<Vec<Uuid>>, LaunchPlanError> {
    // Map names to ids and validate uniqueness.
    let mut by_name: HashMap<&str, Uuid> = HashMap::new();
    for slot in slots {
        if let Some(name) = &slot.name
            && by_name.insert(name.as_str(), slot.slot_id).is_some()
        {
            return Err(LaunchPlanError::DuplicateName { name: name.clone() });
        }
    }

    // Build edges dependency -> dependent, and in-degrees.
    let mut in_degree: HashMap<Uuid, usize> = slots.iter().map(|s| (s.slot_id, 0)).collect();
    let mut dependents: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for slot in slots {
        let Some(launch) = &slot.launch else { continue };
        for dependency in &launch.after {
            let Some(&dep_id) = by_name.get(dependency.as_str()) else {
                return Err(LaunchPlanError::UnknownDependency {
                    slot: slot.label(),
                    dependency: dependency.clone(),
                });
            };
            *in_degree.get_mut(&slot.slot_id).expect("slot present") += 1;
            dependents.entry(dep_id).or_default().push(slot.slot_id);
        }
    }

    // Kahn's algorithm, peeling one wave at a time.
    let mut waves: Vec<Vec<Uuid>> = Vec::new();
    let mut remaining: usize = slots.len();
    let mut ready: Vec<Uuid> = slots
        .iter()
        .filter(|s| in_degree[&s.slot_id] == 0)
        .map(|s| s.slot_id)
        .collect();
    while !ready.is_empty() {
        remaining -= ready.len();
        for id in &ready {
            for dependent in dependents.get(id).cloned().unwrap_or_default() {
                *in_degree.get_mut(&dependent).expect("slot present") -= 1;
            }
        }
        let next: Vec<Uuid> = slots
            .iter()
            .filter(|s| {
                in_degree[&s.slot_id] == 0
                    && !waves.iter().flatten().any(|id| *id == s.slot_id)
                    && !ready.contains(&s.slot_id)
            })
            .map(|s| s.slot_id)
            .collect();
        waves.push(std::mem::replace(&mut ready, next));
    }

    if remaining > 0 {
        let stuck: Vec<String> = slots
            .iter()
            .filter(|s| in_degree[&s.slot_id] > 0)
            .map(|s| s.label())
            .collect();
        return Err(LaunchPlanError::Cycle {
            slots: stuck.join(", "),
        });
    }
    Ok(waves)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LaunchSpec, WindowIdentity};

    fn slot(name: &str, after: &[&str]) -> AppSlot {
        AppSlot {
            slot_id: Uuid::new_v4(),
            name: Some(name.to_owned()),
            identity: WindowIdentity::default(),
            launch: Some(LaunchSpec {
                command: name.to_owned(),
                after: after.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            }),
            group: None,
            placement: Default::default(),
        }
    }

    fn labels(slots: &[AppSlot], waves: &[Vec<Uuid>]) -> Vec<Vec<String>> {
        waves
            .iter()
            .map(|wave| {
                wave.iter()
                    .map(|id| slots.iter().find(|s| s.slot_id == *id).unwrap().label())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn chains_and_parallel_waves() {
        // postgres -> docker -> cursor -> browser, with terminal independent.
        let slots = vec![
            slot("browser", &["cursor"]),
            slot("postgres", &[]),
            slot("docker", &["postgres"]),
            slot("cursor", &["docker"]),
            slot("terminal", &[]),
        ];
        let refs: Vec<&AppSlot> = slots.iter().collect();
        let waves = waves(&refs).unwrap();
        assert_eq!(
            labels(&slots, &waves),
            vec![
                vec!["postgres".to_string(), "terminal".to_string()],
                vec!["docker".to_string()],
                vec!["cursor".to_string()],
                vec!["browser".to_string()],
            ]
        );
    }

    #[test]
    fn cycle_is_an_error_naming_the_slots() {
        let slots = [slot("a", &["b"]), slot("b", &["a"]), slot("free", &[])];
        let refs: Vec<&AppSlot> = slots.iter().collect();
        let err = waves(&refs).unwrap_err();
        match err {
            LaunchPlanError::Cycle { slots } => {
                assert!(slots.contains('a') && slots.contains('b'));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_dependency_and_duplicate_name() {
        let slots = [slot("a", &["ghost"])];
        let refs: Vec<&AppSlot> = slots.iter().collect();
        assert_eq!(
            waves(&refs).unwrap_err(),
            LaunchPlanError::UnknownDependency {
                slot: "a".into(),
                dependency: "ghost".into()
            }
        );

        let slots = [slot("dup", &[]), slot("dup", &[])];
        let refs: Vec<&AppSlot> = slots.iter().collect();
        assert!(matches!(
            waves(&refs).unwrap_err(),
            LaunchPlanError::DuplicateName { .. }
        ));
    }
}
