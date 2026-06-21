//! Discovering box apps and generating wrapper launchers.
//!
//! A wrapper `.desktop` is a *lifecycle binding*: launching it boots the box,
//! runs an app *inside* the box (shown on the host via X-forwarded SSH) for as
//! long as that app lives, then stops (re-locks) the box.
//!
//! [`sync_ssh_config`] keeps a stable `potjie-<box>` SSH alias pointing at the
//! box's current forwarded port, so `ssh potjie-<box>` and the per-box port
//! forwards always resolve while the box is up.

use crate::boxes::Vm;
use crate::paths;
use anyhow::{anyhow, Context, Result};
use ashpd::desktop::dynamic_launcher::{
    DynamicLauncherProxy, InstallOptions, LauncherType, PrepareInstallOptions,
};
use ashpd::desktop::Icon;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Our Flatpak app id. The DynamicLauncher portal requires every launcher we
/// install to be prefixed with it, so we can never name (or clobber) a launcher
/// belonging to another app.
const APP_ID: &str = "com.potjie.Potjie";

/// Launcher icon, embedded so we always have icon *bytes* to hand the portal
/// (PrepareInstall rejects themed-name icons — it wants raw image bytes).
const ICON_PNG: &[u8] = include_bytes!("../../../icons/potjie-128.png");

/// Drive a portal future to completion on a throwaway current-thread runtime.
/// Launcher create/remove are rare, user-initiated actions, so spinning a
/// short-lived runtime per call is fine and keeps the rest of the crate sync.
fn block_on<F: std::future::Future>(fut: F) -> Result<F::Output> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime for portal call")?
        .block_on(fut))
}

/// Where a wrapped app runs. Currently always inside the box (guest); the enum
/// is kept so the launcher arg format (`--launch <box> <kind> <app>`) stays
/// stable for already-installed launchers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Application inside the box (guest).
    Vm,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Vm => "vm",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "vm" => Some(Kind::Vm),
            _ => None,
        }
    }
}

/// A `.desktop` application (on the host or in the guest).
#[derive(Debug, Clone)]
pub struct DesktopEntry {
    /// Desktop file id (basename without `.desktop`), e.g. `org.gnome.gedit`.
    pub id: String,
    /// Human name from `Name=`.
    pub name: String,
    /// Raw `Exec=` line (informational / used to launch host apps).
    pub exec: String,
}

impl Vm {
    /// List GUI apps installed in the (running) guest.
    pub fn list_guest_apps(&self) -> Result<Vec<DesktopEntry>> {
        // Emit "id\tname\texec" per desktop file; parse on the host.
        const SCRIPT: &str = r#"for f in /usr/share/applications/*.desktop "$HOME"/.local/share/applications/*.desktop; do [ -f "$f" ] || continue; grep -q '^NoDisplay=true' "$f" && continue; id=$(basename "$f" .desktop); name=$(sed -n 's/^Name=//p' "$f" | head -1); ex=$(sed -n 's/^Exec=//p' "$f" | head -1); printf '%s\t%s\t%s\n' "$id" "$name" "$ex"; done"#;

        let mut cmd = self.ssh_command(Some(SCRIPT))?;
        let out = cmd.output().context("listing guest apps over ssh")?;
        if !out.status.success() {
            anyhow::bail!(
                "guest app listing failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let mut entries = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut parts = line.splitn(3, '\t');
            let (Some(id), name, exec) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            entries.push(DesktopEntry {
                id: id.to_string(),
                name: name.unwrap_or(id).to_string(),
                exec: exec.unwrap_or("").to_string(),
            });
        }
        finish(entries)
    }
}

/// Sort, dedup, and case-insensitively order a list of entries.
fn finish(mut entries: Vec<DesktopEntry>) -> Result<Vec<DesktopEntry>> {
    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    entries.dedup_by(|a, b| a.id == b.id);
    Ok(entries)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The portal `desktop_file_id` for a launcher. Deterministic, so re-creating a
/// launcher for the same app simply replaces it. Must start with [`APP_ID`].
fn file_id(box_name: &str, kind: Kind, app_id: &str) -> String {
    format!(
        "{APP_ID}.{}-{}-{}.desktop",
        sanitize(box_name),
        kind.as_str(),
        sanitize(app_id)
    )
}

// ---- launcher registry ---------------------------------------------------
//
// The DynamicLauncher portal installs/uninstalls launchers but has no API to
// *enumerate* the ones we created — and we no longer have filesystem access to
// scan `~/.local/share/applications`. So we keep our own small registry in
// Potjie's data dir and self-heal it against the portal (`GetDesktopEntry`) so a
// launcher the user removed by other means doesn't linger in our list.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    file_id: String,
    box_name: String,
    kind: String,
    app_id: String,
    name: String,
}

fn registry_path() -> Result<PathBuf> {
    Ok(paths::root()?.join("launchers.json"))
}

fn load_registry() -> Vec<Record> {
    registry_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_registry(recs: &[Record]) -> Result<()> {
    let p = registry_path()?;
    if let Some(parent) = p.parent() {
        paths::create_private_dir(parent)?;
    }
    let json = serde_json::to_string_pretty(recs).context("serializing launcher registry")?;
    std::fs::write(&p, json).with_context(|| format!("writing {}", p.display()))
}

/// A launcher Potjie installed via the DynamicLauncher portal.
#[derive(Debug, Clone)]
pub struct Wrapper {
    /// The portal `desktop_file_id` (used to uninstall it).
    pub file_id: String,
    pub name: String,
    pub box_name: String,
    pub kind: Kind,
    pub app_id: String,
}

/// Install a launcher for `entry` in `box_name` via the DynamicLauncher portal.
/// Shows the portal's one-time install dialog; `launcher` is the absolute path to
/// the binary that performs the boot/run/stop flow (the running `potjie-gtk`).
///
/// NB: this blocks on the portal dialog, so callers must run it off the UI thread.
pub fn create_wrapper(
    box_name: &str,
    kind: Kind,
    entry: &DesktopEntry,
    display_name: &str,
    launcher: &Path,
) -> Result<()> {
    let id = file_id(box_name, kind, &entry.id);
    let where_ = "inside";
    // The portal rewrites/validates Exec (and adds Icon=); we keep our X-Potjie-*
    // markers, which it preserves verbatim.
    let desktop_entry = format!(
        "[Desktop Entry]\n\
Type=Application\n\
Name={display}\n\
Comment=Potjie: run {app} {where_} encrypted box '{box_name}'\n\
Exec={launcher} --launch {box_name} {kind} {app_id}\n\
Terminal=false\n\
X-Potjie-Box={box_name}\n\
X-Potjie-Kind={kind}\n\
X-Potjie-App={app_id}\n",
        display = display_name,
        app = entry.name,
        launcher = launcher.display(),
        kind = kind.as_str(),
        app_id = entry.id,
    );

    block_on(async {
        let proxy = DynamicLauncherProxy::new().await?;
        let opts = PrepareInstallOptions::default()
            .set_launcher_type(LauncherType::Application)
            .set_editable_name(false)
            .set_editable_icon(false);
        let token = proxy
            .prepare_install(None, display_name, Icon::Bytes(ICON_PNG.to_vec()), opts)
            .await?
            .response()?
            .token()
            .to_owned();
        proxy
            .install(&token, &id, &desktop_entry, InstallOptions::default())
            .await
    })?
    .map_err(|e| anyhow!("portal install failed: {e}"))?;

    let mut recs = load_registry();
    recs.retain(|r| r.file_id != id);
    recs.push(Record {
        file_id: id,
        box_name: box_name.to_string(),
        kind: kind.as_str().to_string(),
        app_id: entry.id.clone(),
        name: display_name.to_string(),
    });
    save_registry(&recs)
}

/// List the launchers Potjie installed, optionally filtered to one box. Reads our
/// registry only (no D-Bus), so it's cheap and safe to call from the UI thread;
/// stale entries are pruned lazily on removal and by [`prune_wrappers`].
pub fn list_wrappers(box_name: Option<&str>) -> Result<Vec<Wrapper>> {
    let mut out: Vec<Wrapper> = load_registry()
        .into_iter()
        .filter(|r| box_name.is_none_or(|b| b == r.box_name))
        .map(|r| Wrapper {
            file_id: r.file_id,
            name: r.name,
            box_name: r.box_name,
            kind: Kind::parse(&r.kind).unwrap_or(Kind::Vm),
            app_id: r.app_id,
        })
        .collect();
    out.sort_by_key(|w| w.name.to_lowercase());
    Ok(out)
}

/// Drop registry entries whose launcher the portal no longer knows about (e.g.
/// the user deleted it through their desktop's settings). Does one
/// `GetDesktopEntry` per record, so callers should run it off the UI thread.
pub fn prune_wrappers() -> Result<()> {
    let recs = load_registry();
    if recs.is_empty() {
        return Ok(());
    }
    let live = block_on(async {
        let Ok(proxy) = DynamicLauncherProxy::new().await else {
            return recs.clone(); // portal unavailable: trust the registry
        };
        let mut live = Vec::new();
        for r in &recs {
            if proxy.desktop_entry(&r.file_id).await.is_ok() {
                live.push(r.clone());
            }
        }
        live
    })?;
    if live.len() != recs.len() {
        save_registry(&live)?;
    }
    Ok(())
}

/// Uninstall one launcher via the portal and drop it from the registry. The
/// uninstall is best-effort (a launcher already gone elsewhere still leaves us in
/// the desired end state), so this only errors if the portal itself is
/// unreachable. Run off the UI thread.
pub fn remove_wrapper(file_id: &str) -> Result<()> {
    block_on(async {
        let proxy = DynamicLauncherProxy::new().await?;
        let _ = proxy.uninstall(file_id, Default::default()).await; // best-effort
        Ok::<(), ashpd::Error>(())
    })?
    .map_err(|e| anyhow!("portal unreachable: {e}"))?;

    let mut recs = load_registry();
    let before = recs.len();
    recs.retain(|r| r.file_id != file_id);
    if recs.len() != before {
        save_registry(&recs)?;
    }
    Ok(())
}

/// Uninstall every launcher for `box_name` (they go stale when the box is
/// deleted). Returns how many were removed. Run off the UI thread.
pub fn remove_wrappers_for_box(box_name: &str) -> Result<usize> {
    let mine: Vec<String> = load_registry()
        .into_iter()
        .filter(|r| r.box_name == box_name)
        .map(|r| r.file_id)
        .collect();
    let mut n = 0;
    for id in &mine {
        if remove_wrapper(id).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

// ---- SSH alias for host apps --------------------------------------------

/// Potjie's managed ssh config fragment (`~/.potjie/ssh/config`). Public so the
/// forward manager can point `ssh -F` at the same file the aliases live in.
pub fn ssh_config_path() -> Result<PathBuf> {
    Ok(paths::root()?.join("ssh").join("config"))
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

    let mut body = String::from("# Managed by Potjie. Do not edit; regenerated on box start/stop.\n");
    for vm in Vm::list().unwrap_or_default() {
        let Ok(st) = vm.status() else { continue };
        let (true, Some(port)) = (st.running, st.ssh_port) else { continue };
        // ControlMaster + a per-box ControlPath enable SSH connection multiplexing,
        // so the daemon can hold one background master and add/remove port forwards
        // live (`ssh -O forward`/`-O cancel`) without restarting the box.
        let control = crate::forward::control_path(&vm.cfg.name)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        body.push_str(&format!(
            "\nHost potjie-{name}\n\
\tHostName 127.0.0.1\n\
\tPort {port}\n\
\tUser {user}\n\
\tIdentityFile {key}\n\
\tIdentitiesOnly yes\n\
\tStrictHostKeyChecking no\n\
\tUserKnownHostsFile /dev/null\n\
\tLogLevel ERROR\n\
\tSetEnv TERM=xterm-256color\n\
\tControlMaster auto\n\
\tControlPath {control}\n\
\tControlPersist 30s\n\
\tExitOnForwardFailure no\n",
            name = vm.cfg.name,
            user = vm.cfg.username,
            key = vm.paths.ssh_key.display(),
        ));
        // Persisted port forwards: any fresh connection (and the daemon's master)
        // picks these up; live edits are reconciled in `forward::reload`.
        for f in &vm.cfg.forwards {
            body.push_str(&f.config_line());
        }
    }
    write_private(&frag, body.as_bytes())?;
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
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}
