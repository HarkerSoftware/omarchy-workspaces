//! Implementations of the CLI subcommands that talk to the daemon.

use std::path::PathBuf;

use workspace_proto::{DaemonStatus, Request, Snapshot};

use crate::client::{DaemonClient, EXIT_DAEMON_DOWN};

fn connect_error(error: anyhow::Error) -> u8 {
    eprintln!("error: {error:#}");
    EXIT_DAEMON_DOWN
}

/// `workspace status` — daemon summary.
pub async fn status(socket: Option<PathBuf>, json: bool) -> u8 {
    let mut client = match DaemonClient::connect(socket).await {
        Ok(client) => client,
        Err(error) => return connect_error(error),
    };
    let result = match client.request(Request::DaemonStatus).await {
        Ok(result) => result,
        Err(error) => {
            eprintln!("error: {error:#}");
            return 1;
        }
    };
    if json {
        println!("{result}");
        return 0;
    }
    match serde_json::from_value::<DaemonStatus>(result) {
        Ok(status) => {
            println!("daemon      v{} (up {}s)", status.version, status.uptime_s);
            println!(
                "hyprland    {}",
                if status.hypr_connected {
                    "connected"
                } else {
                    "DISCONNECTED"
                }
            );
            match &status.active_project {
                Some(slug) => println!("project     {slug}"),
                None => println!("project     (none active)"),
            }
            println!("windows     {}", status.windows);
            println!("projects    {}", status.projects);
            0
        }
        Err(error) => {
            eprintln!("error: unexpected status payload: {error}");
            1
        }
    }
}

/// `workspace windows` — tracked-window table.
pub async fn windows(socket: Option<PathBuf>, json: bool) -> u8 {
    let mut client = match DaemonClient::connect(socket).await {
        Ok(client) => client,
        Err(error) => return connect_error(error),
    };
    let result = match client.request(Request::StateSnapshot).await {
        Ok(result) => result,
        Err(error) => {
            eprintln!("error: {error:#}");
            return 1;
        }
    };
    if json {
        println!("{result}");
        return 0;
    }
    let snapshot: Snapshot = match serde_json::from_value(result) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("error: unexpected snapshot payload: {error}");
            return 1;
        }
    };
    if snapshot.windows.is_empty() {
        println!("no windows tracked");
        return 0;
    }
    let header = format!(
        "{:<16} {:<20} {:<14} {:<6} {}",
        "ADDRESS", "CLASS", "WORKSPACE", "FOCUS", "TITLE"
    );
    println!("{header}");
    for window in &snapshot.windows {
        let focused = snapshot.focused_window.as_deref() == Some(window.address.as_str());
        let title: String = window.facts.title.chars().take(60).collect();
        println!(
            "{:<16} {:<20} {:<14} {:<6} {}",
            window.address,
            truncate(&window.facts.class, 20),
            truncate(&window.facts.workspace, 14),
            if focused { "*" } else { "" },
            title
        );
    }
    0
}

/// `workspace logs` — print the daemon log file's tail.
pub fn logs(lines: usize) -> u8 {
    let Some(path) = workspace_storage::paths::state_dir().map(|d| d.join("logs/daemon.log"))
    else {
        eprintln!("error: cannot determine state directory (HOME unset)");
        return 1;
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let all: Vec<&str> = text.lines().collect();
            let start = all.len().saturating_sub(lines);
            for line in &all[start..] {
                println!("{line}");
            }
            0
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("no log file yet at {}", path.display());
            1
        }
        Err(error) => {
            eprintln!("error: cannot read {}: {error}", path.display());
            1
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max.saturating_sub(1)).chain(['…']).collect()
    }
}
