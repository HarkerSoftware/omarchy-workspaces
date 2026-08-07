//! `workspace-panel`: GTK4 layer-shell left icon rail for omarchy-workspaces.
//!
//! Anchored to the left edge with a fixed 48px exclusive zone (tiled windows
//! shift right and never reflow again). Hovering expands the rail in place —
//! the surface grows over the tiles because the exclusive zone stays at the
//! collapsed width. Colors follow the current Omarchy theme live.

mod client;
mod theme;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4 as gtk;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use workspace_proto::ProjectSummary;

use client::{UiRequest, UiUpdate};

/// Collapsed rail width in pixels; also the (fixed) exclusive zone.
const RAIL_WIDTH: i32 = 48;
/// Delay before hover expands the rail.
const EXPAND_DELAY: Duration = Duration::from_millis(300);
/// Delay before the pointer leaving collapses it.
const COLLAPSE_DELAY: Duration = Duration::from_millis(400);

const APP_ID: &str = "dev.omarchy.WorkspacePanel";

/// Shared UI state for the hover-expansion machinery.
#[derive(Default)]
struct Ui {
    expanded: Cell<bool>,
    revealers: RefCell<Vec<gtk::Revealer>>,
    hover_timer: RefCell<Option<glib::SourceId>>,
}

impl Ui {
    fn cancel_timer(&self) {
        if let Some(source) = self.hover_timer.borrow_mut().take() {
            source.remove();
        }
    }

    fn set_expanded(&self, expanded: bool) {
        self.expanded.set(expanded);
        for revealer in self.revealers.borrow().iter() {
            revealer.set_reveal_child(expanded);
        }
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    // NON_UNIQUE: a panel is a per-session process, not a D-Bus-activated
    // single instance — a second launch must not re-activate (and re-build UI
    // in) an existing process.
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(build_ui);
    // Never handle CLI args as files.
    app.run_with_args::<&str>(&[])
}

fn build_ui(app: &gtk::Application) {
    let window = gtk::ApplicationWindow::new(app);
    window.set_widget_name("workspace-panel");
    window.init_layer_shell();
    window.set_layer(Layer::Top);
    window.set_namespace(Some("workspace-panel"));
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Bottom, true);
    window.set_exclusive_zone(RAIL_WIDTH);
    window.set_keyboard_mode(KeyboardMode::OnDemand);
    window.set_default_width(RAIL_WIDTH);

    load_static_css();
    let colors = gtk::CssProvider::new();
    apply_theme_colors(&colors);
    watch_theme(colors.clone());

    let ui = Rc::new(Ui::default());

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("rail");

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("projects");
    root.append(&list);

    let status = gtk::Label::new(Some("…"));
    status.add_css_class("status");
    status.set_tooltip_text(Some("connecting to workspace-daemon"));
    root.append(&status);

    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    root.append(&spacer);

    let add_button = gtk::Button::with_label("+");
    add_button.add_css_class("add-project");
    add_button.set_tooltip_text(Some("New project"));
    root.append(&add_button);

    // Hover expansion: enter starts the expand timer, leave the collapse
    // timer; re-entering cancels a pending collapse (no flicker).
    let motion = gtk::EventControllerMotion::new();
    motion.connect_enter(glib::clone!(
        #[strong]
        ui,
        move |_, _, _| {
            ui.cancel_timer();
            let ui2 = Rc::clone(&ui);
            let source = glib::timeout_add_local_once(EXPAND_DELAY, move || {
                ui2.hover_timer.borrow_mut().take();
                ui2.set_expanded(true);
            });
            *ui.hover_timer.borrow_mut() = Some(source);
        }
    ));
    motion.connect_leave(glib::clone!(
        #[strong]
        ui,
        move |_| {
            ui.cancel_timer();
            let ui2 = Rc::clone(&ui);
            let source = glib::timeout_add_local_once(COLLAPSE_DELAY, move || {
                ui2.hover_timer.borrow_mut().take();
                ui2.set_expanded(false);
            });
            *ui.hover_timer.borrow_mut() = Some(source);
        }
    ));
    root.add_controller(motion);

    window.set_child(Some(&root));

    let (updates, requests) = client::spawn(None);

    // "+" opens a small popover with a name entry.
    {
        let requests = requests.clone();
        add_button.connect_clicked(move |button| {
            let popover = gtk::Popover::new();
            popover.set_parent(button);
            popover.set_position(gtk::PositionType::Right);
            let entry = gtk::Entry::new();
            entry.set_placeholder_text(Some("Project name"));
            entry.set_width_chars(18);
            let requests = requests.clone();
            entry.connect_activate(glib::clone!(
                #[weak]
                popover,
                move |entry| {
                    let name = entry.text().trim().to_owned();
                    if !name.is_empty() {
                        let _ = requests.send_blocking(UiRequest::Create(name));
                    }
                    popover.popdown();
                }
            ));
            popover.set_child(Some(&entry));
            popover.connect_closed(|popover| {
                let popover = popover.clone();
                glib::idle_add_local_once(move || popover.unparent());
            });
            popover.popup();
        });
    }

    // Strong references: these widgets live as long as the process, and a
    // silently-dying update loop would freeze the rail at its last state.
    glib::spawn_future_local(glib::clone!(
        #[strong]
        list,
        #[strong]
        status,
        #[strong]
        root,
        #[strong]
        ui,
        async move {
            while let Ok(update) = updates.recv().await {
                match update {
                    UiUpdate::Projects(projects) => {
                        root.remove_css_class("disconnected");
                        status.set_visible(false);
                        rebuild_rows(&list, &ui, &projects, &requests);
                    }
                    UiUpdate::Disconnected => {
                        root.add_css_class("disconnected");
                        status.set_visible(true);
                        status.set_label("!");
                        status.set_tooltip_text(Some(
                            "workspace-daemon is not running — start it with \
                             `systemctl --user start omarchy-workspaces`",
                        ));
                    }
                }
            }
        }
    ));

    window.present();
}

fn rebuild_rows(
    list: &gtk::ListBox,
    ui: &Rc<Ui>,
    projects: &[ProjectSummary],
    requests: &async_channel::Sender<UiRequest>,
) {
    while let Some(row) = list.row_at_index(0) {
        list.remove(&row);
    }
    ui.revealers.borrow_mut().clear();

    for project in projects {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("project-row");
        if project.active {
            row.add_css_class("active");
        }

        let avatar = gtk::Label::new(Some(&initial_letters(&project.name)));
        avatar.add_css_class("avatar");
        avatar.set_halign(gtk::Align::Center);
        avatar.set_valign(gtk::Align::Center);

        // Name + window count, revealed while the rail is expanded.
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let name = gtk::Label::new(Some(&project.name));
        name.add_css_class("row-label");
        name.set_halign(gtk::Align::Start);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_width_chars(16);
        name.set_max_width_chars(20);
        let sub = gtk::Label::new(Some(&format!(
            "{} window{}",
            project.windows,
            if project.windows == 1 { "" } else { "s" }
        )));
        sub.add_css_class("row-sub");
        sub.set_halign(gtk::Align::Start);
        text.append(&name);
        text.append(&sub);

        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::SlideRight);
        revealer.set_transition_duration(180);
        revealer.set_child(Some(&text));
        revealer.set_reveal_child(ui.expanded.get());
        ui.revealers.borrow_mut().push(revealer.clone());

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        content.append(&avatar);
        content.append(&revealer);
        row.set_child(Some(&content));
        row.set_tooltip_text(Some(&project.name));

        let slug = project.slug.as_str().to_owned();
        let switch_requests = requests.clone();
        let gesture = gtk::GestureClick::new();
        gesture.connect_released(move |_, _, _, _| {
            let _ = switch_requests.send_blocking(UiRequest::Switch(slug.clone()));
        });
        row.add_controller(gesture);

        // Right-click: management menu.
        let menu_gesture = gtk::GestureClick::new();
        menu_gesture.set_button(3);
        let menu_requests = requests.clone();
        let menu_project = project.clone();
        let menu_row = row.clone();
        menu_gesture.connect_released(move |_, _, _, _| {
            open_row_menu(&menu_row, &menu_project, &menu_requests);
        });
        row.add_controller(menu_gesture);

        list.append(&row);
    }
}

/// The right-click management menu for one project row.
fn open_row_menu(
    row: &gtk::ListBoxRow,
    project: &ProjectSummary,
    requests: &async_channel::Sender<UiRequest>,
) {
    let popover = gtk::Popover::new();
    popover.set_parent(row);
    popover.set_position(gtk::PositionType::Right);

    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu.add_css_class("row-menu");

    let header = gtk::Label::new(Some(&project.name));
    header.add_css_class("menu-header");
    header.set_halign(gtk::Align::Start);
    menu.append(&header);

    let slug = project.slug.as_str().to_owned();
    let action_button = |label: &str, request: UiRequest| {
        let button = gtk::Button::with_label(label);
        button.add_css_class("menu-btn");
        let requests = requests.clone();
        let popover = popover.clone();
        let request = std::cell::Cell::new(Some(request));
        button.connect_clicked(move |_| {
            if let Some(request) = request.take() {
                let _ = requests.send_blocking(request);
            }
            popover.popdown();
        });
        button
    };
    menu.append(&action_button(
        "Save windows",
        UiRequest::Save(slug.clone()),
    ));
    menu.append(&action_button("Restore", UiRequest::Restore(slug.clone())));

    // Rename: swap the popover content for an entry.
    let rename = gtk::Button::with_label("Rename…");
    rename.add_css_class("menu-btn");
    {
        let popover = popover.clone();
        let requests = requests.clone();
        let slug = slug.clone();
        let current = project.name.clone();
        rename.connect_clicked(move |_| {
            let entry = gtk::Entry::new();
            entry.set_text(&current);
            entry.set_width_chars(18);
            entry.select_region(0, -1);
            let requests = requests.clone();
            let slug = slug.clone();
            entry.connect_activate(glib::clone!(
                #[weak]
                popover,
                move |entry| {
                    let name = entry.text().trim().to_owned();
                    if !name.is_empty() {
                        let _ = requests.send_blocking(UiRequest::Rename {
                            slug: slug.clone(),
                            name,
                        });
                    }
                    popover.popdown();
                }
            ));
            popover.set_child(Some(&entry));
            entry.grab_focus();
        });
    }
    menu.append(&rename);

    // Delete: two-step confirmation in place.
    let delete = gtk::Button::with_label("Delete…");
    delete.add_css_class("menu-btn");
    {
        let popover = popover.clone();
        let requests = requests.clone();
        let armed = std::cell::Cell::new(false);
        delete.connect_clicked(move |button| {
            if armed.replace(true) {
                let _ = requests.send_blocking(UiRequest::Delete(slug.clone()));
                popover.popdown();
            } else {
                button.set_label("Really delete?");
                button.add_css_class("destructive");
            }
        });
    }
    menu.append(&delete);

    popover.set_child(Some(&menu));
    popover.connect_closed(|popover| {
        let popover = popover.clone();
        glib::idle_add_local_once(move || popover.unparent());
    });
    popover.popup();
}

/// Up to two initial letters of the display name ("Web Development" → "WD").
fn initial_letters(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

fn load_static_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("../resources/panel.css"));
    add_provider(&provider);
}

fn apply_theme_colors(provider: &gtk::CssProvider) {
    provider.load_from_string(&theme::color_definitions());
    add_provider(provider);
}

/// Reload the color definitions whenever the Omarchy theme changes.
fn watch_theme(provider: gtk::CssProvider) {
    let Some(dir) = theme::omarchy_current_dir() else {
        return;
    };
    let file = gio::File::for_path(&dir);
    match file.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE) {
        Ok(monitor) => {
            monitor.connect_changed(move |_, _, _, _| {
                apply_theme_colors(&provider);
            });
            // Keep the monitor alive for the process lifetime.
            std::mem::forget(monitor);
        }
        Err(error) => tracing::debug!(%error, "cannot watch omarchy theme dir"),
    }
}

fn add_provider(provider: &gtk::CssProvider) {
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
