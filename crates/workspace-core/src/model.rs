//! Domain entities: projects, groups, app slots, window identity, and launch
//! specifications.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Errors produced when validating model values.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelError {
    /// The slug was empty or contained characters outside `[a-z0-9-]`.
    #[error(
        "invalid slug {0:?}: must be non-empty, lowercase [a-z0-9-], and not start or end with '-'"
    )]
    InvalidSlug(String),
}

/// Stable identifier for a project. Survives renames; slugs may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(Uuid);

impl ProjectId {
    /// Generate a fresh random id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ProjectId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A URL-safe short name: lowercase ASCII letters, digits, and hyphens.
///
/// Slugs name projects and groups on the wire, in file names, and in
/// Hyprland named workspaces (`name:<slug>`), so validation is strict.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Slug(String);

impl Slug {
    /// Validate `s` as a slug without transforming it.
    pub fn parse(s: &str) -> Result<Self, ModelError> {
        let valid = !s.is_empty()
            && !s.starts_with('-')
            && !s.ends_with('-')
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(ModelError::InvalidSlug(s.to_owned()))
        }
    }

    /// Derive a slug from a free-form display name ("Web Development" → `web-development`).
    ///
    /// Returns an error only when the name contains no usable characters at all.
    pub fn from_display_name(name: &str) -> Result<Self, ModelError> {
        let mut out = String::with_capacity(name.len());
        let mut last_was_hyphen = true; // suppress leading hyphens
        for c in name.chars() {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                out.push(c);
                last_was_hyphen = false;
            } else if !last_was_hyphen {
                out.push('-');
                last_was_hyphen = true;
            }
        }
        while out.ends_with('-') {
            out.pop();
        }
        Self::parse(&out).map_err(|_| ModelError::InvalidSlug(name.to_owned()))
    }

    /// The slug as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `pad` honors width/alignment flags (needed for table output).
        f.pad(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Slug {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Slug::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A named project: a declarative set of windows (via [`AppSlot`]s) organized
/// into [`Group`]s, mapped onto Hyprland named workspaces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    /// Stable identity; survives renames.
    pub id: ProjectId,
    /// Short machine name; used in workspace names, file names, CLI.
    pub slug: Slug,
    /// Human-readable display name ("Web Development").
    pub name: String,
    /// Window groups. The implicit primary group is not stored here.
    #[serde(default)]
    pub groups: Vec<Group>,
    /// Desired windows and how to recreate them.
    #[serde(default)]
    pub apps: Vec<AppSlot>,
    /// Preferred monitor for the project's primary workspace, by output name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
    /// Manual sort position in project listings (lower sorts first; ties
    /// break by slug).
    #[serde(default)]
    pub position: u32,
}

impl Project {
    /// Create an empty project from a display name, deriving the slug.
    pub fn new(name: &str) -> Result<Self, ModelError> {
        Ok(Self {
            id: ProjectId::new(),
            slug: Slug::from_display_name(name)?,
            name: name.to_owned(),
            groups: Vec::new(),
            apps: Vec::new(),
            monitor: None,
            position: 0,
        })
    }
}

/// A named group of windows inside a project (e.g. "backend", "frontend").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    /// Group slug, unique within its project.
    pub slug: Slug,
    /// Display name.
    pub name: String,
    /// Whether the group's windows are currently parked off the primary workspace.
    #[serde(default)]
    pub hidden: bool,
}

/// One desired window in a project: how to recognize it, how to place it,
/// and (optionally) how to launch it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSlot {
    /// Stable slot identity, referenced by restore progress.
    pub slot_id: Uuid,
    /// Optional human name, unique within the project; launch dependencies
    /// (`after = [...]`) refer to slots by this name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// How to match this slot to a live window.
    pub identity: WindowIdentity,
    /// How to (re)create the window when it is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchSpec>,
    /// Group this slot belongs to; `None` means the project's primary group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<Slug>,
    /// Desired placement (floating geometry, fullscreen, monitor).
    #[serde(default)]
    pub placement: Placement,
}

impl AppSlot {
    /// Display label: the explicit name, else the identity's class, else the
    /// launch command, else the slot id.
    pub fn label(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.identity.class.clone())
            .or_else(|| self.launch.as_ref().map(|l| l.command.clone()))
            .unwrap_or_else(|| self.slot_id.to_string())
    }
}

/// How to detect that a launched slot is ready.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Readiness {
    /// Ready when a window matching the slot's identity appears (default).
    #[default]
    Window,
    /// Ready immediately after spawn (plus `startup_delay_ms`).
    Delay,
    /// Ready when a probe command exits 0, polled at `interval_ms`.
    Command {
        /// The probe command (run via the shell).
        cmd: String,
        /// Poll interval in milliseconds.
        #[serde(default = "default_probe_interval")]
        interval_ms: u64,
    },
}

fn default_probe_interval() -> u64 {
    500
}

/// How to launch a slot's application.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LaunchSpec {
    /// Executable or command line.
    pub command: String,
    /// Arguments appended to the command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment variables.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Working directory (`~` expanded at launch time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// Slot names that must be ready before this one launches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// Extra wait after readiness before dependents may start.
    #[serde(default)]
    pub startup_delay_ms: u64,
    /// Readiness timeout; the daemon's default applies when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Readiness detection.
    #[serde(default)]
    pub readiness: Readiness,
    /// A windowless dependency (database, docker): spawned directly, never
    /// matched to a window, started but not supervised.
    #[serde(default)]
    pub service: bool,
}

/// Criteria for matching a persisted slot to a live window across reboots.
///
/// Matching is scored (see the reconcile logic): executable path is the
/// strongest signal, then class, then initial class, with title patterns as a
/// tie-breaker only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowIdentity {
    /// Exact window class (e.g. `firefox`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// Exact initial class, as reported when the window first mapped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_class: Option<String>,
    /// Absolute path of the process executable, resolved from `/proc/<pid>/exe`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    /// Regex matched against the window title; tie-breaker only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_pattern: Option<String>,
}

/// Desired placement for a window when restored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    /// Whether the window should float.
    #[serde(default)]
    pub floating: bool,
    /// Captured position of the top-left corner. Floating windows are moved
    /// here exactly; tiled windows are swapped toward it in the layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<(i32, i32)>,
    /// Captured size in pixels. Applied exactly to floating windows; for
    /// tiled windows it only scales the layout-matching tolerance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<(i32, i32)>,
    /// Fullscreen mode: 0 = none, 1 = maximize, 2 = fullscreen (Hyprland semantics).
    #[serde(default)]
    pub fullscreen: u8,
    /// Preferred monitor by output name; overrides the project preference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_accepts_valid() {
        for s in ["web-dev", "ai", "a", "x1", "machine-learning-2"] {
            assert_eq!(Slug::parse(s).unwrap().as_str(), s);
        }
    }

    #[test]
    fn slug_rejects_invalid() {
        for s in [
            "", "Web", "web dev", "-web", "web-", "wéb", "web_dev", "a.b", "a:b",
        ] {
            assert!(Slug::parse(s).is_err(), "expected rejection: {s:?}");
        }
    }

    #[test]
    fn slug_from_display_name() {
        let cases = [
            ("Web Development", "web-development"),
            ("  AI / ML!!", "ai-ml"),
            ("gaming", "gaming"),
            ("Rust 101", "rust-101"),
            ("--weird--", "weird"),
        ];
        for (input, want) in cases {
            assert_eq!(Slug::from_display_name(input).unwrap().as_str(), want);
        }
        assert!(Slug::from_display_name("!!!").is_err());
        assert!(Slug::from_display_name("").is_err());
    }

    #[test]
    fn slug_serde_round_trip_and_rejects_invalid_input() {
        let s: Slug = serde_json::from_str("\"web-dev\"").unwrap();
        assert_eq!(s.as_str(), "web-dev");
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"web-dev\"");
        assert!(serde_json::from_str::<Slug>("\"Not A Slug\"").is_err());
    }

    #[test]
    fn project_new_derives_slug() {
        let p = Project::new("Web Development").unwrap();
        assert_eq!(p.slug.as_str(), "web-development");
        assert_eq!(p.name, "Web Development");
        assert!(p.groups.is_empty() && p.apps.is_empty());
    }

    #[test]
    fn project_toml_round_trip() {
        let mut p = Project::new("ML").unwrap();
        p.groups.push(Group {
            slug: Slug::parse("backend").unwrap(),
            name: "Backend".into(),
            hidden: true,
        });
        p.apps.push(AppSlot {
            slot_id: Uuid::new_v4(),
            name: Some("browser".into()),
            identity: WindowIdentity {
                class: Some("firefox".into()),
                ..Default::default()
            },
            launch: Some(LaunchSpec {
                command: "firefox".into(),
                ..Default::default()
            }),
            group: Some(Slug::parse("backend").unwrap()),
            placement: Placement {
                floating: true,
                position: Some((100, 200)),
                size: Some((800, 600)),
                ..Default::default()
            },
        });
        let text = toml::to_string_pretty(&p).unwrap();
        let back: Project = toml::from_str(&text).unwrap();
        assert_eq!(back.id, p.id);
        assert!(back.groups[0].hidden);
        assert_eq!(back.apps[0].placement.size, Some((800, 600)));
    }
}
