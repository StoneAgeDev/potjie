//! The per-box Overview tab: live status, specs, and Delete.

use super::{box_status, info, run_async, Confirm};
use gtk::glib::clone;
use gtk::prelude::*;
use gtk::{Align, ApplicationWindow, Box as GtkBox, Button, Label, Orientation};
use potjie_core::Vm;
use std::rc::Rc;

/// The Overview tab. Returns the page plus a closure that re-reads live status
/// and updates the status row + Delete sensitivity.
pub(super) fn overview_tab(
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
            let st = box_status(&vm.cfg.name);
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
                    "\u{26a0}  Permanently delete box \u{2018}{}\u{2019}? This erases the \
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
