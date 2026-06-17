//! Client side of the guard daemon: leasing boxes so they only run while a
//! trusted process holds them open.
//!
//! Typical use:
//! ```no_run
//! use potjie_core::guard;
//! let lease = guard::acquire("dev", "hunter2")?;   // box boots (or joins)
//! // ... use lease.ssh_port for as long as the app/shell lives ...
//! drop(lease);                                       // box re-locks if last holder
//! # Ok::<(), anyhow::Error>(())
//! ```

use crate::paths;
use crate::protocol::{BoxStatus, Request, Response, SOCKET_NAME};
use crate::tools;
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Path to the daemon control socket.
pub fn socket_path() -> Result<PathBuf> {
    Ok(paths::runtime_root()?.join(SOCKET_NAME))
}

/// An active lease. While this value lives, the box is guaranteed to stay
/// running (the connection is held open). Dropping it releases the lease; the
/// daemon stops the box if no other client holds it.
pub struct Lease {
    stream: UnixStream,
    pub box_name: String,
    pub ssh_port: u16,
}

impl Drop for Lease {
    fn drop(&mut self) {
        // Best-effort explicit release; closing the stream alone also releases.
        let _ = writeln!(
            self.stream,
            "{}",
            serde_json::to_string(&Request::Release {
                box_name: self.box_name.clone()
            })
            .unwrap_or_default()
        );
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// Connect to the daemon, starting it first if necessary.
fn connect() -> Result<UnixStream> {
    let path = socket_path()?;
    if let Ok(s) = UnixStream::connect(&path) {
        return Ok(s);
    }
    ensure_daemon()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(s);
        }
        if Instant::now() >= deadline {
            bail!("guard daemon did not come up at {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Spawn `potjied` detached from this process, so it outlives short-lived CLI
/// invocations and survives the launching client.
pub fn ensure_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let log = paths::runtime_root()?.join("potjied.log");
    paths::create_private_dir(&paths::runtime_root()?)?;
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .ok();

    let mut cmd = std::process::Command::new(tools::potjied());
    cmd.stdin(std::process::Stdio::null());
    if let Some(f) = out {
        let f2 = f.try_clone().ok();
        cmd.stdout(std::process::Stdio::from(f));
        if let Some(f2) = f2 {
            cmd.stderr(std::process::Stdio::from(f2));
        }
    }
    // Detach into its own session so it isn't killed with the client's terminal.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("spawning guard daemon")?;
    Ok(())
}

fn send_recv(stream: &mut UnixStream, req: &Request) -> Result<Response> {
    writeln!(stream, "{}", serde_json::to_string(req)?).context("sending request")?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line).context("reading response")?;
    if line.trim().is_empty() {
        bail!("daemon closed the connection");
    }
    Ok(serde_json::from_str(&line).context("parsing response")?)
}

/// One-shot request on a fresh connection.
fn oneshot(req: Request) -> Result<Response> {
    let mut stream = connect()?;
    send_recv(&mut stream, &req)
}

/// Acquire a lease on `box_name`, booting it via the daemon if needed.
pub fn acquire(box_name: &str, passphrase: &str) -> Result<Lease> {
    let mut stream = connect()?;
    let resp = send_recv(
        &mut stream,
        &Request::Acquire {
            box_name: box_name.to_string(),
            passphrase: passphrase.to_string(),
        },
    )?;
    match resp {
        Response::Acquired { ssh_port } => Ok(Lease {
            stream,
            box_name: box_name.to_string(),
            ssh_port,
        }),
        Response::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Force a box down regardless of leases.
pub fn force_stop(box_name: &str) -> Result<()> {
    match oneshot(Request::ForceStop {
        box_name: box_name.to_string(),
    })? {
        Response::Stopped => Ok(()),
        Response::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Status of a single box (via the daemon, so lease counts are accurate).
pub fn status(box_name: &str) -> Result<BoxStatus> {
    match oneshot(Request::Status {
        box_name: box_name.to_string(),
    })? {
        Response::Status(s) => Ok(s),
        Response::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Status of all boxes.
pub fn list() -> Result<Vec<BoxStatus>> {
    match oneshot(Request::List)? {
        Response::List { boxes } => Ok(boxes),
        Response::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

/// True if a daemon is already listening.
pub fn daemon_running() -> bool {
    socket_path()
        .ok()
        .map(|p| UnixStream::connect(p).is_ok())
        .unwrap_or(false)
}
