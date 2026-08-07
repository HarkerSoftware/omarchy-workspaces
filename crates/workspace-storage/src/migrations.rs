//! Stepwise schema migrations for on-disk files.
//!
//! Each migration takes the raw TOML table at version N and returns the table
//! at version N+1. With only schema v1 in existence the registry is empty;
//! the machinery exists so future versions migrate user data instead of
//! breaking it.

use crate::StorageError;

type Migration = fn(toml::Table) -> Result<toml::Table, String>;

/// Project-file migrations: index 0 migrates v1→v2, and so on.
const PROJECT_MIGRATIONS: &[Migration] = &[];

/// Migrate a project table from `version` up to the current schema.
pub fn migrate_project(mut table: toml::Table, version: u32) -> Result<toml::Table, StorageError> {
    let current = crate::projects::PROJECT_VERSION;
    for step in version..current {
        let index = (step - 1) as usize;
        let migration = PROJECT_MIGRATIONS
            .get(index)
            .ok_or_else(|| StorageError::Invalid {
                path: std::path::PathBuf::new(),
                message: format!("no migration registered for project schema v{step}"),
            })?;
        table = migration(table).map_err(|message| StorageError::Invalid {
            path: std::path::PathBuf::new(),
            message: format!("migration from v{step} failed: {message}"),
        })?;
        table.insert("version".into(), toml::Value::Integer((step + 1) as i64));
    }
    Ok(table)
}
