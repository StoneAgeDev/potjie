//! Running a process and waiting for its **entire descendant tree** to exit.
//!
//! Naively waiting on a child is not enough: many real apps (browsers, editors,
//! Electron things like VS Code) fork a background process and let the launched
//! binary return immediately, or double-fork to daemonize. A plain `wait` then
//! returns while the app is still very much alive — which, for Potjie, would
//! mean re-locking the box out from under a running editor. Unacceptable.
//!
//! The fix: become a **child subreaper** (`PR_SET_CHILD_SUBREAPER`). After that,
//! any descendant that orphans itself reparents to *us* instead of to PID 1, so
//! a `waitpid(-1, …)` loop reaps the whole tree. We only return once `waitpid`
//! reports `ECHILD` — i.e. not a single descendant remains.
//!
//! This MUST run in a process that doesn't otherwise spawn children it expects
//! to reap itself (GTK/glib do), so it lives in a dedicated `potjie` helper
//! subprocess (`potjie __run-tracked`).

use std::process::Command;

/// Spawn `cmd`, then block until it and every descendant it spawned have exited.
/// Returns the direct child's exit code (or 128+signal).
#[cfg(target_os = "linux")]
pub fn run_until_descendants_exit(mut cmd: Command) -> std::io::Result<i32> {
    // Reparent orphaned descendants to us instead of PID 1.
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1 as libc::c_ulong);
    }

    let child = cmd.spawn()?;
    let direct = child.id() as i32;
    // We reap via waitpid(-1) ourselves; stop std from tracking this pid.
    std::mem::forget(child);

    let mut direct_code = 0;
    loop {
        let mut status: libc::c_int = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid == -1 {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                // No children of any kind remain: the whole tree is gone.
                Some(libc::ECHILD) => break,
                _ => return Err(err),
            }
        }
        if pid == direct {
            direct_code = exit_code(status);
        }
    }
    Ok(direct_code)
}

#[cfg(not(target_os = "linux"))]
pub fn run_until_descendants_exit(mut cmd: Command) -> std::io::Result<i32> {
    // Best effort on non-Linux: wait for the direct child only.
    Ok(cmd.status()?.code().unwrap_or(0))
}

/// Decode a `waitpid` status into a shell-style exit code.
#[cfg(target_os = "linux")]
fn exit_code(status: libc::c_int) -> i32 {
    if status & 0x7f == 0 {
        (status >> 8) & 0xff // WEXITSTATUS
    } else {
        128 + (status & 0x7f) // killed by signal
    }
}
