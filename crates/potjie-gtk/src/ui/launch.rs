//! `potjie-gtk --ask-passphrase <box>`: a single passphrase prompt.
//!
//! The guard daemon spawns this when an ssh connection boots a locked box and
//! there's no terminal to prompt at. On submit it prints the passphrase to
//! stdout (sentinel-prefixed so the daemon can find it past any GTK stdout
//! noise) and exits 0; on cancel/close it exits non-zero so the daemon fails the
//! ssh connection cleanly and the box stays locked.

use adw::prelude::*;
use gtk::glib::{self, clone};
use gtk::Entry;

/// Show the passphrase entry dialog. This is the one deliberate standalone
/// window in Potjie, since the trigger is an ssh connect with no inline surface.
pub fn ask_passphrase(app: &adw::Application, box_name: &str) {
    let entry = Entry::builder()
        .visibility(false)
        .activates_default(true)
        .build();

    let dialog = adw::AlertDialog::builder()
        .heading(format!("Unlock '{box_name}'"))
        .body("Enter the passphrase to unlock this box.")
        .extra_child(&entry)
        .default_response("unlock")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("unlock", "Unlock");
    dialog.set_response_appearance("unlock", adw::ResponseAppearance::Suggested);

    // Keep the app alive until the dialog is dismissed.
    let hold = app.hold();

    dialog.connect_response(None, clone!(
        #[strong] entry,
        move |_, response| {
            let _hold = &hold; // keep app alive until dialog responds
            if response == "unlock" {
                let pass = entry.text().to_string();
                if pass.is_empty() {
                    return;
                }
                println!("{}{}", potjie_core::tools::ASK_PASSPHRASE_PREFIX, pass);
                use std::io::Write;
                let _ = std::io::stdout().flush();
                std::process::exit(0);
            } else {
                std::process::exit(1);
            }
        }
    ));

    dialog.present(None::<&gtk::Widget>);

    glib::idle_add_local_once(clone!(#[strong] entry, move || {
        entry.grab_focus();
    }));
}
