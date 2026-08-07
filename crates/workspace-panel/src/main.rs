//! `workspace-panel`: GTK4 layer-shell left icon rail for omarchy-workspaces.
//!
//! Anchored to the left edge with a fixed 48px exclusive zone (tiled windows
//! shift right; the desktop never reflows on hover). Shows one row per
//! project with a letter avatar; clicking switches optimistically. When the
//! daemon is down the rail greys out and keeps its footprint.

mod client;

use gtk4 as gtk;

use gtk::glib;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use workspace_proto::ProjectSummary;

use client::{UiRequest, UiUpdate};

/// Collapsed rail width in pixels; also the exclusive zone.
const RAIL_WIDTH: i32 = 48;

const APP_ID: &str = "dev.omarchy.WorkspacePanel";

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

    load_css();

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

    window.set_child(Some(&root));

    let (updates, requests) = client::spawn(None);

    // Rebuild rows whenever the client pushes a fresh project list.
    glib::spawn_future_local(glib::clone!(
        #[weak]
        list,
        #[weak]
        status,
        #[weak]
        root,
        async move {
            while let Ok(update) = updates.recv().await {
                match update {
                    UiUpdate::Projects(projects) => {
                        root.remove_css_class("disconnected");
                        status.set_visible(false);
                        rebuild_rows(&list, &projects, &requests);
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
    projects: &[ProjectSummary],
    requests: &async_channel::Sender<UiRequest>,
) {
    while let Some(row) = list.row_at_index(0) {
        list.remove(&row);
    }
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

        row.set_child(Some(&avatar));
        row.set_tooltip_text(Some(&format!(
            "{} ({} window{})",
            project.name,
            project.windows,
            if project.windows == 1 { "" } else { "s" }
        )));

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

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("../resources/panel.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
