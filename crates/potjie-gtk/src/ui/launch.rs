//! Wrapper mode: `potjie-gtk --launch <box> <host|vm> <app-id>`.
//!
//! Invoked by generated `.desktop` files. It *leases* the box from the guard
//! daemon for exactly as long as the wrapped app runs, then drops the lease so
//! the daemon re-locks the box. A crash of this launcher also re-locks the box
//! (the lease is a live socket).
//!
//!   * `vm`   — run the guest app in the foreground over X-forwarded SSH.
//!   * `host` — run a native host app in the foreground; it reaches the box over
//!     local SSH via the `potjie-<box>` alias (kept current by the daemon).

use super::{info, run_async};
use gtk::glib::clone;
use gtk::prelude::*;
use gtk::{Align, ApplicationWindow, Box as GtkBox, Button, Entry, Label, Orientation};
use potjie_core::desktop::Kind;
use potjie_core::{desktop, guard, Vm};

pub fn run(app: &adw::Application, argv: &[String]) {
    let (Some(box_name), Some(kind_s), Some(app_id)) =
        (argv.get(2).cloned(), argv.get(3).cloned(), argv.get(4).cloned())
    else {
        eprintln!("usage: potjie-gtk --launch <box> <host|vm> <app-id>");
        return;
    };
    let Some(kind) = Kind::parse(&kind_s) else {
        eprintln!("unknown launch kind '{kind_s}' (expected host|vm)");
        return;
    };

    let hold = app.hold();

    let vm = match Vm::load(&box_name) {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("potjie: {e}");
            return;
        }
    };

    let parent = ApplicationWindow::builder()
        .application(app)
        .title("Potjie")
        .default_width(360)
        .build();

    let running = guard::status(&box_name).map(|s| s.running).unwrap_or(false);
    if running {
        parent.set_visible(false);
        launch_with(&parent, vm, box_name, kind, app_id, String::new(), hold);
        return;
    }

    // Ask for the passphrase, then acquire + run.
    let vbox = GtkBox::new(Orientation::Vertical, 12);
    vbox.set_margin_top(16);
    vbox.set_margin_bottom(16);
    vbox.set_margin_start(16);
    vbox.set_margin_end(16);
    let label = Label::new(Some(&format!("Unlock box '{box_name}' to launch the app:")));
    label.set_wrap(true);
    label.set_halign(Align::Start);
    let entry = Entry::new();
    entry.set_visibility(false);
    entry.set_activates_default(true);
    let go = Button::with_label("Unlock & launch");
    go.add_css_class("suggested-action");
    go.set_halign(Align::End);
    vbox.append(&label);
    vbox.append(&entry);
    vbox.append(&go);
    parent.set_child(Some(&vbox));

    let do_launch = clone!(
        #[strong] parent, #[strong] entry, #[strong] vm,
        #[strong] box_name, #[strong] app_id,
        move |hold: gtk::gio::ApplicationHoldGuard| {
            let pass = entry.text().to_string();
            if pass.is_empty() {
                return;
            }
            parent.set_visible(false);
            launch_with(&parent, vm.clone(), box_name.clone(), kind, app_id.clone(), pass, hold);
        }
    );

    let hold_cell = std::rc::Rc::new(std::cell::RefCell::new(Some(hold)));
    go.connect_clicked(clone!(
        #[strong] hold_cell, #[strong] do_launch,
        move |_| if let Some(h) = hold_cell.borrow_mut().take() { do_launch(h); }
    ));
    entry.connect_activate(clone!(
        #[strong] hold_cell, #[strong] do_launch,
        move |_| if let Some(h) = hold_cell.borrow_mut().take() { do_launch(h); }
    ));

    parent.present();
    entry.grab_focus();
}

/// Acquire the lease, run the app in the foreground, then release.
fn launch_with(
    parent: &ApplicationWindow,
    vm: Vm,
    box_name: String,
    kind: Kind,
    app_id: String,
    pass: String,
    hold: gtk::gio::ApplicationHoldGuard,
) {
    let parent = parent.clone();
    run_async(
        move || {
            let lease = guard::acquire(&box_name, &pass).map_err(|e| e.to_string())?;
            let status = match kind {
                Kind::Vm => run_guest_app(&vm, &app_id),
                Kind::Host => run_host_app(&box_name, &vm, lease.ssh_port, &app_id),
            };
            drop(lease); // release the lease -> daemon re-locks the box
            status
        },
        move |res: Result<(), String>| {
            if let Err(e) = res {
                info(&parent, "App exited with an error", &e);
            }
            drop(hold); // last hold released -> application quits
        },
    );
}

/// Run a guest app in the foreground over X-forwarded SSH so this call lasts
/// exactly as long as the app does.
fn run_guest_app(vm: &Vm, app_id: &str) -> Result<(), String> {
    let remote = format!(
        "f=$(ls /usr/share/applications/{id}.desktop \
             $HOME/.local/share/applications/{id}.desktop 2>/dev/null | head -1); \
         [ -n \"$f\" ] || {{ echo 'app not found' >&2; exit 1; }}; \
         cmd=$(sed -n 's/^Exec=//p' \"$f\" | head -1 | sed 's/%[A-Za-z]//g'); \
         exec sh -c \"$cmd\"",
        id = app_id
    );
    let mut cmd = vm.ssh_command_x11(Some(&remote)).map_err(|e| e.to_string())?;
    cmd.status().map_err(|e| e.to_string()).map(|_| ())
}

/// Run a native host app, with the box reachable as `potjie-<box>` (and via
/// `POTJIE_SSH_*` env vars). The app is run through `potjie __run-tracked`, which
/// holds on until the app's **entire descendant tree** exits — so an app that
/// forks/daemonizes and returns (VS Code, browsers, …) keeps the box up for its
/// real lifetime instead of for a few milliseconds.
fn run_host_app(box_name: &str, vm: &Vm, port: u16, app_id: &str) -> Result<(), String> {
    let exec = desktop::resolve_host_exec(app_id)
        .ok_or_else(|| format!("host app '{app_id}' not found"))?;
    let mut cmd = std::process::Command::new(super::potjie_cli());
    cmd.arg("__run-tracked")
        .arg("--")
        .arg(&exec)
        .env("POTJIE_BOX", box_name)
        .env("POTJIE_SSH_ALIAS", format!("potjie-{box_name}"))
        .env("POTJIE_SSH_HOST", "127.0.0.1")
        .env("POTJIE_SSH_PORT", port.to_string())
        .env("POTJIE_SSH_USER", &vm.cfg.username);
    cmd.status().map_err(|e| e.to_string()).map(|_| ())
}
