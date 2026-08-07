//! One TOML file per project under `<state>/projects/<slug>.toml`.
//!
//! Files carry a schema version. Files written by a *newer* build are refused
//! (never rewritten or destroyed); older versions run through the migration
//! registry in [`crate::migrations`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use workspace_core::model::{Project, Slug};

use crate::StorageError;
use crate::atomic::atomic_write;

/// Current project-file schema version.
pub const PROJECT_VERSION: u32 = 1;

/// On-disk shape of a project file.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectFile {
    /// Schema version.
    pub version: u32,
    /// The project definition.
    pub project: Project,
}

/// Directory holding the per-project files.
pub fn projects_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("projects")
}

fn project_path(state_dir: &Path, slug: &Slug) -> PathBuf {
    projects_dir(state_dir).join(format!("{slug}.toml"))
}

/// Write one project file atomically.
pub fn save_project(state_dir: &Path, project: &Project) -> Result<(), StorageError> {
    let dir = projects_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let file = ProjectFile {
        version: PROJECT_VERSION,
        project: project.clone(),
    };
    let text = toml::to_string_pretty(&file).map_err(StorageError::TomlSer)?;
    atomic_write(&project_path(state_dir, &project.slug), text.as_bytes())?;
    Ok(())
}

/// Remove a project's file; missing files are fine.
pub fn delete_project(state_dir: &Path, slug: &Slug) -> Result<(), StorageError> {
    match std::fs::remove_file(project_path(state_dir, slug)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Load every project file. Unreadable or invalid files are reported and
/// skipped — one bad file never takes down the rest, and files from newer
/// builds are left untouched on disk.
pub fn load_projects(state_dir: &Path) -> (Vec<Project>, Vec<StorageError>) {
    let dir = projects_dir(state_dir);
    let mut projects = Vec::new();
    let mut errors = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (projects, errors);
        }
        Err(error) => {
            errors.push(error.into());
            return (projects, errors);
        }
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match load_project_file(&path) {
            Ok(project) => projects.push(project),
            Err(error) => errors.push(error),
        }
    }
    projects.sort_by(|a, b| a.slug.cmp(&b.slug));
    (projects, errors)
}

fn load_project_file(path: &Path) -> Result<Project, StorageError> {
    let text = std::fs::read_to_string(path)?;
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| StorageError::Invalid {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
    let version = table
        .get("version")
        .and_then(|v| v.as_integer())
        .unwrap_or(PROJECT_VERSION as i64) as u32;
    if version > PROJECT_VERSION {
        return Err(StorageError::VersionTooNew {
            path: path.to_owned(),
            found: version,
            supported: PROJECT_VERSION,
        });
    }
    let table = crate::migrations::migrate_project(table, version)?;
    let file: ProjectFile = toml::Table::try_into(table).map_err(|e| StorageError::Invalid {
        path: path.to_owned(),
        message: e.to_string(),
    })?;
    Ok(file.project)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::new("Web Development").unwrap();
        save_project(dir.path(), &project).unwrap();

        let (loaded, errors) = load_projects(dir.path());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].slug, project.slug);
        assert_eq!(loaded[0].id, project.id);

        delete_project(dir.path(), &project.slug).unwrap();
        delete_project(dir.path(), &project.slug).unwrap(); // idempotent
        assert!(load_projects(dir.path()).0.is_empty());
    }

    #[test]
    fn newer_version_is_refused_but_others_load() {
        let dir = tempfile::tempdir().unwrap();
        let good = Project::new("Good").unwrap();
        save_project(dir.path(), &good).unwrap();
        std::fs::write(
            projects_dir(dir.path()).join("future.toml"),
            "version = 99\n[project]\nslug = \"future\"\n",
        )
        .unwrap();
        std::fs::write(projects_dir(dir.path()).join("broken.toml"), "not toml [").unwrap();

        let (loaded, errors) = load_projects(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(errors.len(), 2);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, StorageError::VersionTooNew { found: 99, .. }))
        );
        // The refused file is untouched on disk.
        assert!(projects_dir(dir.path()).join("future.toml").exists());
    }

    #[test]
    fn missing_dir_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let (loaded, errors) = load_projects(&dir.path().join("nope"));
        assert!(loaded.is_empty() && errors.is_empty());
    }
}
