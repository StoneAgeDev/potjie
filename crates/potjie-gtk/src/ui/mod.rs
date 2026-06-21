//! GTK UI: shared helpers, the main window, and per-box detail tabs.

pub mod launch;

use gtk::glib::clone;
use gtk::prelude::*;
use gtk::{
    gio, glib, Align, ApplicationWindow, Box as GtkBox, Button, DropDown, Entry, Expander,
    HeaderBar, Label, ListBox, Notebook, Orientation, ScrolledWindow, SelectionMode, Spinner,
    SpinButton, TextView,
};
use vte::prelude::*;
use potjie_core::config::{Forward, ForwardDirection};
use potjie_core::desktop::{DesktopEntry, Kind};
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

/// Path to embed in generated `.desktop` launchers as the thing to re-exec. Under
/// an AppImage, `current_exe()` is the per-launch FUSE mountpoint (gone next run),
/// so prefer `$APPIMAGE` — the stable path of the `.AppImage` file itself, which
/// AppRun re-enters as this GUI binary.
pub fn launcher_path() -> PathBuf {
    if let Ok(p) = std::env::var("APPIMAGE") {
        return PathBuf::from(p);
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("potjie-gtk"))
}

// ---- modal prompts -------------------------------------------------------

/// Show a modal single-field prompt. `on_done` is called exactly once with the
/// entered text, or `None` if cancelled.
pub fn prompt_text(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    message: &str,
    secret: bool,
    on_done: impl FnOnce(Option<String>) + 'static,
) {
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
    label.set_halign(Align::Start);
    label.set_wrap(true);
    let entry = Entry::new();
    entry.set_visibility(!secret);
    entry.set_activates_default(true);

    let buttons = GtkBox::new(Orientation::Horizontal, 8);
    buttons.set_halign(Align::End);
    let cancel = Button::with_label("Cancel");
    let ok = Button::with_label("OK");
    ok.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&ok);

    vbox.append(&label);
    vbox.append(&entry);
    vbox.append(&buttons);
    win.set_child(Some(&vbox));

    // Ensure on_done runs exactly once.
    let cell: Rc<RefCell<Option<Box<dyn FnOnce(Option<String>)>>>> =
        Rc::new(RefCell::new(Some(Box::new(on_done))));

    let fire = {
        let cell = cell.clone();
        move |val: Option<String>| {
            if let Some(cb) = cell.borrow_mut().take() {
                cb(val);
            }
        }
    };

    // NB: fire(Some(..)) *before* win.close(), because close() emits
    // close-request which calls fire(None); whoever fires first wins the cell.
    ok.connect_clicked(clone!(
        #[strong] entry,
        #[strong] win,
        #[strong] fire,
        move |_| {
            let text = entry.text().to_string();
            fire(Some(text));
            win.close();
        }
    ));
    entry.connect_activate(clone!(
        #[strong] win,
        #[strong] fire,
        move |e| {
            let text = e.text().to_string();
            fire(Some(text));
            win.close();
        }
    ));
    cancel.connect_clicked(clone!(
        #[strong] win,
        move |_| win.close()
    ));
    win.connect_close_request(clone!(
        #[strong] fire,
        move |_| {
            fire(None);
            glib::Propagation::Proceed
        }
    ));

    win.present();
    entry.grab_focus();
}

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
                let running = vm.status().map(|s| s.running).unwrap_or(false);
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
                    let panel = build_detail(&window, vm, do_refresh.clone(), confirm.clone());
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
                    detail.append(&create_box_form(&sidebar, do_refresh.clone()));
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
        for vm in Vm::list().unwrap_or_default() {
            let running = vm.status().map(|s| s.running).unwrap_or(false);
            set_sidebar_dot(&sidebar, &vm.cfg.name, running);
        }
        glib::ControlFlow::Continue
    });
}

/// Inline new-box form rendered in the detail panel.
fn create_box_form(sidebar: &ListBox, do_refresh: impl Fn() + Clone + 'static) -> GtkBox {
    let page = GtkBox::new(Orientation::Vertical, 12);
    page.set_margin_top(16);
    page.set_margin_bottom(16);
    page.set_margin_start(16);
    page.set_margin_end(16);

    let title = Label::new(None);
    title.set_markup("<span size='x-large' weight='bold'>New box</span>");
    title.set_halign(Align::Start);
    page.append(&title);

    let defaults = potjie_core::BoxConfig::new("x");
    let grid = gtk::Grid::new();
    grid.set_row_spacing(8);
    grid.set_column_spacing(12);
    let name = Entry::new();
    name.set_hexpand(true);
    name.set_placeholder_text(Some("e.g. dev"));
    let pass = Entry::new();
    pass.set_visibility(false);
    pass.set_hexpand(true);
    let pass2 = Entry::new();
    pass2.set_visibility(false);
    pass2.set_hexpand(true);
    // Spec controls. (min, max, step, default)
    let cpus = SpinButton::with_range(1.0, 64.0, 1.0);
    cpus.set_value(defaults.cpus as f64);
    cpus.set_hexpand(true);
    let memory = SpinButton::with_range(256.0, 131072.0, 256.0);
    memory.set_value(defaults.memory_mib as f64);
    memory.set_hexpand(true);
    let disk = SpinButton::with_range(2.0, 2048.0, 1.0);
    disk.set_value(defaults.disk_gib as f64);
    disk.set_hexpand(true);

    let text_rows: [(&str, &gtk::Widget); 3] = [
        ("Name", name.upcast_ref()),
        ("LUKS passphrase", pass.upcast_ref()),
        ("Repeat passphrase", pass2.upcast_ref()),
    ];
    let spec_rows: [(&str, &gtk::Widget); 3] = [
        ("vCPUs", cpus.upcast_ref()),
        ("Memory (MiB)", memory.upcast_ref()),
        ("Disk (GiB)", disk.upcast_ref()),
    ];
    for (i, (k, w)) in text_rows.iter().chain(spec_rows.iter()).enumerate() {
        let key = Label::new(Some(k));
        key.set_halign(Align::Start);
        key.add_css_class("dim-label");
        grid.attach(&key, 0, i as i32, 1, 1);
        grid.attach(*w, 1, i as i32, 1, 1);
    }
    page.append(&grid);

    let status_row = GtkBox::new(Orientation::Horizontal, 8);
    let spinner = Spinner::new();
    let status = Label::new(None);
    status.set_halign(Align::Start);
    status.set_wrap(true);
    status_row.append(&spinner);
    status_row.append(&status);
    page.append(&status_row);

    // Live creation log (base-image download progress, etc.).
    let logview = TextView::new();
    logview.set_editable(false);
    logview.set_monospace(true);
    logview.set_cursor_visible(false);
    let logscroll = ScrolledWindow::builder()
        .child(&logview)
        .min_content_height(160)
        .vexpand(true)
        .margin_top(4)
        .build();
    logscroll.add_css_class("card");
    page.append(&logscroll);

    let create = Button::with_label("Create box");
    create.add_css_class("suggested-action");
    create.set_halign(Align::Start);
    page.append(&create);

    create.connect_clicked(clone!(
        #[strong] name, #[strong] pass, #[strong] pass2, #[strong] cpus,
        #[strong] memory, #[strong] disk, #[strong] spinner, #[strong] status,
        #[strong] create, #[strong] logview, #[strong] sidebar, #[strong] do_refresh,
        move |_| {
            let n = name.text().trim().to_string();
            let p = pass.text().to_string();
            let p2 = pass2.text().to_string();
            status.remove_css_class("error");
            if n.is_empty() {
                status.add_css_class("error");
                status.set_text("Please enter a name.");
                return;
            }
            if p.is_empty() || p != p2 {
                status.add_css_class("error");
                status.set_text("Passphrases are empty or do not match.");
                return;
            }
            create.set_sensitive(false);
            spinner.start();
            status.set_text("Creating box…");
            logview.buffer().set_text("");

            let mut cfg = potjie_core::BoxConfig::new(&n);
            cfg.cpus = cpus.value_as_int() as u32;
            cfg.memory_mib = memory.value_as_int() as u32;
            cfg.disk_gib = disk.value_as_int() as u32;
            append_log(&logview, &format!(
                "Creating '{}' — {} vCPU, {} MiB RAM, {} GiB LUKS disk.",
                n, cfg.cpus, cfg.memory_mib, cfg.disk_gib,
            ));

            // Stream progress lines from the worker into the log.
            let (ptx, prx) = async_channel::unbounded::<String>();
            glib::spawn_future_local(clone!(#[weak] logview, async move {
                while let Ok(line) = prx.recv().await {
                    append_log(&logview, &line);
                }
            }));

            let created_name = n.clone();
            run_async(
                move || {
                    let _ = ptx.send_blocking("Fetching and verifying base image…".into());
                    let mut last = 0u64;
                    let r = Vm::create(cfg, &p, |done, total| {
                        if done.saturating_sub(last) >= 16 << 20 || (total != 0 && done == total) {
                            last = done;
                            let _ = ptx.send_blocking(if total != 0 {
                                format!("  base image: {} / {} MiB", done >> 20, total >> 20)
                            } else {
                                format!("  base image: {} MiB", done >> 20)
                            });
                        }
                    })
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                    if r.is_ok() {
                        let _ = ptx.send_blocking("Encrypting disk and writing cloud-init seed… done.".into());
                    }
                    r
                    // ptx drops here → progress consumer loop ends.
                },
                clone!(#[strong] spinner, #[strong] status, #[strong] create,
                    #[strong] logview, #[strong] sidebar, #[strong] do_refresh,
                    move |res: Result<(), String>| {
                        spinner.stop();
                        create.set_sensitive(true);
                        match res {
                            Ok(()) => {
                                append_log(&logview, "Box created.");
                                do_refresh();
                                select_box_row(&sidebar, &created_name);
                            }
                            Err(e) => {
                                status.add_css_class("error");
                                status.set_text(&format!("Could not create box: {e}"));
                                append_log(&logview, &format!("FAILED: {e}"));
                            }
                        }
                    }),
            );
        }
    ));

    page
}

/// Append a line to a log `TextView` and scroll to the bottom.
fn append_log(view: &TextView, line: &str) {
    let buf = view.buffer();
    let mut end = buf.end_iter();
    let text = if buf.char_count() == 0 {
        line.to_string()
    } else {
        format!("\n{line}")
    };
    buf.insert(&mut end, &text);
    let mut end = buf.end_iter();
    view.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
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

// ---- per-box detail ------------------------------------------------------

/// Controls for the embedded shell: start/stop the box, idempotent.
#[derive(Clone)]
struct ShellCtl {
    start: Rc<dyn Fn()>,
    stop: Rc<dyn Fn()>,
    /// True while the shell (and thus the box) is running.
    active: Rc<dyn Fn() -> bool>,
}

const SHELL_PAGE: u32 = 3;

fn build_detail(
    window: &ApplicationWindow,
    vm: Vm,
    do_refresh: impl Fn() + Clone + 'static,
    confirm: Confirm,
) -> GtkBox {
    let vm = Rc::new(vm);
    let wrapper = GtkBox::new(Orientation::Vertical, 0);
    wrapper.set_hexpand(true);
    wrapper.set_vexpand(true);

    let nb = Notebook::new();
    nb.set_margin_top(8);
    nb.set_margin_bottom(8);
    nb.set_margin_start(8);
    nb.set_margin_end(8);
    nb.set_vexpand(true);
    wrapper.append(&nb);

    let (overview_page, refresh_overview) =
        overview_tab(window, vm.clone(), do_refresh.clone(), confirm.clone());
    let (shell_page, shell) = shell_tab(vm.clone());

    nb.append_page(&overview_page, Some(&Label::new(Some("Overview"))));
    nb.append_page(&apps_tab(window, vm.clone()), Some(&Label::new(Some("Apps"))));
    nb.append_page(&ports_tab(vm.clone()), Some(&Label::new(Some("Ports"))));
    nb.append_page(&shell_page, Some(&Label::new(Some("Shell"))));

    // The box runs only while the Shell tab is current. Switching away while it's
    // running asks first (shared inline y/n bar) before stopping. We re-enter
    // switch_page when reverting, so guard against asking twice.
    nb.connect_switch_page(clone!(
        #[strong] shell, #[strong] refresh_overview, #[strong] confirm, #[strong] vm,
        #[weak] nb,
        move |_, _, page_num| {
            if page_num == SHELL_PAGE {
                // Returning to the shell cancels any pending "stop the VM?" prompt.
                confirm.dismiss();
                (shell.start)();
            } else if (shell.active)() && !confirm.is_confirming() {
                let stop_now: Box<dyn FnOnce()> = {
                    let shell = shell.clone();
                    let refresh_overview = refresh_overview.clone();
                    Box::new(move || {
                        (shell.stop)();
                        refresh_overview();
                        let refresh_overview = refresh_overview.clone();
                        glib::timeout_add_local_once(
                            std::time::Duration::from_millis(1500),
                            move || refresh_overview(),
                        );
                    })
                };
                // Only warn if leaving would actually re-lock the box. When a host
                // app (e.g. Zed) also holds a lease, ending this shell just closes
                // the ssh session — the box stays up — so don't trap the user
                // behind a prompt about a stop that won't happen.
                if would_relock(&vm.cfg.name) {
                    let back_to_shell: Box<dyn FnOnce()> = {
                        let nb = nb.clone();
                        Box::new(move || nb.set_current_page(Some(SHELL_PAGE)))
                    };
                    confirm.ask(
                        "\u{26a0}  Leaving the Shell tab stops and re-locks the VM.  \
                         Stop it?   <b>y</b> = stop   \u{00b7}   <b>n</b> = keep it running",
                        stop_now,
                        back_to_shell,
                    );
                } else {
                    stop_now();
                }
            } else if !confirm.is_confirming() {
                refresh_overview();
            }
        }
    ));

    // Backstop: if the whole panel is torn down (another box selected) or the
    // window is hidden, the notebook unmaps — stop the box then too.
    nb.connect_unmap(clone!(#[strong] shell, move |_| (shell.stop)()));

    wrapper
}

/// The Overview tab. Returns the page plus a closure that re-reads live status
/// and updates the status row + Delete sensitivity.
fn overview_tab(
    window: &ApplicationWindow,
    vm: Rc<Vm>,
    do_refresh: impl Fn() + Clone + 'static,
    confirm: Confirm,
) -> (GtkBox, Rc<dyn Fn()>) {
    let page = GtkBox::new(Orientation::Vertical, 12);
    page.set_margin_top(16);
    page.set_margin_bottom(16);
    page.set_margin_start(16);
    page.set_margin_end(16);

    let title = Label::new(None);
    title.set_markup(&format!("<span size='x-large' weight='bold'>{}</span>", vm.cfg.name));
    title.set_halign(Align::Start);
    page.append(&title);

    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(16);
    let status_val = Label::new(None);
    let ssh_val = Label::new(None);
    let rows: [(&str, &Label); 7] = [
        ("Status", &status_val),
        ("User", &Label::new(Some(&vm.cfg.username))),
        ("Base image", &Label::new(Some(&vm.cfg.base))),
        ("vCPUs", &Label::new(Some(&vm.cfg.cpus.to_string()))),
        ("Memory", &Label::new(Some(&format!("{} MiB", vm.cfg.memory_mib)))),
        ("Disk", &Label::new(Some(&format!("{} GiB (LUKS-encrypted)", vm.cfg.disk_gib)))),
        ("SSH", &ssh_val),
    ];
    for (i, (k, val)) in rows.iter().enumerate() {
        let key = Label::new(Some(k));
        key.set_halign(Align::Start);
        key.add_css_class("dim-label");
        val.set_halign(Align::Start);
        val.set_selectable(true);
        grid.attach(&key, 0, i as i32, 1, 1);
        grid.attach(*val, 1, i as i32, 1, 1);
    }
    page.append(&grid);

    let lifecycle = Label::new(Some(
        "The box starts automatically when you open the Shell tab and re-locks \
         when you leave it. You'll get a desktop notification each time it starts \
         or stops.",
    ));
    lifecycle.set_halign(Align::Start);
    lifecycle.set_wrap(true);
    lifecycle.add_css_class("dim-label");
    lifecycle.set_margin_top(4);
    page.append(&lifecycle);

    let actions = GtkBox::new(Orientation::Horizontal, 8);
    actions.set_margin_top(8);
    let delete = Button::with_label("Delete");
    delete.add_css_class("destructive-action");
    actions.append(&delete);
    page.append(&actions);

    // Live status refresh, reused on tab-switch.
    let refresh_overview: Rc<dyn Fn()> = {
        let vm = vm.clone();
        let status_val = status_val.clone();
        let ssh_val = ssh_val.clone();
        let delete = delete.clone();
        Rc::new(move || {
            let st = vm.status().ok();
            let running = st.as_ref().map(|s| s.running).unwrap_or(false);
            let port = st.as_ref().and_then(|s| s.ssh_port);
            status_val.set_text(if running { "running" } else { "stopped (sealed)" });
            ssh_val.set_text(
                &port
                    .map(|p| format!("ssh -p {p} {}@127.0.0.1", vm.cfg.username))
                    .unwrap_or_else(|| "—".into()),
            );
            delete.set_sensitive(!running);
            delete.set_tooltip_text(if running {
                Some("Leave the Shell tab to stop the box first.")
            } else {
                None
            });
        })
    };
    refresh_overview();

    delete.connect_clicked(clone!(
        #[strong] vm, #[strong] do_refresh, #[strong] confirm, #[strong] window,
        move |_| {
            // Count the launchers we'll cascade-delete so the prompt is honest.
            let n = potjie_core::desktop::list_wrappers(Some(&vm.cfg.name))
                .map(|w| w.len())
                .unwrap_or(0);
            let extra = match n {
                0 => String::new(),
                1 => " and its 1 launcher".into(),
                _ => format!(" and its {n} launchers"),
            };
            let do_delete: Box<dyn FnOnce()> = {
                let vm = vm.clone();
                let do_refresh = do_refresh.clone();
                let window = window.clone();
                Box::new(move || {
                    let vm2 = (*vm).clone();
                    run_async(move || vm2.delete().map_err(|e| e.to_string()),
                        clone!(#[strong] do_refresh, move |res: Result<(), String>| {
                            if let Err(e) = res { info(&window, "Delete failed", &e); }
                            do_refresh();
                        }));
                })
            };
            confirm.ask(
                &format!(
                    "\u{26a0}  Permanently delete box \u{2018}{}\u{2019}{extra}? This erases the \
                     encrypted disk and cannot be undone.   <b>y</b> = delete   \u{00b7}   \
                     <b>n</b> = cancel",
                    vm.cfg.name
                ),
                do_delete,
                Box::new(|| {}),
            );
        }
    ));

    (page, refresh_overview)
}

fn apps_tab(window: &ApplicationWindow, vm: Rc<Vm>) -> GtkBox {
    let page = GtkBox::new(Orientation::Vertical, 12);
    page.set_margin_top(16);
    page.set_margin_bottom(16);
    page.set_margin_start(16);
    page.set_margin_end(16);

    let hint = Label::new(Some(
        "Create a launcher that runs an app while this box is up, then re-locks \
         the box when the app exits. Box apps run inside the box and display on \
         the host over local SSH. The list scans when expanded.",
    ));
    hint.set_wrap(true);
    hint.set_halign(Align::Start);
    page.append(&hint);

    // Existing launchers for this box, each removable in place.
    let (wrappers_box, refresh_wrappers) = wrappers_section(vm.clone());
    page.append(&wrappers_box);

    // Self-heal the registry in the background (drop launchers the user removed
    // via their desktop), then re-render the list.
    run_async(
        || { let _ = potjie_core::desktop::prune_wrappers(); },
        clone!(#[strong] refresh_wrappers, move |_: ()| refresh_wrappers()),
    );

    page.append(&app_section(
        window, vm, Kind::Vm,
        "Box applications  —  run inside the box, shown on the host",
        refresh_wrappers,
    ));
    page
}

/// The list of launchers already created for this box, each with a Remove
/// button. Returns the widget and a closure that re-reads the list from disk
/// (called after a new launcher is created elsewhere in the tab).
fn wrappers_section(vm: Rc<Vm>) -> (GtkBox, Rc<dyn Fn()>) {
    let container = GtkBox::new(Orientation::Vertical, 6);
    let title = Label::new(None);
    title.set_markup("<b>Created launchers</b>");
    title.set_halign(Align::Start);
    container.append(&title);

    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::None);
    list.add_css_class("boxed-list");
    container.append(&list);

    let refresh: Rc<dyn Fn()> = {
        let list = list.clone();
        let vm = vm.clone();
        Rc::new(move || {
            while let Some(c) = list.first_child() {
                list.remove(&c);
            }
            let wrappers =
                potjie_core::desktop::list_wrappers(Some(&vm.cfg.name)).unwrap_or_default();
            if wrappers.is_empty() {
                list.append(&placeholder("No launchers yet — create one from the lists below."));
                return;
            }
            for w in wrappers {
                let rowbox = GtkBox::new(Orientation::Horizontal, 8);
                rowbox.set_margin_top(6);
                rowbox.set_margin_bottom(6);
                rowbox.set_margin_start(10);
                rowbox.set_margin_end(10);
                let name = Label::new(Some(&w.name));
                name.set_halign(Align::Start);
                name.set_hexpand(true);
                let remove = Button::from_icon_name("user-trash-symbolic");
                remove.add_css_class("flat");
                remove.set_tooltip_text(Some("Remove this launcher"));
                rowbox.append(&name);
                rowbox.append(&remove);
                let row = gtk::ListBoxRow::new();
                row.set_selectable(false);
                row.set_child(Some(&rowbox));
                let file_id = w.file_id.clone();
                remove.connect_clicked(clone!(
                    #[strong] list, #[strong] row,
                    move |btn| {
                        btn.set_sensitive(false);
                        let file_id = file_id.clone();
                        run_async(
                            move || potjie_core::desktop::remove_wrapper(&file_id)
                                .map_err(|e| e.to_string()),
                            clone!(#[strong] list, #[strong] row, #[weak] btn,
                                move |res: Result<(), String>| match res {
                                    Ok(()) => list.remove(&row),
                                    Err(_) => btn.set_sensitive(true),
                                }),
                        );
                    }
                ));
                list.append(&row);
            }
        })
    };
    refresh();
    (container, refresh)
}

/// A collapsible (collapsed by default) app list that scans the first time it is
/// expanded.
fn app_section(
    window: &ApplicationWindow,
    vm: Rc<Vm>,
    kind: Kind,
    title: &str,
    refresh_wrappers: Rc<dyn Fn()>,
) -> Expander {
    let expander = Expander::new(Some(title));
    expander.set_expanded(false);

    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::None);
    let scroll = ScrolledWindow::builder()
        .child(&list)
        .min_content_height(220)
        .vexpand(true)
        .margin_top(6)
        .build();
    expander.set_child(Some(&scroll));

    // Scan once, the first time the section is opened.
    let scanned = Rc::new(RefCell::new(false));
    expander.connect_expanded_notify(clone!(
        #[strong] vm, #[strong] list, #[strong] scanned, #[weak] window,
        #[strong] refresh_wrappers,
        move |exp| {
            // The open section should fill the available vertical space; a closed
            // one stays compact so the other can grow.
            exp.set_vexpand(exp.is_expanded());
            if !exp.is_expanded() || *scanned.borrow() {
                return;
            }
            // For box apps we need the box running; allow a retry if it isn't.
            if kind == Kind::Vm && !vm.status().map(|s| s.running).unwrap_or(false) {
                set_rows(&list, &[placeholder("Box not running — open the Shell tab to start it, then reopen.")]);
                return;
            }
            *scanned.borrow_mut() = true;
            set_rows(&list, &[placeholder("Scanning…")]);

            let vm_scan = (*vm).clone();
            run_async(
                move || vm_scan.list_guest_apps().map_err(|e| e.to_string()),
                clone!(#[strong] vm, #[strong] list, #[strong] scanned, #[weak] window,
                #[strong] refresh_wrappers,
                move |res: Result<Vec<DesktopEntry>, String>| {
                    match res {
                        Err(e) => {
                            *scanned.borrow_mut() = false; // let the user retry
                            set_rows(&list, &[placeholder(&format!("Scan failed: {e}"))]);
                        }
                        Ok(entries) if entries.is_empty() => {
                            set_rows(&list, &[placeholder("No applications found.")]);
                        }
                        Ok(entries) => {
                            while let Some(c) = list.first_child() { list.remove(&c); }
                            for entry in entries {
                                let row = Button::with_label(&entry.name);
                                row.set_halign(Align::Fill);
                                row.add_css_class("flat");
                                let entry = entry.clone();
                                row.connect_clicked(clone!(
                                    #[strong] vm, #[weak] window, #[strong] refresh_wrappers,
                                    move |_| make_wrapper(&window, vm.clone(), kind, entry.clone(),
                                        refresh_wrappers.clone())));
                                list.append(&row);
                            }
                        }
                    }
                }),
            );
        }
    ));

    expander
}

fn placeholder(text: &str) -> Label {
    let l = Label::new(Some(text));
    l.set_halign(Align::Start);
    l.set_margin_top(8);
    l.set_margin_start(6);
    l.add_css_class("dim-label");
    l
}

fn set_rows(list: &ListBox, rows: &[Label]) {
    while let Some(c) = list.first_child() {
        list.remove(&c);
    }
    for r in rows {
        list.append(r);
    }
}

fn make_wrapper(
    window: &ApplicationWindow,
    vm: Rc<Vm>,
    kind: Kind,
    entry: DesktopEntry,
    refresh_wrappers: Rc<dyn Fn()>,
) {
    prompt_text(window, "Launcher name",
        &format!("Name for the launcher for '{}':", entry.name),
        false,
        clone!(#[weak] window, move |name| {
            let Some(name) = name.filter(|n| !n.trim().is_empty()) else { return; };
            let launcher = launcher_path();
            // create_wrapper drives the portal install dialog, so it must run off
            // the UI thread; the result comes back on the main context.
            let box_name = vm.cfg.name.clone();
            let name = name.trim().to_string();
            let entry = entry.clone();
            run_async(
                move || potjie_core::desktop::create_wrapper(&box_name, kind, &entry, &name, &launcher)
                    .map_err(|e| e.to_string()),
                clone!(#[weak] window, #[strong] refresh_wrappers,
                    move |res: Result<(), String>| match res {
                        Ok(()) => refresh_wrappers(), // show it in the list right away
                        Err(e) => info(&window, "Could not create launcher", &e),
                    }),
            );
        }));
}

/// The Ports tab: view, add, and remove the box's SSH port forwards. Edits are
/// sent to the daemon, which persists them and — if the box is running — applies
/// the change live over the SSH control master (no restart).
fn ports_tab(vm: Rc<Vm>) -> GtkBox {
    let page = GtkBox::new(Orientation::Vertical, 12);
    page.set_margin_top(16);
    page.set_margin_bottom(16);
    page.set_margin_start(16);
    page.set_margin_end(16);

    let hint = Label::new(Some(
        "Forwards tunnel over the box's SSH connection while it runs. Pick \u{201c}Host \
         \u{2192} Box\u{201d} to reach a service running inside the box from this machine \
         (e.g. a dev server), or \u{201c}Box \u{2192} Host\u{201d} to let something inside \
         the box reach a service on this machine. Changes apply live to a running box and \
         persist for next boot.",
    ));
    hint.set_wrap(true);
    hint.set_halign(Align::Start);
    page.append(&hint);

    // Status line for save feedback (errors, "applied", etc.).
    let status = Label::new(None);
    status.set_halign(Align::Start);
    status.set_wrap(true);
    status.add_css_class("dim-label");

    // Source of truth for the editor: the daemon's persisted set if reachable,
    // else the config we loaded with.
    let forwards: Rc<RefCell<Vec<Forward>>> = Rc::new(RefCell::new(
        guard::get_forwards(&vm.cfg.name).unwrap_or_else(|_| vm.cfg.forwards.clone()),
    ));

    // Current forwards, each removable in place.
    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::None);
    list.add_css_class("boxed-list");
    list.set_margin_top(6);

    // `refresh` rebuilds the rows; both `apply` and each row's Remove button need
    // to call it, so hold a self-reference those closures can invoke.
    let refresh_holder: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

    // Apply a *proposed* forward set to the daemon, then — only once it confirms —
    // commit it to the in-memory list and redraw. The UI therefore always mirrors
    // what the daemon actually persisted: a failed write leaves the rows untouched
    // and shows a clear error, instead of optimistically dropping a row that's
    // still in `box.json` (which made deletes look like they "came back").
    let apply: Rc<dyn Fn(Vec<Forward>)> = {
        let forwards = forwards.clone();
        let status = status.clone();
        let name = vm.cfg.name.clone();
        let refresh_holder = refresh_holder.clone();
        Rc::new(move |set: Vec<Forward>| {
            let name = name.clone();
            let to_send = set.clone();
            status.remove_css_class("error");
            status.set_text("Applying\u{2026}");
            run_async(
                move || guard::set_forwards(&name, to_send).map_err(|e| e.to_string()),
                clone!(
                    #[strong] forwards, #[strong] status, #[strong] refresh_holder,
                    move |res: Result<(), String>| {
                        match res {
                            Ok(()) => {
                                *forwards.borrow_mut() = set;
                                status.remove_css_class("error");
                                status.set_text("Saved. Live on the box if it's running; \
                                                 applied on next boot otherwise.");
                            }
                            Err(e) => {
                                status.add_css_class("error");
                                status.set_text(&format!(
                                    "Could not apply \u{2014} nothing changed: {e}"
                                ));
                            }
                        }
                        if let Some(r) = refresh_holder.borrow().as_ref() {
                            r();
                        }
                    }
                ),
            );
        })
    };

    let refresh: Rc<dyn Fn()> = {
        let list = list.clone();
        let forwards = forwards.clone();
        let apply = apply.clone();
        Rc::new(move || {
            while let Some(c) = list.first_child() {
                list.remove(&c);
            }
            if forwards.borrow().is_empty() {
                list.append(&placeholder("No forwards yet \u{2014} add one below."));
                return;
            }
            let rows: Vec<(usize, String)> = forwards
                .borrow()
                .iter()
                .enumerate()
                .map(|(i, f)| (i, f.summary()))
                .collect();
            for (i, summary) in rows {
                let rowbox = GtkBox::new(Orientation::Horizontal, 8);
                rowbox.set_margin_top(6);
                rowbox.set_margin_bottom(6);
                rowbox.set_margin_start(10);
                rowbox.set_margin_end(10);
                let label = Label::new(Some(&summary));
                label.set_halign(Align::Start);
                label.set_hexpand(true);
                label.set_selectable(true);
                let remove = Button::from_icon_name("user-trash-symbolic");
                remove.add_css_class("flat");
                remove.set_tooltip_text(Some("Remove this forward"));
                rowbox.append(&label);
                rowbox.append(&remove);
                let row = gtk::ListBoxRow::new();
                row.set_selectable(false);
                row.set_child(Some(&rowbox));
                remove.connect_clicked(clone!(
                    #[strong] forwards, #[strong] apply,
                    move |_| {
                        let mut set = forwards.borrow().clone();
                        if i < set.len() {
                            set.remove(i);
                            apply(set);
                        }
                    }
                ));
                list.append(&row);
            }
        })
    };
    *refresh_holder.borrow_mut() = Some(refresh.clone());
    refresh();

    page.append(&list);

    // ---- Add form -------------------------------------------------------
    // The form speaks in plain "Host port / Box port" terms so the meaning of each
    // field never silently swaps with the direction (the old listen-vs-destination
    // framing was the confusing part). The direction dropdown just flips which way
    // traffic flows, a live sentence spells out the result, and the rarely-touched
    // destination host hides in an inline "Advanced" expander.

    // Direction: index 0 = Host → Box (Local), index 1 = Box → Host (Remote).
    let dir = DropDown::from_strings(&["Host \u{2192} Box", "Box \u{2192} Host"]);
    dir.set_tooltip_text(Some(
        "Host \u{2192} Box: reach a service running inside the box from this machine.\n\
         Box \u{2192} Host: let something inside the box reach a service on this machine.",
    ));

    let host_port = SpinButton::with_range(1.0, 65535.0, 1.0);
    host_port.set_value(8000.0);
    host_port.set_tooltip_text(Some("Port on this machine"));

    let box_port = SpinButton::with_range(1.0, 65535.0, 1.0);
    box_port.set_value(8000.0);
    box_port.set_tooltip_text(Some("Port inside the box"));

    let label_entry = Entry::new();
    label_entry.set_placeholder_text(Some("label (optional)"));
    label_entry.set_width_chars(14);

    let add = Button::with_label("Add forward");
    add.add_css_class("suggested-action");

    // Two-row grid so every column lines up: captions on top, controls beneath.
    //   Host port | Direction | Box port |        |
    //    [8000]   | [Host→Box]|  [8000]  | [label]| [Add]
    let caption = |text: &str| {
        let l = Label::new(Some(text));
        l.add_css_class("dim-label");
        l.set_halign(Align::Start);
        l
    };
    let form = gtk::Grid::new();
    form.set_row_spacing(2);
    form.set_column_spacing(8);
    form.set_margin_top(8);
    form.attach(&caption("Host port"), 0, 0, 1, 1);
    form.attach(&caption("Direction"), 1, 0, 1, 1);
    form.attach(&caption("Box port"), 2, 0, 1, 1);
    form.attach(&host_port, 0, 1, 1, 1);
    form.attach(&dir, 1, 1, 1, 1);
    form.attach(&box_port, 2, 1, 1, 1);
    form.attach(&label_entry, 3, 1, 1, 1);
    form.attach(&add, 4, 1, 1, 1);
    page.append(&form);

    // Advanced: destination host (defaults to loopback on the far side).
    let dest_host = Entry::new();
    dest_host.set_text("127.0.0.1");
    dest_host.set_width_chars(16);
    dest_host.set_tooltip_text(Some(
        "Address of the service on the destination side. Default 127.0.0.1 \
         (loopback) is right unless the service listens elsewhere.",
    ));
    let adv_row = GtkBox::new(Orientation::Horizontal, 8);
    let adv_caption = Label::new(Some("Destination host"));
    adv_caption.add_css_class("dim-label");
    adv_row.append(&adv_caption);
    adv_row.append(&dest_host);
    let advanced = Expander::new(Some("Advanced"));
    advanced.set_child(Some(&adv_row));
    advanced.set_margin_top(4);
    page.append(&advanced);

    // Live plain-language description of the forward being built.
    let explain = Label::new(None);
    explain.set_halign(Align::Start);
    explain.set_wrap(true);
    explain.add_css_class("dim-label");
    explain.set_margin_top(4);
    page.append(&explain);
    let update_explain: Rc<dyn Fn()> = {
        let dir = dir.clone();
        let host_port = host_port.clone();
        let box_port = box_port.clone();
        let explain = explain.clone();
        Rc::new(move || {
            let h = host_port.value() as u16;
            let b = box_port.value() as u16;
            let text = if dir.selected() == 0 {
                format!(
                    "Reach the box's port {b} at localhost:{h} on this machine."
                )
            } else {
                format!(
                    "Let the box reach this machine's port {h} at localhost:{b} inside the box."
                )
            };
            explain.set_text(&text);
        })
    };
    update_explain();
    dir.connect_selected_notify(clone!(#[strong] update_explain, move |_| update_explain()));

    // Auto-mirror the host port onto the box port so the common "same port both
    // sides" case needs no second edit. Mirroring stays on only while the two
    // values agree; once the box port is set apart, the two move independently.
    let mirror = Rc::new(Cell::new(true));
    host_port.connect_value_changed(clone!(
        #[strong] mirror, #[strong] update_explain, #[weak] box_port,
        move |hp| {
            if mirror.get() {
                box_port.set_value(hp.value());
            }
            update_explain();
        }
    ));
    box_port.connect_value_changed(clone!(
        #[strong] mirror, #[strong] update_explain, #[weak] host_port,
        move |bp| {
            mirror.set(bp.value() as u16 == host_port.value() as u16);
            update_explain();
        }
    ));

    page.append(&status);

    let do_add: Rc<dyn Fn()> = {
        let forwards = forwards.clone();
        let apply = apply.clone();
        let status = status.clone();
        let dir = dir.clone();
        let host_port = host_port.clone();
        let box_port = box_port.clone();
        let dest_host = dest_host.clone();
        let label_entry = label_entry.clone();
        Rc::new(move || {
            let direction = if dir.selected() == 0 {
                ForwardDirection::Local
            } else {
                ForwardDirection::Remote
            };
            let host = dest_host.text().trim().to_string();
            let host = if host.is_empty() { "127.0.0.1".to_string() } else { host };
            let label = label_entry.text().trim().to_string();
            let fwd = Forward::from_ports(
                direction,
                host_port.value() as u16,
                box_port.value() as u16,
                host,
                if label.is_empty() { None } else { Some(label) },
            );
            let mut set = forwards.borrow().clone();
            if set.contains(&fwd) {
                status.add_css_class("error");
                status.set_text("That forward already exists.");
                return;
            }
            set.push(fwd);
            label_entry.set_text("");
            apply(set);
        })
    };
    add.connect_clicked(clone!(#[strong] do_add, move |_| do_add()));
    // Enter in the label field also adds the forward.
    label_entry.connect_activate(clone!(#[strong] do_add, move |_| do_add()));

    page
}

fn shell_tab(vm: Rc<Vm>) -> (GtkBox, ShellCtl) {
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
