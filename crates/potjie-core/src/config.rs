//! Static configuration: the pinned base image, and per-box metadata.

use serde::{Deserialize, Serialize};

/// Direction of a box port forward, carried over the box's SSH tunnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardDirection {
    /// Host → guest: a port on the host tunnels into the guest (SSH `LocalForward`).
    /// Use this to reach a service running *inside* the box from the host.
    Local,
    /// Guest → host: a port on the guest tunnels back to the host (SSH `RemoteForward`).
    /// Use this to expose a host service to processes running *inside* the box.
    Remote,
}

impl ForwardDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }

    /// The `ssh` command-line flag for this direction (`-L` / `-R`), shared by the
    /// live `ssh -O forward`/`-O cancel` control requests.
    pub fn flag(self) -> &'static str {
        match self {
            Self::Local => "-L",
            Self::Remote => "-R",
        }
    }

    /// The OpenSSH config keyword for this direction.
    pub fn config_keyword(self) -> &'static str {
        match self {
            Self::Local => "LocalForward",
            Self::Remote => "RemoteForward",
        }
    }
}

fn default_dest_host() -> String {
    "127.0.0.1".to_string()
}

/// A single port forward attached to a box. Persisted in `box.json` and applied
/// live over the box's SSH control master while it runs (see `forward.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    pub direction: ForwardDirection,
    /// Port the listening side binds (host side for `Local`, guest side for `Remote`).
    pub listen_port: u16,
    /// Destination host, resolved on the far side of the tunnel. Defaults to
    /// `127.0.0.1` — the guest's loopback for `Local`, the host's for `Remote`.
    #[serde(default = "default_dest_host")]
    pub dest_host: String,
    /// Destination port on `dest_host`.
    pub dest_port: u16,
    /// Optional human label shown in the UI.
    #[serde(default)]
    pub label: Option<String>,
}

impl Forward {
    /// Build a forward from the plain "host port / box port" framing the UI speaks,
    /// hiding the SSH listen-vs-destination asymmetry (which side listens depends on
    /// the direction). For `Local` (host→box) the host listens and the box is the
    /// destination; for `Remote` (box→host) it's the reverse.
    pub fn from_ports(
        direction: ForwardDirection,
        host_port: u16,
        box_port: u16,
        dest_host: String,
        label: Option<String>,
    ) -> Self {
        let (listen_port, dest_port) = match direction {
            ForwardDirection::Local => (host_port, box_port),
            ForwardDirection::Remote => (box_port, host_port),
        };
        Forward {
            direction,
            listen_port,
            dest_host,
            dest_port,
            label,
        }
    }

    /// The host-side port of this forward, regardless of direction.
    pub fn host_port(&self) -> u16 {
        match self.direction {
            ForwardDirection::Local => self.listen_port,
            ForwardDirection::Remote => self.dest_port,
        }
    }

    /// The box-side port of this forward, regardless of direction.
    pub fn box_port(&self) -> u16 {
        match self.direction {
            ForwardDirection::Local => self.dest_port,
            ForwardDirection::Remote => self.listen_port,
        }
    }

    /// The `-L`/`-R` argument string, used both for `ssh -O forward`/`-O cancel`
    /// and (conceptually) the config line. Always binds the listening side to
    /// loopback so a forward never accidentally exposes a port to the LAN.
    pub fn spec(&self) -> String {
        format!(
            "127.0.0.1:{}:{}:{}",
            self.listen_port, self.dest_host, self.dest_port
        )
    }

    /// A single OpenSSH-config line for this forward, e.g.
    /// `\tLocalForward 127.0.0.1:8080 127.0.0.1:80\n`.
    pub fn config_line(&self) -> String {
        format!(
            "\t{} 127.0.0.1:{} {}:{}\n",
            self.direction.config_keyword(),
            self.listen_port,
            self.dest_host,
            self.dest_port
        )
    }

    /// Human one-liner for UIs, reading in the direction traffic actually flows,
    /// e.g. `Host :8000  →  Box :8080` (Local) or `Box :8080  →  Host :8000`
    /// (Remote). A non-loopback destination host is shown inline.
    pub fn summary(&self) -> String {
        let host = format!("Host :{}", self.host_port());
        // Surface a non-default destination host on whichever side is the target.
        let box_extra = if self.dest_host != "127.0.0.1" && self.direction == ForwardDirection::Local
        {
            format!(" ({})", self.dest_host)
        } else {
            String::new()
        };
        let host_extra =
            if self.dest_host != "127.0.0.1" && self.direction == ForwardDirection::Remote {
                format!(" ({})", self.dest_host)
            } else {
                String::new()
            };
        let bx = format!("Box :{}{box_extra}", self.box_port());
        let host = format!("{host}{host_extra}");
        let body = match self.direction {
            ForwardDirection::Local => format!("{host}  \u{2192}  {bx}"),
            ForwardDirection::Remote => format!("{bx}  \u{2192}  {host}"),
        };
        let label = self
            .label
            .as_deref()
            .filter(|l| !l.is_empty())
            .map(|l| format!("  ({l})"))
            .unwrap_or_default();
        format!("{body}{label}")
    }
}

/// A base image Potjie knows how to fetch and verify.
///
/// The pin is by exact dated build + SHA-512 (the checksum Debian publishes in
/// `SHA512SUMS`), so every install converts the *same* bytes into its LUKS box.
/// "Updating every now and then" means bumping these three fields.
#[derive(Debug, Clone)]
pub struct BaseImage {
    /// Stable local filename for the cached image.
    pub cache_name: &'static str,
    /// HTTPS URL of the upstream qcow2.
    pub url: &'static str,
    /// Lowercase hex SHA-512 of the file.
    pub sha512: &'static str,
}

/// The base image new boxes are built from.
///
/// Debian 13 (trixie) `genericcloud` amd64 — cloud-init enabled, no
/// hypervisor-specific drivers, boots clean under plain qemu + virtio.
pub const DEFAULT_BASE: BaseImage = BaseImage {
    cache_name: "debian-13-genericcloud-amd64-20260601-2496.qcow2",
    url: "https://cloud.debian.org/images/cloud/trixie/20260601-2496/debian-13-genericcloud-amd64-20260601-2496.qcow2",
    sha512: "61264ae6968d765e61cf5607a664ba63099ddfb66b8404aa737d06f89b39c8e0fbaa1517b13705909ff01686d32015a0147436662672b4dacc10b4a171d7993d",
};

/// Persistent metadata for a single box, stored as `box.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxConfig {
    /// Box name (also the directory name under `img/`).
    pub name: String,
    /// Guest login user created by cloud-init.
    pub username: String,
    /// Base image cache name this box was built from.
    pub base: String,
    /// Number of vCPUs to give the guest.
    pub cpus: u32,
    /// Guest RAM in MiB.
    pub memory_mib: u32,
    /// Virtual disk size in GiB (the LUKS qcow2 is grown to this).
    pub disk_gib: u32,
    /// Whether cloud-init has completed at least one boot.
    #[serde(default)]
    pub provisioned: bool,
    /// Port forwards tunnelled over the box's SSH connection while it runs.
    #[serde(default)]
    pub forwards: Vec<Forward>,
}

impl BoxConfig {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            username: "potjie".to_string(),
            base: DEFAULT_BASE.cache_name.to_string(),
            cpus: 2,
            memory_mib: 2048,
            disk_gib: 20,
            provisioned: false,
            forwards: Vec::new(),
        }
    }
}
