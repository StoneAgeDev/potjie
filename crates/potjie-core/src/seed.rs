//! Building the cloud-init NoCloud seed.
//!
//! cloud-init's NoCloud datasource looks for a filesystem labeled `CIDATA`
//! holding `meta-data` and `user-data`. We build that as a small FAT image in
//! pure Rust (the `fatfs` crate) — no host `genisoimage`/`cloud-localds`, which
//! is what lets Potjie run on a minimal/immutable host. The image is attached
//! to the guest as a read-only drive.

use crate::config::BoxConfig;
use anyhow::{Context, Result};
use fatfs::{format_volume, FileSystem, FormatVolumeOptions, FsOptions};
use fscommon::BufStream;
use std::io::Write;
use std::path::Path;

/// Size of the seed image. 1 MiB is plenty for a few small text files and is
/// the smallest size that formats cleanly as FAT.
const SEED_SIZE: u64 = 1024 * 1024;

/// (Re)write the NoCloud seed image at `seed_path` for `cfg`, authorizing
/// `ssh_pubkey` for the guest user.
pub fn write_seed(seed_path: &Path, cfg: &BoxConfig, ssh_pubkey: &str) -> Result<()> {
    let meta_data = format!(
        "instance-id: potjie-{name}\nlocal-hostname: {name}\n",
        name = cfg.name
    );
    let user_data = render_user_data(cfg, ssh_pubkey);

    // Create and FAT-format a fresh image file.
    let img = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(seed_path)
        .with_context(|| format!("creating seed image {}", seed_path.display()))?;
    img.set_len(SEED_SIZE)?;
    let mut buf = BufStream::new(img);
    format_volume(
        &mut buf,
        FormatVolumeOptions::new().volume_label(*b"CIDATA     "),
    )
    .context("formatting seed image as FAT")?;

    // Write the two cloud-init files into the new filesystem.
    let fs = FileSystem::new(buf, FsOptions::new()).context("opening seed filesystem")?;
    {
        let root = fs.root_dir();
        write_file(&root, "meta-data", meta_data.as_bytes())?;
        write_file(&root, "user-data", user_data.as_bytes())?;
    }
    fs.unmount().context("flushing seed filesystem")?;
    Ok(())
}

fn write_file<T: fatfs::ReadWriteSeek>(
    root: &fatfs::Dir<T>,
    name: &str,
    data: &[u8],
) -> Result<()> {
    let mut f = root
        .create_file(name)
        .with_context(|| format!("creating {name} in seed"))?;
    f.truncate().ok();
    f.write_all(data)
        .with_context(|| format!("writing {name} in seed"))?;
    Ok(())
}

fn render_user_data(cfg: &BoxConfig, ssh_pubkey: &str) -> String {
    // Minimal #cloud-config: one passwordless-sudo user trusting our key, with
    // password auth disabled. Everything else is Debian cloud defaults.
    format!(
        "#cloud-config\n\
hostname: {name}\n\
ssh_pwauth: false\n\
users:\n\
\x20 - name: {user}\n\
\x20   sudo: 'ALL=(ALL) NOPASSWD:ALL'\n\
\x20   shell: /bin/bash\n\
\x20   lock_passwd: true\n\
\x20   ssh_authorized_keys:\n\
\x20     - {key}\n",
        name = cfg.name,
        user = cfg.username,
        key = ssh_pubkey.trim(),
    )
}
