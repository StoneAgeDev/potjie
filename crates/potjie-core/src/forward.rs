//! SSH connection-multiplexing master per box, and live port-forward management.
//!
//! While a box runs, the guard daemon holds one background SSH "control master"
//! to it (`ssh -N -f` against the Potjie-managed ssh config, which sets
//! `ControlMaster auto` and a per-box `ControlPath`). Every port forward the box
//! has configured rides on that single multiplexed connection.
//!
//! Because the connection is multiplexed, forwards can be added and removed
//! **live** with `ssh -O forward` / `ssh -O cancel` — no box restart and no
//! dropped sessions. The Potjie-managed `~/.potjie/ssh/config` fragment is the
//! source of truth (it carries the `LocalForward`/`RemoteForward` lines, so any
//! fresh connection or a restart picks them up); this module applies the *delta*
//! to an already-running master so changes take effect immediately.

use crate::config::Forward;
use crate::desktop::ssh_config_path;
use crate::paths;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Per-box control socket for SSH multiplexing. Kept on tmpfs (the runtime root)
/// so it's short — Unix socket paths are length-limited — and disappears on reboot.
pub fn control_path(name: &str) -> Result<PathBuf> {
    Ok(paths::runtime_root()?.join(format!("control-{name}.sock")))
}

/// The SSH host alias Potjie generates for a box.
fn alias(name: &str) -> String {
    format!("potjie-{name}")
}

/// A base `ssh` command pointed at the box via the Potjie-managed config, with
/// stdio silenced. Callers append the operation-specific flags then the alias.
fn ssh_base() -> Result<Command> {
    let cfg = ssh_config_path()?;
    let mut cmd = Command::new("ssh");
    cmd.arg("-F")
        .arg(&cfg)
        .args(["-o", "BatchMode=yes"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(cmd)
}

/// Is a live control master currently up for this box?
pub fn is_master_alive(name: &str) -> bool {
    let Ok(mut cmd) = ssh_base() else {
        return false;
    };
    cmd.args(["-O", "check"])
        .arg(alias(name))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Start the background control master for a running box (idempotent). All of the
/// box's configured forwards come up with it, courtesy of the config fragment.
/// Best-effort by design: a forward that can't bind (e.g. host port already taken)
/// must never block the box itself, so `ExitOnForwardFailure` stays off.
pub fn start_master(name: &str) -> Result<()> {
    if is_master_alive(name) {
        return Ok(());
    }
    let status = ssh_base()?
        .args(["-N", "-f"]) // no remote command; fork to background after auth
        .args(["-o", "ExitOnForwardFailure=no"])
        // This master carries the forwards and never runs a session, so the
        // config's short `ControlPersist` would idle it out (and drop every
        // forward) seconds after boot. Override it to persist for the box's
        // lifetime; the daemon tears it down explicitly via `stop_master`.
        .args(["-o", "ControlPersist=yes"])
        .arg(alias(name))
        .status()
        .context("starting ssh control master")?;
    if !status.success() {
        bail!("ssh control master failed to start for '{name}'");
    }
    Ok(())
}

/// Tear the control master down (best effort). Called when the box stops.
pub fn stop_master(name: &str) {
    if let Ok(mut cmd) = ssh_base() {
        let _ = cmd.args(["-O", "exit"]).arg(alias(name)).status();
    }
    // If ssh never cleaned the socket (e.g. the box died under it), remove it so a
    // stale path can't confuse the next `ControlMaster auto`.
    if let Ok(p) = control_path(name) {
        let _ = std::fs::remove_file(p);
    }
}

/// Apply one forward to the live master (`op` is `"forward"` or `"cancel"`).
fn control(name: &str, op: &str, fwd: &Forward) -> Result<()> {
    let cfg = ssh_config_path()?;
    let out = Command::new("ssh")
        .arg("-F")
        .arg(&cfg)
        .args(["-o", "BatchMode=yes"])
        .args(["-O", op])
        .arg(fwd.direction.flag())
        .arg(fwd.spec())
        .arg(alias(name))
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("ssh -O {op}"))?;
    if !out.status.success() {
        bail!(
            "ssh -O {op} {} failed: {}",
            fwd.spec(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Reconcile the live master from `old` forwards to `new`, applying only the
/// delta. No-op when no master is running (the box is down, or never had one):
/// the regenerated config fragment will establish the forwards on the next
/// connection. Cancellations are best-effort; new forwards surface their errors.
pub fn reload(name: &str, old: &[Forward], new: &[Forward]) -> Result<()> {
    if !is_master_alive(name) {
        return Ok(());
    }
    for f in old {
        if !new.contains(f) {
            let _ = control(name, "cancel", f);
        }
    }
    let mut errs = Vec::new();
    for f in new {
        if !old.contains(f) {
            if let Err(e) = control(name, "forward", f) {
                errs.push(e.to_string());
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        bail!("{}", errs.join("; "))
    }
}
