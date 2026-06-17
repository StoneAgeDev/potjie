//! Driving the qemu-system process for a box.
//!
//! A running box is one headless, daemonized qemu process:
//!   * root disk: the LUKS qcow2 (key handed in via a `secret` object)
//!   * cloud-init seed: the CIDATA FAT image, read-only
//!   * networking: slirp user-mode, with a host port forwarded to guest :22
//!
//! No privileges, no TAP devices, no bridges — everything is user-space, which
//! is the whole point. Lifecycle state (pid, forwarded port, QMP socket) lives
//! on tmpfs under the runtime dir.

use crate::disk;
use crate::paths::{create_private_dir, BoxPaths};
use crate::secret::SecretFile;
use crate::tools::{kvm_available, qemu_system, run};
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Whether a box is currently running, and if so where to reach it.
#[derive(Debug, Clone)]
pub struct Status {
    pub running: bool,
    pub pid: Option<i32>,
    pub ssh_port: Option<u16>,
}

/// Boot the box. Returns the host port forwarded to the guest's SSH.
/// Errors if the box is already running.
pub fn start(paths: &BoxPaths, cfg: &crate::config::BoxConfig, passphrase: &str) -> Result<u16> {
    if status(paths)?.running {
        bail!("box '{}' is already running", paths.name);
    }
    create_private_dir(&paths.runtime_dir)?;

    let port = free_port()?;
    let secret = SecretFile::new(passphrase)?;
    let node = "disk0";
    let (secret_arg, blockdev_arg) = disk::blockdev_args(&paths.disk, &secret, node);

    let mut cmd = Command::new(qemu_system());
    cmd.arg("-name").arg(format!("potjie-{}", paths.name));

    // Accelerator + CPU model.
    if kvm_available() {
        cmd.args(["-machine", "q35,accel=kvm"]).args(["-cpu", "host"]);
    } else {
        cmd.args(["-machine", "q35,accel=tcg"]).args(["-cpu", "max"]);
    }
    cmd.args(["-m", &cfg.memory_mib.to_string()])
        .args(["-smp", &cfg.cpus.to_string()]);

    // Encrypted root disk.
    cmd.args(["-object", &secret_arg])
        .args(["-blockdev", &blockdev_arg])
        .args(["-device", &format!("virtio-blk-pci,drive={node}")]);

    // cloud-init seed (read-only raw drive).
    cmd.args([
        "-drive",
        &format!(
            "file={},format=raw,if=virtio,readonly=on",
            paths.seed.display()
        ),
    ]);

    // slirp user networking with host->guest SSH forward.
    cmd.args([
        "-netdev",
        &format!("user,id=net0,hostfwd=tcp:127.0.0.1:{port}-:22"),
    ])
    .args(["-device", "virtio-net-pci,netdev=net0"]);

    // Headless, backgrounded, controllable.
    cmd.arg("-display").arg("none");
    cmd.arg("-serial")
        .arg(format!("file:{}", paths.runtime_dir.join("console.log").display()));
    cmd.args([
        "-qmp",
        &format!("unix:{},server,nowait", paths.qmp_sock.display()),
    ]);
    cmd.arg("-pidfile").arg(&paths.pid_file);
    cmd.arg("-daemonize");

    // qemu opens the encrypted disk (reading the secret) before it daemonizes,
    // so the secret file is consumed by the time this returns.
    run(&mut cmd).context("launching qemu")?;

    std::fs::write(&paths.ssh_port_file, port.to_string())
        .with_context(|| format!("writing {}", paths.ssh_port_file.display()))?;
    Ok(port)
}

/// Scan `/proc` for *any* qemu process running this box, independent of our
/// pidfile. This is the trustworthy "is anything decrypting this box right now?"
/// check for the sealed-assurance: it catches an escaped or stale qemu that a
/// missing pidfile would hide.
pub fn box_process_running(name: &str) -> bool {
    let marker = format!("potjie-{name}");
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for e in entries.flatten() {
        let fname = e.file_name();
        let Some(s) = fname.to_str() else { continue };
        if !s.bytes().all(|b| b.is_ascii_digit()) {
            continue; // not a pid dir
        }
        let Ok(cmdline) = std::fs::read(e.path().join("cmdline")) else { continue };
        // cmdline is NUL-separated argv; require an exact "potjie-<name>" arg so
        // box "dev" doesn't match box "dev2".
        if cmdline.split(|b| *b == 0).any(|arg| arg == marker.as_bytes()) {
            return true;
        }
    }
    false
}

/// Current status of the box.
pub fn status(paths: &BoxPaths) -> Result<Status> {
    let Some(pid) = read_pid(&paths.pid_file) else {
        return Ok(Status { running: false, pid: None, ssh_port: None });
    };
    if !pid_alive(pid) {
        // Stale pidfile from a crashed/killed qemu; clean it up.
        std::fs::remove_file(&paths.pid_file).ok();
        std::fs::remove_file(&paths.ssh_port_file).ok();
        return Ok(Status { running: false, pid: None, ssh_port: None });
    }
    let ssh_port = std::fs::read_to_string(&paths.ssh_port_file)
        .ok()
        .and_then(|s| s.trim().parse().ok());
    Ok(Status { running: true, pid: Some(pid), ssh_port })
}

/// Gracefully stop the box: ACPI powerdown via QMP, falling back to SIGTERM,
/// then SIGKILL. Blocks until the process is gone or `timeout` elapses.
pub fn stop(paths: &BoxPaths, timeout: Duration) -> Result<()> {
    let st = status(paths)?;
    let Some(pid) = st.pid else {
        return Ok(()); // already stopped
    };

    // Best-effort clean shutdown.
    if let Err(e) = qmp_powerdown(&paths.qmp_sock) {
        eprintln!("QMP powerdown failed ({e}); will signal directly");
        signal(pid, libc::SIGTERM);
    }

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            cleanup_runtime(paths);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Out of patience.
    signal(pid, libc::SIGKILL);
    cleanup_runtime(paths);
    Ok(())
}

fn cleanup_runtime(paths: &BoxPaths) {
    std::fs::remove_file(&paths.pid_file).ok();
    std::fs::remove_file(&paths.ssh_port_file).ok();
    std::fs::remove_file(&paths.qmp_sock).ok();
}

// ---- QMP -----------------------------------------------------------------

/// Minimal QMP exchange: read greeting, negotiate, send `system_powerdown`.
fn qmp_powerdown(sock: &Path) -> Result<()> {
    let stream = UnixStream::connect(sock)
        .with_context(|| format!("connecting QMP socket {}", sock.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let mut line = String::new();
    reader.read_line(&mut line)?; // server greeting

    writeln!(writer, "{{\"execute\":\"qmp_capabilities\"}}")?;
    line.clear();
    reader.read_line(&mut line)?; // capabilities reply

    writeln!(writer, "{{\"execute\":\"system_powerdown\"}}")?;
    line.clear();
    reader.read_line(&mut line)?; // powerdown reply
    Ok(())
}

// ---- process helpers -----------------------------------------------------

fn read_pid(pid_file: &Path) -> Option<i32> {
    std::fs::read_to_string(pid_file).ok()?.trim().parse().ok()
}

fn pid_alive(pid: i32) -> bool {
    // signal 0 probes existence without delivering anything.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn signal(pid: i32, sig: i32) {
    unsafe {
        libc::kill(pid, sig);
    }
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .context("finding a free host port")?;
    Ok(listener.local_addr()?.port())
}
