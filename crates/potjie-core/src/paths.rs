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
///
/// In the Flatpak this is the sandbox's own `$XDG_RUNTIME_DIR`, which is fine
/// because qemu/ssh are *bundled* and run in-sandbox — they share the same mount
/// and PID namespaces as the daemon, so LUKS key material and sockets stay on
/// tmpfs and never touch persistent storage.
pub fn runtime_root() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Ok(base.join("potjie"))
}

/// True if we're running inside a Flatpak sandbox.
pub fn in_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// True if `path` lives on a RAM-backed filesystem (tmpfs or ramfs), so anything
/// written there never reaches persistent storage. Used to guard LUKS key
/// material before it's written. On non-Linux we can't cheaply tell, so we
/// optimistically return `true` rather than block the app.
#[cfg(target_os = "linux")]
pub fn is_tmpfs(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    const RAMFS_MAGIC: i64 = 0x8584_58f6;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return false;
    }
    let ty = buf.f_type as i64;
    ty == TMPFS_MAGIC || ty == RAMFS_MAGIC
}

#[cfg(not(target_os = "linux"))]
pub fn is_tmpfs(_path: &Path) -> bool {
    true
}

/// Validate a box name: it becomes a directory name *and* a host hostname, and is
/// taken straight off the wire by the daemon — so reject anything that could path-
/// traverse (`..`, `/`) or make an invalid hostname.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 63 {
        anyhow::bail!("box name must be 1..=63 characters");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        anyhow::bail!("box name may only contain ASCII letters, digits and '-'");
    }
    if name.starts_with('-') || name.ends_with('-') {
        anyhow::bail!("box name must not start or end with '-'");
    }
    Ok(())
}

/// Directory holding the daemon's control socket.
///
/// Under Flatpak this **must** be shared across app instances: the GUI and each
/// `flatpak run … proxy` ProxyCommand run in *separate* sandbox instances with
/// their own private `$XDG_RUNTIME_DIR`, so a socket there would give every
/// instance its *own* daemon — and two daemons fighting over the (shared on-disk)
/// box disk produces a qcow2 write-lock error. `~/.potjie/run` is on the shared
/// home, identical in every instance and on the host. Only the socket goes here;
/// secrets and pidfiles stay on the tmpfs `runtime_root`, owned by the single
/// daemon that actually spawns qemu. Native keeps the socket on tmpfs too.
pub fn control_dir() -> Result<PathBuf> {
    if in_flatpak() {
        Ok(root()?.join("run"))
    } else {
        runtime_root()
    }
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
        validate_name(name)?;
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
    create_private_dir(&control_dir()?)?;
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

/// Write `bytes` to `path` as an owner-only (0600) file, truncating any existing
/// content. The single home for the "private file" pattern used for secrets, ssh
/// keys, and the managed ssh config.
#[cfg(unix)]
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}
