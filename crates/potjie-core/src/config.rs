//! Static configuration: the pinned base image, and per-box metadata.

use serde::{Deserialize, Serialize};

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
        }
    }
}
