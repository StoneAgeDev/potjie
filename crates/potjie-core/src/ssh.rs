//! SSH: the per-box keypair Potjie uses, and building `ssh` invocations.
//!
//! Each box gets its own ed25519 key. The public half goes into the cloud-init
//! seed (so the guest trusts it on first boot); the private half stays in the
//! box directory. Potjie reaches the guest over slirp's host-port forward, so
//! the connection is always `ssh -p <port> user@127.0.0.1`.

use crate::paths::BoxPaths;
use anyhow::{Context, Result};
use rand_core::OsRng;
use ssh_key::{Algorithm, LineEnding, PrivateKey};
use std::process::Command;

/// Generate the box's ed25519 keypair if it does not already exist.
/// Returns the public key as an OpenSSH `authorized_keys` line.
pub fn ensure_keypair(paths: &BoxPaths) -> Result<String> {
    if paths.ssh_key.exists() && paths.ssh_pubkey.exists() {
        return std::fs::read_to_string(&paths.ssh_pubkey)
            .with_context(|| format!("reading {}", paths.ssh_pubkey.display()))
            .map(|s| s.trim().to_string());
    }

    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
        .context("generating ed25519 key")?;
    let priv_pem = key
        .to_openssh(LineEnding::LF)
        .context("encoding private key")?;
    let pub_line = key
        .public_key()
        .to_openssh()
        .context("encoding public key")?;

    crate::paths::write_private(&paths.ssh_key, priv_pem.as_bytes())?;
    std::fs::write(&paths.ssh_pubkey, format!("{pub_line}\n"))
        .with_context(|| format!("writing {}", paths.ssh_pubkey.display()))?;
    Ok(pub_line)
}

/// Build an `ssh` command targeting the box on `port`, running `command` if
/// given (otherwise an interactive login shell).
pub fn ssh_command(paths: &BoxPaths, username: &str, port: u16, command: Option<&str>) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-i")
        .arg(&paths.ssh_key)
        .args(["-p", &port.to_string()])
        // Skip the system-wide ssh_config: its Include'd files (e.g. the
        // systemd-ssh-proxy stub) can have permissions that OpenSSH treats as
        // fatal, killing the connection before it starts. We never need proxy
        // rules or system-level overrides for a localhost-only slirp forward.
        .args(["-F", "/dev/null"])
        // The guest is freshly minted and reachable only via our forward, so a
        // pinned known_hosts adds nothing; keep it from polluting ~/.ssh.
        .args(["-o", "StrictHostKeyChecking=no"])
        .args(["-o", "UserKnownHostsFile=/dev/null"])
        .args(["-o", "LogLevel=ERROR"]);
    cmd.arg(format!("{username}@127.0.0.1"));
    if let Some(c) = command {
        cmd.arg(c);
    }
    cmd
}
