//! Host SSH integration: the managed `potjie-<box>` aliases.
//!
//! [`sync_ssh_config`] keeps a stable `potjie-<box>` SSH alias pointing at the
//! box's current forwarded port, so `ssh potjie-<box>` (and the per-box port
//! forwards) always resolve while the box is up — and the host-facing alias can
//! boot a locked box on demand via its `ProxyCommand`.

use crate::boxes::Vm;
use crate::paths;
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Our Flatpak app id, used to re-enter the sandbox as a box's `ProxyCommand`.
const APP_ID: &str = "com.potjie.Potjie";

// ---- SSH alias for host apps --------------------------------------------

/// Potjie's managed ssh config fragment (`~/.potjie/ssh/config`). Public so the
/// forward manager can point `ssh -F` at the same file the aliases live in.
pub fn ssh_config_path() -> Result<PathBuf> {
    Ok(paths::root()?.join("ssh").join("config"))
}

/// The identity + hardening lines shared by both managed `Host` blocks: which key
/// to use, and the localhost-forward-appropriate host-key handling. Each line is
/// tab-indented and newline-terminated so it drops straight into a `Host` stanza.
fn identity_lines(user: &str, key: &str) -> String {
    format!(
        "\tUser {user}\n\
\tIdentityFile {key}\n\
\tIdentitiesOnly yes\n\
\tStrictHostKeyChecking no\n\
\tUserKnownHostsFile /dev/null\n\
\tLogLevel ERROR\n"
    )
}

/// Rebuild the `potjie-<box>` SSH aliases for all *running* boxes and make sure
/// the user's `~/.ssh/config` includes our fragment. This is what lets a host
/// app (VS Code Remote-SSH, `ssh potjie-dev`, …) reach a box at a stable name
/// even though the forwarded port changes per boot.
pub fn sync_ssh_config() -> Result<()> {
    let frag = ssh_config_path()?;
    if let Some(parent) = frag.parent() {
        paths::create_private_dir(parent)?;
    }

    // How the host's ssh re-invokes Potjie as the box's ProxyCommand. Under a
    // Flatpak the host ssh must re-enter the sandbox via `flatpak run`; natively
    // it's the `potjie` binary directly.
    let proxy_cmd = if paths::in_flatpak() {
        format!("flatpak run --command=potjie {APP_ID} proxy")
    } else {
        format!("{} proxy", crate::tools::potjie_bin())
    };

    let mut body = String::from("# Managed by Potjie. Do not edit; regenerated on box start/stop.\n");
    for vm in Vm::list().unwrap_or_default() {
        let identity = identity_lines(&vm.cfg.username, &vm.paths.ssh_key.display().to_string());

        // Host-facing alias for *every* box (running or not). Its ProxyCommand
        // boots the box on connect (the daemon prompts the passphrase via the GUI
        // when it's locked) and re-locks it when the session ends. No port here —
        // the proxy learns the live port from the daemon at connect time.
        body.push_str(&format!(
            "\nHost potjie-{name}\n\
\tHostName {name}\n\
{identity}\
\tSetEnv TERM=xterm-256color\n\
\tConnectTimeout 180\n\
\tProxyCommand {proxy_cmd} {name}\n",
            name = vm.cfg.name,
        ));

        // Direct alias for a *running* box: used by the daemon's own forward
        // control master. It connects straight to the forwarded port with no
        // ProxyCommand, so it never takes a lease, and it carries the box's port
        // forwards. ControlMaster multiplexing lets forwards be edited live
        // (`ssh -O forward`/`-O cancel`) without a restart.
        let Ok(st) = vm.status() else { continue };
        let (true, Some(port)) = (st.running, st.ssh_port) else { continue };
        let control = crate::forward::control_path(&vm.cfg.name)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        body.push_str(&format!(
            "\nHost potjie-fwd-{name}\n\
\tHostName 127.0.0.1\n\
\tPort {port}\n\
{identity}\
\tControlMaster auto\n\
\tControlPath {control}\n\
\tControlPersist 30s\n\
\tExitOnForwardFailure no\n",
            name = vm.cfg.name,
        ));
        // Persisted port forwards ride the direct master; live edits are
        // reconciled in `forward::reload`.
        for f in &vm.cfg.forwards {
            body.push_str(&f.config_line());
        }
    }
    paths::write_private(&frag, body.as_bytes())?;
    // NB: we deliberately do *not* touch `~/.ssh/config` here. Potjie holds only
    // read-only access to it (least privilege — the user's SSH config can carry
    // ProxyCommand/IdentityFile and is too sensitive to grant write access to).
    // Adding the one-time `Include` line is the user's call; the GUI gates on
    // [`ssh_include_status`] and walks them through it.
    Ok(())
}

/// Path to the user's `~/.ssh/config` — where the one-time `Include` of our
/// managed fragment belongs.
pub fn user_ssh_config_path() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home directory")?.join(".ssh").join("config"))
}

/// The exact line the user must add to `~/.ssh/config` so host tools (terminal
/// `ssh`, VS Code Remote-SSH, …) resolve the `potjie-<box>` aliases.
pub fn ssh_include_line() -> Result<String> {
    Ok(format!("Include {}", ssh_config_path()?.display()))
}

/// Whether the user has opted out of host SSH integration. Stored as a marker
/// file in Potjie's own (writable) data dir, so the GUI gate stays dismissed.
fn ssh_include_optout_path() -> Result<PathBuf> {
    Ok(paths::root()?.join("skip-ssh-include"))
}

/// Record (or clear) the user's choice to skip the `~/.ssh/config` Include.
pub fn set_ssh_include_optout(skip: bool) -> Result<()> {
    let marker = ssh_include_optout_path()?;
    if skip {
        if let Some(parent) = marker.parent() {
            paths::create_private_dir(parent)?;
        }
        std::fs::write(&marker, b"").with_context(|| format!("writing {}", marker.display()))?;
    } else if marker.exists() {
        std::fs::remove_file(&marker).with_context(|| format!("removing {}", marker.display()))?;
    }
    Ok(())
}

/// State of the host-side SSH integration, from a read-only look at the user's
/// `~/.ssh/config` plus our opt-out marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncludeStatus {
    /// The `Include` line is present — host aliases resolve.
    Present,
    /// Missing, but the user explicitly opted out; proceed without nagging.
    OptedOut,
    /// Missing and not opted out — host aliases won't resolve until added.
    Missing,
}

/// Read-only check of whether `~/.ssh/config` includes our fragment. Never
/// writes (Potjie only holds `~/.ssh/config:ro`).
pub fn ssh_include_status() -> IncludeStatus {
    let present = (|| {
        let line = ssh_include_line().ok()?;
        let cfg = user_ssh_config_path().ok()?;
        let text = std::fs::read_to_string(&cfg).ok()?;
        Some(text.lines().any(|l| l.trim() == line))
    })()
    .unwrap_or(false);

    if present {
        IncludeStatus::Present
    } else if ssh_include_optout_path().map(|p| p.exists()).unwrap_or(false) {
        IncludeStatus::OptedOut
    } else {
        IncludeStatus::Missing
    }
}
