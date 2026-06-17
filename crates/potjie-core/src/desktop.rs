//! Discovering apps and generating host wrapper launchers.
//!
//! A wrapper `.desktop` is a *lifecycle binding*: launching it boots the box,
//! runs an app for as long as that app lives, then stops (re-locks) the box.
//! Two kinds:
//!
//!   * [`Kind::Vm`]   — the app lives *inside* the box and is shown on the host
//!     via X-forwarded SSH.
//!   * [`Kind::Host`] — a *native host* app runs on the host while the box is up,
//!     and talks into the box over local SSH (e.g. host VS Code + Remote-SSH to
//!     the `potjie-<box>` alias). Native UI, but all your code/tools stay sealed
//!     in the encrypted box.
//!
//! For the host case to "just work", [`sync_ssh_config`] keeps a stable
//! `potjie-<box>` SSH alias pointing at the box's current forwarded port.

use crate::boxes::Vm;
use crate::paths;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Where a wrapped app runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Native host application.
    Host,
    /// Application inside the box (guest).
    Vm,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Host => "host",
            Kind::Vm => "vm",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "host" => Some(Kind::Host),
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

/// Standard host directories that hold `.desktop` files.
fn host_app_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
    ];
    if let Some(data) = dirs::data_dir() {
        dirs.push(data.join("applications"));
        dirs.push(data.join("flatpak/exports/share/applications"));
    }
    dirs
}

/// List GUI apps installed on the host (excluding Potjie's own wrappers).
pub fn list_host_apps() -> Result<Vec<DesktopEntry>> {
    let mut entries = Vec::new();
    for dir in host_app_dirs() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
                continue;
            }
            let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            // Don't offer our own generated wrappers as wrappable apps.
            if id.is_empty() || id.starts_with("potjie-") {
                continue;
            }
            if let Some(entry) = parse_desktop_file(&path, &id) {
                entries.push(entry);
            }
        }
    }
    finish(entries)
}

/// Resolve a host app's `Exec`, with `.desktop` field codes (`%U`, `%f`, …)
/// stripped so it can be run directly.
pub fn resolve_host_exec(id: &str) -> Option<String> {
    for dir in host_app_dirs() {
        let path = dir.join(format!("{id}.desktop"));
        if let Some(entry) = parse_desktop_file(&path, id) {
            return Some(strip_field_codes(&entry.exec));
        }
    }
    None
}

/// Parse the `[Desktop Entry]` group of a `.desktop` file. Returns `None` for
/// hidden / non-application entries.
fn parse_desktop_file(path: &Path, id: &str) -> Option<DesktopEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_main = false;
    let (mut name, mut exec, mut typ) = (None, None, None);
    let mut no_display = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            name.get_or_insert_with(|| v.to_string());
        } else if let Some(v) = line.strip_prefix("Exec=") {
            exec.get_or_insert_with(|| v.to_string());
        } else if let Some(v) = line.strip_prefix("Type=") {
            typ.get_or_insert_with(|| v.to_string());
        } else if line == "NoDisplay=true" || line == "Hidden=true" {
            no_display = true;
        }
    }
    if no_display || typ.as_deref() != Some("Application") {
        return None;
    }
    Some(DesktopEntry {
        id: id.to_string(),
        name: name.unwrap_or_else(|| id.to_string()),
        exec: exec.unwrap_or_default(),
    })
}

fn strip_field_codes(exec: &str) -> String {
    // Remove %f %F %u %U %i %c %k etc. (single-letter field codes).
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some(&n) = chars.peek() {
                if n == '%' {
                    out.push('%');
                }
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// Sort, dedup, and case-insensitively order a list of entries.
fn finish(mut entries: Vec<DesktopEntry>) -> Result<Vec<DesktopEntry>> {
    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    entries.dedup_by(|a, b| a.id == b.id);
    Ok(entries)
}

/// The host applications directory (`~/.local/share/applications`).
pub fn applications_dir() -> Result<PathBuf> {
    let data = dirs::data_dir().context("could not determine data dir")?;
    Ok(data.join("applications"))
}

/// Write a host wrapper `.desktop` for `entry` in `box_name`, of the given
/// `kind`, displayed as `display_name`. `launcher` is the absolute path to the
/// binary that performs the boot/run/stop flow (the running `potjie-gtk`).
pub fn create_wrapper(
    box_name: &str,
    kind: Kind,
    entry: &DesktopEntry,
    display_name: &str,
    launcher: &Path,
) -> Result<PathBuf> {
    let dir = applications_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let file_id = format!("potjie-{}-{}-{}", box_name, kind.as_str(), sanitize(&entry.id));
    let path = dir.join(format!("{file_id}.desktop"));

    let where_ = match kind {
        Kind::Host => "on the host, connected into",
        Kind::Vm => "inside",
    };
    let contents = format!(
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
    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

// ---- SSH alias for host apps --------------------------------------------

/// Potjie's managed ssh config fragment (`~/.potjie/ssh/config`).
fn ssh_config_path() -> Result<PathBuf> {
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
        body.push_str(&format!(
            "\nHost potjie-{name}\n\
\tHostName 127.0.0.1\n\
\tPort {port}\n\
\tUser {user}\n\
\tIdentityFile {key}\n\
\tIdentitiesOnly yes\n\
\tStrictHostKeyChecking no\n\
\tUserKnownHostsFile /dev/null\n\
\tLogLevel ERROR\n",
            name = vm.cfg.name,
            user = vm.cfg.username,
            key = vm.paths.ssh_key.display(),
        ));
    }
    write_private(&frag, body.as_bytes())?;
    ensure_ssh_include(&frag)?;
    Ok(())
}

/// Ensure `~/.ssh/config` has an `Include` of our fragment (prepended once).
fn ensure_ssh_include(frag: &Path) -> Result<()> {
    let home = dirs::home_dir().context("no home directory")?;
    let ssh_dir = home.join(".ssh");
    paths::create_private_dir(&ssh_dir)?;
    let cfg = ssh_dir.join("config");
    let include = format!("Include {}", frag.display());

    let existing = std::fs::read_to_string(&cfg).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == include) {
        return Ok(());
    }
    // OpenSSH applies the first matching value, so put the Include at the top.
    let new = format!("{include}\n\n{existing}");
    write_private(&cfg, new.as_bytes())?;
    Ok(())
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
