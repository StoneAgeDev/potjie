//! `potjied` — Potjie's user-space guard daemon.
//!
//! It is the *only* thing that starts boxes, and it ties each running box to one
//! or more open client connections ("leases"). The guarantees it enforces:
//!
//!   * A box is decrypted/running only while a trusted client holds a lease.
//!   * When the last lease drops — including when a client crashes and the
//!     kernel closes its socket — the box is stopped and re-locked.
//!   * A watchdog stops any box found running with zero leases (e.g. one started
//!     out-of-band), and a startup sweep locks everything inherited from a
//!     previous run.
//!   * On shutdown the daemon stops every box it is managing.
//!
//! No privileges, no root: the control socket lives in the user-only runtime dir.
//!
//! This is a library crate: the daemon runs as `potjie daemon` (a hidden
//! subcommand of the multicall `potjie` binary), so [`run`] is its entry point.

use anyhow::{Context, Result};
use potjie_core::config::Forward;
use potjie_core::protocol::{BoxStatus, Request, Response};
use potjie_core::{forward, guard, paths, Vm};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Exit after this many seconds with zero leases (and no client activity), so the
/// daemon is session-scoped: spawned on demand, gone when idle. This also lets a
/// bundled distribution swap the binary on upgrade, and tears down the daemon's
/// `/nix/store` namespace when nothing needs it. `POTJIE_DAEMON_IDLE_SECS=0`
/// disables it (run forever). The GUI polls status ~1/s, so it stays warm while
/// the app is open.
const DEFAULT_IDLE_SECS: u64 = 90;

fn idle_secs() -> u64 {
    std::env::var("POTJIE_DAEMON_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_IDLE_SECS)
}

fn total_leases(leases: &Leases) -> u32 {
    leases.lock().unwrap().values().sum()
}

/// True if any box is currently running. Used to keep the daemon alive past its
/// idle deadline while it still owns a decrypted box (see the accept loop).
fn any_box_running() -> bool {
    Vm::list()
        .unwrap_or_default()
        .iter()
        .any(|vm| vm.status().map(|s| s.running).unwrap_or(false))
}

/// Lease counts, keyed by box name. Wrapped in a mutex shared across handlers.
type Leases = Arc<Mutex<HashMap<String, u32>>>;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Run the guard daemon. Blocks until shutdown (SIGTERM/SIGINT). Entry point for
/// the `potjie daemon` subcommand.
pub fn run() -> Result<()> {
    paths::ensure_layout()?;
    let sock = guard::socket_path()?;

    // Single instance: if someone is already listening, step aside.
    if UnixStream::connect(&sock).is_ok() {
        eprintln!("potjied already running at {}", sock.display());
        return Ok(());
    }
    // Remove a stale socket file from a previous crash.
    let _ = std::fs::remove_file(&sock);

    install_signal_handlers();

    let leases: Leases = Arc::new(Mutex::new(HashMap::new()));

    // Fail-safe: lock anything that survived a previous daemon.
    sweep(&leases);

    // Write the managed ssh fragment up front so `ssh potjie-<box>` resolves (and
    // can boot the box on demand via its ProxyCommand) for *every* box, even ones
    // that aren't running yet.
    refresh_aliases();

    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("binding {}", sock.display()))?;
    listener
        .set_nonblocking(true)
        .context("setting listener non-blocking")?;
    eprintln!("potjied listening on {}", sock.display());

    // Watchdog: periodically re-lock unleased boxes, fire start/stop desktop
    // notifications on every running-state transition (whatever caused it — GUI,
    // CLI, wrapper, or our own sweep), and honor shutdown.
    {
        let leases = leases.clone();
        std::thread::spawn(move || {
            // Seed with current state so we don't notify for boxes already up when
            // the daemon starts.
            let mut last: HashMap<String, bool> = Vm::list()
                .unwrap_or_default()
                .into_iter()
                .map(|vm| {
                    let running = vm.status().map(|s| s.running).unwrap_or(false);
                    (vm.cfg.name, running)
                })
                .collect();
            loop {
                if SHUTDOWN.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(2));
                sweep(&leases);
                notify_transitions(&mut last);
            }
        });
    }

    // Accept loop (non-blocking so we can react to shutdown). Idle-exit is decided
    // here, on the idle ticks, so it can never race with accepting a connection:
    // any accepted connection (even a status poll) counts as activity.
    let idle_limit = idle_secs();
    let mut idle_since = Instant::now();
    for stream in incoming(&listener) {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        let Some(stream) = stream else {
            // Idle tick: a live lease keeps us up; otherwise exit once we've been
            // quiet long enough.
            if total_leases(&leases) > 0 {
                idle_since = Instant::now();
            } else if idle_limit > 0 && idle_since.elapsed() >= Duration::from_secs(idle_limit) {
                // Never exit while a box is still running: we're the only thing
                // that re-locks it, and exiting would (a) strand a decrypted box
                // and (b) drop the lease connections of clients still using it,
                // so the *next* daemon re-locks the box out from under them. If a
                // box is up but unleased the watchdog will stop it shortly; stay
                // alive until it actually has.
                if any_box_running() {
                    idle_since = Instant::now();
                } else {
                    eprintln!("idle {idle_limit}s; exiting");
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        idle_since = Instant::now(); // a connection is activity
        let leases = leases.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, leases) {
                eprintln!("client error: {e}");
            }
        });
    }

    shutdown(&leases, &sock);
    Ok(())
}

/// Iterator over accepted connections; yields `None` when momentarily idle.
fn incoming(listener: &UnixListener) -> impl Iterator<Item = Option<UnixStream>> + '_ {
    std::iter::from_fn(move || {
        if SHUTDOWN.load(Ordering::SeqCst) {
            return None;
        }
        match listener.accept() {
            Ok((s, _)) => Some(Some(s)),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Some(None),
            Err(_) => Some(None),
        }
    })
}

/// Milliseconds since the UNIX epoch, for correlating daemon and proxy logs.
fn ts() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

static CONN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Handle one client connection until it closes, releasing all its leases.
fn handle(stream: UnixStream, leases: Leases) -> Result<()> {
    let id = CONN_ID.fetch_add(1, Ordering::SeqCst);
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);
    let mut held: HashSet<String> = HashSet::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[{}] conn#{id}: read error: {e}", ts());
                return Err(e.into());
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        // NB: never log `line` — an Acquire request carries the LUKS passphrase.
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch(req, &leases, &mut held),
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        };
        if let Err(e) = writeln!(writer, "{}", serde_json::to_string(&resp)?) {
            eprintln!("[{}] conn#{id}: write error: {e}", ts());
            return Err(e.into());
        }
    }

    // Connection closed (clean or crash): drop every lease it held.
    for name in held {
        release(&leases, &name);
    }
    Ok(())
}

fn dispatch(req: Request, leases: &Leases, held: &mut HashSet<String>) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Acquire { box_name, passphrase } => {
            match acquire(leases, &box_name, &passphrase) {
                Ok(port) => {
                    held.insert(box_name);
                    Response::Acquired { ssh_port: port }
                }
                Err(e) => Response::Error { message: format!("{e:#}") },
            }
        }
        Request::Release { box_name } => {
            if held.remove(&box_name) {
                release(leases, &box_name);
            }
            Response::Released
        }
        Request::ForceStop { box_name } => match force_stop(leases, &box_name) {
            Ok(()) => Response::Stopped,
            Err(e) => Response::Error { message: format!("{e:#}") },
        },
        Request::Status { box_name } => match status_of(leases, &box_name) {
            Ok(s) => Response::Status(s),
            Err(e) => Response::Error { message: format!("{e:#}") },
        },
        Request::List => Response::List { boxes: list_all(leases) },
        Request::SetForwards { box_name, forwards } => match set_forwards(&box_name, forwards) {
            Ok(()) => Response::ForwardsSet,
            Err(e) => Response::Error { message: format!("{e:#}") },
        },
        Request::GetForwards { box_name } => match get_forwards(&box_name) {
            Ok(forwards) => Response::Forwards { forwards },
            Err(e) => Response::Error { message: format!("{e:#}") },
        },
    }
}

/// Persist a box's port forwards and, if it's running, reconcile them live on the
/// SSH control master without a restart.
fn set_forwards(name: &str, forwards: Vec<Forward>) -> Result<()> {
    for f in &forwards {
        f.validate()?;
    }
    let mut vm = Vm::load(name)?;
    let old = std::mem::replace(&mut vm.cfg.forwards, forwards.clone());
    vm.save_config()?;
    // Regenerate the managed ssh config so future connections (and any restart)
    // carry the new forwards.
    refresh_aliases();
    if vm.status()?.running {
        // Make sure a master exists, then apply only the delta.
        if let Err(e) = forward::start_master(name) {
            eprintln!("forward master for '{name}': {e}");
        }
        forward::reload(name, &old, &forwards)
            .with_context(|| format!("applying forwards to '{name}'"))?;
    }
    Ok(())
}

fn get_forwards(name: &str) -> Result<Vec<Forward>> {
    Ok(Vm::load(name)?.cfg.forwards)
}

/// Start the box if it isn't already up, then bump its lease count.
fn acquire(leases: &Leases, name: &str, passphrase: &str) -> Result<u16> {
    let vm = Vm::load(name)?;

    // Decide whether we need to start the box under the lock, then immediately
    // release it. Holding the lock across vm.start() (which launches QEMU and
    // waits for it to daemonize) blocks every status-poll from the UI and makes
    // the window appear frozen. Pre-incrementing the count to 1 before we drop
    // the lock prevents a concurrent acquire or the watchdog from trying to
    // start/stop the same box at the same time.
    let needs_start = {
        let mut map = leases.lock().unwrap();
        let count = map.entry(name.to_string()).or_insert(0);
        let start = *count == 0 && !vm.status()?.running;
        if start {
            *count = 1; // reserve the slot; we own the first lease
        } else {
            *count += 1;
        }
        start
    };

    if needs_start {
        // An ssh-triggered boot (the `potjie proxy` ProxyCommand) has no terminal
        // to prompt at and sends an empty passphrase: ask the GUI to collect it.
        let pass = if passphrase.is_empty() {
            match prompt_passphrase(name) {
                Ok(p) => p,
                Err(e) => {
                    release(leases, name);
                    return Err(e);
                }
            }
        } else {
            passphrase.to_string()
        };
        // Start outside the lock so other requests (status polls, etc.) remain
        // responsive while QEMU launches.
        if let Err(e) = vm
            .start(&pass)
            .with_context(|| format!("starting box '{name}'"))
        {
            // Undo the pre-incremented lease so the box doesn't look leased.
            release(leases, name);
            return Err(e);
        }
    }

    // Wait (outside the lock) until SSH answers so the client gets a live port.
    let port = vm
        .wait_for_ssh(Duration::from_secs(180))
        .context("waiting for box SSH")
        .inspect_err(|_| release(leases, name))?; // undo our lease on failure
    refresh_aliases();
    // Bring up the SSH control master so the box's port forwards are live for as
    // long as it runs (best effort — a forward issue must never block the box).
    if let Err(e) = forward::start_master(name) {
        eprintln!("forward master for '{name}': {e}");
    }
    Ok(port)
}

/// Collect a box's LUKS passphrase via the GUI, for boots triggered by an ssh
/// connection (the `potjie proxy` ProxyCommand) that have no terminal to prompt
/// at. Spawns `potjie-gtk --ask-passphrase <box>` and reads the passphrase from
/// its stdout; a non-zero exit (user cancelled / no display) is an error so the
/// ssh connection fails cleanly and the box stays locked.
fn prompt_passphrase(name: &str) -> Result<String> {
    use std::io::Read;
    let mut child = std::process::Command::new(potjie_core::tools::potjie_gtk_bin())
        .arg("--ask-passphrase")
        .arg(name)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning passphrase prompt (potjie-gtk --ask-passphrase)")?;
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut out).ok();
    }
    let status = child.wait().context("waiting for passphrase prompt")?;
    if !status.success() {
        anyhow::bail!("passphrase prompt cancelled");
    }
    let prefix = potjie_core::tools::ASK_PASSPHRASE_PREFIX;
    let pass = out
        .lines()
        .find_map(|l| l.strip_prefix(prefix))
        .unwrap_or("")
        .to_string();
    if pass.is_empty() {
        anyhow::bail!("no passphrase entered");
    }
    Ok(pass)
}

/// Regenerate the `potjie-<box>` SSH aliases after any box starts or stops, so
/// host tools (`ssh potjie-<box>`, VS Code/Zed Remote-SSH, …) always resolve.
fn refresh_aliases() {
    if let Err(e) = potjie_core::desktop::sync_ssh_config() {
        eprintln!("ssh alias sync failed: {e}");
    }
}

/// Compare every box's running state against `last`; fire a desktop notification
/// on each flip and record the new state. Called from the watchdog so a
/// notification is sent no matter what caused the box to start or stop.
fn notify_transitions(last: &mut HashMap<String, bool>) {
    for vm in Vm::list().unwrap_or_default() {
        let running = vm.status().map(|s| s.running).unwrap_or(false);
        match last.get(&vm.cfg.name) {
            Some(&was) if was == running => {}
            Some(_) => {
                notify(&vm.cfg.name, running);
                last.insert(vm.cfg.name.clone(), running);
            }
            // First time we've seen this box: record without notifying.
            None => {
                last.insert(vm.cfg.name.clone(), running);
            }
        }
    }
}

fn notify_body(box_name: &str, running: bool) -> String {
    if running {
        format!("Box '{box_name}' started — decrypted and running.")
    } else {
        format!("Box '{box_name}' stopped — re-locked and sealed.")
    }
}

/// Quote a string as a GVariant text-format literal (double-quoted so embedded
/// single quotes — e.g. `Box 'dev' …` — don't need escaping).
fn gvariant_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Fire-and-forget a `gdbus call`, returning whether it succeeded.
fn gdbus_call(args: &[&str]) -> bool {
    std::process::Command::new("gdbus")
        .args(["call", "--session"])
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Send a desktop notification. Prefers the **Notification portal**
/// (`org.freedesktop.portal.Notification.AddNotification`) — it needs no bus
/// `--talk-name` finish-arg, so the Flatpak stays least-privilege. Outside
/// Flatpak (e.g. the daemon run directly during dev) the portal may be absent,
/// so we fall back to a direct `org.freedesktop.Notifications.Notify`; inside the
/// sandbox that name isn't reachable and the fallback simply no-ops.
fn send_notification(body: &str) {
    // AddNotification replaces any notification with the same id, so a unique id
    // per event lets start/stop stack instead of overwriting each other.
    let id = format!(
        "potjie-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let vardict = format!(
        "{{'title': <{}>, 'body': <{}>, 'icon': <('themed', <['com.potjie.Potjie']>)>}}",
        gvariant_str("Potjie"),
        gvariant_str(body),
    );
    let portal_ok = gdbus_call(&[
        "--dest", "org.freedesktop.portal.Desktop",
        "--object-path", "/org/freedesktop/portal/desktop",
        "--method", "org.freedesktop.portal.Notification.AddNotification",
        &id, &vardict,
    ]);
    if portal_ok {
        return;
    }
    let _ = gdbus_call(&[
        "--dest", "org.freedesktop.Notifications",
        "--object-path", "/org/freedesktop/Notifications",
        "--method", "org.freedesktop.Notifications.Notify",
        "Potjie", "0", "com.potjie.Potjie", "Potjie", body,
        "[]", "{}", "5000",
    ]);
}

/// Fire a start/stop notification from the watchdog. Spawned on its own thread so
/// a slow or absent notification server can't stall the watchdog loop.
fn notify(box_name: &str, running: bool) {
    let body = notify_body(box_name, running);
    std::thread::spawn(move || send_notification(&body));
}

/// Drop one lease; stop the box if that was the last.
fn release(leases: &Leases, name: &str) {
    let should_stop = {
        let mut map = leases.lock().unwrap();
        if let Some(count) = map.get_mut(name) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(name);
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if should_stop {
        forward::stop_master(name);
        if let Ok(vm) = Vm::load(name) {
            if let Err(e) = vm.stop() {
                eprintln!("stopping '{name}': {e}");
            }
        }
        refresh_aliases();
    }
}

fn force_stop(leases: &Leases, name: &str) -> Result<()> {
    leases.lock().unwrap().remove(name);
    forward::stop_master(name);
    let r = Vm::load(name)?.stop();
    refresh_aliases();
    r
}

fn status_of(leases: &Leases, name: &str) -> Result<BoxStatus> {
    let vm = Vm::load(name)?;
    let st = vm.status()?;
    let leases = *leases.lock().unwrap().get(name).unwrap_or(&0);
    Ok(BoxStatus {
        name: name.to_string(),
        running: st.running,
        ssh_port: st.ssh_port,
        leases,
    })
}

fn list_all(leases: &Leases) -> Vec<BoxStatus> {
    let map = leases.lock().unwrap().clone();
    Vm::list()
        .unwrap_or_default()
        .into_iter()
        .map(|vm| {
            let st = vm.status().ok();
            BoxStatus {
                leases: *map.get(&vm.cfg.name).unwrap_or(&0),
                running: st.as_ref().map(|s| s.running).unwrap_or(false),
                ssh_port: st.and_then(|s| s.ssh_port),
                name: vm.cfg.name,
            }
        })
        .collect()
}

/// Stop every box that is running with zero leases.
fn sweep(leases: &Leases) {
    let map = leases.lock().unwrap().clone();
    let mut changed = false;
    for vm in Vm::list().unwrap_or_default() {
        let leased = map.get(&vm.cfg.name).copied().unwrap_or(0) > 0;
        if !leased {
            if let Ok(st) = vm.status() {
                if st.running {
                    eprintln!("watchdog: re-locking unleased box '{}'", vm.cfg.name);
                    forward::stop_master(&vm.cfg.name);
                    let _ = vm.stop();
                    changed = true;
                }
            }
        }
    }
    if changed {
        refresh_aliases();
    }
}

/// Stop everything we manage and remove the socket.
fn shutdown(leases: &Leases, sock: &std::path::Path) {
    eprintln!("potjied shutting down; locking all boxes");
    leases.lock().unwrap().clear();
    for vm in Vm::list().unwrap_or_default() {
        if let Ok(st) = vm.status() {
            if st.running {
                forward::stop_master(&vm.cfg.name);
                let _ = vm.stop();
                // Synchronous: the watchdog loop is gone and the process is about
                // to exit, so a detached thread wouldn't deliver in time.
                send_notification(&notify_body(&vm.cfg.name, false));
            }
        }
    }
    refresh_aliases();
    let _ = std::fs::remove_file(sock);
}

fn install_signal_handlers() {
    extern "C" fn on_signal(_: libc::c_int) {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
}
