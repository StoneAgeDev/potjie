//! Fetching and verifying the pinned base image.

use crate::config::BaseImage;
use crate::paths;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha512};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Return the cached path of `base`, downloading and verifying it if missing.
///
/// `progress` is called with `(downloaded_bytes, total_bytes_or_0)` so a UI can
/// show a bar; pass `|_, _| {}` to ignore it.
pub fn ensure_base(
    base: &BaseImage,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    paths::ensure_layout()?;
    let dest = paths::base_dir()?.join(base.cache_name);

    if dest.exists() {
        // Re-verify a cached copy; a mismatch means a corrupt/partial download.
        match verify(&dest, base.sha512) {
            Ok(()) => return Ok(dest),
            Err(e) => {
                eprintln!("cached base failed verification ({e}); re-downloading");
                std::fs::remove_file(&dest).ok();
            }
        }
    }

    // These images are hundreds of MiB; a transient network blip shouldn't sink
    // the whole `create`. Retry a few times (download writes to a `.part` temp and
    // only renames on success, so a failed attempt leaves no half file at `dest`).
    const ATTEMPTS: u32 = 3;
    let mut last_err = None;
    for attempt in 1..=ATTEMPTS {
        match download(base, &dest, &mut progress)
            .with_context(|| format!("downloading base image from {}", base.url))
            .and_then(|()| {
                verify(&dest, base.sha512).context("verifying downloaded base image")
            }) {
            Ok(()) => return Ok(dest),
            Err(e) => {
                std::fs::remove_file(&dest).ok();
                if attempt < ATTEMPTS {
                    eprintln!("base image attempt {attempt}/{ATTEMPTS} failed ({e}); retrying");
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.expect("loop runs at least once"))
}

fn download(base: &BaseImage, dest: &Path, progress: &mut impl FnMut(u64, u64)) -> Result<()> {
    // Download to a temp sibling, then rename — never leave a half file at `dest`.
    let tmp = dest.with_extension("part");
    let resp = ureq::get(base.url)
        .call()
        .with_context(|| format!("GET {}", base.url))?;

    let total: u64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;

    let mut buf = vec![0u8; 1 << 20];
    let mut done: u64 = 0;
    loop {
        let n = reader.read(&mut buf).context("reading response body")?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n])?;
        done += n as u64;
        progress(done, total);
    }
    file.sync_all().ok();
    drop(file);

    std::fs::rename(&tmp, dest)
        .with_context(|| format!("moving {} -> {}", tmp.display(), dest.display()))?;
    Ok(())
}

/// Verify a file against an expected lowercase-hex SHA-512.
pub fn verify(path: &Path, expected_sha512: &str) -> Result<()> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha512::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hex_encode(&hasher.finalize());
    if !got.eq_ignore_ascii_case(expected_sha512) {
        bail!(
            "sha512 mismatch for {}:\n  expected {}\n  got      {}",
            path.display(),
            expected_sha512,
            got
        );
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
