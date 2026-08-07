//! `workspace panel enable|disable|status`: wire the panel into the Omarchy
//! desktop and remove it again cleanly.
//!
//! Everything written is confined to marked blocks / dedicated files so
//! `disable` can reverse it surgically:
//! - autostart entry inside `# >>> … >>> / # <<< … <<<` markers in
//!   `~/.config/hypr/autostart.conf`;
//! - layer rules in `~/.local/state/omarchy/toggles/hypr/workspace-panel.conf`
//!   (a directory Omarchy glob-sources into hyprland.conf).
//!
//! Nothing under `~/.local/share/omarchy` (the Omarchy checkout) is touched.

use std::path::PathBuf;

use workspace_hypr::{Dispatch, HyprCtl, HyprPaths};

const BLOCK_BEGIN: &str = "# >>> omarchy-workspaces panel >>>";
const BLOCK_END: &str = "# <<< omarchy-workspaces panel <<<";

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn autostart_path() -> Option<PathBuf> {
    Some(home()?.join(".config/hypr/autostart.conf"))
}

fn toggles_path() -> Option<PathBuf> {
    Some(home()?.join(".local/state/omarchy/toggles/hypr/workspace-panel.conf"))
}

/// The panel binary: sibling of the running `workspace` binary when present
/// (dev builds), else the bare name resolved via PATH.
fn panel_command() -> String {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("workspace-panel");
        if sibling.exists() {
            return sibling.to_string_lossy().into_owned();
        }
    }
    "workspace-panel".to_owned()
}

/// Replace or append our marked block; returns whether the file changed.
fn upsert_block(path: &PathBuf, body: &str) -> std::io::Result<bool> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let block = format!("{BLOCK_BEGIN}\n{body}\n{BLOCK_END}\n");
    let updated = match (existing.find(BLOCK_BEGIN), existing.find(BLOCK_END)) {
        (Some(start), Some(end)) => {
            let end = end + BLOCK_END.len();
            let mut next = existing[..start].to_owned();
            next.push_str(&block);
            // Skip the trailing newline of the old block if present.
            let rest = existing[end..]
                .strip_prefix('\n')
                .unwrap_or(&existing[end..]);
            next.push_str(rest);
            next
        }
        _ => {
            let mut next = existing.clone();
            if !next.is_empty() && !next.ends_with('\n') {
                next.push('\n');
            }
            next.push_str(&block);
            next
        }
    };
    if updated == existing {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, updated)?;
    Ok(true)
}

/// Remove our marked block; returns whether the file changed.
fn remove_block(path: &PathBuf) -> std::io::Result<bool> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let (Some(start), Some(end)) = (existing.find(BLOCK_BEGIN), existing.find(BLOCK_END)) else {
        return Ok(false);
    };
    let end = end + BLOCK_END.len();
    let mut updated = existing[..start].to_owned();
    let rest = existing[end..]
        .strip_prefix('\n')
        .unwrap_or(&existing[end..]);
    updated.push_str(rest);
    std::fs::write(path, updated)?;
    Ok(true)
}

const LAYER_RULES: &str = "\
# Managed by `workspace panel enable`; removed by `workspace panel disable`.
layerrule = blur on, match:namespace workspace-panel
layerrule = ignore_alpha 0.2, match:namespace workspace-panel";

/// `workspace panel enable`.
pub async fn enable() -> u8 {
    let (Some(autostart), Some(toggles)) = (autostart_path(), toggles_path()) else {
        eprintln!("error: HOME is not set");
        return 1;
    };
    let command = panel_command();

    match upsert_block(&autostart, &format!("exec-once = uwsm-app -- {command}")) {
        Ok(true) => println!("autostart entry added to {}", autostart.display()),
        Ok(false) => println!("autostart entry already present"),
        Err(error) => {
            eprintln!("error: cannot update {}: {error}", autostart.display());
            return 1;
        }
    }

    if let Some(parent) = toggles.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("error: cannot create {}: {error}", parent.display());
        return 1;
    }
    if let Err(error) = std::fs::write(&toggles, format!("{LAYER_RULES}\n")) {
        eprintln!("error: cannot write {}: {error}", toggles.display());
        return 1;
    }
    println!("layer rules written to {}", toggles.display());

    // Apply immediately: reload config for the layer rules, start the panel.
    match HyprPaths::from_env() {
        Ok(paths) => {
            let ctl = HyprCtl::new(paths);
            if let Err(error) = ctl.raw_request("reload").await {
                eprintln!("warning: hyprctl reload failed: {error}");
            }
            if !panel_running() {
                if let Err(error) = ctl
                    .dispatch(&Dispatch::Exec {
                        rules: vec![],
                        command: format!("uwsm-app -- {command}"),
                    })
                    .await
                {
                    eprintln!("warning: could not start the panel: {error}");
                } else {
                    println!("panel started");
                }
            } else {
                println!("panel already running");
            }
        }
        Err(error) => eprintln!("warning: {error} — panel will start on next login"),
    }
    0
}

/// `workspace panel disable`.
pub async fn disable() -> u8 {
    let (Some(autostart), Some(toggles)) = (autostart_path(), toggles_path()) else {
        eprintln!("error: HOME is not set");
        return 1;
    };
    match remove_block(&autostart) {
        Ok(true) => println!("autostart entry removed"),
        Ok(false) => println!("no autostart entry found"),
        Err(error) => eprintln!("warning: cannot update {}: {error}", autostart.display()),
    }
    match std::fs::remove_file(&toggles) {
        Ok(()) => println!("layer rules removed"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("warning: cannot remove {}: {error}", toggles.display()),
    }
    let _ = std::process::Command::new("pkill")
        .args(["-x", "workspace-panel"])
        .status();
    println!("panel stopped");
    if let Ok(paths) = HyprPaths::from_env() {
        let _ = HyprCtl::new(paths).raw_request("reload").await;
    }
    0
}

fn panel_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", "workspace-panel"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// `workspace panel status`.
pub fn status() -> u8 {
    let autostart_ok = autostart_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|text| text.contains(BLOCK_BEGIN));
    let toggles_ok = toggles_path().is_some_and(|p| p.exists());
    println!(
        "autostart entry   {}",
        if autostart_ok { "present" } else { "absent" }
    );
    println!(
        "layer rules       {}",
        if toggles_ok { "present" } else { "absent" }
    );
    println!(
        "panel process     {}",
        if panel_running() {
            "running"
        } else {
            "not running"
        }
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_upsert_and_remove_are_surgical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("autostart.conf");
        std::fs::write(&path, "# user stuff\nexec-once = something\n").unwrap();

        assert!(upsert_block(&path, "exec-once = panel").unwrap());
        // Idempotent.
        assert!(!upsert_block(&path, "exec-once = panel").unwrap());
        // Updating replaces in place.
        assert!(upsert_block(&path, "exec-once = panel-v2").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("exec-once = panel-v2"));
        assert!(!text.contains("exec-once = panel\n"));
        assert_eq!(text.matches(BLOCK_BEGIN).count(), 1);

        assert!(remove_block(&path).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "# user stuff\nexec-once = something\n");
        assert!(!remove_block(&path).unwrap());
    }
}
