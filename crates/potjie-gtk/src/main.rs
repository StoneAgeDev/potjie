//! potjie-gtk — the GTK4 front-end for Potjie.
//!
//! Two modes:
//!   * normal: a window with a sidebar of boxes and per-box Overview / Apps /
//!     Shell tabs (the `+` button creates a box).
//!   * `--launch <box> <app-id>`: headless wrapper mode invoked by generated
//!     `.desktop` files — prompt for the passphrase, boot the box, run the
//!     guest app with X forwarding, then stop the box when it exits.
//!
//! All long operations run on a worker thread via `gio::spawn_blocking` so the
//! UI never blocks; results come back on the main context.
//!
//! The app is an `adw::Application` (libadwaita): it follows the system
//! light/dark preference automatically via the XDG settings portal, and
//! libadwaita's stylesheet is built into the library — so dark mode works even
//! bundled in an AppImage that can't see the host's GTK theme files.

mod ui;

use adw::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "com.potjie.Potjie";

fn main() -> glib::ExitCode {
    // Wrapper mode: `potjie-gtk --launch <box> <app-id>`.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--launch") {
        // NON_UNIQUE: each `--launch` wrapper is an independent headless process.
        // Without this it would register the unique APP_ID bus name and become the
        // "primary" instance — a lingering wrapper then steals activations from the
        // real GUI (and other wrappers), so the main window never opens.
        let app = adw::Application::builder()
            .application_id(APP_ID)
            .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE | gio::ApplicationFlags::NON_UNIQUE)
            .build();
        let argv = args.clone();
        app.connect_command_line(move |app, _| {
            ui::launch::run(app, &argv);
            0
        });
        return app.run_with_args(&args);
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(ui::build_main_window);
    app.run_with_args(&args)
}
