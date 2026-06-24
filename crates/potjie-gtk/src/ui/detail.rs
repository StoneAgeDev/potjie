//! The per-box detail panel: a notebook of Overview / Ports / Shell tabs, plus
//! the lifecycle wiring that boots the box while the Shell tab is current and
//! re-locks it on leave.

use super::overview::overview_tab;
use super::ports::ports_tab;
use super::shell::shell_tab;
use super::{would_relock, Confirm};
use gtk::glib::clone;
use gtk::prelude::*;
use gtk::{glib, ApplicationWindow, Box as GtkBox, Label, Notebook, Orientation};
use potjie_core::Vm;
use std::rc::Rc;

// Tab order: Overview(0), Ports(1), Shell(2).
const SHELL_PAGE: u32 = 2;

pub(super) fn build_detail(
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
