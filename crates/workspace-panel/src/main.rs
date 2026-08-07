//! `workspace-panel`: GTK4 layer-shell left icon rail for omarchy-workspaces.
//!
//! Anchored to the left edge with a fixed 48px exclusive zone (tiled windows
//! shift right and never reflow again). Hovering expands the rail in place —
//! the surface grows over the tiles because the exclusive zone stays at the
//! collapsed width. Colors follow the current Omarchy theme live.

mod client;
mod settings;
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
    /// A row drag is in flight: normal pointer crossing events are
    /// suppressed by GTK, so hover state must not collapse the rail.
    drag_active: Cell<bool>,
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

    /// Collapse after the usual delay (unless a drag is pinning the rail).
    fn schedule_collapse(self: &Rc<Self>) {
        self.cancel_timer();
        let ui = Rc::clone(self);
        let source = glib::timeout_add_local_once(COLLAPSE_DELAY, move || {
            ui.hover_timer.borrow_mut().take();
            if !ui.drag_active.get() {
                ui.set_expanded(false);
            }
        });
        *self.hover_timer.borrow_mut() = Some(source);
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
    // After a drop the pointer may still be inside without a fresh enter
    // event; any movement re-arms expansion.
    motion.connect_motion(glib::clone!(
        #[strong]
        ui,
        move |_, _, _| {
            if !ui.expanded.get() && ui.hover_timer.borrow().is_none() && !ui.drag_active.get() {
                let ui2 = Rc::clone(&ui);
                let source = glib::timeout_add_local_once(EXPAND_DELAY, move || {
                    ui2.hover_timer.borrow_mut().take();
                    ui2.set_expanded(true);
                });
                *ui.hover_timer.borrow_mut() = Some(source);
            }
        }
    ));
    motion.connect_leave(glib::clone!(
        #[strong]
        ui,
        move |_| {
            // A starting drag delivers a synthetic leave; ignore it — the
            // drag keeps the rail pinned open until it ends.
            if ui.drag_active.get() {
                return;
            }
            ui.schedule_collapse();
        }
    ));
    root.add_controller(motion);

    window.set_child(Some(&root));

    let (updates, requests) = client::spawn(None);
    let settings_window: settings::Shared = Rc::new(RefCell::new(None));
    // The daemon-confirmed order, and what the list currently shows —
    // they diverge only while a drag is shuffling rows live.
    let row_order: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let visual_order: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    // Slug being dragged, if a drag is in flight.
    let drag_state: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    // Handles the per-row drag sources need (begin/cancel bookkeeping).
    let drag = Rc::new(DragCtx {
        list: list.clone(),
        state: Rc::clone(&drag_state),
        committed: Rc::clone(&row_order),
        visual: Rc::clone(&visual_order),
    });

    // Rows shuffle out of the way live while the drag hovers; the drop
    // commits whatever arrangement is on screen.
    {
        let target = gtk::DropTarget::new(glib::types::Type::STRING, gtk::gdk::DragAction::MOVE);
        {
            let list = list.clone();
            let visual_order = Rc::clone(&visual_order);
            let drag_state = Rc::clone(&drag_state);
            target.connect_motion(move |_, _x, y| {
                let dragged = drag_state.borrow().clone();
                if let Some(slug) = dragged
                    && let Some(row) = list.row_at_y(y as i32)
                {
                    let to = row.index().max(0) as usize;
                    move_row(&list, &mut visual_order.borrow_mut(), &slug, to);
                }
                gtk::gdk::DragAction::MOVE
            });
        }
        {
            let requests = requests.clone();
            let row_order = Rc::clone(&row_order);
            let visual_order = Rc::clone(&visual_order);
            let drag_state = Rc::clone(&drag_state);
            target.connect_drop(move |_, value, _x, _y| {
                if value.get::<String>().is_err() {
                    return false;
                }
                drag_state.borrow_mut().take();
                let order = visual_order.borrow().clone();
                *row_order.borrow_mut() = order.clone();
                let _ = requests.send_blocking(UiRequest::Reorder(order));
                true
            });
        }
        list.add_controller(target);
    }

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
        #[strong]
        settings_window,
        #[strong]
        row_order,
        #[strong]
        visual_order,
        #[strong]
        drag,
        async move {
            while let Ok(update) = updates.recv().await {
                match update {
                    UiUpdate::Projects(projects) => {
                        root.remove_css_class("disconnected");
                        status.set_visible(false);
                        let order: Vec<String> = projects
                            .iter()
                            .map(|p| p.slug.as_str().to_owned())
                            .collect();
                        *row_order.borrow_mut() = order.clone();
                        *visual_order.borrow_mut() = order;
                        rebuild_rows(&list, &ui, &projects, &requests, &settings_window, &drag);
                    }
                    UiUpdate::ProjectDetails(project) => {
                        if let Some(open) = settings_window.borrow().as_ref() {
                            open.populate(&project);
                        }
                    }
                    UiUpdate::CaptureResult(capture) => {
                        if let Some(open) = settings_window.borrow().as_ref() {
                            open.apply_capture(&capture);
                        }
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

/// Shared handles for drag-to-reorder: the list, the in-flight drag slug,
/// and the committed vs on-screen orders.
struct DragCtx {
    list: gtk::ListBox,
    state: Rc<RefCell<Option<String>>>,
    committed: Rc<RefCell<Vec<String>>>,
    visual: Rc<RefCell<Vec<String>>>,
}

/// Move the row for `slug` to visual position `to`, keeping `visual` in
/// sync. Re-inserting the same widget preserves it (controllers included).
fn move_row(list: &gtk::ListBox, visual: &mut Vec<String>, slug: &str, to: usize) {
    let Some(from) = visual.iter().position(|s| s == slug) else {
        return;
    };
    let to = to.min(visual.len().saturating_sub(1));
    if from == to {
        return;
    }
    let Some(row) = list.row_at_index(from as i32) else {
        return;
    };
    list.remove(&row);
    list.insert(&row, to as i32);
    let moved = visual.remove(from);
    visual.insert(to, moved);
}

fn rebuild_rows(
    list: &gtk::ListBox,
    ui: &Rc<Ui>,
    projects: &[ProjectSummary],
    requests: &async_channel::Sender<UiRequest>,
    settings_window: &settings::Shared,
    drag_ctx: &Rc<DragCtx>,
) {
    while let Some(row) = list.row_at_index(0) {
        list.remove(&row);
    }
    ui.revealers.borrow_mut().clear();

    for project in projects {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("project-row");
        if project.active {
            // Its workspace is on screen right now: green ring.
            row.add_css_class("active");
            row.add_css_class("viewing");
        } else if project.windows > 0 {
            // Open (has windows) but not the workspace being viewed: yellow.
            row.add_css_class("open");
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

        // Drag to reorder: rows shuffle live under the drag (the list's
        // drop target moves them on hover); dropping commits, a cancelled
        // drag snaps everything back. A plain click still switches (drags
        // only start past the movement threshold).
        let source = gtk::DragSource::new();
        source.set_actions(gtk::gdk::DragAction::MOVE);
        {
            let slug = project.slug.as_str().to_owned();
            source.connect_prepare(move |_, _, _| {
                Some(gtk::gdk::ContentProvider::for_value(&slug.to_value()))
            });
        }
        {
            let row = row.clone();
            let drag = Rc::clone(drag_ctx);
            let ui = Rc::clone(ui);
            let slug = project.slug.as_str().to_owned();
            source.connect_drag_begin(move |source, _| {
                let paintable = gtk::WidgetPaintable::new(Some(&row));
                source.set_icon(Some(&paintable), 0, 0);
                row.add_css_class("dragging");
                *drag.state.borrow_mut() = Some(slug.clone());
                // Pin the rail open for the whole drag.
                ui.cancel_timer();
                ui.drag_active.set(true);
                ui.set_expanded(true);
            });
        }
        {
            let row = row.clone();
            let drag = Rc::clone(drag_ctx);
            let ui = Rc::clone(ui);
            source.connect_drag_end(move |_, _, _| {
                row.remove_css_class("dragging");
                // Still marked in-flight: the drop never happened
                // (cancelled/escaped) — shuffle back to the daemon's order.
                if drag.state.borrow_mut().take().is_some() {
                    let committed = drag.committed.borrow().clone();
                    let mut visual = drag.visual.borrow_mut();
                    for (index, slug) in committed.iter().enumerate() {
                        move_row(&drag.list, &mut visual, slug, index);
                    }
                }
                // Unpin; the usual collapse countdown takes over.
                ui.drag_active.set(false);
                ui.schedule_collapse();
            });
        }
        row.add_controller(source);

        // Right-click: management menu.
        let menu_gesture = gtk::GestureClick::new();
        menu_gesture.set_button(3);
        let menu_requests = requests.clone();
        let menu_project = project.clone();
        let menu_row = row.clone();
        let menu_settings = Rc::clone(settings_window);
        menu_gesture.connect_released(move |_, _, _, _| {
            open_row_menu(&menu_row, &menu_project, &menu_requests, &menu_settings);
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
    settings_window: &settings::Shared,
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
    menu.append(&action_button("Close windows", UiRequest::Close(slug.clone())));

    // Apps…: the per-slot launch settings window.
    let apps = gtk::Button::with_label("Apps…");
    apps.add_css_class("menu-btn");
    {
        let popover = popover.clone();
        let requests = requests.clone();
        let settings_window = Rc::clone(settings_window);
        let slug = slug.clone();
        let name = project.name.clone();
        apps.connect_clicked(move |_| {
            settings::open(&slug, &name, &requests, &settings_window);
            popover.popdown();
        });
    }
    menu.append(&apps);

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
