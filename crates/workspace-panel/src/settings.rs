//! Per-project app settings window: view and edit each saved app slot's
//! launch command, working directory, and browser profile, with a one-click
//! "detect from open windows" fill that runs the daemon's capture preview.
//!
//! The window is populated asynchronously: it opens in a loading state, the
//! client thread fetches `project.get`, and the main loop routes the reply
//! (and any later capture preview) here through the shared `Open` handle.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;

use gtk::glib;
use gtk::prelude::*;

use crate::client::UiRequest;

/// The currently open settings window, if any. One at a time is plenty.
pub type Shared = Rc<RefCell<Option<Open>>>;

/// One editable app slot row.
struct SlotRow {
    slot_id: String,
    class: String,
    command: gtk::Entry,
    workdir: gtk::Entry,
    /// Present only for browser slots.
    profile: Option<gtk::Entry>,
}

/// An open settings window and its live widgets.
pub struct Open {
    pub slug: String,
    window: gtk::Window,
    list: gtk::Box,
    status: gtk::Label,
    rows: RefCell<Vec<SlotRow>>,
    requests: async_channel::Sender<UiRequest>,
}

/// Open the settings window for a project and request its definition.
pub fn open(
    slug: &str,
    name: &str,
    requests: &async_channel::Sender<UiRequest>,
    shared: &Shared,
) {
    // Re-presenting an existing window beats stacking a second one. The
    // borrow must be released before touching the window: `close()` emits
    // `close_request` synchronously, whose handler borrows this same cell —
    // closing (or presenting) under the borrow aborts the process.
    let previous = {
        let mut guard = shared.borrow_mut();
        match guard.as_ref() {
            Some(open) if open.slug == slug => {
                let window = open.window.clone();
                drop(guard);
                window.present();
                return;
            }
            Some(_) => guard.take(),
            None => None,
        }
    };
    if let Some(previous) = previous {
        previous.window.close();
    }

    let window = gtk::Window::builder()
        .title(format!("{name} — Apps"))
        .default_width(560)
        .default_height(440)
        .build();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.add_css_class("settings");

    let status = gtk::Label::new(Some("Loading…"));
    status.add_css_class("settings-status");
    status.set_halign(gtk::Align::Start);
    root.append(&status);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    root.append(&scroll);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let detect = gtk::Button::with_label("Detect from open windows");
    detect.add_css_class("menu-btn");
    detect.set_tooltip_text(Some(
        "Fill the fields from the project's currently open windows \
         (chromium profile, VS Code folder, terminal directory)",
    ));
    let save = gtk::Button::with_label("Save");
    save.add_css_class("menu-btn");
    save.add_css_class("suggested");
    buttons.append(&detect);
    buttons.append(&save);
    root.append(&buttons);

    window.set_child(Some(&root));

    {
        let requests = requests.clone();
        let slug = slug.to_owned();
        detect.connect_clicked(move |_| {
            let _ = requests.send_blocking(UiRequest::Capture(slug.clone()));
        });
    }
    {
        let shared = Rc::clone(shared);
        save.connect_clicked(move |_| {
            // Send the updates under the borrow, but release it before
            // closing: `close()` re-enters the shared cell through the
            // `close_request` handler.
            let window = {
                let borrowed = shared.borrow();
                let Some(open) = borrowed.as_ref() else { return };
                for row in open.rows.borrow().iter() {
                    let _ = open.requests.send_blocking(UiRequest::UpdateSlot {
                        slug: open.slug.clone(),
                        slot_id: row.slot_id.clone(),
                        command: row.command.text().trim().to_owned(),
                        workdir: row.workdir.text().trim().to_owned(),
                        profile: row
                            .profile
                            .as_ref()
                            .map(|entry| entry.text().trim().to_owned()),
                    });
                }
                open.window.clone()
            };
            window.close();
        });
    }
    {
        let shared = Rc::clone(shared);
        window.connect_close_request(move |_| {
            shared.borrow_mut().take();
            glib::Propagation::Proceed
        });
    }

    window.present();
    *shared.borrow_mut() = Some(Open {
        slug: slug.to_owned(),
        window,
        list,
        status,
        rows: RefCell::new(Vec::new()),
        requests: requests.clone(),
    });
    let _ = requests.send_blocking(UiRequest::Get(slug.to_owned()));
}

impl Open {
    /// Fill the window from a `project.get` reply. Ignores other projects.
    pub fn populate(&self, project: &serde_json::Value) {
        if project.get("slug").and_then(|s| s.as_str()) != Some(self.slug.as_str()) {
            return;
        }
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.rows.borrow_mut().clear();

        let apps = project
            .get("apps")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        if apps.is_empty() {
            self.status.set_label(
                "No saved apps yet — use “Save windows” first, \
                 with the project's windows open.",
            );
            return;
        }
        self.status
            .set_label("Launch settings for each saved app:");

        for app in &apps {
            let Some(slot_id) = app.get("slot_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let class = app
                .pointer("/identity/class")
                .and_then(|v| v.as_str())
                .unwrap_or("app")
                .to_owned();
            let launch = app.get("launch");
            // A slot saved without a launch spec still restores via its
            // identity executable; show that as the effective command.
            let command_text = launch
                .map(command_line)
                .filter(|text| !text.is_empty())
                .or_else(|| {
                    app.pointer("/identity/executable")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            let workdir_text = launch
                .and_then(|l| l.get("workdir"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            let profile_text = launch.and_then(profile_argument);

            let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
            card.add_css_class("slot-card");
            let header = gtk::Label::new(Some(&class));
            header.add_css_class("slot-class");
            header.set_halign(gtk::Align::Start);
            card.append(&header);

            let grid = gtk::Grid::new();
            grid.set_column_spacing(8);
            grid.set_row_spacing(4);
            let mut next_grid_row = 0;
            let mut field = |label: &str, text: &str| -> gtk::Entry {
                let caption = gtk::Label::new(Some(label));
                caption.add_css_class("field-label");
                caption.set_halign(gtk::Align::Start);
                let entry = gtk::Entry::new();
                entry.set_text(text);
                entry.set_hexpand(true);
                grid.attach(&caption, 0, next_grid_row, 1, 1);
                grid.attach(&entry, 1, next_grid_row, 1, 1);
                next_grid_row += 1;
                entry
            };

            let command = field("Command", &command_text);
            let workdir = field("Directory", &workdir_text);
            let profile = is_browser(&class)
                .then(|| field("Profile", profile_text.as_deref().unwrap_or_default()));
            card.append(&grid);
            self.list.append(&card);

            self.rows.borrow_mut().push(SlotRow {
                slot_id: slot_id.to_owned(),
                class,
                command,
                workdir,
                profile,
            });
        }
    }

    /// Fill entries from a `project.capture` preview: captured slots are
    /// matched to the displayed rows by window class, in order.
    pub fn apply_capture(&self, capture: &serde_json::Value) {
        if capture.get("project").and_then(|s| s.as_str()) != Some(self.slug.as_str()) {
            return;
        }
        let apps = capture
            .get("apps")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        if apps.is_empty() {
            self.status.set_label(
                "Nothing detected — the project has no windows open right now.",
            );
            return;
        }
        self.status.set_label("Detected from open windows — review and save:");

        let rows = self.rows.borrow();
        let mut used = vec![false; apps.len()];
        for row in rows.iter() {
            let matched = apps.iter().enumerate().find(|(i, app)| {
                !used[*i]
                    && app
                        .pointer("/identity/class")
                        .and_then(|v| v.as_str())
                        .is_some_and(|class| class.eq_ignore_ascii_case(&row.class))
            });
            let Some((index, app)) = matched else { continue };
            used[index] = true;
            let Some(launch) = app.get("launch") else {
                continue;
            };
            let profile = profile_argument(launch);
            row.command.set_text(&command_line(launch));
            if let Some(workdir) = launch.get("workdir").and_then(|v| v.as_str()) {
                row.workdir.set_text(workdir);
            }
            if let (Some(entry), Some(profile)) = (&row.profile, profile) {
                entry.set_text(&profile);
            }
        }
    }
}

/// Whether a window class belongs to a profile-capable browser.
fn is_browser(class: &str) -> bool {
    let class = class.to_ascii_lowercase();
    class.contains("chromium") || class.contains("chrome")
}

/// Render a launch spec as one editable command line, leaving the profile
/// argument to its dedicated field. The line is saved back as a raw shell
/// command, so arguments with shell-special characters (tab URLs full of
/// `?` and `&`) are single-quoted.
fn command_line(launch: &serde_json::Value) -> String {
    let command = launch
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let args = launch
        .get("args")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .filter(|a| !a.starts_with("--profile-directory"))
        .map(shell_quote);
    std::iter::once(command)
        .chain(args)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Single-quote an argument unless it is plain enough to need no quoting.
fn shell_quote(arg: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_-./=:@+%,".contains(c);
    if !arg.is_empty() && arg.chars().all(plain) {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// Extract the profile from a launch spec's `--profile-directory=` argument.
fn profile_argument(launch: &serde_json::Value) -> Option<String> {
    launch
        .get("args")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .find_map(|a| a.strip_prefix("--profile-directory="))
        .map(str::to_owned)
}
