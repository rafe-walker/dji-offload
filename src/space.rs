//! Free-space checks before copying, and low-space warnings after.

use std::path::Path;

/// Bytes available to us on the volume holding `path` (or its nearest
/// existing parent, since the destination folder may not exist yet).
pub fn available(path: &Path) -> Option<u64> {
    let mut p = path;
    loop {
        if p.exists() {
            return fs4::available_space(p).ok();
        }
        p = p.parent()?;
    }
}

pub fn gb(bytes: u64) -> String {
    let g = bytes as f64 / 1e9;
    if g >= 10.0 { format!("{g:.0} GB") } else if g >= 1.0 { format!("{g:.1} GB") } else { format!("{:.0} MB", bytes as f64 / 1e6) }
}

/// Head-room kept free on top of what a copy needs.
pub const MARGIN: u64 = 512 << 20;

/// `Some(message)` when `needed` bytes will not fit on `dest`.
pub fn shortfall(dest: &Path, needed: u64) -> Option<String> {
    let free = available(dest)?;
    if needed + MARGIN > free {
        Some(format!(
            "Not enough space on {}: needs {}, only {} free. Nothing was copied there.",
            dest.display(),
            gb(needed),
            gb(free)
        ))
    } else {
        None
    }
}

/// `Some(message)` when `dest` has dropped below `warn_gb` free.
pub fn low(dest: &Path, warn_gb: f64) -> Option<String> {
    let free = available(dest)?;
    if (free as f64) < warn_gb * 1e9 {
        Some(format!("Storage getting low on {}: {} left.", dest.display(), gb(free)))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_against_real_free_space() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("not/yet/created");
        let free = available(&missing).unwrap();
        assert!(free > 0);
        assert!(shortfall(&missing, 1).is_none());
        assert!(shortfall(&missing, free).is_some());
        assert!(low(&missing, 0.0).is_none());
        assert!(low(&missing, 1e9).is_some()); // nobody has an exabyte free
    }
}
