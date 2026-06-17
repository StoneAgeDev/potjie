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

use anyhow::{Context, Result};
use potjie_core::protocol::{BoxStatus, Request, Response};
use potjie_core::{guard, paths, Vm};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Lease counts, keyed by box name. Wrapped in a mutex shared across handlers.
type Leases = Arc<Mutex<HashMap<String, u32>>>;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

fn main() -> Result<()> {
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

    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("binding {}", sock.display()))?;
    listener
        .set_nonblocking(true)
        .context("setting listener non-blocking")?;
    eprintln!("potjied listening on {}", sock.display());

    // Watchdog: periodically re-lock unleased boxes and honor shutdown.
    {
        let leases = leases.clone();
        std::thread::spawn(move || loop {
            if SHUTDOWN.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_secs(2));
            sweep(&leases);
        });
    }

    // Accept loop (non-blocking so we can react to shutdown).
    for stream in incoming(&listener) {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        let Some(stream) = stream else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
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

/// Handle one client connection until it closes, releasing all its leases.
fn handle(stream: UnixStream, leases: Leases) -> Result<()> {
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);
    let mut held: HashSet<String> = HashSet::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch(req, &leases, &mut held),
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        };
        writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
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
                Err(e) => Response::Error { message: e.to_string() },
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
            Err(e) => Response::Error { message: e.to_string() },
        },
        Request::Status { box_name } => match status_of(leases, &box_name) {
            Ok(s) => Response::Status(s),
            Err(e) => Response::Error { message: e.to_string() },
        },
        Request::List => Response::List { boxes: list_all(leases) },
    }
}

/// Start the box if it isn't already up, then bump its lease count.
fn acquire(leases: &Leases, name: &str, passphrase: &str) -> Result<u16> {
    let vm = Vm::load(name)?;

    // Hold the lock only to decide-and-start, not across the SSH wait.
    {
        let mut map = leases.lock().unwrap();
        let count = map.entry(name.to_string()).or_insert(0);
        if *count == 0 && !vm.status()?.running {
            vm.start(passphrase)
                .with_context(|| format!("starting box '{name}'"))?;
        }
        *count += 1;
    }

    // Wait (outside the lock) until SSH answers so the client gets a live port.
    let port = vm
        .wait_for_ssh(Duration::from_secs(180))
        .context("waiting for box SSH")
        .inspect_err(|_| release(leases, name))?; // undo our lease on failure
    refresh_aliases();
    Ok(port)
}

/// Regenerate the `potjie-<box>` SSH aliases after any box starts or stops, so
/// host apps (VS Code Remote-SSH, `ssh potjie-<box>`, …) always resolve.
fn refresh_aliases() {
    if let Err(e) = potjie_core::desktop::sync_ssh_config() {
        eprintln!("ssh alias sync failed: {e}");
    }
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
                let _ = vm.stop();
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
