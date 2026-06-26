//! potjie-gtk — the GTK4 front-end for Potjie.
//!
//! Two modes:
//!   * normal: a window with a sidebar of boxes and per-box Overview / Ports /
//!     Shell tabs (the `+` button creates a box).
//!   * `--ask-passphrase <box>`: a single passphrase prompt the guard daemon
//!     spawns when an ssh connection boots a locked box (no terminal to ask at).
//!
//! All long operations run on a worker thread via `gio::spawn_blocking` so the
//! UI never blocks; results come back on the main context.
//!
//! The app is an `adw::Application` (libadwaita): it follows the system
//! light/dark preference automatically via the XDG settings portal, and
//! libadwaita's stylesheet is built into the library — so dark mode works even
//! bundled in a sandbox that can't see the host's GTK theme files.

mod ui;

use adw::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "io.github.StoneAgeDev.potjie";

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Passphrase-prompt mode: `potjie-gtk --ask-passphrase <box>`. Spawned by the
    // guard daemon when an ssh connection boots a locked box (no terminal to
    // prompt at). Prints the passphrase to stdout on submit, exits non-zero on
    // cancel. NON_UNIQUE so it never defers to a running main GUI instance.
    if args.get(1).map(String::as_str) == Some("--ask-passphrase") {
        let Some(box_name) = args.get(2).cloned() else {
            eprintln!("usage: potjie-gtk --ask-passphrase <box>");
            return glib::ExitCode::FAILURE;
        };
        let app = adw::Application::builder()
            .application_id(APP_ID)
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.connect_activate(move |app| ui::launch::ask_passphrase(app, &box_name));
        // Pass only argv[0]; the box name is captured above, not parsed by GIO.
        return app.run_with_args(&args[..1]);
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(ui::build_main_window);
    app.run_with_args(&args)
}
