//! `potjie-gtk --ask-passphrase <box>`: a single passphrase prompt.
//!
//! The guard daemon spawns this when an ssh connection boots a locked box and
//! there's no terminal to prompt at. On submit it prints the passphrase to
//! stdout (sentinel-prefixed so the daemon can find it past any GTK stdout
//! noise) and exits 0; on cancel/close it exits non-zero so the daemon fails the
//! ssh connection cleanly and the box stays locked.

use gtk::glib::{self, clone};
use gtk::prelude::*;
use gtk::{Align, ApplicationWindow, Box as GtkBox, Button, Entry, Label, Orientation};
use std::cell::Cell;
use std::rc::Rc;

/// Show the passphrase entry window. This is the one deliberate standalone
/// window in Potjie, since the trigger is an ssh connect with no inline surface.
pub fn ask_passphrase(app: &adw::Application, box_name: &str) {
    let win = ApplicationWindow::builder()
        .application(app)
        .title("Unlock box")
        .default_width(380)
        .resizable(false)
        .build();

    let vbox = GtkBox::new(Orientation::Vertical, 12);
    vbox.set_margin_top(16);
    vbox.set_margin_bottom(16);
    vbox.set_margin_start(16);
    vbox.set_margin_end(16);

    let label = Label::new(Some(&format!(
        "Enter the passphrase to unlock box '{box_name}':"
    )));
    label.set_wrap(true);
    label.set_halign(Align::Start);
    let entry = Entry::new();
    entry.set_visibility(false);
    entry.set_activates_default(true);
    let go = Button::with_label("Unlock");
    go.add_css_class("suggested-action");
    go.set_halign(Align::End);
    vbox.append(&label);
    vbox.append(&entry);
    vbox.append(&go);
    win.set_child(Some(&vbox));
    win.set_default_widget(Some(&go));

    // Set on submit so the close handler knows it was an intentional unlock.
    let submitted = Rc::new(Cell::new(false));

    let submit = clone!(
        #[strong] entry, #[strong] win, #[strong] submitted,
        move || {
            let pass = entry.text().to_string();
            if pass.is_empty() {
                return;
            }
            // Sentinel-prefixed so the daemon can pick it out of any stdout noise.
            println!("{}{}", potjie_core::tools::ASK_PASSPHRASE_PREFIX, pass);
            use std::io::Write;
            let _ = std::io::stdout().flush();
            submitted.set(true);
            win.close();
        }
    );
    go.connect_clicked(clone!(#[strong] submit, move |_| submit()));
    entry.connect_activate(clone!(#[strong] submit, move |_| submit()));

    // Closing without submitting is a cancellation: exit non-zero so the daemon
    // fails the ssh connection cleanly and the box stays locked.
    win.connect_close_request(clone!(
        #[strong] submitted,
        move |_| {
            if !submitted.get() {
                std::process::exit(1);
            }
            glib::Propagation::Proceed
        }
    ));

    win.present();
    entry.grab_focus();
}
