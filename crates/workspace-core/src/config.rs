//! Daemon configuration schema (`config.toml`) and validation.
//!
//! Parsing lives here (pure, no file IO) so the CLI's `doctor` can validate
//! configuration without a running daemon. The daemon and CLI read the file
//! and hand the string to [`Config::parse`].

use serde::{Deserialize, Serialize};

use crate::model::Slug;

/// Current config file schema version.
pub const CONFIG_VERSION: u32 = 1;

/// Errors from parsing or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The TOML failed to parse or deserialize.
    #[error("invalid config: {0}")]
    Toml(#[from] toml::de::Error),
    /// The file declares a schema version newer than this build understands.
    #[error(
        "config version {found} is newer than supported version {supported}; refusing to guess"
    )]
    VersionTooNew {
        /// Version found in the file.
        found: u32,
        /// Newest version this build supports.
        supported: u32,
    },
    /// A field value failed semantic validation.
    #[error("invalid config: {field}: {message}")]
    Invalid {
        /// Dotted path of the offending field.
        field: &'static str,
        /// What is wrong with it.
        message: String,
    },
}

/// What the daemon does when a rule matches a newly opened window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuleAction {
    /// Record the assignment only; leave the window where it is.
    Assign,
    /// Move the window to its project workspace without focusing it.
    #[default]
    Move,
    /// Move the window and focus its project workspace.
    MoveFocus,
}

/// Top-level daemon configuration (`~/.config/omarchy-workspaces/config.toml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Schema version; defaults to [`CONFIG_VERSION`] when absent.
    pub version: Option<u32>,
    /// General behavior.
    pub general: General,
    /// Autosave behavior.
    pub autosave: Autosave,
    /// Project-switching behavior.
    pub switch: Switch,
    /// Launcher defaults.
    pub launcher: Launcher,
    /// Logging.
    pub log: Log,
}

/// `[general]` section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Prefix prepended to all our Hyprland workspace names (e.g. `"ws:"`).
    pub workspace_prefix: String,
    /// Action taken when a rule matches a new window.
    pub rule_action: RuleAction,
    /// Projects restored automatically shortly after the daemon starts.
    pub restore_on_boot: Vec<Slug>,
}

/// `[autosave]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Autosave {
    /// Whether session state is saved automatically.
    pub enabled: bool,
    /// Quiet period after the last relevant event before saving.
    pub debounce_ms: u64,
    /// Upper bound between saves while events keep arriving.
    pub interval_s: u64,
}

impl Default for Autosave {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_ms: 2000,
            interval_s: 60,
        }
    }
}

/// `[switch]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Switch {
    /// Move a project's workspace to its preferred monitor before focusing.
    pub move_workspace_to_preferred_monitor: bool,
}

impl Default for Switch {
    fn default() -> Self {
        Self {
            move_workspace_to_preferred_monitor: true,
        }
    }
}

/// `[launcher]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Launcher {
    /// Default per-slot readiness timeout.
    pub default_timeout_ms: u64,
}

impl Default for Launcher {
    fn default() -> Self {
        Self {
            default_timeout_ms: 15_000,
        }
    }
}

/// `[log]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Log {
    /// Log level filter (overridden by `RUST_LOG`).
    pub level: String,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

impl Config {
    /// Parse and validate a config file's contents. An empty string yields defaults.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Semantic validation beyond what serde enforces.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let version = self.version.unwrap_or(CONFIG_VERSION);
        if version > CONFIG_VERSION {
            return Err(ConfigError::VersionTooNew {
                found: version,
                supported: CONFIG_VERSION,
            });
        }
        let prefix = &self.general.workspace_prefix;
        if prefix
            .chars()
            .any(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == ':'))
        {
            return Err(ConfigError::Invalid {
                field: "general.workspace_prefix",
                message: format!("{prefix:?} may only contain [a-z0-9-] and ':'"),
            });
        }
        if self.autosave.enabled && self.autosave.interval_s == 0 {
            return Err(ConfigError::Invalid {
                field: "autosave.interval_s",
                message: "must be greater than 0 when autosave is enabled".into(),
            });
        }
        if self.launcher.default_timeout_ms == 0 {
            return Err(ConfigError::Invalid {
                field: "launcher.default_timeout_ms",
                message: "must be greater than 0".into(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.general.workspace_prefix, "");
        assert_eq!(c.general.rule_action, RuleAction::Move);
        assert!(c.autosave.enabled);
        assert_eq!(c.autosave.debounce_ms, 2000);
        assert_eq!(c.launcher.default_timeout_ms, 15_000);
        assert!(c.switch.move_workspace_to_preferred_monitor);
    }

    #[test]
    fn full_config_parses() {
        let c = Config::parse(
            r#"
            version = 1
            [general]
            workspace_prefix = "ws:"
            rule_action = "move-focus"
            restore_on_boot = ["web-dev", "ml"]
            [autosave]
            enabled = false
            debounce_ms = 500
            interval_s = 30
            [switch]
            move_workspace_to_preferred_monitor = false
            [launcher]
            default_timeout_ms = 5000
            [log]
            level = "debug"
            "#,
        )
        .unwrap();
        assert_eq!(c.general.workspace_prefix, "ws:");
        assert_eq!(c.general.rule_action, RuleAction::MoveFocus);
        assert_eq!(c.general.restore_on_boot.len(), 2);
        assert_eq!(c.log.level, "debug");
    }

    #[test]
    fn newer_version_is_refused() {
        let err = Config::parse("version = 99").unwrap_err();
        assert!(matches!(
            err,
            ConfigError::VersionTooNew {
                found: 99,
                supported: CONFIG_VERSION
            }
        ));
    }

    #[test]
    fn invalid_values_are_named() {
        let err = Config::parse("[general]\nworkspace_prefix = \"WS \"").unwrap_err();
        assert!(err.to_string().contains("general.workspace_prefix"));

        let err = Config::parse("[launcher]\ndefault_timeout_ms = 0").unwrap_err();
        assert!(err.to_string().contains("launcher.default_timeout_ms"));

        let err = Config::parse("[general]\nrestore_on_boot = [\"Not A Slug\"]").unwrap_err();
        assert!(err.to_string().contains("invalid slug"));
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        // Forward compatibility: an older daemon must not choke on new keys.
        let c = Config::parse("[general]\nfuture_option = true").unwrap();
        assert_eq!(c.general.workspace_prefix, "");
    }
}
