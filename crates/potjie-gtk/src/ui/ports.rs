//! The per-box Ports tab: view, add, and remove SSH port forwards.

use super::run_async;
use gtk::glib;
use gtk::glib::clone;
use gtk::prelude::*;
use gtk::{
    Align, Box as GtkBox, Button, DropDown, Entry, Expander, Label, ListBox, Orientation,
    SelectionMode, SpinButton,
};
use potjie_core::config::{Forward, ForwardDirection};
use potjie_core::{guard, Vm};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// A dim-label placeholder row.
fn placeholder(text: &str) -> Label {
    let l = Label::new(Some(text));
    l.set_halign(Align::Start);
    l.set_margin_top(8);
    l.set_margin_start(6);
    l.add_css_class("dim-label");
    l
}

/// The Ports tab: view, add, and remove the box's SSH port forwards. Edits are
/// sent to the daemon, which persists them and — if the box is running — applies
/// the change live over the SSH control master (no restart).
pub(super) fn ports_tab(vm: Rc<Vm>) -> GtkBox {
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
