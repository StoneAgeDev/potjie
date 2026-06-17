//! Passing the LUKS passphrase to qemu / qemu-img without leaking it.
//!
//! qemu's `secret` object can read raw key bytes from a file. We write the
//! passphrase to a 0600 file on tmpfs (`$XDG_RUNTIME_DIR`), hand qemu the path,
//! and delete it as soon as qemu has it. The passphrase therefore never lands
//! on persistent storage and never appears in `argv` (which is world-readable
//! via `/proc/<pid>/cmdline`).

use crate::paths;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// A passphrase written to a temporary tmpfs file, deleted on drop.
pub struct SecretFile {
    path: PathBuf,
}

impl SecretFile {
    /// Write `passphrase` to a fresh 0600 file under the runtime dir.
    pub fn new(passphrase: &str) -> Result<Self> {
        let dir = paths::runtime_root()?;
        paths::create_private_dir(&dir)?;
        // Unique-ish name; runtime dir is already user-private (0700).
        let path = dir.join(format!("secret-{}", std::process::id()));
        write_private(&path, passphrase.as_bytes())?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
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

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating secret file {}", path.display()))?;
    f.write_all(bytes)?;
    f.sync_all().ok();
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)
        .with_context(|| format!("creating secret file {}", path.display()))?;
    Ok(())
}
