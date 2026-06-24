//! The per-box Shell tab: an embedded VTE terminal running `potjie ssh <box>`.

use super::potjie_cli;
use gtk::glib::clone;
use gtk::prelude::*;
use gtk::{gio, glib, Align, Box as GtkBox, Label, Orientation, ScrolledWindow};
use potjie_core::Vm;
use std::cell::Cell;
use std::rc::Rc;
use vte::prelude::*;

/// Controls for the embedded shell: start/stop the box, idempotent.
#[derive(Clone)]
pub(super) struct ShellCtl {
    pub(super) start: Rc<dyn Fn()>,
    pub(super) stop: Rc<dyn Fn()>,
    /// True while the shell (and thus the box) is running.
    pub(super) active: Rc<dyn Fn() -> bool>,
}

pub(super) fn shell_tab(vm: Rc<Vm>) -> (GtkBox, ShellCtl) {
    let page = GtkBox::new(Orientation::Vertical, 12);
    page.set_margin_top(16);
    page.set_margin_bottom(16);
    page.set_margin_start(16);
    page.set_margin_end(16);

    let state = Label::new(Some(
        "Opening this tab boots the box (you'll be asked for the passphrase below); \
         leaving it re-locks the box.  Copy: Ctrl+Shift+C · Paste: Ctrl+Shift+V",
    ));
    state.set_halign(Align::Start);
    state.set_wrap(true);
    state.add_css_class("dim-label");
    page.append(&state);

    // The embedded VTE terminal. Its PTY child is `potjie ssh <box>`, so the
    // daemon leases the box for exactly the shell's lifetime: the CLI boots it
    // (prompting the passphrase in *this* terminal), gives you the shell, and on
    // exit releases the lease so the daemon re-locks the box.
    let term = vte::Terminal::new();
    term.set_scrollback_lines(10_000);
    term.set_vexpand(true);
    term.set_hexpand(true);
    let scroller = ScrolledWindow::builder()
        .child(&term)
        .vexpand(true)
        .hexpand(true)
        .build();
    page.append(&scroller);

    // Copy/paste: VTE has no default bindings, so wire the usual terminal keys.
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(clone!(
        #[weak] term,
        #[upgrade_or] glib::Propagation::Proceed,
        move |_, keyval, _code, mods| {
            let ctrl_shift = gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::SHIFT_MASK;
            if mods.contains(ctrl_shift) {
                match keyval.to_lower() {
                    gtk::gdk::Key::c => {
                        term.copy_clipboard_format(vte::Format::Text);
                        return glib::Propagation::Stop;
                    }
                    gtk::gdk::Key::v => {
                        term.paste_clipboard();
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }
            glib::Propagation::Proceed
        }
    ));
    term.add_controller(keys);

    let pid: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));

    term.connect_child_exited(clone!(
        #[strong] pid, #[weak] state,
        move |_term, _status| {
            pid.set(None);
            state.set_text("Shell closed. Switch away and back to reconnect.");
        }
    ));

    // start: boot the box in the terminal (no-op if already running).
    let start: Rc<dyn Fn()> = {
        let vm = vm.clone();
        let pid = pid.clone();
        let term = term.clone();
        let state = state.clone();
        Rc::new(move || {
            if pid.get().is_none() {
                state.set_text("Starting box — enter the passphrase below if asked…");
                spawn_shell_in_terminal(&term, &state, &vm.cfg.name, &pid);
                term.grab_focus();
            }
        })
    };

    // stop: SIGHUP the shell's process group (no-op if not running). The CLI
    // process dies, the daemon sees its lease socket close, and re-locks the box.
    let stop: Rc<dyn Fn()> = {
        let pid = pid.clone();
        Rc::new(move || {
            if let Some(p) = pid.take() {
                unsafe { libc::kill(-p, libc::SIGHUP); }
            }
        })
    };

    let active: Rc<dyn Fn() -> bool> = {
        let pid = pid.clone();
        Rc::new(move || pid.get().is_some())
    };

    (page, ShellCtl { start, stop, active })
}

/// Run `potjie ssh <box>` inside the embedded VTE terminal, recording the child
/// pid so the tab can stop it on leave.
fn spawn_shell_in_terminal(
    term: &vte::Terminal,
    state: &Label,
    box_name: &str,
    pid: &Rc<Cell<Option<i32>>>,
) {
    // Clear screen + scrollback so each session starts on a blank slate.
    // Doing this via VTE's API (not escape sequences) is synchronous —
    // VTE's cursor is at (1,1) before the child process runs, preventing the
    // CPR-at-row-N garbage caused by a cursor tracking race on reconnect.
    term.reset(false, true);

    let cli = potjie_cli();
    let cli = cli.to_string_lossy().to_string();
    let argv = [cli.as_str(), "ssh", box_name];

    term.spawn_async(
        vte::PtyFlags::DEFAULT,
        None,                       // inherit the UI's working directory
        &argv,
        &[],                        // inherit the UI's environment
        glib::SpawnFlags::DEFAULT,
        || {},                      // no extra child setup
        -1,                         // no timeout
        gio::Cancellable::NONE,
        clone!(
            #[strong] pid, #[weak] state,
            move |res: Result<glib::Pid, glib::Error>| match res {
                Ok(p) => pid.set(Some(p.0)),
                Err(e) => state.set_text(&format!("Could not start shell: {e}")),
            }
        ),
    );
}
