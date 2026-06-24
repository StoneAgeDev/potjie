//! potjie-core — the engine behind Potjie.
//!
//! Potjie manages secure, encrypted, user-space virtual machines ("boxes").
//! Each box is a LUKS-encrypted qcow2 disk that qemu decrypts only while it is
//! running; networking is slirp (no root, no TAP); provisioning is cloud-init
//! from a seed image we build in pure Rust. Nothing here needs elevated
//! privileges or host-installed helpers beyond `qemu` and `ssh`.
//!
//! The typical flow:
//! ```no_run
//! use potjie_core::{BoxConfig, Vm};
//! use std::time::Duration;
//!
//! let cfg = BoxConfig::new("dev");
//! let vm = Vm::create(cfg, "hunter2", |_, _| {})?;   // build encrypted disk
//! vm.start("hunter2")?;                               // boot qemu
//! vm.wait_for_ssh(Duration::from_secs(120))?;         // wait for cloud-init
//! // ... vm.ssh_command(None) to get a shell ...
//! vm.stop()?;                                         // re-lock the box
//! # Ok::<(), anyhow::Error>(())
//! ```

pub mod base;
pub mod boxes;
pub mod config;
pub mod desktop;
pub mod disk;
pub mod forward;
pub mod guard;
pub mod paths;
pub mod protocol;
pub mod qemu;
pub mod secret;
pub mod seed;
pub mod ssh;
pub mod tools;

pub use boxes::{SealProof, Vm};
pub use config::{BaseImage, BoxConfig, DEFAULT_BASE};
pub use qemu::Status;
