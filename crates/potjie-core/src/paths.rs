//! Filesystem layout for Potjie.
//!
//! Everything lives under `~/.potjie` and, for ephemeral runtime state,
//! `$XDG_RUNTIME_DIR/potjie` (a user-only tmpfs, so LUKS key material and
//! control sockets never touch persistent storage).
//!
//! ```text
//! ~/.potjie/
//!   base/                      cached, verified base images (shared, plaintext)
//!     debian-13-genericcloud-amd64.qcow2
//!   img/<box>/                 one directory per box
//!     box.json                 box metadata
//!     disk.qcow2               per-box LUKS-encrypted root disk
//!     seed.img                 cloud-init NoCloud seed (FAT, label CIDATA)
//!     id_ed25519[.pub]         ssh key Potjie uses to reach the box
//! $XDG_RUNTIME_DIR/potjie/<box>/
//!     qemu.pid                 pid of the running qemu process
//!     qmp.sock                 QMP control socket
//!     ssh.port                 host port mapped to guest :22
//! ```

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Root of the persistent Potjie store: `~/.potjie`.
pub fn root() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".potjie"))
}

/// Directory holding cached base images.
pub fn base_dir() -> Result<PathBuf> {
    Ok(root()?.join("base"))
}

/// Directory holding all boxes.
pub fn img_dir() -> Result<PathBuf> {
    Ok(root()?.join("img"))
}

/// Ephemeral per-user runtime root, on tmpfs where available.
pub fn runtime_root() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir());
    Ok(base.join("potjie"))
}

/// Resolved set of paths for a single box.
#[derive(Debug, Clone)]
pub struct BoxPaths {
    pub name: String,
    pub dir: PathBuf,
    pub config: PathBuf,
    pub disk: PathBuf,
    pub seed: PathBuf,
    pub ssh_key: PathBuf,
    pub ssh_pubkey: PathBuf,
    pub runtime_dir: PathBuf,
    pub pid_file: PathBuf,
    pub qmp_sock: PathBuf,
    pub ssh_port_file: PathBuf,
}

impl BoxPaths {
    pub fn new(name: &str) -> Result<Self> {
        let dir = img_dir()?.join(name);
        let runtime_dir = runtime_root()?.join(name);
        Ok(Self {
            name: name.to_string(),
            config: dir.join("box.json"),
            disk: dir.join("disk.qcow2"),
            seed: dir.join("seed.img"),
            ssh_key: dir.join("id_ed25519"),
            ssh_pubkey: dir.join("id_ed25519.pub"),
            pid_file: runtime_dir.join("qemu.pid"),
            qmp_sock: runtime_dir.join("qmp.sock"),
            ssh_port_file: runtime_dir.join("ssh.port"),
            runtime_dir,
            dir,
        })
    }

    pub fn exists(&self) -> bool {
        self.config.exists()
    }

    /// Where qemu writes the guest serial console (boot log) for this box.
    pub fn console_log(&self) -> PathBuf {
        self.runtime_dir.join("console.log")
    }
}

/// Ensure the base directories exist with restrictive permissions.
pub fn ensure_layout() -> Result<()> {
    create_private_dir(&root()?)?;
    create_private_dir(&base_dir()?)?;
    create_private_dir(&img_dir()?)?;
    create_private_dir(&runtime_root()?)?;
    Ok(())
}

/// Create a directory (and parents) restricted to the owner (0700).
pub fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("creating directory {}", path.display()))?;
    set_private(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perm = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(path, perm)
        .with_context(|| format!("chmod 700 {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<()> {
    Ok(())
}
