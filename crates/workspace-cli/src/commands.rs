//! Implementations of the CLI subcommands that talk to the daemon.

use std::path::PathBuf;

use workspace_proto::{DaemonStatus, ProjectSummary, Request, Snapshot};

use crate::client::{DaemonClient, EXIT_DAEMON_DOWN};

fn connect_error(error: anyhow::Error) -> u8 {
    eprintln!("error: {error:#}");
    EXIT_DAEMON_DOWN
}

/// Connect + single request + shared error handling; returns the result
/// payload or an exit code.
async fn one_request(socket: Option<PathBuf>, request: Request) -> Result<serde_json::Value, u8> {
    let mut client = match DaemonClient::connect(socket).await {
        Ok(client) => client,
        Err(error) => return Err(connect_error(error)),
    };
    client.request(request).await.map_err(|error| {
        eprintln!("error: {error:#}");
        1
    })
}

/// `workspace create <name>`.
pub async fn create(socket: Option<PathBuf>, name: String, slug: Option<String>, json: bool) -> u8 {
    match one_request(socket, Request::ProjectCreate { name, slug }).await {
        Ok(result) => {
            if json {
                println!("{result}");
            } else if let Ok(summary) = serde_json::from_value::<ProjectSummary>(result) {
                println!(
                    "created project '{}' ({}) on workspace name:{}",
                    summary.name, summary.slug, summary.workspace
                );
            }
            0
        }
        Err(code) => code,
    }
}

/// `workspace delete <slug>`.
pub async fn delete(socket: Option<PathBuf>, slug: String, yes: bool) -> u8 {
    if !yes {
        eprint!("delete project '{slug}'? [y/N] ");
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err()
            || !matches!(answer.trim(), "y" | "Y" | "yes")
        {
            eprintln!("aborted");
            return 1;
        }
    }
    match one_request(socket, Request::ProjectDelete { slug }).await {
        Ok(result) => {
            println!("deleted {}", result["deleted"].as_str().unwrap_or("?"));
            0
        }
        Err(code) => code,
    }
}

/// `workspace rename <slug> <new-name>`.
pub async fn rename(socket: Option<PathBuf>, slug: String, name: String) -> u8 {
    match one_request(socket, Request::ProjectRename { slug, name }).await {
        Ok(result) => {
            if let Ok(summary) = serde_json::from_value::<ProjectSummary>(result) {
                println!("renamed '{}' to '{}'", summary.slug, summary.name);
            }
            0
        }
        Err(code) => code,
    }
}

/// `workspace switch <query>` (fuzzy).
pub async fn switch(socket: Option<PathBuf>, query: String) -> u8 {
    match one_request(socket, Request::ProjectSwitch { project: query }).await {
        Ok(result) => {
            if let Ok(summary) = serde_json::from_value::<ProjectSummary>(result) {
                println!("switched to '{}' ({})", summary.name, summary.slug);
            }
            0
        }
        Err(code) => code,
    }
}

/// `workspace list` — project table.
pub async fn list(socket: Option<PathBuf>, json: bool) -> u8 {
    match one_request(socket, Request::ProjectList).await {
        Ok(result) => {
            if json {
                println!("{result}");
                return 0;
            }
            let projects: Vec<ProjectSummary> = match serde_json::from_value(result) {
                Ok(projects) => projects,
                Err(error) => {
                    eprintln!("error: unexpected payload: {error}");
                    return 1;
                }
            };
            if projects.is_empty() {
                println!("no projects yet — create one with `workspace create <name>`");
                return 0;
            }
            let header = format!("{:<2} {:<20} {:<8} {}", "", "SLUG", "WINDOWS", "NAME");
            println!("{header}");
            for project in &projects {
                println!(
                    "{:<2} {:<20} {:<8} {}",
                    if project.active { "*" } else { "" },
                    project.slug,
                    project.windows,
                    project.name
                );
            }
            0
        }
        Err(code) => code,
    }
}

/// `workspace assign <address> <project> [--group g]`.
pub async fn assign(
    socket: Option<PathBuf>,
    address: String,
    project: String,
    group: Option<String>,
) -> u8 {
    match one_request(
        socket,
        Request::WindowAssign {
            address,
            project,
            group,
        },
    )
    .await
    {
        Ok(result) => {
            println!(
                "assigned {} to {}",
                result["assigned"].as_str().unwrap_or("?"),
                result["project"].as_str().unwrap_or("?")
            );
            0
        }
        Err(code) => code,
    }
}

/// `workspace save [project]` — snapshot assigned windows into the project file.
pub async fn save(socket: Option<PathBuf>, project: Option<String>) -> u8 {
    match one_request(socket, Request::ProjectSave { project }).await {
        Ok(result) => {
            println!(
                "saved '{}' ({} app slot{})",
                result["saved"].as_str().unwrap_or("?"),
                result["slots"].as_u64().unwrap_or(0),
                if result["slots"].as_u64() == Some(1) {
                    ""
                } else {
                    "s"
                }
            );
            0
        }
        Err(code) => code,
    }
}

/// `workspace restore [project] [--dry-run]` — rebuild a project's windows,
/// streaming progress until the run finishes.
pub async fn restore(
    socket: Option<PathBuf>,
    project: Option<String>,
    dry_run: bool,
    json: bool,
) -> u8 {
    let mut client = match DaemonClient::connect(socket).await {
        Ok(client) => client,
        Err(error) => return connect_error(error),
    };
    if !dry_run && let Err(error) = client.subscribe(&["restore"]).await {
        eprintln!("error: {error:#}");
        return 1;
    }
    let result = match client
        .request(Request::ProjectRestore { project, dry_run })
        .await
    {
        Ok(result) => result,
        Err(error) => {
            eprintln!("error: {error:#}");
            return 1;
        }
    };
    if dry_run {
        if json {
            println!("{result}");
        } else {
            print_plan(&result);
        }
        return 0;
    }

    let plan = &result["plan"];
    let total = plan["waves"]
        .as_array()
        .map(|waves| waves.iter().filter_map(|w| w.as_array()).flatten().count())
        .unwrap_or(0);
    println!(
        "restoring '{}': adopting {}, launching {}",
        plan["project"].as_str().unwrap_or("?"),
        plan["adopt"].as_array().map(Vec::len).unwrap_or(0),
        total
    );
    loop {
        let event = match client.next_event().await {
            Ok(event) => event,
            Err(error) => {
                eprintln!("error: {error:#}");
                return 1;
            }
        };
        match event.data {
            workspace_core::DomainEvent::RestoreProgress { slot, state, .. } => {
                println!("  {slot}: {state}");
            }
            workspace_core::DomainEvent::RestoreFinished {
                adopted,
                launched,
                failed,
                ..
            } => {
                if failed.is_empty() {
                    println!("done: {adopted} adopted, {launched} launched");
                    return 0;
                }
                println!(
                    "finished with failures: {adopted} adopted, {launched} launched, failed: {}",
                    failed.join(", ")
                );
                return 1;
            }
            _ => {}
        }
    }
}

fn print_plan(plan: &serde_json::Value) {
    println!("plan for '{}':", plan["project"].as_str().unwrap_or("?"));
    for adopt in plan["adopt"].as_array().into_iter().flatten() {
        println!(
            "  adopt   {} ({}){}",
            adopt["label"].as_str().unwrap_or("?"),
            adopt["address"].as_str().unwrap_or("?"),
            if adopt["needs_move"].as_bool() == Some(true) {
                " -> move to project workspace"
            } else {
                ""
            }
        );
    }
    for (i, wave) in plan["waves"].as_array().into_iter().flatten().enumerate() {
        for step in wave.as_array().into_iter().flatten() {
            println!(
                "  launch  [wave {}] {} ({})",
                i + 1,
                step["label"].as_str().unwrap_or("?"),
                step["spec"]["command"].as_str().unwrap_or("?"),
            );
        }
    }
    for extra in plan["extra"].as_array().into_iter().flatten() {
        println!(
            "  keep    {} (no matching slot)",
            extra.as_str().unwrap_or("?")
        );
    }
}

/// `workspace rules test [address]` — dry-run the rules engine.
pub async fn rules_test(socket: Option<PathBuf>, address: Option<String>, json: bool) -> u8 {
    match one_request(socket, Request::RulesTest { address }).await {
        Ok(result) => {
            if json {
                println!("{result}");
                return 0;
            }
            println!(
                "window {} (class {:?}, title {:?})",
                result["address"].as_str().unwrap_or("?"),
                result["class"].as_str().unwrap_or(""),
                result["title"].as_str().unwrap_or("")
            );
            match result["matches"].as_array() {
                Some(matches) if !matches.is_empty() => {
                    for m in matches {
                        println!(
                            "  matches rule {:?} -> project {}{}",
                            m["rule"].as_str().unwrap_or("?"),
                            m["project"].as_str().unwrap_or("?"),
                            m["group"]
                                .as_str()
                                .map(|g| format!(" (group {g})"))
                                .unwrap_or_default()
                        );
                    }
                }
                _ => println!("  no rules match"),
            }
            0
        }
        Err(code) => code,
    }
}

/// `workspace daemon reload` — re-read config and rules.
pub async fn daemon_reload(socket: Option<PathBuf>) -> u8 {
    match one_request(socket, Request::ConfigReload).await {
        Ok(result) => {
            println!(
                "reloaded ({} rules active)",
                result["rules"].as_u64().unwrap_or(0)
            );
            0
        }
        Err(code) => code,
    }
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
