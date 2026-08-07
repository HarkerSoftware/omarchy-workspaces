//! Best-effort launch capture for `project.save`.
//!
//! A window's pid gives us its command line and working directory, but
//! multi-window applications share one process for every window, so the
//! per-window facts live elsewhere: for a terminal, in its child shell's
//! cwd; for VS Code, in the window title plus VS Code's own session state;
//! for chromium, in the profile list in `Local State`. Everything here is
//! best-effort — on any miss the capture degrades to cmdline + process cwd.

use workspace_core::model::LaunchSpec;
use workspace_core::world::WindowFacts;

/// Build a launch spec for a window captured by `project.save`: the full
/// command line from `/proc/<pid>/cmdline` (falling back to the resolved
/// executable path), a working directory, and app-specific enrichment.
/// `None` only when no command can be determined at all — such a slot can
/// be adopted but never relaunched.
pub fn captured_launch(facts: &WindowFacts, project_name: &str) -> Option<LaunchSpec> {
    let (command, mut args) = match read_cmdline(facts.pid) {
        Some(mut argv) => (argv.remove(0), argv),
        None => (
            facts.executable.as_ref()?.to_string_lossy().into_owned(),
            Vec::new(),
        ),
    };
    // `LaunchSpec.command` reaches the shell unquoted (it may be a full
    // command line when hand-written); a captured argv[0] must stay one word.
    let command = if command.chars().any(char::is_whitespace) {
        format!("'{}'", command.replace('\'', r"'\''"))
    } else {
        command
    };
    let mut workdir = process_workdir(facts.pid);

    let class = facts.class.to_ascii_lowercase();
    if class.contains("chromium") || class.contains("chrome") {
        let config = if class.contains("chromium") {
            "chromium"
        } else {
            "google-chrome"
        };
        let profile = args
            .iter()
            .find_map(|a| a.strip_prefix("--profile-directory="))
            .map(str::to_owned)
            .or_else(|| chromium_profile_from_disk(config, project_name));
        if let Some(profile) = profile {
            if !args.iter().any(|a| a.starts_with("--profile-directory")) {
                args.push(format!("--profile-directory={profile}"));
            }
            // The window's open tabs, from the profile's session journal;
            // passed as URLs so restore reopens them together. `--new-window`
            // keeps them out of any existing window of the same profile —
            // without it a running browser absorbs the URLs as extra tabs.
            if let Some(home) = std::env::var_os("HOME") {
                let profile_dir = std::path::Path::new(&home)
                    .join(".config")
                    .join(config)
                    .join(&profile);
                if let Some(tabs) = crate::snss::window_tabs(&profile_dir, &facts.title) {
                    args.push("--new-window".to_owned());
                    args.extend(tabs);
                }
            }
        }
    } else if (class == "code" || class.starts_with("code-") || class.contains("vscodium"))
        && let Some(folder) = vscode_folder(&facts.title)
    {
        workdir = Some(folder.clone());
        args.push(folder);
    }

    Some(LaunchSpec {
        command,
        args,
        workdir,
        ..Default::default()
    })
}

/// A title pattern distinguishing this window from same-class siblings, for
/// slot identities. VS Code windows advertise their folder in the title
/// ("file - folder - Visual Studio Code"), which survives restarts; most
/// other titles are too volatile to pin down.
pub fn captured_title_pattern(facts: &WindowFacts) -> Option<String> {
    let class = facts.class.to_ascii_lowercase();
    if !(class == "code" || class.starts_with("code-") || class.contains("vscodium")) {
        return None;
    }
    let folder = vscode_title_folder(&facts.title)?;
    Some(format!(" - {} - ", regex::escape(&folder)))
}

fn read_cmdline(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }
    split_cmdline(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?)
}

/// Split `/proc/<pid>/cmdline` into argv. Normally NUL-separated — but
/// chromium rewrites its cmdline in place with spaces, leaving one giant
/// "argument"; that shape is re-split on whitespace instead.
fn split_cmdline(bytes: &[u8]) -> Option<Vec<String>> {
    let argv: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    match argv.as_slice() {
        [] => None,
        [only] if only.contains(char::is_whitespace) => {
            Some(only.split_whitespace().map(str::to_owned).collect())
        }
        _ => Some(argv),
    }
}

/// Working directory for a captured window. A terminal's own cwd is where it
/// was launched; the directory the user actually sits in belongs to its child
/// shell — so when the process has exactly one child, prefer the child's cwd.
fn process_workdir(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    only_child(pid)
        .and_then(read_cwd)
        .or_else(|| read_cwd(pid))
        .filter(|cwd| cwd != "/")
}

fn only_child(pid: i32) -> Option<i32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).ok()?;
    let mut pids = text.split_whitespace();
    let first = pids.next()?.parse().ok()?;
    pids.next().is_none().then_some(first)
}

fn read_cwd(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|cwd| cwd.to_string_lossy().into_owned())
}

// ---- chromium ---------------------------------------------------------------

/// Pick the profile directory for a captured chromium/chrome window. All
/// windows share one process, so the cmdline rarely says; instead prefer a
/// profile whose display name matches the project, then the only active one.
fn chromium_profile_from_disk(config_dir: &str, project_name: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = format!("{home}/.config/{config_dir}/Local State");
    let local_state: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    chromium_profile(&local_state, project_name)
}

fn chromium_profile(local_state: &serde_json::Value, project_name: &str) -> Option<String> {
    let profile = local_state.get("profile")?;
    if let Some(cache) = profile.get("info_cache").and_then(|v| v.as_object()) {
        for (dir, info) in cache {
            if info
                .get("name")
                .and_then(|n| n.as_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(project_name))
            {
                return Some(dir.clone());
            }
        }
    }
    let active = profile.get("last_active_profiles")?.as_array()?;
    match active.as_slice() {
        [only] => only.as_str().map(str::to_owned),
        _ => None,
    }
}

// ---- vs code ----------------------------------------------------------------

/// Resolve a VS Code window's folder from its title. The title's second-to-
/// last ` - ` segment is the folder's basename ("file — folder — Visual
/// Studio Code"); VS Code's `storage.json` windowsState maps open windows to
/// full folder URIs, and the basename picks the right one.
fn vscode_folder(title: &str) -> Option<String> {
    let name = vscode_title_folder(title)?;
    let home = std::env::var("HOME").ok()?;
    for config in ["Code", "Code - OSS", "VSCodium"] {
        let path = format!("{home}/.config/{config}/User/globalStorage/storage.json");
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(storage) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if let Some(folder) = vscode_folder_matching(&storage, &name) {
            return Some(folder);
        }
    }
    None
}

fn vscode_title_folder(title: &str) -> Option<String> {
    let segments: Vec<&str> = title.split(" - ").collect();
    if segments.len() < 3 || !segments.last()?.contains("Visual Studio Code") {
        return None;
    }
    Some(segments[segments.len() - 2].to_owned())
}

fn vscode_folder_matching(storage: &serde_json::Value, name: &str) -> Option<String> {
    let state = storage.get("windowsState")?;
    let last = state
        .pointer("/lastActiveWindow/folder")
        .and_then(|v| v.as_str());
    let opened = state
        .get("openedWindows")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|w| w.get("folder").and_then(|v| v.as_str()));
    for uri in last.into_iter().chain(opened) {
        let Some(path) = uri.strip_prefix("file://") else {
            continue;
        };
        let path = percent_decode(path);
        if path
            .rsplit('/')
            .next()
            .is_some_and(|base| base.eq_ignore_ascii_case(name))
        {
            return Some(path);
        }
    }
    None
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len()
                && let Ok(byte) = u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                    16,
                ) =>
            {
                out.push(byte);
                i += 3;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vscode_title_parsing() {
        assert_eq!(
            vscode_title_folder("Welcome - omarchy-workspaces - Visual Studio Code"),
            Some("omarchy-workspaces".to_owned())
        );
        assert_eq!(
            vscode_title_folder("● main.rs - Harksoft - Visual Studio Code"),
            Some("Harksoft".to_owned())
        );
        // No folder open, or not a VS Code title.
        assert_eq!(vscode_title_folder("Untitled-1 - Visual Studio Code"), None);
        assert_eq!(vscode_title_folder("New Tab - Chromium"), None);
    }

    #[test]
    fn cmdline_splitting_handles_chromium_rewrite() {
        // Normal NUL-separated argv.
        assert_eq!(
            split_cmdline(b"/usr/bin/kitty\0--single-instance\0"),
            Some(vec!["/usr/bin/kitty".into(), "--single-instance".into()])
        );
        // Chromium-style: one giant space-separated "argument".
        assert_eq!(
            split_cmdline(b"/usr/lib/chromium/chromium --ozone-platform=wayland\0"),
            Some(vec![
                "/usr/lib/chromium/chromium".into(),
                "--ozone-platform=wayland".into()
            ])
        );
        assert_eq!(split_cmdline(b""), None);
    }

    #[test]
    fn title_pattern_pins_vscode_windows_to_their_folder() {
        let facts = |class: &str, title: &str| WindowFacts {
            class: class.to_owned(),
            title: title.to_owned(),
            ..Default::default()
        };
        assert_eq!(
            captured_title_pattern(&facts("code", "● main.rs - Harksoft - Visual Studio Code")),
            Some(" - Harksoft - ".to_owned())
        );
        // Regex metacharacters in folder names are escaped.
        assert_eq!(
            captured_title_pattern(&facts("code", "a - my (v2) app - Visual Studio Code")),
            Some(r" - my \(v2\) app - ".to_owned())
        );
        // Not VS Code, or no folder: no pattern.
        assert_eq!(
            captured_title_pattern(&facts("chromium", "Docs - Chromium")),
            None
        );
        assert_eq!(
            captured_title_pattern(&facts("code", "Untitled-1 - Visual Studio Code")),
            None
        );
    }

    #[test]
    fn vscode_folder_lookup_matches_basename() {
        let storage = serde_json::json!({
            "windowsState": {
                "lastActiveWindow": { "folder": "file:///home/u/Projects/omarchy-workspaces" },
                "openedWindows": [
                    { "folder": "file:///home/u/Harksoft" },
                    { "folder": "file:///home/u/My%20Site" },
                ]
            }
        });
        assert_eq!(
            vscode_folder_matching(&storage, "Harksoft"),
            Some("/home/u/Harksoft".to_owned())
        );
        assert_eq!(
            vscode_folder_matching(&storage, "My Site"),
            Some("/home/u/My Site".to_owned())
        );
        assert_eq!(vscode_folder_matching(&storage, "elsewhere"), None);
    }

    #[test]
    fn chromium_profile_prefers_project_name_match() {
        let local_state = serde_json::json!({
            "profile": {
                "info_cache": {
                    "Default": { "name": "Work" },
                    "Profile 4": { "name": "American Review Center" },
                },
                "last_active_profiles": ["Default", "Profile 4"],
            }
        });
        assert_eq!(
            chromium_profile(&local_state, "American Review Center"),
            Some("Profile 4".to_owned())
        );
        // No name match, several active: ambiguous, stay unset.
        assert_eq!(chromium_profile(&local_state, "Testing"), None);

        let single = serde_json::json!({
            "profile": {
                "info_cache": { "Default": { "name": "Work" } },
                "last_active_profiles": ["Default"],
            }
        });
        assert_eq!(
            chromium_profile(&single, "Testing"),
            Some("Default".to_owned())
        );
    }
}
