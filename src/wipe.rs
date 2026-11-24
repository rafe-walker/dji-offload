//! Optional: clear the drone after a successful offload.
//!
//! A file on the drone is deleted only when every destination used in this
//! run holds a copy whose hash, re-read now from the Mac's disk, matches the
//! hash recorded when it was copied from the drone. Anything else stays.
//! Only files under `DCIM/` are ever touched.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::ledger::Ledger;
use crate::media::{self, MediaFile};

#[derive(Debug, Default)]
pub struct Plan {
    pub delete: Vec<PathBuf>,
    pub bytes: u64,
    /// Files that stay, with the reason.
    pub keep: Vec<(String, String)>,
}

fn verified_everywhere(ledger: &Ledger, f: &MediaFile, dests: &[PathBuf]) -> Result<Result<(), String>> {
    for d in dests {
        let key = d.display().to_string();
        let Some((rel, hash)) = ledger.copy_record(&key, &f.key())? else {
            return Ok(Err(format!("not copied to {}", d.display())));
        };
        let copy = d.join(&rel);
        match crate::copy::hash_hex(&copy) {
            Ok(h) if h == hash => {}
            Ok(_) => return Ok(Err(format!("copy on {} no longer matches", d.display()))),
            Err(_) => return Ok(Err(format!("copy missing from {}", d.display()))),
        }
    }
    Ok(Ok(()))
}

/// Work out what can be deleted from the drone at `mount`.
/// `skip_ext` files (e.g. LRF previews) go only when their video is safe.
pub fn plan(ledger: &Ledger, mount: &Path, dests: &[PathBuf], skip_ext: &[String]) -> Result<Plan> {
    let mut p = Plan::default();
    let all = media::scan(mount, &[])?;
    let mut safe_stems = std::collections::BTreeSet::new();
    let (previews, real): (Vec<&MediaFile>, Vec<&MediaFile>) =
        all.iter().partition(|f| skip_ext.iter().any(|s| s.eq_ignore_ascii_case(&f.ext)));
    for f in real {
        match verified_everywhere(ledger, f, dests)? {
            Ok(()) => {
                safe_stems.insert(f.path.with_extension(""));
                p.bytes += f.size;
                p.delete.push(f.path.clone());
            }
            Err(why) => p.keep.push((f.name.clone(), why)),
        }
    }
    for f in previews {
        if safe_stems.contains(&f.path.with_extension("")) {
            p.bytes += f.size;
            p.delete.push(f.path.clone());
        } else {
            p.keep.push((f.name.clone(), "preview of a file that is not safely copied".into()));
        }
    }
    Ok(p)
}

/// Delete the planned files. Returns how many were removed.
pub fn execute(plan: &Plan) -> (usize, Vec<String>) {
    let mut n = 0;
    let mut errors = Vec::new();
    for path in &plan.delete {
        match fs::remove_file(path) {
            Ok(()) => n += 1,
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    (n, errors)
}

/// "Wipe the drone?" dialog. Defaults to keeping the files, and gives up
/// (keeps) after two minutes if nobody answers.
pub fn ask(files: usize, bytes: u64) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let script = format!(
        "display dialog \"All {files} files ({}) are verified on your Mac. Delete them from the drone?\" \
         with title \"Drone offload\" buttons {{\"Keep on drone\", \"Wipe drone\"}} \
         default button \"Keep on drone\" giving up after 120",
        crate::space::gb(bytes)
    );
    std::process::Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Wipe drone"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy::{Outcome, copy_verified};

    #[test]
    fn deletes_only_verified_files() {
        let d = tempfile::tempdir().unwrap();
        let mount = d.path().join("Untitled");
        let dcim = mount.join("DCIM/DJI_001");
        fs::create_dir_all(&dcim).unwrap();
        fs::create_dir_all(mount.join("MISC")).unwrap();
        fs::write(mount.join("MISC/keep.db"), b"x").unwrap();
        for n in ["DJI_20260428182706_0001_D.MP4", "DJI_20260428182706_0001_D.LRF", "DJI_20260428182800_0002_D.JPG", "DJI_20260428182900_0003_D.JPG"] {
            fs::write(dcim.join(n), n.as_bytes()).unwrap();
        }
        let dest = d.path().join("Drone");
        let ledger = Ledger::open(&d.path().join("l.db")).unwrap();
        let files = media::scan(&mount, &[]).unwrap();
        for f in files.iter().filter(|f| f.ext != "LRF") {
            let rel = format!("x/{}", f.name);
            let Outcome::Copied(h) = copy_verified(&f.path, &dest.join(&rel)).unwrap() else { panic!() };
            ledger.record_copy(&dest.display().to_string(), &f.key(), &rel, &h).unwrap();
        }
        // Corrupt one copy on the Mac: its original must stay on the drone.
        fs::write(dest.join("x/DJI_20260428182900_0003_D.JPG"), b"bitrot!!!!!!!!!!!!!!!!!!!!!!!!").unwrap();

        let plan = plan(&ledger, &mount, &[dest.clone()], &["LRF".into()]).unwrap();
        assert_eq!(plan.delete.len(), 3, "{:?}", plan.keep); // MP4, its LRF, photo 2
        assert_eq!(plan.keep.len(), 1);
        let (n, errs) = execute(&plan);
        assert_eq!((n, errs.len()), (3, 0));
        assert!(dcim.join("DJI_20260428182900_0003_D.JPG").exists());
        assert!(!dcim.join("DJI_20260428182706_0001_D.LRF").exists());
        assert!(mount.join("MISC/keep.db").exists());

        // A second, unconnected destination with no copy blocks everything.
        let blocked = super::plan(&ledger, &mount, &[dest, d.path().join("T7")], &["LRF".into()]).unwrap();
        assert!(blocked.delete.is_empty());
    }
}
