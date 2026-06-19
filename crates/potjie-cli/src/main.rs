//! `potjie` — CLI driver and the helper the desktop wrappers call.
//!
//! Box lifecycle goes through the guard daemon (`potjied`): commands *lease* a
//! box for the duration they need it, and the daemon guarantees the box is only
//! running while a lease is held. `potjie ssh` holds a lease for the shell
//! session; `potjie run` for one command; `potjie hold` until interrupted.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use potjie_core::config::{Forward, ForwardDirection};
use potjie_core::{guard, BoxConfig, Vm};

#[derive(Parser)]
#[command(name = "potjie", about = "Manage secure, encrypted, user-space VMs")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new box from the pinned base image.
    Create {
        name: String,
        #[arg(long, default_value_t = 2)]
        cpus: u32,
        #[arg(long, default_value_t = 2048)]
        memory: u32,
        #[arg(long, default_value_t = 20)]
        disk: u32,
    },
    /// List all boxes with running state and lease counts.
    List,
    /// Show a box's status.
    Status { name: String },
    /// Open an interactive shell in a box (boots it, locks it on exit).
    Ssh { name: String },
    /// Hold a box up until interrupted (Ctrl-C releases and re-locks it).
    Hold { name: String },
    /// Boot the box, run a command over SSH, then release (lock) the box.
    /// This is what generated `.desktop` wrappers invoke.
    Run {
        name: String,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Force a box down regardless of leases.
    Down { name: String },
    /// Check, with positive evidence, that a box is sealed (encrypted at rest
    /// and not decrypted/reachable anywhere).
    Verify { name: String },

    /// Manage a box's SSH port forwards (host↔guest). Changes apply live to a
    /// running box (no restart) and persist for next boot.
    Forward {
        #[command(subcommand)]
        action: ForwardCmd,
    },

    /// Permanently delete a box.
    Rm { name: String },

    /// Internal: run a shell command and wait for its whole descendant tree to
    /// exit (used by host-app wrappers so the box stays up for the app's *real*
    /// lifetime, not just until a launcher process forks and returns).
    #[command(name = "__run-tracked", hide = true)]
    RunTracked {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },

    /// Internal: run the guard daemon. Auto-spawned (detached) by the first
    /// client that needs it; not meant to be invoked by hand.
    #[command(name = "daemon", hide = true)]
    Daemon,
}

#[derive(Subcommand)]
enum ForwardCmd {
    /// List a box's configured forwards (with their index, for `rm`).
    List { name: String },
    /// Add a forward. Default is host→guest (LocalForward, `-L`); pass --remote
    /// for guest→host (RemoteForward, `-R`).
    Add {
        name: String,
        /// Port the listening side binds.
        listen_port: u16,
        /// Destination port on the far side of the tunnel.
        dest_port: u16,
        /// Destination host on the far side (default: loopback).
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Make this guest→host (RemoteForward) instead of host→guest.
        #[arg(long)]
        remote: bool,
        /// Optional human label.
        #[arg(long)]
        label: Option<String>,
    },
    /// Remove a forward by its index in `forward list`.
    Rm { name: String, index: usize },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Create { name, cpus, memory, disk } => create(&name, cpus, memory, disk),
        Cmd::List => list(),
        Cmd::Status { name } => status(&name),
        Cmd::Ssh { name } => shell(&name),
        Cmd::Hold { name } => hold(&name),
        Cmd::Run { name, command } => run(&name, &command),
        Cmd::Down { name } => down(&name),
        Cmd::Verify { name } => verify(&name),
        Cmd::Forward { action } => forward(action),
        Cmd::Rm { name } => rm(&name),
        Cmd::RunTracked { command } => run_tracked(&command),
        Cmd::Daemon => potjie_daemon::run(),
    }
}

fn verify(name: &str) -> Result<()> {
    let vm = Vm::load(name)?;
    let p = vm.verify_sealed()?;
    let yn = |b: bool| if b { "yes" } else { "no" };
    println!("encrypted at rest (LUKS): {}", yn(p.disk_is_luks));
    println!("qemu decrypting it now:   {}", yn(p.qemu_running));
    println!("ssh reachable:            {}", yn(p.ssh_reachable));
    println!("key material in runtime:  {}", yn(p.secret_files_present));
    if p.is_sealed() {
        println!("\n\u{2713} SEALED — encrypted at rest and not decrypted or reachable anywhere.");
        Ok(())
    } else {
        println!("\n\u{2717} NOT SEALED — see the checks above.");
        std::process::exit(1);
    }
}

/// Run a shell command, blocking until its entire descendant tree exits.
fn run_tracked(command: &[String]) -> Result<()> {
    let joined = command.join(" ");
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(&joined);
    let code = potjie_core::proc::run_until_descendants_exit(cmd)
        .context("running tracked command")?;
    std::process::exit(code);
}

/// Prompt for the LUKS passphrase, or read `POTJIE_PASSPHRASE` for scripting.
fn passphrase(prompt: &str) -> Result<String> {
    if let Ok(p) = std::env::var("POTJIE_PASSPHRASE") {
        return Ok(p);
    }
    rpassword::prompt_password(prompt).context("reading passphrase")
}

/// Acquire a lease, prompting for the passphrase only if the box is stopped.
fn acquire(name: &str) -> Result<guard::Lease> {
    let st = guard::status(name)?;
    let pass = if st.running {
        String::new() // already decrypted; the daemon won't use this
    } else {
        passphrase(&format!("LUKS passphrase for '{name}': "))?
    };
    guard::acquire(name, &pass)
}

fn create(name: &str, cpus: u32, memory: u32, disk: u32) -> Result<()> {
    let mut cfg = BoxConfig::new(name);
    cfg.cpus = cpus;
    cfg.memory_mib = memory;
    cfg.disk_gib = disk;

    let pass = passphrase("New LUKS passphrase for this box: ")?;
    if std::env::var("POTJIE_PASSPHRASE").is_err() {
        let again = passphrase("Repeat passphrase: ")?;
        if again != pass {
            anyhow::bail!("passphrases did not match");
        }
    }

    println!("Creating box '{name}' (downloading/verifying base image if needed)...");
    let mut last = 0u64;
    Vm::create(cfg, &pass, |done, total| {
        if done - last >= 32 << 20 || (total != 0 && done == total) {
            last = done;
            if total != 0 {
                eprint!("\r  base image: {} / {} MiB", done >> 20, total >> 20);
            } else {
                eprint!("\r  base image: {} MiB", done >> 20);
            }
        }
    })?;
    eprintln!();
    println!("Box '{name}' created. Start a shell with: potjie ssh {name}");
    Ok(())
}

fn list() -> Result<()> {
    let boxes = guard::list()?;
    if boxes.is_empty() {
        println!("No boxes yet. Create one with: potjie create <name>");
        return Ok(());
    }
    println!("{:<20} {:<10} {:>6} {:>7}", "NAME", "STATE", "PORT", "LEASES");
    for b in boxes {
        let state = if b.running { "running" } else { "stopped" };
        let port = b.ssh_port.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
        println!("{:<20} {:<10} {:>6} {:>7}", b.name, state, port, b.leases);
    }
    Ok(())
}

fn status(name: &str) -> Result<()> {
    let vm = Vm::load(name)?;
    let st = guard::status(name)?;
    println!("name:    {}", vm.cfg.name);
    println!("user:    {}", vm.cfg.username);
    println!("base:    {}", vm.cfg.base);
    println!("cpus:    {}", vm.cfg.cpus);
    println!("memory:  {} MiB", vm.cfg.memory_mib);
    println!("disk:    {} GiB", vm.cfg.disk_gib);
    println!("running: {}", st.running);
    println!("leases:  {}", st.leases);
    if let Some(p) = st.ssh_port {
        println!("ssh:     ssh -p {p} {}@127.0.0.1", vm.cfg.username);
    }
    Ok(())
}

fn shell(name: &str) -> Result<()> {
    let vm = Vm::load(name)?;
    let booting = !guard::status(name)?.running;
    let pass = if booting {
        passphrase(&format!("LUKS passphrase for '{name}': "))?
    } else {
        String::new()
    };

    let lease = if booting {
        boot_with_logs(name, &pass, &vm)?
    } else {
        println!("Connecting to '{name}'…");
        guard::acquire(name, &pass)?
    };

    if booting {
        // Clear scrollback + screen so the interactive shell starts fresh.
        print!("\x1b[3J\x1b[2J\x1b[H");
        use std::io::Write;
        std::io::stdout().flush().ok();
        println!("\u{2713} Booted. Opening shell…\n");
    }

    let mut cmd = vm.ssh_command(None)?;
    let st = cmd.status().context("running ssh")?;
    drop(lease); // releases the lease -> box re-locks if we were the last holder
    std::process::exit(st.code().unwrap_or(1));
}

/// Boot the box (on a worker thread) while streaming its serial console *and* a
/// heartbeat to stdout, so the terminal always shows something during the wait.
fn boot_with_logs(name: &str, pass: &str, vm: &Vm) -> Result<guard::Lease> {
    use std::sync::mpsc::{self, TryRecvError};
    use std::time::{Duration, Instant};

    println!("Starting '{name}' — unlocking and booting…\n");
    let log = vm.paths.console_log();

    let (tx, rx) = mpsc::channel();
    let name2 = name.to_string();
    let pass2 = pass.to_string();
    let handle = std::thread::spawn(move || {
        let _ = tx.send(guard::acquire(&name2, &pass2));
    });

    // Start past any leftover console from a previous boot; truncation (a fresh
    // boot rewriting the file) is detected and resets us to the start.
    let mut pos = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    let start = Instant::now();
    let mut last_beat = 0u64;

    let result = loop {
        pos = stream_console(&log, pos);
        match rx.try_recv() {
            Ok(r) => break r,
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => break Err(anyhow::anyhow!("boot worker vanished")),
        }
        let secs = start.elapsed().as_secs();
        if secs >= last_beat + 2 {
            last_beat = secs;
            println!("  …still booting ({secs}s) — waiting for the guest to come up…");
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let _ = handle.join();
    stream_console(&log, pos); // final drain
    result
}

/// Print any new bytes of qemu's serial console `path` since `pos`; return the
/// new position. Resets to 0 if the file was truncated by a fresh boot.
fn stream_console(path: &std::path::Path, mut pos: u64) -> u64 {
    use std::io::{Read, Seek, SeekFrom, Write};
    if let Ok(meta) = std::fs::metadata(path) {
        let len = meta.len();
        if len < pos {
            pos = 0;
        }
        if len > pos {
            if let Ok(mut f) = std::fs::File::open(path) {
                if f.seek(SeekFrom::Start(pos)).is_ok() {
                    let mut buf = Vec::new();
                    if f.read_to_end(&mut buf).is_ok() {
                        let out = std::io::stdout();
                        let mut h = out.lock();
                        h.write_all(&buf).ok();
                        h.flush().ok();
                        pos += buf.len() as u64;
                    }
                }
            }
        }
    }
    pos
}

fn hold(name: &str) -> Result<()> {
    let lease = acquire(name)?;
    println!(
        "Holding '{name}' up (ssh -p {} {}@127.0.0.1).\nPress Ctrl-C to release and re-lock.",
        lease.ssh_port,
        Vm::load(name)?.cfg.username
    );
    // Park; on Ctrl-C the process exits, the socket closes, and the daemon
    // releases the lease (re-locking the box).
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

fn run(name: &str, command: &[String]) -> Result<()> {
    let lease = acquire(name)?;
    let vm = Vm::load(name)?;
    let joined = command.join(" ");
    let mut cmd = vm.ssh_command(Some(&joined))?;
    let result = cmd.status().context("running command over ssh");
    drop(lease);
    let st = result?;
    std::process::exit(st.code().unwrap_or(1));
}

fn down(name: &str) -> Result<()> {
    guard::force_stop(name)?;
    println!("Box '{name}' stopped and re-locked.");
    Ok(())
}

fn forward(action: ForwardCmd) -> Result<()> {
    match action {
        ForwardCmd::List { name } => {
            let fwds = guard::get_forwards(&name)?;
            if fwds.is_empty() {
                println!("No forwards configured for '{name}'.");
                return Ok(());
            }
            println!("IDX  FORWARD");
            for (i, f) in fwds.iter().enumerate() {
                println!("{i:<4} {}", f.summary());
            }
            Ok(())
        }
        ForwardCmd::Add { name, listen_port, dest_port, host, remote, label } => {
            if listen_port == 0 || dest_port == 0 {
                anyhow::bail!("ports must be between 1 and 65535");
            }
            let fwd = Forward {
                direction: if remote {
                    ForwardDirection::Remote
                } else {
                    ForwardDirection::Local
                },
                listen_port,
                dest_host: host,
                dest_port,
                label,
            };
            let mut fwds = guard::get_forwards(&name)?;
            if fwds.contains(&fwd) {
                anyhow::bail!("that forward already exists");
            }
            fwds.push(fwd.clone());
            guard::set_forwards(&name, fwds)?;
            println!("Added: {}", fwd.summary());
            Ok(())
        }
        ForwardCmd::Rm { name, index } => {
            let mut fwds = guard::get_forwards(&name)?;
            if index >= fwds.len() {
                anyhow::bail!("no forward at index {index}; see `potjie forward list {name}`");
            }
            let removed = fwds.remove(index);
            guard::set_forwards(&name, fwds)?;
            println!("Removed: {}", removed.summary());
            Ok(())
        }
    }
}

fn rm(name: &str) -> Result<()> {
    let vm = Vm::load(name)?;
    vm.delete()?;
    println!("Box '{name}' deleted.");
    Ok(())
}
