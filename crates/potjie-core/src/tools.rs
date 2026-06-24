//! Locating the external qemu binaries Potjie drives.
//!
//! By default we trust `PATH`. A bundled distribution (the Flatpak) can
//! point at its own copies via `POTJIE_QEMU_IMG` / `POTJIE_QEMU_SYSTEM`, which
//! is how Potjie stays self-contained on immutable hosts.

use anyhow::{bail, Context, Result};
use std::process::Command;

fn resolve(env: &str, default: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| default.to_string())
}

pub fn qemu_img() -> String {
    resolve("POTJIE_QEMU_IMG", "qemu-img")
}

pub fn qemu_system() -> String {
    resolve("POTJIE_QEMU_SYSTEM", "qemu-system-x86_64")
}

/// Resolve a sibling binary: an explicit `$<env>` override → a binary of the
/// given name next to the current executable → bare `name` (found via `PATH`).
/// This is how the GTK app finds the sibling `potjie`, the CLI finds itself, and
/// a bundled distribution can point at its own copies.
fn resolve_sibling(env: &str, name: &str) -> String {
    if let Ok(p) = std::env::var(env) {
        return p;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join(name);
            if cand.exists() {
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    name.to_string()
}

/// Path to the multicall `potjie` binary (CLI + `potjie daemon`). `POTJIE_BIN` →
/// sibling `potjie` → `PATH`.
pub fn potjie_bin() -> String {
    resolve_sibling("POTJIE_BIN", "potjie")
}

/// Sentinel prefix the `potjie-gtk --ask-passphrase` window prints its result
/// with, so the daemon can pick the passphrase line out of any incidental GTK
/// stdout noise. The control bytes make an accidental collision implausible.
pub const ASK_PASSPHRASE_PREFIX: &str = "\u{1}potjie-pass\u{1}";

/// Path to the `potjie-gtk` binary (the GUI). The daemon spawns it to draw the
/// passphrase prompt when an ssh connection boots a locked box (`--ask-passphrase`).
/// `POTJIE_GTK_BIN` → sibling `potjie-gtk` → `PATH`, mirroring [`potjie_bin`].
pub fn potjie_gtk_bin() -> String {
    resolve_sibling("POTJIE_GTK_BIN", "potjie-gtk")
}

/// Run a command, returning an error that includes stderr on failure.
pub fn run(cmd: &mut Command) -> Result<()> {
    let pretty = format!("{cmd:?}");
    let out = cmd
        .output()
        .with_context(|| format!("spawning {pretty}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("command failed ({}): {}\n{}", out.status, pretty, stderr.trim());
    }
    Ok(())
}

/// True if `/dev/kvm` is usable, so callers can decide whether to add `-enable-kvm`.
pub fn kvm_available() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata("/dev/kvm") {
            // Readable+writable bit for someone; actual access is enforced by the
            // kernel, but this avoids passing -enable-kvm where the node is absent.
            return meta.permissions().mode() & 0o006 != 0
                || std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/kvm")
                    .is_ok();
        }
    }
    false
}
