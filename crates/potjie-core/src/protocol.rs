//! Wire protocol between the guard daemon (`potjied`) and its clients.
//!
//! One request/response per line of JSON over a unix socket. The defining rule:
//! a box stays running only while at least one client holds an *open* connection
//! that has `Acquire`d it. Closing the connection (cleanly or by crashing)
//! releases the lease, and the daemon re-locks the box when the count hits zero.

use crate::config::Forward;
use serde::{Deserialize, Serialize};

/// Path of the daemon's control socket under the runtime dir.
pub const SOCKET_NAME: &str = "potjied.sock";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Take a lease on a box, starting it if it isn't running. The lease is held
    /// for the lifetime of this connection.
    Acquire { box_name: String, passphrase: String },
    /// Explicitly drop this connection's lease on a box (closing the connection
    /// does the same thing).
    Release { box_name: String },
    /// Force a box down regardless of leases (admin/override).
    ForceStop { box_name: String },
    /// Status of one box.
    Status { box_name: String },
    /// Status of all boxes.
    List,
    /// Replace a box's port-forward set: persists to `box.json` and applies the
    /// change live if the box is running. Does not require a lease.
    SetForwards { box_name: String, forwards: Vec<Forward> },
    /// Read a box's persisted port-forward set.
    GetForwards { box_name: String },
    /// Liveness check.
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Acquired { ssh_port: u16 },
    Released,
    Stopped,
    Status(BoxStatus),
    List { boxes: Vec<BoxStatus> },
    ForwardsSet,
    Forwards { forwards: Vec<Forward> },
    Pong,
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxStatus {
    pub name: String,
    pub running: bool,
    pub ssh_port: Option<u16>,
    /// Number of trusted clients currently holding the box open.
    pub leases: u32,
}
