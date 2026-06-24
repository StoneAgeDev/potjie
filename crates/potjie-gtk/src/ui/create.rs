//! The inline "new box" form rendered in the detail panel.

use super::{run_async, select_box_row};
use gtk::glib::clone;
use gtk::prelude::*;
use gtk::{
    glib, Align, Box as GtkBox, Button, Entry, Label, ListBox, Orientation, ScrolledWindow, Spinner,
    SpinButton, TextView,
};
use potjie_core::{BoxConfig, Vm};

/// Inline new-box form rendered in the detail panel.
pub(super) fn create_box_form(sidebar: &ListBox, do_refresh: impl Fn() + Clone + 'static) -> GtkBox {
    let page = GtkBox::new(Orientation::Vertical, 12);
    page.set_margin_top(16);
    page.set_margin_bottom(16);
    page.set_margin_start(16);
    page.set_margin_end(16);

    let title = Label::new(None);
    title.set_markup("<span size='x-large' weight='bold'>New box</span>");
    title.set_halign(Align::Start);
    page.append(&title);

    let defaults = BoxConfig::new("x");
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

            let mut cfg = BoxConfig::new(&n);
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
