//! Minimal reader for chromium's SNSS session files — just enough to list
//! the open tabs of a browser window at capture time.
//!
//! Chromium continuously journals its session to
//! `<profile>/Sessions/Session_<timestamp>` as a command log: fixed-header
//! records (u16 size, u8 command id) whose payloads are either raw structs
//! or "pickles" (u32 payload size, then 4-byte-aligned fields). Replaying
//! the commands of the newest file yields the live windows and their tabs.
//! Only the handful of commands needed for tab URLs are interpreted; all
//! others are skipped by size. Validated against SNSS versions 1 and 3.

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// One open browser window: tabs in strip order.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct BrowserWindow {
    /// `(strip index, url, title)` per tab, sorted by strip index.
    pub tabs: Vec<(i32, String, String)>,
    /// Strip index of the selected (foreground) tab.
    pub selected_index: Option<i32>,
}

impl BrowserWindow {
    /// The selected tab's title, if known.
    fn selected_title(&self) -> Option<&str> {
        let index = self.selected_index?;
        self.tabs
            .iter()
            .find(|(i, _, _)| *i == index)
            .map(|(_, _, title)| title.as_str())
    }
}

/// The launchable tab URLs of the profile's window best matching a
/// compositor window title. Best-effort: `None` when the session cannot be
/// read, no window matches, or the match is ambiguous.
pub fn window_tabs(profile_dir: &Path, window_title: &str) -> Option<Vec<String>> {
    let session = latest_session_file(profile_dir)?;
    let windows = open_windows(&std::fs::read(session).ok()?);
    let window = match_window(&windows, window_title)?;
    let urls: Vec<String> = window
        .tabs
        .iter()
        .map(|(_, url, _)| url.clone())
        .filter(|url| is_launchable(url))
        .collect();
    (!urls.is_empty()).then_some(urls)
}

/// Newest `Session_*` file — the one chromium is journaling right now.
fn latest_session_file(profile_dir: &Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(profile_dir.join("Sessions"))
        .ok()?
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("Session_")
        })
        .max_by_key(|entry| {
            entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH)
        })
        .map(|entry| entry.path())
}

/// Pick the session window belonging to a compositor window. The window
/// title is the active tab's title plus a browser suffix; unread-count
/// prefixes like `(3) ` drift, so titles compare with those stripped.
fn match_window<'w>(
    windows: &'w HashMap<i32, BrowserWindow>,
    window_title: &str,
) -> Option<&'w BrowserWindow> {
    let wanted = normalize_title(strip_browser_suffix(window_title));
    // Selected-tab match first (the title source), then any tab.
    for selected_only in [true, false] {
        for window in windows.values() {
            let hit = if selected_only {
                window
                    .selected_title()
                    .is_some_and(|t| normalize_title(t) == wanted)
            } else {
                window
                    .tabs
                    .iter()
                    .any(|(_, _, t)| normalize_title(t) == wanted)
            };
            if hit {
                return Some(window);
            }
        }
    }
    // No title matched (title drifted since the last journal write): with a
    // single open window there is nothing to confuse it with.
    match windows.len() {
        1 => windows.values().next(),
        _ => None,
    }
}

fn strip_browser_suffix(title: &str) -> &str {
    for suffix in [" - Chromium", " - Google Chrome", " - Brave"] {
        if let Some(stripped) = title.strip_suffix(suffix) {
            return stripped;
        }
    }
    title
}

/// Drop a leading unread-count marker: `(3) Calendar` → `Calendar`.
fn normalize_title(title: &str) -> &str {
    let Some(rest) = title.strip_prefix('(') else {
        return title;
    };
    let Some(close) = rest.find(") ") else {
        return title;
    };
    if rest[..close].chars().all(|c| c.is_ascii_digit()) && close > 0 {
        &rest[close + 2..]
    } else {
        title
    }
}

fn is_launchable(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://") || url.starts_with("file://")
}

// ---- command replay ---------------------------------------------------------

const SET_TAB_WINDOW: u8 = 0;
const SET_TAB_INDEX_IN_WINDOW: u8 = 2;
const UPDATE_TAB_NAVIGATION: u8 = 6;
const SET_SELECTED_NAVIGATION_INDEX: u8 = 7;
const SET_SELECTED_TAB_IN_INDEX: u8 = 8;
const TAB_CLOSED: u8 = 16;
const WINDOW_CLOSED: u8 = 17;

/// Replay a session file's command log into its open windows.
pub fn open_windows(data: &[u8]) -> HashMap<i32, BrowserWindow> {
    let mut tab_window: HashMap<i32, i32> = HashMap::new();
    let mut tab_index: HashMap<i32, i32> = HashMap::new();
    let mut navs: HashMap<(i32, i32), (String, String)> = HashMap::new();
    let mut selected_nav: HashMap<i32, i32> = HashMap::new();
    let mut selected_tab: HashMap<i32, i32> = HashMap::new();
    let mut closed_tabs: HashSet<i32> = HashSet::new();
    let mut closed_windows: HashSet<i32> = HashSet::new();

    for (id, payload) in Commands::new(data) {
        match id {
            SET_TAB_WINDOW => {
                if let Some((window, tab)) = raw_pair(payload) {
                    tab_window.insert(tab, window);
                    closed_tabs.remove(&tab);
                    closed_windows.remove(&window);
                }
            }
            SET_TAB_INDEX_IN_WINDOW => {
                if let Some((tab, index)) = raw_pair(payload) {
                    tab_index.insert(tab, index);
                }
            }
            UPDATE_TAB_NAVIGATION => {
                let mut pickle = Pickle::new(payload);
                if let (Some(tab), Some(index), Some(url), Some(title)) = (
                    pickle.read_i32(),
                    pickle.read_i32(),
                    pickle.read_string(),
                    pickle.read_string16(),
                ) {
                    navs.insert((tab, index), (url, title));
                }
            }
            SET_SELECTED_NAVIGATION_INDEX => {
                if let Some((tab, index)) = raw_pair(payload) {
                    selected_nav.insert(tab, index);
                }
            }
            SET_SELECTED_TAB_IN_INDEX => {
                if let Some((window, index)) = raw_pair(payload) {
                    selected_tab.insert(window, index);
                }
            }
            TAB_CLOSED => {
                if let Some(tab) = raw_i32(payload) {
                    closed_tabs.insert(tab);
                }
            }
            WINDOW_CLOSED => {
                if let Some(window) = raw_i32(payload) {
                    closed_windows.insert(window);
                }
            }
            _ => {}
        }
    }

    let mut windows: HashMap<i32, BrowserWindow> = HashMap::new();
    for (tab, window) in &tab_window {
        if closed_tabs.contains(tab) || closed_windows.contains(window) {
            continue;
        }
        // The tab's current page: its selected navigation entry, falling
        // back to the highest recorded one.
        let entry = selected_nav
            .get(tab)
            .and_then(|index| navs.get(&(*tab, *index)))
            .or_else(|| {
                navs.iter()
                    .filter(|((t, _), _)| t == tab)
                    .max_by_key(|((_, index), _)| *index)
                    .map(|(_, entry)| entry)
            });
        let Some((url, title)) = entry else { continue };
        windows.entry(*window).or_default().tabs.push((
            tab_index.get(tab).copied().unwrap_or(0),
            url.clone(),
            title.clone(),
        ));
    }
    for (id, window) in &mut windows {
        window.tabs.sort();
        window.selected_index = selected_tab.get(id).copied();
    }
    windows
}

/// Iterator over `(command id, payload)` records after the 8-byte header.
struct Commands<'d> {
    data: &'d [u8],
    offset: usize,
}

impl<'d> Commands<'d> {
    fn new(data: &'d [u8]) -> Self {
        // "SNSS" magic + i32 version; bad magic yields an empty iterator.
        let valid = data.len() >= 8 && &data[..4] == b"SNSS";
        Self {
            data,
            offset: if valid { 8 } else { data.len() },
        }
    }
}

impl<'d> Iterator for Commands<'d> {
    type Item = (u8, &'d [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let header = self.data.get(self.offset..self.offset + 2)?;
        let size = u16::from_le_bytes([header[0], header[1]]) as usize;
        let body = self.data.get(self.offset + 2..self.offset + 2 + size)?;
        self.offset += 2 + size;
        let (&id, payload) = body.split_first()?;
        Some((id, payload))
    }
}

fn raw_i32(payload: &[u8]) -> Option<i32> {
    payload
        .get(..4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn raw_pair(payload: &[u8]) -> Option<(i32, i32)> {
    Some((raw_i32(payload)?, raw_i32(payload.get(4..)?)?))
}

/// Reader for chromium's pickle format: u32 payload size header, then
/// 4-byte-aligned fields.
struct Pickle<'d> {
    data: &'d [u8],
    offset: usize,
}

impl<'d> Pickle<'d> {
    fn new(data: &'d [u8]) -> Self {
        Self { data, offset: 4 }
    }

    fn read_i32(&mut self) -> Option<i32> {
        let value = raw_i32(self.data.get(self.offset..)?)?;
        self.offset += 4;
        Some(value)
    }

    fn read_string(&mut self) -> Option<String> {
        let length = usize::try_from(self.read_i32()?).ok()?;
        let bytes = self.data.get(self.offset..self.offset + length)?;
        self.offset += (length + 3) & !3;
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    fn read_string16(&mut self) -> Option<String> {
        let chars = usize::try_from(self.read_i32()?).ok()?;
        let bytes = self.data.get(self.offset..self.offset + 2 * chars)?;
        self.offset += (2 * chars + 3) & !3;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        Some(String::from_utf16_lossy(&units))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(id: u8, payload: &[u8]) -> Vec<u8> {
        let size = (payload.len() + 1) as u16;
        let mut out = size.to_le_bytes().to_vec();
        out.push(id);
        out.extend_from_slice(payload);
        out
    }

    fn pair(a: i32, b: i32) -> Vec<u8> {
        let mut out = a.to_le_bytes().to_vec();
        out.extend_from_slice(&b.to_le_bytes());
        out
    }

    fn navigation(tab: i32, index: i32, url: &str, title: &str) -> Vec<u8> {
        let mut body = pair(tab, index);
        body.extend_from_slice(&(url.len() as i32).to_le_bytes());
        body.extend_from_slice(url.as_bytes());
        body.resize(body.len().next_multiple_of(4), 0);
        let units: Vec<u16> = title.encode_utf16().collect();
        body.extend_from_slice(&(units.len() as i32).to_le_bytes());
        for unit in &units {
            body.extend_from_slice(&unit.to_le_bytes());
        }
        body.resize(body.len().next_multiple_of(4), 0);
        let mut pickled = (body.len() as u32).to_le_bytes().to_vec();
        pickled.extend_from_slice(&body);
        pickled
    }

    fn session(commands: &[Vec<u8>]) -> Vec<u8> {
        let mut data = b"SNSS".to_vec();
        data.extend_from_slice(&3i32.to_le_bytes());
        for c in commands {
            data.extend_from_slice(c);
        }
        data
    }

    fn two_window_session() -> Vec<u8> {
        session(&[
            command(SET_TAB_WINDOW, &pair(1, 10)),
            command(SET_TAB_WINDOW, &pair(1, 11)),
            command(SET_TAB_WINDOW, &pair(2, 20)),
            command(SET_TAB_INDEX_IN_WINDOW, &pair(10, 0)),
            command(SET_TAB_INDEX_IN_WINDOW, &pair(11, 1)),
            command(UPDATE_TAB_NAVIGATION, &navigation(10, 0, "https://old.example", "Old")),
            command(UPDATE_TAB_NAVIGATION, &navigation(10, 1, "https://docs.example", "Docs")),
            command(UPDATE_TAB_NAVIGATION, &navigation(11, 0, "https://mail.example", "Mail")),
            command(UPDATE_TAB_NAVIGATION, &navigation(20, 0, "https://gone.example", "Gone")),
            command(SET_SELECTED_NAVIGATION_INDEX, &pair(10, 1)),
            command(SET_SELECTED_TAB_IN_INDEX, &pair(1, 0)),
            command(WINDOW_CLOSED, &2i32.to_le_bytes()),
        ])
    }

    #[test]
    fn replays_open_windows_and_tabs() {
        let windows = open_windows(&two_window_session());
        assert_eq!(windows.len(), 1);
        let window = &windows[&1];
        // Tab 10 shows its *selected* navigation (Docs, not Old).
        assert_eq!(
            window.tabs,
            vec![
                (0, "https://docs.example".into(), "Docs".into()),
                (1, "https://mail.example".into(), "Mail".into()),
            ]
        );
        assert_eq!(window.selected_index, Some(0));
    }

    #[test]
    fn title_matching_survives_unread_prefix_and_suffix() {
        let windows = open_windows(&two_window_session());
        let tabs = |title: &str| {
            match_window(&windows, title).map(|w| w.tabs.len())
        };
        assert_eq!(tabs("Docs - Chromium"), Some(2));
        assert_eq!(tabs("(7) Docs - Chromium"), Some(2)); // unread drift
        assert_eq!(tabs("Mail - Chromium"), Some(2)); // background tab still matches
        // Single open window: matched even when the title finds nothing.
        assert_eq!(tabs("Something Else - Chromium"), Some(2));
    }

    #[test]
    fn ambiguous_title_with_multiple_windows_matches_nothing() {
        let mut commands = vec![
            command(SET_TAB_WINDOW, &pair(1, 10)),
            command(SET_TAB_WINDOW, &pair(2, 20)),
            command(UPDATE_TAB_NAVIGATION, &navigation(10, 0, "https://a.example", "A")),
            command(UPDATE_TAB_NAVIGATION, &navigation(20, 0, "https://b.example", "B")),
        ];
        let windows = open_windows(&session(&std::mem::take(&mut commands)));
        assert_eq!(windows.len(), 2);
        assert!(match_window(&windows, "Nope - Chromium").is_none());
        assert_eq!(
            match_window(&windows, "B - Chromium").map(|w| w.tabs[0].1.as_str()),
            Some("https://b.example")
        );
    }

    #[test]
    fn non_launchable_urls_are_dropped() {
        assert!(is_launchable("https://example.com"));
        assert!(!is_launchable("chrome://newtab/"));
        assert!(!is_launchable("devtools://devtools/bundled/"));
    }

    #[test]
    fn garbage_input_yields_nothing() {
        assert!(open_windows(b"not a session").is_empty());
        assert!(open_windows(&[]).is_empty());
        // Truncated command record.
        let mut data = b"SNSS".to_vec();
        data.extend_from_slice(&3i32.to_le_bytes());
        data.extend_from_slice(&99u16.to_le_bytes());
        data.push(6);
        assert!(open_windows(&data).is_empty());
    }
}
