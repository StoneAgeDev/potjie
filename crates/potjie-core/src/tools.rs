//! Locating the external qemu binaries Potjie drives.
//!
//! By default we trust `PATH`. A bundled distribution (AppImage/Flatpak) can
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

/// Path to the guard daemon binary. Defaults to a `potjied` sitting next to the
/// current executable, falling back to `PATH`.
pub fn potjied() -> String {
    if let Ok(p) = std::env::var("POTJIE_DAEMON") {
        return p;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("potjied");
            if cand.exists() {
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    "potjied".to_string()
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
