//! `workspace doctor`: environment and configuration health checks.
//!
//! Works entirely without the daemon so it can diagnose why the daemon itself
//! will not start. Checks are grouped; each prints one line and the command
//! exits non-zero if any check fails outright.

use std::fmt;

use workspace_core::config::Config;
use workspace_hypr::{HyprCtl, HyprPaths};

/// Outcome counters for the final summary and exit code.
#[derive(Debug, Default)]
pub struct Report {
    failures: u32,
    warnings: u32,
}

enum Status {
    Ok,
    Warn,
    Fail,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::Ok => "  ok  ",
            Status::Warn => " warn ",
            Status::Fail => " FAIL ",
        })
    }
}

impl Report {
    fn ok(&mut self, message: impl fmt::Display) {
        println!("[{}] {message}", Status::Ok);
    }

    fn warn(&mut self, message: impl fmt::Display) {
        self.warnings += 1;
        println!("[{}] {message}", Status::Warn);
    }

    fn fail(&mut self, message: impl fmt::Display) {
        self.failures += 1;
        println!("[{}] {message}", Status::Fail);
    }

    /// True when no check failed (warnings allowed).
    pub fn healthy(&self) -> bool {
        self.failures == 0
    }
}

/// Run all checks and print the report. Returns the process exit code.
pub async fn run() -> u8 {
    let mut report = Report::default();

    check_hyprland(&mut report).await;
    check_config(&mut report);
    check_daemon(&mut report);

    println!();
    if report.healthy() {
        println!(
            "doctor: healthy ({} warning{})",
            report.warnings,
            if report.warnings == 1 { "" } else { "s" }
        );
        0
    } else {
        println!(
            "doctor: {} check(s) failed, {} warning(s)",
            report.failures, report.warnings
        );
        1
    }
}

async fn check_hyprland(report: &mut Report) {
    let paths = match HyprPaths::from_env() {
        Ok(paths) => paths,
        Err(error) => {
            report.fail(format!("hyprland: {error}"));
            return;
        }
    };
    for (label, path) in [
        ("control socket", &paths.ctl),
        ("event socket", &paths.events),
    ] {
        if path.exists() {
            report.ok(format!("hyprland {label}: {}", path.display()));
        } else {
            report.fail(format!("hyprland {label} missing: {}", path.display()));
        }
    }

    let ctl = HyprCtl::new(paths);
    match ctl.version().await {
        Ok(version) => {
            let tag = version
                .get("tag")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            report.ok(format!("hyprland responds to IPC (version {tag})"));
        }
        Err(error) => {
            report.fail(format!("hyprland IPC request failed: {error}"));
            return;
        }
    }

    // Schema probe: make sure the clients JSON still decodes and carries the
    // fields we depend on for window identity.
    match ctl.clients().await {
        Ok(clients) => {
            report.ok(format!(
                "clients schema decodes ({} windows)",
                clients.len()
            ));
            if !clients.is_empty() && clients.iter().all(|c| c.stable_id.is_none()) {
                report
                    .warn("no window reports stableId — older Hyprland? falling back to addresses");
            }
        }
        Err(error) => report.fail(format!("clients schema probe failed: {error}")),
    }
}

fn check_config(report: &mut Report) {
    let Some(path) = workspace_storage::paths::config_file() else {
        report.fail("config: cannot determine config directory (HOME unset)");
        return;
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match Config::parse(&text) {
            Ok(_) => report.ok(format!("config valid: {}", path.display())),
            Err(error) => report.fail(format!("config invalid ({}): {error}", path.display())),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.ok(format!(
                "config not present, using defaults ({})",
                path.display()
            ));
        }
        Err(error) => report.fail(format!("config unreadable ({}): {error}", path.display())),
    }
}

fn check_daemon(report: &mut Report) {
    let Some(socket) = workspace_storage::paths::daemon_socket() else {
        report.fail("daemon: XDG_RUNTIME_DIR is not set");
        return;
    };
    if socket.exists() {
        report.ok(format!("daemon socket present: {}", socket.display()));
    } else {
        report.warn(format!(
            "daemon not running (no socket at {})",
            socket.display()
        ));
    }
}
