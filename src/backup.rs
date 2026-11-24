//! Backup mode: sweep this Mac for DJI footage and make sure every file has
//! an identical copy on the backup drive.
//!
//! Files are matched by name and size first, then confirmed by BLAKE3 hash
//! (cached by path, size and mtime, so later sweeps only hash what changed).
//! Footage already sorted by dji-offload keeps its `date/flight` folders on
//! the backup; anything found elsewhere goes to `<date>/Found/`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::copy::{Outcome, copy_verified};
use crate::ledger::Ledger;
use crate::media::time_from_name;

const MEDIA: &[&str] = &["JPG", "JPEG", "DNG", "MP4", "MOV", "SRT"];

/// Directories never worth descending into.
fn skip_dir(name: &str) -> bool {
    name.starts_with('.')
        || matches!(name, "Library" | "node_modules" | "target" | "Applications")
        || name.ends_with(".photoslibrary")
        || name.ends_with(".app")
}

/// Is this a DJI media file (`DJI_…` with a media extension)?
pub fn is_dji_media(name: &str) -> bool {
    let upper = name.to_uppercase();
    upper.starts_with("DJI_")
        && Path::new(&upper).extension().map(|e| MEDIA.contains(&e.to_string_lossy().as_ref())).unwrap_or(false)
}

fn walk(dir: &Path, exclude: &[PathBuf], out: &mut Vec<PathBuf>) {
    if exclude.iter().any(|x| dir.starts_with(x)) {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if !skip_dir(&name) {
                walk(&e.path(), exclude, out);
            }
        } else if ft.is_file() && !name.starts_with('.') && is_dji_media(&name) {
            out.push(e.path());
        }
    }
}

/// Every DJI media file under `roots`, skipping `exclude` trees.
pub fn find(roots: &[PathBuf], exclude: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for r in roots {
        walk(r, exclude, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

#[derive(Debug, Default)]
pub struct Report {
    pub found: usize,
    /// Identical copies of the same file found in several places on the Mac.
    pub duplicates: usize,
    pub already: usize,
    pub copied: usize,
    pub copied_bytes: u64,
    pub conflicts: usize,
    pub errors: Vec<String>,
}

/// Where a found file belongs on the backup drive.
fn target_rel(path: &Path, sorted_roots: &[PathBuf]) -> PathBuf {
    let name = path.file_name().unwrap_or_default();
    for root in sorted_roots {
        if let Ok(rel) = path.strip_prefix(root) {
            return rel.to_path_buf();
        }
    }
    let day = time_from_name(&name.to_string_lossy())
        .map(|t| t.date().to_string())
        .unwrap_or_else(|| "Undated".into());
    PathBuf::from(day).join("Found").join(name)
}

/// Index of the backup: (file name, size) -> paths already there.
fn index(root: &Path) -> BTreeMap<(String, u64), Vec<PathBuf>> {
    let mut files = Vec::new();
    walk(root, &[], &mut files);
    let mut idx: BTreeMap<(String, u64), Vec<PathBuf>> = BTreeMap::new();
    for p in files {
        if let (Some(n), Ok(m)) = (p.file_name(), fs::metadata(&p)) {
            idx.entry((n.to_string_lossy().to_string(), m.len())).or_default().push(p);
        }
    }
    idx
}

/// Bytes that `sweep` would need to copy (files with no same-name, same-size
/// match on the backup). A quick upper bound for the free-space check.
pub fn estimate(found: &[PathBuf], backup_root: &Path) -> u64 {
    let idx = index(backup_root);
    found
        .iter()
        .filter_map(|p| {
            let size = fs::metadata(p).ok()?.len();
            let name = p.file_name()?.to_string_lossy().to_string();
            (!idx.contains_key(&(name, size))).then_some(size)
        })
        .sum()
}

/// Make sure every file in `found` has an identical copy under `backup_root`.
pub fn sweep(
    ledger: &Ledger,
    found: &[PathBuf],
    backup_root: &Path,
    sorted_roots: &[PathBuf],
    dry_run: bool,
    log: &mut dyn FnMut(&str),
) -> Result<Report> {
    let mut r = Report { found: found.len(), ..Report::default() };
    let mut idx = index(backup_root);
    let mut seen: BTreeSet<String> = BTreeSet::new(); // hashes handled this sweep
    // Files already sorted by dji-offload go first, so their date/flight
    // folders win over stray duplicates elsewhere on the Mac.
    let mut ordered: Vec<&PathBuf> = found.iter().collect();
    ordered.sort_by_key(|p| !sorted_roots.iter().any(|r| p.starts_with(r)));
    for src in ordered {
        let Ok(meta) = fs::metadata(src) else { continue };
        let name = src.file_name().unwrap_or_default().to_string_lossy().to_string();
        let src_hash = match ledger.cached_hash(src) {
            Ok(h) => h,
            Err(e) => {
                r.errors.push(format!("{}: {e:#}", src.display()));
                continue;
            }
        };
        if seen.contains(&src_hash) {
            r.duplicates += 1; // same bytes as a file already handled this sweep
            continue;
        }
        let mut present = false;
        for cand in idx.get(&(name.clone(), meta.len())).into_iter().flatten() {
            if ledger.cached_hash(cand).map(|h| h == src_hash).unwrap_or(false) {
                present = true;
                break;
            }
        }
        seen.insert(src_hash.clone());
        if present {
            r.already += 1;
            continue;
        }
        let mut rel = target_rel(src, sorted_roots);
        let mut dst = backup_root.join(&rel);
        if dst.exists() {
            // Same name, different content: keep both.
            r.conflicts += 1;
            let parent = rel.parent().map(Path::to_path_buf).unwrap_or_default();
            rel = parent.join("conflicts").join(format!("{}_{}", &src_hash[..8], name));
            dst = backup_root.join(&rel);
        }
        if dry_run {
            log(&format!("would back up {} -> {}", src.display(), dst.display()));
            r.copied += 1;
            r.copied_bytes += meta.len();
            continue;
        }
        match copy_verified(src, &dst) {
            Ok(Outcome::Copied(_)) | Ok(Outcome::AlreadyThere(_)) => {
                r.copied += 1;
                r.copied_bytes += meta.len();
                idx.entry((name, meta.len())).or_default().push(dst);
            }
            Err(e) => r.errors.push(format!("{}: {e:#}", src.display())),
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_dji_names() {
        assert!(is_dji_media("DJI_20260428182706_0001_D.JPG"));
        assert!(is_dji_media("dji_0042.mp4"));
        assert!(!is_dji_media("DJI_20260428182706_0001_D.LRF"));
        assert!(!is_dji_media("IMG_0001.JPG"));
    }

    #[test]
    fn sweeps_once_and_keeps_conflicts() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let sorted = home.join("Drone");
        let a = sorted.join("2026-04-28/Flight-1_1827/DJI_20260428182706_0001_D.JPG");
        let b = home.join("Downloads/DJI_20260501090000_0007_D.MP4");
        let dup = home.join("Desktop/copy/DJI_20260428182706_0001_D.JPG"); // same bytes as `a`
        let lib = home.join("Library/Caches/DJI_20260101000000_0001_D.JPG"); // ignored
        for (p, body) in [(&a, "photo"), (&b, "video"), (&dup, "photo"), (&lib, "cache")] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        let backup = d.path().join("T7/Drone Backup");
        fs::create_dir_all(&backup).unwrap();
        let ledger = Ledger::open(&d.path().join("l.db")).unwrap();
        let mut log = |_: &str| {};

        let found = find(&[home.clone()], &[]);
        assert_eq!(found.len(), 3);
        let r = sweep(&ledger, &found, &backup, &[sorted.clone()], false, &mut log).unwrap();
        assert_eq!((r.copied, r.duplicates, r.already, r.errors.len()), (2, 1, 0, 0));
        assert!(backup.join("2026-04-28/Flight-1_1827/DJI_20260428182706_0001_D.JPG").exists());
        assert!(backup.join("2026-05-01/Found/DJI_20260501090000_0007_D.MP4").exists());

        // Second sweep: nothing to do.
        let r = sweep(&ledger, &found, &backup, &[sorted.clone()], false, &mut log).unwrap();
        assert_eq!((r.copied, r.duplicates, r.already), (0, 1, 2));

        // A different file under a name the backup already has: kept as a conflict.
        fs::write(&b, "video, re-exported and longer").unwrap();
        let r = sweep(&ledger, &found, &backup, &[sorted], false, &mut log).unwrap();
        assert_eq!((r.copied, r.conflicts), (1, 1));
        assert_eq!(fs::read_dir(backup.join("2026-05-01/Found/conflicts")).unwrap().count(), 1);
    }
}
