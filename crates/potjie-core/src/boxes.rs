//! High-level box lifecycle — the API the CLI and GTK UI both call.
//!
//! A "box" (`Vm` in code) is a directory under `~/.potjie/img/<name>` holding a
//! LUKS qcow2 root disk, a cloud-init seed, an ssh keypair, and `box.json`.
//! Creating one converts the pinned base into an encrypted disk; starting one
//! boots qemu; the box is only ever decrypted while a `Vm` is running.

use crate::config::{BoxConfig, DEFAULT_BASE};
use crate::paths::{self, BoxPaths};
use crate::{base, disk, qemu, seed, ssh};
use anyhow::{bail, Context, Result};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// A single box: its on-disk config and resolved paths.
#[derive(Clone)]
pub struct Vm {
    pub cfg: BoxConfig,
    pub paths: BoxPaths,
}

/// Positive, checked evidence that a box is genuinely decrypted and live — not a
/// reassuring message, but facts gathered by actually talking to the guest.
#[derive(Debug, Clone)]
pub struct DecryptionProof {
    /// The on-disk image really is LUKS-encrypted at rest (`qemu-img info`).
    pub disk_is_luks: bool,
    /// We authenticated to the guest with the box's own key — only possible if
    /// qemu decrypted the disk with the passphrase and the guest booted.
    pub ssh_authenticated: bool,
    /// `/etc/machine-id` read live from the (decrypted) root filesystem.
    pub machine_id: String,
    /// Backing source of `/` inside the guest.
    pub root_source: String,
    /// `uname -srm` from the running guest.
    pub kernel: String,
}

impl DecryptionProof {
    /// True only when every check passed: encrypted at rest AND proven decrypted
    /// live. This is the "100% confidence" gate.
    pub fn is_verified(&self) -> bool {
        self.disk_is_luks && self.ssh_authenticated && !self.machine_id.is_empty()
    }
}

/// Positive, checked evidence that a box is **sealed** — encrypted at rest and
/// not decrypted or reachable anywhere right now. This is the assurance you want
/// when you're done: proof your data is locked away, not a comforting message.
#[derive(Debug, Clone)]
pub struct SealProof {
    /// The on-disk image is LUKS-encrypted at rest (`qemu-img info`).
    pub disk_is_luks: bool,
    /// A qemu process is decrypting this box right now (scanned from `/proc`,
    /// not just our pidfile). For a sealed box this is **false**.
    pub qemu_running: bool,
    /// The box still answers on its forwarded SSH port. Sealed ⇒ **false**.
    pub ssh_reachable: bool,
    /// Leftover key material exists in the runtime dir. Sealed ⇒ **false**.
    pub secret_files_present: bool,
}

impl SealProof {
    /// True only when the box is encrypted at rest AND nothing is decrypting or
    /// exposing it. The "100% confidence it's locked" gate.
    pub fn is_sealed(&self) -> bool {
        self.disk_is_luks
            && !self.qemu_running
            && !self.ssh_reachable
            && !self.secret_files_present
    }
}

impl Vm {
    /// Load an existing box by name.
    pub fn load(name: &str) -> Result<Self> {
        let paths = BoxPaths::new(name)?;
        if !paths.exists() {
            bail!("no such box: {name}");
        }
        let cfg: BoxConfig = serde_json::from_slice(
            &std::fs::read(&paths.config)
                .with_context(|| format!("reading {}", paths.config.display()))?,
        )
        .context("parsing box.json")?;
        Ok(Self { cfg, paths })
    }

    /// List all boxes, newest config first is not guaranteed — sorted by name.
    pub fn list() -> Result<Vec<Self>> {
        let dir = paths::img_dir()?;
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if let Some(name) = e.file_name().to_str() {
                    if let Ok(vm) = Vm::load(name) {
                        out.push(vm);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.cfg.name.cmp(&b.cfg.name));
        Ok(out)
    }

    /// Create a brand-new box from the default base image.
    ///
    /// `progress` reports base-image download progress (bytes, total).
    pub fn create(
        cfg: BoxConfig,
        passphrase: &str,
        progress: impl FnMut(u64, u64),
    ) -> Result<Self> {
        validate_name(&cfg.name)?;
        let paths = BoxPaths::new(&cfg.name)?;
        if paths.exists() || paths.dir.exists() {
            bail!("box '{}' already exists", cfg.name);
        }

        // Fetch+verify base first; it's the slow, failure-prone step.
        let base_path = base::ensure_base(&DEFAULT_BASE, progress)?;

        paths::create_private_dir(&paths.dir)?;
        // From here on, clean up the directory if anything fails.
        let result = (|| {
            let pubkey = ssh::ensure_keypair(&paths)?;
            disk::create_encrypted(&base_path, &paths.disk, cfg.disk_gib, passphrase)?;
            seed::write_seed(&paths.seed, &cfg, &pubkey)?;
            std::fs::write(&paths.config, serde_json::to_vec_pretty(&cfg)?)
                .with_context(|| format!("writing {}", paths.config.display()))?;
            Ok::<_, anyhow::Error>(())
        })();

        if let Err(e) = result {
            std::fs::remove_dir_all(&paths.dir).ok();
            return Err(e);
        }
        Ok(Self { cfg, paths })
    }

    pub fn status(&self) -> Result<qemu::Status> {
        qemu::status(&self.paths)
    }

    /// Persist the in-memory [`BoxConfig`] back to `box.json` (e.g. after editing
    /// port forwards).
    pub fn save_config(&self) -> Result<()> {
        std::fs::write(&self.paths.config, serde_json::to_vec_pretty(&self.cfg)?)
            .with_context(|| format!("writing {}", self.paths.config.display()))?;
        Ok(())
    }

    /// Boot the box, returning the forwarded SSH port.
    pub fn start(&self, passphrase: &str) -> Result<u16> {
        qemu::start(&self.paths, &self.cfg, passphrase)
    }

    /// Stop the box (graceful, with fallback).
    pub fn stop(&self) -> Result<()> {
        qemu::stop(&self.paths, Duration::from_secs(30))
    }

    /// Block until the guest accepts SSH (cloud-init may still be running on a
    /// first boot, so allow a generous timeout).
    pub fn wait_for_ssh(&self, timeout: Duration) -> Result<u16> {
        let port = self
            .status()?
            .ssh_port
            .context("box is not running")?;
        let addr = ("127.0.0.1", port)
            .to_socket_addrs_first()
            .context("resolving forward address")?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            // A bare TCP connect isn't enough: with slirp the forwarded port
            // accepts before the guest's sshd is actually up. Require the SSH
            // identification banner ("SSH-…"), so callers (and a host app
            // resuming over ssh) only proceed once a real session will succeed.
            if Self::ssh_banner_ok(&addr) {
                return Ok(port);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        bail!("timed out waiting for SSH on 127.0.0.1:{port}")
    }

    /// Connect and confirm the peer actually speaks SSH (sends an `SSH-` banner),
    /// not just that the forwarded TCP port accepted.
    fn ssh_banner_ok(addr: &std::net::SocketAddr) -> bool {
        use std::io::Read;
        let Ok(mut stream) = TcpStream::connect_timeout(addr, Duration::from_secs(2)) else {
            return false;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buf = [0u8; 4];
        matches!(stream.read(&mut buf), Ok(n) if n == 4) && &buf == b"SSH-"
    }

    /// Build an `ssh` command into the running box. `command` runs non-interactively
    /// if given, otherwise an interactive shell.
    pub fn ssh_command(&self, command: Option<&str>) -> Result<std::process::Command> {
        let port = self.status()?.ssh_port.context("box is not running")?;
        Ok(ssh::ssh_command(&self.paths, &self.cfg.username, port, command))
    }

    /// Gather checked evidence that the box is genuinely decrypted and running.
    ///
    /// Two independent facts: (1) `qemu-img info` shows the disk is LUKS-encrypted
    /// at rest, and (2) we authenticate to the guest with the box's key and read
    /// its root filesystem live — which is *only* possible if qemu decrypted that
    /// disk with the correct passphrase. Together: the box is encrypted at rest
    /// and proven decrypted right now.
    pub fn verify_decrypted(&self) -> Result<DecryptionProof> {
        let disk_is_luks = disk::is_luks_encrypted(&self.paths.disk).unwrap_or(false);

        // Single round-trip reading positive evidence from the decrypted guest.
        const PROBE: &str = "printf 'mid=%s\\n' \"$(cat /etc/machine-id)\"; \
             printf 'root=%s\\n' \"$(findmnt -no SOURCE / 2>/dev/null || \
                 df --output=source / 2>/dev/null | tail -1)\"; \
             printf 'kern=%s\\n' \"$(uname -srm)\"";
        let mut cmd = self.ssh_command(Some(PROBE))?;
        let out = cmd.output().context("probing guest over ssh")?;
        let ssh_authenticated = out.status.success();
        let text = String::from_utf8_lossy(&out.stdout);
        let field = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")))
                .unwrap_or("")
                .trim()
                .to_string()
        };

        Ok(DecryptionProof {
            disk_is_luks,
            ssh_authenticated,
            machine_id: field("mid"),
            root_source: field("root"),
            kernel: field("kern"),
        })
    }

    /// Gather checked evidence that the box is **sealed**: encrypted at rest and
    /// not decrypted/reachable anywhere. This is the post-use assurance.
    pub fn verify_sealed(&self) -> Result<SealProof> {
        let disk_is_luks = disk::is_luks_encrypted(&self.paths.disk).unwrap_or(false);

        // Trust /proc over our own pidfile here — catch anything decrypting it.
        let status = self.status()?;
        let qemu_running = status.running || qemu::box_process_running(&self.cfg.name);

        // If a forward is somehow still up, prove it's actually unreachable.
        let ssh_reachable = match status.ssh_port {
            Some(port) => TcpStream::connect_timeout(
                &("127.0.0.1", port).to_socket_addrs_first()?,
                Duration::from_millis(300),
            )
            .is_ok(),
            None => false,
        };

        // Any leftover `secret-*` key files in the runtime dir?
        let secret_files_present = std::fs::read_dir(&self.paths.runtime_dir)
            .map(|rd| {
                rd.flatten().any(|e| {
                    e.file_name()
                        .to_str()
                        .map(|n| n.starts_with("secret-"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        Ok(SealProof {
            disk_is_luks,
            qemu_running,
            ssh_reachable,
            secret_files_present,
        })
    }

    /// Like [`Vm::ssh_command`] but with X11 forwarding for guest GUI apps.
    pub fn ssh_command_x11(&self, command: Option<&str>) -> Result<std::process::Command> {
        let port = self.status()?.ssh_port.context("box is not running")?;
        Ok(ssh::ssh_command_opts(
            &self.paths,
            &self.cfg.username,
            port,
            command,
            true,
        ))
    }

    /// Permanently delete the box (must be stopped).
    pub fn delete(&self) -> Result<()> {
        if self.status()?.running {
            bail!("box '{}' is running; stop it first", self.cfg.name);
        }
        std::fs::remove_dir_all(&self.paths.dir)
            .with_context(|| format!("removing {}", self.paths.dir.display()))?;
        std::fs::remove_dir_all(&self.paths.runtime_dir).ok();
        // The box's generated launchers now point at a box that no longer exists;
        // remove them so they don't linger as dead menu entries. (Cascades for the
        // CLI `potjie rm` too, not just the GUI.)
        if let Err(e) = crate::desktop::remove_wrappers_for_box(&self.cfg.name) {
            eprintln!("warning: could not remove launchers for '{}': {e}", self.cfg.name);
        }
        Ok(())
    }
}

/// Box names become directory names and host hostnames, so keep them strict.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 63 {
        bail!("box name must be 1..=63 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        bail!("box name may only contain ASCII letters, digits and '-'");
    }
    if name.starts_with('-') || name.ends_with('-') {
        bail!("box name must not start or end with '-'");
    }
    Ok(())
}

/// Tiny helper: first resolved socket address.
trait FirstAddr {
    fn to_socket_addrs_first(&self) -> Result<std::net::SocketAddr>;
}
impl FirstAddr for (&str, u16) {
    fn to_socket_addrs_first(&self) -> Result<std::net::SocketAddr> {
        use std::net::ToSocketAddrs;
        self.to_socket_addrs()?
            .next()
            .context("no address resolved")
    }
}
