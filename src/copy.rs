//! Verified copies: hash while copying, re-read the copy, compare, then rename
//! into place. A file only ever appears under its real name once it is proven
//! identical to the original.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

const BUF: usize = 8 << 20;

fn hash_file(path: &Path) -> Result<blake3::Hash> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; BUF];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize())
}

pub enum Outcome {
    Copied(String),
    AlreadyThere(String),
}

/// Copy `src` to `dst`, verifying with BLAKE3. Returns the hex hash.
pub fn copy_verified(src: &Path, dst: &Path) -> Result<Outcome> {
    let parent = dst.parent().context("destination has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let src_meta = fs::metadata(src)?;

    if let Ok(m) = fs::metadata(dst) {
        if m.len() == src_meta.len() {
            let (a, b) = (hash_file(src)?, hash_file(dst)?);
            if a == b {
                return Ok(Outcome::AlreadyThere(a.to_hex().to_string()));
            }
        }
        bail!("{} already exists and differs from the drone's copy; leaving it alone", dst.display());
    }

    let name = dst.file_name().context("no file name")?.to_string_lossy();
    let partial = parent.join(format!(".{name}.partial"));
    let result = (|| -> Result<String> {
        let mut input = File::open(src).with_context(|| format!("opening {}", src.display()))?;
        let mut output = File::create(&partial)?;
        let mut h = blake3::Hasher::new();
        let mut buf = vec![0u8; BUF];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            output.write_all(&buf[..n])?;
        }
        output.sync_all()?;
        if let Ok(t) = src_meta.modified() {
            let _ = output.set_modified(t);
        }
        drop(output);
        let want = h.finalize();
        let got = hash_file(&partial)?;
        if want != got {
            bail!("verification failed for {} (copy does not match the drone)", src.display());
        }
        fs::rename(&partial, dst)?;
        Ok(want.to_hex().to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result.map(Outcome::Copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_and_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("a.JPG");
        fs::write(&src, vec![7u8; 1_000_003]).unwrap();
        let dst = d.path().join("out/2026-04-28/Flight-1_1827/a.JPG");
        let h1 = match copy_verified(&src, &dst).unwrap() {
            Outcome::Copied(h) => h,
            _ => panic!("expected a copy"),
        };
        assert_eq!(fs::read(&dst).unwrap(), fs::read(&src).unwrap());
        match copy_verified(&src, &dst).unwrap() {
            Outcome::AlreadyThere(h) => assert_eq!(h, h1),
            _ => panic!("second run must not copy again"),
        }
        assert!(!dst.parent().unwrap().join(".a.JPG.partial").exists());
    }

    #[test]
    fn refuses_to_overwrite_a_different_file() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("a.JPG");
        let dst = d.path().join("b.JPG");
        fs::write(&src, b"drone").unwrap();
        fs::write(&dst, b"other").unwrap();
        assert!(copy_verified(&src, &dst).is_err());
        assert_eq!(fs::read(&dst).unwrap(), b"other");
    }
}
