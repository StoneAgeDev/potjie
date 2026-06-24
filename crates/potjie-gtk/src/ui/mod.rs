//! GTK UI: shared helpers and the main window. Per-box tabs live in submodules
//! (`overview`, `ports`, `shell`, assembled by `detail`); `create` is the new-box
//! form; `launch` is the standalone passphrase prompt.

pub mod launch;

mod create;
mod detail;
mod overview;
mod ports;
mod shell;

use gtk::glib::clone;
use gtk::prelude::*;
use gtk::{
    Align, ApplicationWindow, Box as GtkBox, Button, Expander, HeaderBar, Label, ListBox,
    Orientation, ScrolledWindow, SelectionMode,
};
use gtk::glib;
use potjie_core::{guard, Vm};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

// ---- async + process helpers --------------------------------------------

/// Run `work` on a worker thread; deliver its result to `then` on the main
/// thread. Keeps the UI responsive during downloads/boots/ssh.
pub fn run_async<T, F, G>(work: F, then: G)
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
    G: FnOnce(T) + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(work());
    });
    glib::spawn_future_local(async move {
        if let Ok(v) = rx.recv().await {
            then(v);
        }
    });
}

/// Path to the multicall `potjie` binary (`POTJIE_BIN` → sibling `potjie` →
/// `PATH`). Shared with the daemon-spawn resolver so both agree.
pub fn potjie_cli() -> PathBuf {
    PathBuf::from(potjie_core::tools::potjie_bin())
}

// ---- modal prompts -------------------------------------------------------

/// Brief informational dialog.
pub fn info(parent: &impl IsA<gtk::Window>, title: &str, message: &str) {
    let win = gtk::Window::builder()
        .title(title)
        .transient_for(parent)
        .modal(true)
        .default_width(360)
        .build();
    let vbox = GtkBox::new(Orientation::Vertical, 12);
    vbox.set_margin_top(16);
    vbox.set_margin_bottom(16);
    vbox.set_margin_start(16);
    vbox.set_margin_end(16);
    let label = Label::new(Some(message));
    label.set_wrap(true);
    label.set_halign(Align::Start);
    let ok = Button::with_label("OK");
    ok.set_halign(Align::End);
    ok.connect_clicked(clone!(#[strong] win, move |_| win.close()));
    vbox.append(&label);
    vbox.append(&ok);
    win.set_child(Some(&vbox));
    win.present();
}

// ---- inline y/n confirmation (shared, floats over content) ----------------

/// A single inline y/n confirmation that **floats** over the window content (a
/// `gtk::Overlay` child, so it never reflows or shifts the UI) and is answered
/// from the keyboard. Used to guard *every* action that would stop a running VM.
#[derive(Clone)]
struct Confirm {
    label: Label,
    revealer: gtk::Revealer,
    confirming: Rc<Cell<bool>>,
    on_yes: Rc<RefCell<Option<Box<dyn FnOnce()>>>>,
    on_no: Rc<RefCell<Option<Box<dyn FnOnce()>>>>,
}

impl Confirm {
    /// Build the overlay child widget plus the controller; returns the `Confirm`
    /// and the banner widget to add as an overlay.
    fn new() -> (Self, gtk::Revealer) {
        let bar = GtkBox::new(Orientation::Horizontal, 8);
        bar.add_css_class("osd");
        bar.add_css_class("toolbar");
        let label = Label::new(None);
        label.set_wrap(true);
        label.set_margin_top(10);
        label.set_margin_bottom(10);
        label.set_margin_start(14);
        label.set_margin_end(14);
        bar.append(&label);

        let revealer = gtk::Revealer::new();
        revealer.set_child(Some(&bar));
        revealer.set_reveal_child(false);
        revealer.set_halign(Align::Center);
        revealer.set_valign(Align::End);
        revealer.set_margin_bottom(18);
        revealer.set_transition_type(gtk::RevealerTransitionType::SlideUp);

        let me = Confirm {
            label,
            revealer: revealer.clone(),
            confirming: Rc::new(Cell::new(false)),
            on_yes: Rc::new(RefCell::new(None)),
            on_no: Rc::new(RefCell::new(None)),
        };
        (me, revealer)
    }

    fn is_confirming(&self) -> bool {
        self.confirming.get()
    }

    /// Hide the bar and drop any pending actions without running either.
    fn dismiss(&self) {
        self.confirming.set(false);
        self.revealer.set_reveal_child(false);
        *self.on_yes.borrow_mut() = None;
        *self.on_no.borrow_mut() = None;
    }

    fn ask(&self, markup: &str, yes: Box<dyn FnOnce()>, no: Box<dyn FnOnce()>) {
        self.label.set_markup(markup);
        *self.on_yes.borrow_mut() = Some(yes);
        *self.on_no.borrow_mut() = Some(no);
        self.confirming.set(true);
        self.revealer.set_reveal_child(true);
    }

    fn resolve(&self, yes: bool) {
        if !self.confirming.get() {
            return;
        }
        self.confirming.set(false);
        self.revealer.set_reveal_child(false);
        let y = self.on_yes.borrow_mut().take();
        let n = self.on_no.borrow_mut().take();
        if yes {
            if let Some(y) = y {
                y();
            }
        } else if let Some(n) = n {
            n();
        }
    }
}

/// Standard wording for a guard prompt.
fn guard_markup(name: &str) -> String {
    format!(
        "\u{26a0}  This stops and re-locks the running VM \u{2018}{name}\u{2019}.  \
         Continue?   <b>y</b> = yes   \u{00b7}   <b>n</b> = no",
    )
}

/// Would *our* action actually stop and re-lock the box? True only when it's
/// running and we appear to hold the only lease, so releasing it (by leaving the
/// Shell tab, closing the window, or navigating away) brings the daemon's count
/// to zero. When other clients also hold the box open — e.g. a host-app wrapper
/// like Zed that leased it independently — none of those actions re-lock it, so
/// there is nothing to guard against and we must not prompt.
///
/// While the Shell tab is active its `potjie ssh` child counts as one lease, so
/// any *additional* holder makes `leases > 1` and this returns false.
fn would_relock(name: &str) -> bool {
    guard::status(name).map(|s| s.running && s.leases <= 1).unwrap_or(false)
}

/// Box status, asked of the **daemon** — the single source of truth. The GUI runs
/// in a separate Flatpak sandbox (and PID namespace) from the daemon and qemu, so
/// a direct `Vm::status()` here can't see the box's pidfile or process and would
/// always report "stopped". Routing through the daemon both fixes that and keeps
/// the daemon warm while the app is open (it idle-exits on no activity).
fn box_status(name: &str) -> Option<potjie_core::protocol::BoxStatus> {
    guard::status(name).ok()
}

// ---- main window ---------------------------------------------------------

pub fn build_main_window(app: &adw::Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Potjie")
        .default_width(960)
        .default_height(620)
        .build();

    let header = HeaderBar::new();
    let add_btn = Button::from_icon_name("list-add-symbolic");
    add_btn.set_tooltip_text(Some("Add a box"));
    header.pack_start(&add_btn);
    header.set_title_widget(Some(&Label::new(Some("Potjie"))));
    window.set_titlebar(Some(&header));

    // Sidebar (left) | detail (right).
    let split = GtkBox::new(Orientation::Horizontal, 0);
    let sidebar = ListBox::new();
    sidebar.set_selection_mode(SelectionMode::Single);
    sidebar.set_width_request(240);
    let sidebar_scroll = ScrolledWindow::builder().child(&sidebar).build();
    sidebar_scroll.set_width_request(240);

    let detail = GtkBox::new(Orientation::Vertical, 0);
    detail.set_hexpand(true);
    detail.set_vexpand(true);

    split.append(&sidebar_scroll);
    split.append(&gtk::Separator::new(Orientation::Vertical));
    split.append(&detail);

    // The confirmation bar floats over everything via an Overlay, so revealing it
    // never shifts the layout (which used to throw off mouse aim).
    let (confirm, confirm_banner) = Confirm::new();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&split));
    overlay.add_overlay(&confirm_banner);

    // Host SSH integration gate: if the user's `~/.ssh/config` doesn't yet
    // Include our managed fragment (and they haven't opted out), show an inline
    // setup screen instead of the main UI. Potjie only holds *read-only* access
    // to `~/.ssh/config` — we never edit it for them; they add one line.
    use potjie_core::desktop::IncludeStatus;
    if potjie_core::desktop::ssh_include_status() == IncludeStatus::Missing {
        window.set_child(Some(&build_ssh_gate(&window, &overlay)));
    } else {
        window.set_child(Some(&overlay));
    }

    // Window-level key capture: answer the confirmation with y / n / Esc from
    // anywhere, so you never have to click.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(clone!(
        #[strong] confirm,
        move |_, keyval, _code, _mods| {
            if !confirm.is_confirming() {
                return glib::Propagation::Proceed;
            }
            match keyval.to_lower() {
                gtk::gdk::Key::y => { confirm.resolve(true); glib::Propagation::Stop }
                gtk::gdk::Key::n | gtk::gdk::Key::Escape => {
                    confirm.resolve(false);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        }
    ));
    window.add_controller(keys);

    // Which box's detail panel is currently shown, and a flag to suppress the
    // reentrant row-selected signal when we revert a selection programmatically.
    let current_box: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let suppress_sel = Rc::new(Cell::new(false));

    // Refresh closure rebuilds the sidebar from disk.
    let refresh: Rc<RefCell<Option<Box<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    {
        let sidebar = sidebar.clone();
        let refresh_inner = refresh.clone();
        let f = move || {
            while let Some(child) = sidebar.first_child() {
                sidebar.remove(&child);
            }
            let boxes = Vm::list().unwrap_or_default();
            if boxes.is_empty() {
                let row = Label::new(Some("No boxes yet —\nclick + to add one."));
                row.set_margin_top(20);
                sidebar.append(&row);
            }
            for vm in boxes {
                let running = box_status(&vm.cfg.name).map(|s| s.running).unwrap_or(false);
                let row = GtkBox::new(Orientation::Horizontal, 8);
                row.set_margin_top(8);
                row.set_margin_bottom(8);
                row.set_margin_start(10);
                row.set_margin_end(10);
                let dot = Label::new(Some(if running { "🟢" } else { "⚪" }));
                let name = Label::new(Some(&vm.cfg.name));
                name.set_halign(Align::Start);
                name.set_hexpand(true);
                row.append(&dot);
                row.append(&name);
                let list_row = gtk::ListBoxRow::new();
                list_row.set_child(Some(&row));
                list_row.set_widget_name(&vm.cfg.name);
                sidebar.append(&list_row);
            }
            let _ = &refresh_inner; // keep alive
        };
        *refresh.borrow_mut() = Some(Box::new(f));
    }
    let do_refresh = {
        let refresh = refresh.clone();
        move || {
            if let Some(f) = refresh.borrow().as_ref() {
                f();
            }
        }
    };
    do_refresh();

    // Rebuild the detail panel for `name` (or clear it for None).
    let show_detail: Rc<dyn Fn(Option<String>)> = {
        let detail = detail.clone();
        let window = window.clone();
        let do_refresh = do_refresh.clone();
        let confirm = confirm.clone();
        let current_box = current_box.clone();
        Rc::new(move |name: Option<String>| {
            while let Some(child) = detail.first_child() {
                detail.remove(&child);
            }
            if let Some(name) = name.clone() {
                if let Ok(vm) = Vm::load(&name) {
                    let panel = detail::build_detail(&window, vm, do_refresh.clone(), confirm.clone());
                    detail.append(&panel);
                }
            }
            *current_box.borrow_mut() = name;
        })
    };

    // Re-select the row for the currently-shown box (used to revert a guarded
    // selection the user declined). Sets `suppress_sel` so the resulting
    // row-selected signal is ignored.
    let reselect_current: Rc<dyn Fn()> = {
        let sidebar = sidebar.clone();
        let current_box = current_box.clone();
        let suppress_sel = suppress_sel.clone();
        Rc::new(move || {
            suppress_sel.set(true);
            match current_box.borrow().clone() {
                Some(name) => select_box_row(&sidebar, &name),
                None => sidebar.unselect_all(),
            }
        })
    };

    // Selecting a box rebuilds the detail panel — but first guard against
    // stopping a running box we're navigating away from.
    sidebar.connect_row_selected(clone!(
        #[strong] confirm, #[strong] current_box, #[strong] suppress_sel,
        #[strong] show_detail, #[strong] reselect_current,
        move |_, row| {
            if suppress_sel.replace(false) {
                return; // programmatic revert; ignore
            }
            let target = row.and_then(|r| {
                let n = r.widget_name().to_string();
                (!n.is_empty()).then_some(n)
            });
            if *current_box.borrow() == target {
                return;
            }
            let cur = current_box.borrow().clone();
            let proceed: Box<dyn FnOnce()> = {
                let show_detail = show_detail.clone();
                let target = target.clone();
                Box::new(move || show_detail(target))
            };
            match cur {
                Some(name) if would_relock(&name) => {
                    let cancel: Box<dyn FnOnce()> = {
                        let reselect_current = reselect_current.clone();
                        Box::new(move || reselect_current())
                    };
                    confirm.ask(&guard_markup(&name), proceed, cancel);
                }
                _ => proceed(),
            }
        }
    ));

    // The + button shows an inline "new box" form in the detail panel (no popup
    // windows — this user runs a tiling WM and floating dialogs are disruptive).
    add_btn.connect_clicked(clone!(
        #[strong] detail, #[strong] sidebar, #[strong] do_refresh,
        #[strong] confirm, #[strong] current_box, #[strong] suppress_sel,
        move |_| {
            let open_form: Box<dyn FnOnce()> = {
                let detail = detail.clone();
                let sidebar = sidebar.clone();
                let do_refresh = do_refresh.clone();
                let current_box = current_box.clone();
                let suppress_sel = suppress_sel.clone();
                Box::new(move || {
                    suppress_sel.set(true);
                    sidebar.unselect_all();
                    while let Some(child) = detail.first_child() {
                        detail.remove(&child);
                    }
                    detail.append(&create::create_box_form(&sidebar, do_refresh.clone()));
                    *current_box.borrow_mut() = None;
                })
            };
            let cur = current_box.borrow().clone();
            match cur {
                Some(name) if would_relock(&name) => {
                    confirm.ask(&guard_markup(&name), open_form, Box::new(|| {}));
                }
                _ => open_form(),
            }
        }
    ));

    // Closing the window while a box is running is also guarded.
    window.connect_close_request(clone!(
        #[strong] confirm, #[strong] current_box,
        move |w| {
            if confirm.is_confirming() {
                return glib::Propagation::Stop;
            }
            let cur = current_box.borrow().clone();
            match cur {
                Some(name) if would_relock(&name) => {
                    let w = w.clone();
                    confirm.ask(
                        &format!(
                            "\u{26a0}  Closing stops and re-locks the running VM \
                             \u{2018}{name}\u{2019}.  Close anyway?   <b>y</b> = yes   \
                             \u{00b7}   <b>n</b> = no"
                        ),
                        Box::new(move || w.destroy()),
                        Box::new(|| {}),
                    );
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        }
    ));

    // Live-update the sidebar status dots in place from daemon truth. We
    // deliberately do NOT rebuild the sidebar here (that would drop the selection
    // and tear down the detail panel, unmapping and thus stopping the Shell tab we
    // just started). Start/stop *notifications* are fired by the daemon, not here,
    // so they happen whatever the trigger (CLI, wrapper) and even with no GUI open.
    start_sidebar_poller(&sidebar);

    window.present();
}

/// Inline setup screen shown when `~/.ssh/config` doesn't yet Include Potjie's
/// managed SSH fragment. Potjie only has *read-only* access to that file, so we
/// can't (and won't) edit it — the user pastes one line. Resolving the gate (or
/// the deliberately-buried skip) swaps the real UI (`overlay`) into the window.
fn build_ssh_gate(window: &ApplicationWindow, overlay: &gtk::Overlay) -> GtkBox {
    let line = potjie_core::desktop::ssh_include_line().unwrap_or_default();
    let cfg = potjie_core::desktop::user_ssh_config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.ssh/config".into());

    let outer = GtkBox::new(Orientation::Vertical, 14);
    outer.set_halign(Align::Center);
    outer.set_valign(Align::Center);
    outer.set_margin_top(32);
    outer.set_margin_bottom(32);
    outer.set_margin_start(32);
    outer.set_margin_end(32);
    outer.set_width_request(560);

    let title = Label::new(Some("One-time setup: let host tools reach your boxes"));
    title.add_css_class("title-2");
    title.set_halign(Align::Start);
    title.set_wrap(true);

    let body = Label::new(Some(
        "So terminal \u{2018}ssh\u{2019} and VS Code Remote-SSH can reach a box by a \
         stable name (\u{2018}potjie-<box>\u{2019}), one line needs to go at the top of \
         your SSH config. Potjie only has read-only access to that file and will \
         never edit it for you \u{2014} add the line yourself, then continue:",
    ));
    body.set_halign(Align::Start);
    body.set_wrap(true);
    body.set_xalign(0.0);

    // The line to paste, in a selectable monospace row with a Copy button.
    let line_row = GtkBox::new(Orientation::Horizontal, 8);
    line_row.set_halign(Align::Start);
    let line_label = Label::new(Some(&line));
    line_label.add_css_class("monospace");
    line_label.set_selectable(true);
    line_label.set_halign(Align::Start);
    line_label.set_wrap(true);
    line_label.set_xalign(0.0);
    let copy_btn = Button::from_icon_name("edit-copy-symbolic");
    copy_btn.set_tooltip_text(Some("Copy"));
    copy_btn.set_valign(Align::Center);
    line_row.append(&line_label);
    line_row.append(&copy_btn);

    let where_ = Label::new(Some(&format!("Add it to:  {cfg}")));
    where_.add_css_class("dim-label");
    where_.set_halign(Align::Start);
    where_.set_wrap(true);
    where_.set_xalign(0.0);

    let status = Label::new(None);
    status.add_css_class("error");
    status.set_halign(Align::Start);
    status.set_wrap(true);
    status.set_xalign(0.0);

    let continue_btn = Button::with_label("I've added it \u{2014} continue");
    continue_btn.add_css_class("suggested-action");
    continue_btn.set_halign(Align::Start);

    // Deliberately buried escape hatch: skip touching the SSH config entirely.
    let skip_exp = Expander::new(Some("Potjie not working for you?"));
    let skip_box = GtkBox::new(Orientation::Vertical, 8);
    skip_box.set_margin_top(8);
    skip_box.set_margin_start(8);
    let skip_warn = Label::new(Some(
        "You can skip this, but Potjie is half-broken without it: host tools \
         won't resolve \u{2018}potjie-<box>\u{2019}, so VS Code Remote-SSH and \
         \u{2018}ssh potjie-…\u{2019} won't work. Boxes and the built-in Shell tab \
         still run. You can re-enable this any time by adding the line above.",
    ));
    skip_warn.set_wrap(true);
    skip_warn.set_xalign(0.0);
    skip_warn.add_css_class("dim-label");
    let skip_btn = Button::with_label("Skip \u{2014} leave my SSH config alone");
    skip_btn.add_css_class("destructive-action");
    skip_btn.set_halign(Align::Start);
    skip_box.append(&skip_warn);
    skip_box.append(&skip_btn);
    skip_exp.set_child(Some(&skip_box));

    outer.append(&title);
    outer.append(&body);
    outer.append(&line_row);
    outer.append(&where_);
    outer.append(&status);
    outer.append(&continue_btn);
    outer.append(&skip_exp);

    // Reveal the real UI by swapping the window's child.
    let reveal: Rc<dyn Fn()> = {
        let window = window.clone();
        let overlay = overlay.clone();
        Rc::new(move || window.set_child(Some(&overlay)))
    };

    copy_btn.connect_clicked(clone!(
        #[strong] line,
        move |btn| btn.clipboard().set_text(&line)
    ));

    continue_btn.connect_clicked(clone!(
        #[strong] reveal, #[strong] status,
        move |_| {
            use potjie_core::desktop::IncludeStatus;
            if potjie_core::desktop::ssh_include_status() == IncludeStatus::Missing {
                status.set_text(
                    "Still not found in your SSH config. Make sure you saved the file \
                     with the exact line above. (If you just created ~/.ssh/config, \
                     restart Potjie so it can see the new file.)",
                );
            } else {
                reveal();
            }
        }
    ));

    skip_btn.connect_clicked(clone!(
        #[strong] reveal,
        move |_| {
            let _ = potjie_core::desktop::set_ssh_include_optout(true);
            reveal();
        }
    ));

    outer
}

/// Poll box running-state once a second and live-update each sidebar status dot
/// in place.
fn start_sidebar_poller(sidebar: &ListBox) {
    let sidebar = sidebar.clone();
    glib::timeout_add_seconds_local(1, move || {
        // One daemon round-trip for all boxes (the daemon is authoritative across
        // the sandbox boundary, and polling it keeps it warm while the app is open).
        for st in guard::list().unwrap_or_default() {
            set_sidebar_dot(&sidebar, &st.name, st.running);
        }
        glib::ControlFlow::Continue
    });
}

/// Update the running/stopped dot for `name` in place (no sidebar rebuild).
fn set_sidebar_dot(sidebar: &ListBox, name: &str, running: bool) {
    let mut i = 0;
    while let Some(row) = sidebar.row_at_index(i) {
        if row.widget_name() == name {
            if let Some(rowbox) = row.child() {
                if let Some(dot) = rowbox.first_child().and_downcast::<Label>() {
                    dot.set_text(if running { "🟢" } else { "⚪" });
                }
            }
            return;
        }
        i += 1;
    }
}

/// Select the sidebar row for `name` (which triggers the detail rebuild).
fn select_box_row(sidebar: &ListBox, name: &str) {
    let mut i = 0;
    while let Some(row) = sidebar.row_at_index(i) {
        if row.widget_name() == name {
            sidebar.select_row(Some(&row));
            return;
        }
        i += 1;
    }
}
