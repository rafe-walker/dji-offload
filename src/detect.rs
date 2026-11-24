//! Telling a DJI drone (or its card) apart from any other mounted drive.

use std::fs;
use std::path::{Path, PathBuf};

/// A mounted volume that looks like DJI media.
#[derive(Debug, Clone)]
pub struct Source {
    pub mount: PathBuf,
    /// e.g. "Mavic 4" when the volume carries a model fingerprint, else "DJI".
    pub model: String,
}

/// File that marks an external drive as an offload destination.
pub const DEST_MARKER: &str = ".dji-offload-destination";

/// Is this volume DJI media? We need a `DCIM/` folder holding a DJI-style
/// subfolder (`DJI_001`, `100MEDIA`, ...). Volume names are useless: the
/// Mavic 4 Pro mounts as "Untitled".
pub fn inspect(mount: &Path) -> Option<Source> {
    if mount.join(DEST_MARKER).exists() {
        return None; // our own backup drive, never a source
    }
    let dcim = mount.join("DCIM");
    let rd = fs::read_dir(&dcim).ok()?;
    let dji_dir = rd.flatten().any(|e| {
        let n = e.file_name().to_string_lossy().to_uppercase();
        e.path().is_dir() && (n.starts_with("DJI_") || n.ends_with("MEDIA"))
    });
    if !dji_dir {
        return None;
    }
    Some(Source { mount: mount.to_path_buf(), model: model(mount) })
}

/// DJI drones leave a database in `MISC/` named after the model, e.g. `mavic4_edcf.db`.
fn model(mount: &Path) -> String {
    let Ok(rd) = fs::read_dir(mount.join("MISC")) else { return "DJI".into() };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_lowercase();
        if let Some(stem) = n.strip_suffix(".db") {
            let base = stem.split('_').next().unwrap_or(stem);
            let (word, num): (String, String) = base.chars().partition(|c| c.is_alphabetic());
            if !word.is_empty() {
                let mut w = word.clone();
                w[..1].make_ascii_uppercase();
                return if num.is_empty() { w } else { format!("{w} {num}") };
            }
        }
    }
    "DJI".into()
}

/// Every DJI source currently mounted under `root` (normally `/Volumes`).
pub fn sources(root: &Path) -> Vec<Source> {
    let Ok(rd) = fs::read_dir(root) else { return vec![] };
    let mut v: Vec<Source> = rd.flatten().filter_map(|e| inspect(&e.path())).collect();
    v.sort_by(|a, b| a.mount.cmp(&b.mount));
    v
}

/// External drives the user marked as destinations (`dji-offload mark-drive`).
pub fn marked_destinations(root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(root) else { return vec![] };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join(DEST_MARKER).exists())
        .collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_mavic4_layout() {
        let d = tempfile::tempdir().unwrap();
        let v = d.path().join("Untitled");
        fs::create_dir_all(v.join("DCIM/DJI_001")).unwrap();
        fs::create_dir_all(v.join("MISC")).unwrap();
        fs::write(v.join("MISC/mavic4_edcf.db"), b"").unwrap();
        fs::create_dir_all(d.path().join("Backup/DCIM")).unwrap(); // not DJI
        let s = sources(d.path());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].model, "Mavic 4");
    }

    #[test]
    fn marked_drive_is_never_a_source() {
        let d = tempfile::tempdir().unwrap();
        let v = d.path().join("T7");
        fs::create_dir_all(v.join("DCIM/DJI_001")).unwrap();
        fs::write(v.join(DEST_MARKER), b"").unwrap();
        assert!(sources(d.path()).is_empty());
        assert_eq!(marked_destinations(d.path()), vec![v]);
    }
}
