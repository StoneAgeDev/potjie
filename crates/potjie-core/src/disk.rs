//! Creating the per-box LUKS-encrypted root disk.
//!
//! qemu encrypts qcow2 disks natively in LUKS format. We never decrypt the base;
//! we `qemu-img convert` it straight into a new qcow2 whose payload is LUKS,
//! keyed by the user's passphrase. The key only ever exists inside the running
//! qemu/qemu-img process — there is no root daemon and no `cryptsetup` mapping.

use crate::secret::SecretFile;
use crate::tools::{qemu_img, run};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

const SECRET_ID: &str = "sec0";

/// Create `dest` as a LUKS-encrypted qcow2 copy of `base`, then grow it to
/// `disk_gib`. Fails if `dest` already exists.
pub fn create_encrypted(
    base: &Path,
    dest: &Path,
    disk_gib: u32,
    passphrase: &str,
) -> Result<()> {
    if dest.exists() {
        anyhow::bail!("disk already exists: {}", dest.display());
    }
    let secret = SecretFile::new(passphrase)?;

    // convert: base (plaintext qcow2) -> dest (LUKS qcow2)
    let mut cmd = Command::new(qemu_img());
    cmd.arg("convert")
        .args(["--object", &secret.object_arg(SECRET_ID)])
        .args(["-O", "qcow2"])
        .args([
            "-o",
            &format!("encrypt.format=luks,encrypt.key-secret={SECRET_ID}"),
        ])
        .arg(base)
        .arg(dest);
    run(&mut cmd).context("qemu-img convert (encrypt to LUKS)")?;

    resize(dest, disk_gib, &secret).context("growing encrypted disk")?;
    Ok(())
}

/// Grow an existing encrypted disk to `disk_gib` (absolute) GiB.
fn resize(dest: &Path, disk_gib: u32, secret: &SecretFile) -> Result<()> {
    // Resizing an encrypted image still needs the key to open it, so use
    // --image-opts to pass the key-secret alongside the filename.
    let image_opts = format!(
        "driver=qcow2,encrypt.key-secret={SECRET_ID},file.filename={}",
        dest.display()
    );
    let mut cmd = Command::new(qemu_img());
    cmd.arg("resize")
        .args(["--object", &secret.object_arg(SECRET_ID)])
        .arg("--image-opts")
        .arg(image_opts)
        .arg(format!("{disk_gib}G"));
    run(&mut cmd)
}

/// The `-blockdev` JSON for booting this disk under qemu-system, plus the
/// `secret` object it references. Returns `(secret_object_arg, blockdev_arg)`.
pub fn blockdev_args(disk: &Path, secret: &SecretFile, node_name: &str) -> (String, String) {
    let blockdev = serde_json::json!({
        "driver": "qcow2",
        "node-name": node_name,
        "encrypt": { "format": "luks", "key-secret": SECRET_ID },
        "file": { "driver": "file", "filename": disk.to_string_lossy() }
    })
    .to_string();
    (secret.object_arg(SECRET_ID), blockdev)
}

/// The shared secret id used across disk operations and boot.
pub fn secret_id() -> &'static str {
    SECRET_ID
}

/// Inspect the *on-disk* image (no key needed — this reads only metadata) and
/// report whether its payload is LUKS-encrypted. Used as one half of the
/// decryption assurance: the disk is provably encrypted at rest.
pub fn is_luks_encrypted(disk: &Path) -> Result<bool> {
    let out = std::process::Command::new(qemu_img())
        // -U (force-share) so we can read metadata even while a running qemu
        // holds the image lock.
        .args(["info", "-U", "--output=json"])
        .arg(disk)
        .output()
        .context("qemu-img info")?;
    if !out.status.success() {
        anyhow::bail!(
            "qemu-img info failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("parsing qemu-img info")?;
    let encrypted = v.get("encrypted").and_then(|b| b.as_bool()).unwrap_or(false);
    let fmt = v
        .pointer("/format-specific/data/encrypt/format")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    Ok(encrypted && fmt.eq_ignore_ascii_case("luks"))
}
