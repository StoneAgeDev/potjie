//! Wrapper mode: `potjie-gtk --launch <box> <kind> <app-id>`.
//!
//! Invoked by generated `.desktop` files. It *leases* the box from the guard
//! daemon for exactly as long as the wrapped app runs, then drops the lease so
//! the daemon re-locks the box. A crash of this launcher also re-locks the box
//! (the lease is a live socket).
//!
//! The wrapped app runs *inside* the box (guest) in the foreground over
//! X-forwarded SSH, so this process lasts exactly as long as the app does.

use super::run_async;
use gtk::glib::{self, clone};
use gtk::prelude::*;
use gtk::{
    Align, ApplicationWindow, Box as GtkBox, Button, Entry, Label, Orientation, ScrolledWindow,
    TextView,
};
use potjie_core::desktop::Kind;
use potjie_core::{guard, Vm};
use std::cell::Cell;
use std::rc::Rc;

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
        .default_width(480)
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
            launch_with(&parent, vm.clone(), box_name.clone(), kind, app_id.clone(), pass, hold);
        }
    );

    let hold_cell = Rc::new(std::cell::RefCell::new(Some(hold)));
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
/// When the box needs to boot, the window stays visible and streams the
/// console log so the user can see boot progress instead of a blank wait.
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

    // If already running, nothing to show — hide immediately.
    let needs_boot = !guard::status(&box_name).map(|s| s.running).unwrap_or(false)
        && !pass.is_empty();

    if !needs_boot {
        parent.set_visible(false);
        run_async(
            move || {
                let lease = guard::acquire(&box_name, &pass).map_err(|e| e.to_string())?;
                let status = match kind {
                    Kind::Vm => run_guest_app(&vm, &app_id),
                };
                drop(lease);
                status
            },
            move |res: Result<(), String>| {
                parent.close(); // release the window's hold so the app can quit
                if let Err(e) = res {
                    eprintln!("potjie launch error: {e}");
                }
                drop(hold);
            },
        );
        return;
    }

    // Box needs to boot: show a log view so the user can see progress.
    let log_label = Label::new(Some(&format!("Starting '{box_name}'…")));
    log_label.set_halign(Align::Start);

    let logview = TextView::new();
    logview.set_editable(false);
    logview.set_monospace(true);
    logview.set_cursor_visible(false);
    let logscroll = ScrolledWindow::builder()
        .child(&logview)
        .min_content_height(200)
        .vexpand(true)
        .build();
    logscroll.add_css_class("card");

    let vbox = GtkBox::new(Orientation::Vertical, 10);
    vbox.set_margin_top(14);
    vbox.set_margin_bottom(14);
    vbox.set_margin_start(14);
    vbox.set_margin_end(14);
    vbox.append(&log_label);
    vbox.append(&logscroll);
    parent.set_child(Some(&vbox));
    parent.present();

    // Stream the console log file into the TextView every 200 ms.
    let console_log = vm.paths.console_log();
    let pos = Rc::new(Cell::new(
        std::fs::metadata(&console_log).map(|m| m.len()).unwrap_or(0),
    ));
    let streaming = Rc::new(Cell::new(true));

    glib::timeout_add_local(
        std::time::Duration::from_millis(200),
        clone!(
            #[weak(rename_to = lv)] logview,
            #[strong] pos,
            #[strong] streaming,
            #[upgrade_or] glib::ControlFlow::Break,
            move || {
                if !streaming.get() {
                    glib::ControlFlow::Break
                } else {
                    let new_pos = read_console_into(&lv, &console_log, pos.get());
                    pos.set(new_pos);
                    glib::ControlFlow::Continue
                }
            }
        ),
    );

    // Signal: worker sends () the moment the lease is acquired (box is up, SSH
    // is ready). The main thread hides the boot log window immediately so the
    // user sees the app without the boot log sitting on top of it.
    let (booted_tx, booted_rx) = async_channel::bounded::<()>(1);
    glib::spawn_future_local(clone!(
        #[strong] parent, #[strong] streaming,
        async move {
            if booted_rx.recv().await.is_ok() {
                streaming.set(false);
                parent.set_visible(false);
            }
        }
    ));

    // Acquire the lease (boots the box) on a worker thread.
    let streaming2 = streaming.clone();
    run_async(
        move || {
            let lease = guard::acquire(&box_name, &pass).map_err(|e| e.to_string())?;
            // Box is up and SSH is ready — dismiss the boot log window now.
            let _ = booted_tx.send_blocking(());
            let status = match kind {
                Kind::Vm => run_guest_app(&vm, &app_id),
            };
            drop(lease);
            status
        },
        move |res: Result<(), String>| {
            // Stop the poller and destroy the window (safe even if already hidden).
            streaming2.set(false);
            parent.close();
            if let Err(e) = res {
                eprintln!("potjie launch error: {e}");
            }
            drop(hold);
        },
    );
}

/// Append any new bytes from `path` since `pos` into `view`; return new pos.
/// Resets to 0 if the file was truncated (fresh boot overwrote it).
fn read_console_into(view: &TextView, path: &std::path::Path, mut pos: u64) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    if let Ok(meta) = std::fs::metadata(path) {
        let len = meta.len();
        if len < pos {
            pos = 0;
        }
        if len > pos {
            if let Ok(mut f) = std::fs::File::open(path) {
                if f.seek(SeekFrom::Start(pos)).is_ok() {
                    let mut buf = Vec::new();
                    if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                        let text = String::from_utf8_lossy(&buf);
                        let buf = view.buffer();
                        let mut end = buf.end_iter();
                        buf.insert(&mut end, &text);
                        let mut end = buf.end_iter();
                        view.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
                        pos += text.len() as u64;
                    }
                }
            }
        }
    }
    pos
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
