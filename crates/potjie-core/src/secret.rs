//! Passing the LUKS passphrase to qemu / qemu-img without leaking it.
//!
//! qemu's `secret` object can read raw key bytes from a file. We write the
//! passphrase to a 0600 file on tmpfs (`$XDG_RUNTIME_DIR`), hand qemu the path,
//! and delete it as soon as qemu has it. The passphrase therefore never lands
//! on persistent storage and never appears in `argv` (which is world-readable
//! via `/proc/<pid>/cmdline`).

use crate::paths;
use anyhow::Result;
use std::path::PathBuf;

/// A passphrase written to a temporary tmpfs file, deleted on drop.
pub struct SecretFile {
    path: PathBuf,
}

impl SecretFile {
    /// Write `passphrase` to a fresh 0600 file under the runtime dir.
    pub fn new(passphrase: &str) -> Result<Self> {
        let dir = paths::runtime_root()?;
        paths::create_private_dir(&dir)?;
        // The whole point of this dance is that the key never lands on persistent
        // storage. Refuse to write it if the runtime dir isn't RAM-backed (tmpfs),
        // rather than silently leak plaintext key material onto a disk.
        if !paths::is_tmpfs(&dir) {
            anyhow::bail!(
                "refusing to write the LUKS key: runtime dir {} is not tmpfs \
                 (RAM-backed); the key would touch persistent storage. Set \
                 XDG_RUNTIME_DIR to a tmpfs mount (normally /run/user/<uid>).",
                dir.display()
            );
        }
        // Unique-ish name; runtime dir is already user-private (0700).
        let path = dir.join(format!("secret-{}", std::process::id()));
        paths::write_private(&path, passphrase.as_bytes())?;
        Ok(Self { path })
    }

    /// qemu `-object secret,...` argument referencing this file.
    pub fn object_arg(&self, id: &str) -> String {
        format!("secret,id={},file={}", id, self.path.display())
    }
}

impl Drop for SecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
