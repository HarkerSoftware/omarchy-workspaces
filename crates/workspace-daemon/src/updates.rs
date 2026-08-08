//! Periodic update check: ask GitHub for the latest release and tell the
//! actor when it is newer than the running daemon.
//!
//! Checking only ever notifies — installing stays with pacman. The fetch
//! shells out to `curl` (always present on Arch) instead of pulling an HTTP
//! stack into the daemon; failures are silent at info level because an
//! offline machine is not an error.

use std::time::Duration;

use tokio::sync::mpsc;

use crate::actor::Command;

const RELEASES_URL: &str =
    "https://api.github.com/repos/HarkerSoftware/omarchy-workspaces/releases/latest";

/// Run forever: check shortly after boot, then on the configured interval.
pub async fn run(config: workspace_core::config::Updates, commands: mpsc::Sender<Command>) {
    if !config.check {
        return;
    }
    let interval = Duration::from_secs(config.interval_hours.max(1) * 3600);
    // Let the daemon settle (and the network come up) before the first call.
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        if let Some(latest) = latest_release_version().await
            && is_newer(env!("CARGO_PKG_VERSION"), &latest)
            && commands
                .send(Command::UpdateAvailable(latest))
                .await
                .is_err()
        {
            return;
        }
        tokio::time::sleep(interval).await;
    }
}

/// The latest published release version, without the `v` prefix.
async fn latest_release_version() -> Option<String> {
    let output = tokio::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "20",
            "-H",
            "User-Agent: omarchy-workspaces",
            RELEASES_URL,
        ])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        tracing::debug!("update check: curl failed (offline or rate-limited)");
        return None;
    }
    let release: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let tag = release.get("tag_name")?.as_str()?;
    Some(tag.trim_start_matches('v').to_owned())
}

/// Whether `latest` is a strictly newer dotted version than `current`.
/// Non-numeric segments compare as 0; missing segments too — tolerant on
/// purpose, a malformed tag must never announce an update.
pub fn is_newer(current: &str, latest: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split('.')
            .map(|part| {
                part.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    };
    let (current, latest) = (parse(current), parse(latest));
    for i in 0..current.len().max(latest.len()) {
        let c = current.get(i).copied().unwrap_or(0);
        let l = latest.get(i).copied().unwrap_or(0);
        if l != c {
            return l > c;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.2.0", "0.3.0"));
        assert!(is_newer("0.2.0", "0.2.1"));
        assert!(is_newer("0.9.9", "1.0.0"));
        assert!(is_newer("0.2.0", "0.2.0.1"));
        assert!(is_newer("0.2", "v0.2.1"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.3.0", "0.2.9"));
        assert!(!is_newer("1.0.0", "0.9.9"));
        // Malformed tags never announce an update.
        assert!(!is_newer("0.2.0", "garbage"));
        assert!(!is_newer("0.2.0", ""));
    }
}
