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

    let app = gtk::Application::builder().application_id(APP_ID).build();
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

    glib::spawn_future_local(glib::clone!(
        #[weak]
        list,
        #[weak]
        status,
        #[weak]
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
        let requests = requests.clone();
        let gesture = gtk::GestureClick::new();
        gesture.connect_released(move |_, _, _, _| {
            let _ = requests.send_blocking(UiRequest::Switch(slug.clone()));
        });
        row.add_controller(gesture);
        list.append(&row);
    }
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
