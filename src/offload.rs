//! The offload itself: find new files, sort them into flights, tag the
//! flights with parcels, copy and verify to every destination.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::Result;
use chrono::{Duration, NaiveDate};

use crate::config::Config;
use crate::copy::{Outcome, copy_verified};
use crate::detect::Source;
use crate::flights::{self, Flight};
use crate::geo::{Lookup, Place};
use crate::ledger::Ledger;
use crate::media::{self, MediaFile};
use crate::report;

#[derive(Debug, Default)]
pub struct Summary {
    pub copied: usize,
    pub copied_bytes: u64,
    pub skipped: usize,
    pub errors: Vec<String>,
    /// Non-fatal notices (low space).
    pub warnings: Vec<String>,
    /// e.g. "2026-04-28/Flight-1_1827_APN-999-12-345A"
    pub folders: BTreeSet<String>,
    /// Flights that received new files in the main destination, with their folder there.
    pub touched: Vec<(Flight, Place, PathBuf)>,
}

pub struct Offloader<'a> {
    pub cfg: &'a Config,
    pub ledger: &'a Ledger,
    pub lookup: Option<&'a Lookup>,
    pub dests: Vec<PathBuf>,
    pub dry_run: bool,
    pub log: &'a mut dyn FnMut(&str),
}

type FlightKey = (NaiveDate, u32);

impl Offloader<'_> {
    pub fn run(&mut self, src: &Source) -> Result<Summary> {
        let mut sum = Summary::default();
        let files = media::scan(&src.mount, &self.cfg.skip_extensions)?;
        let dest_keys: Vec<String> = self.dests.iter().map(|d| d.display().to_string()).collect();

        let mut pending: Vec<&MediaFile> = Vec::new();
        for f in &files {
            let mut needed = false;
            for d in &dest_keys {
                if !self.ledger.is_copied(d, &f.key())? {
                    needed = true;
                }
            }
            if needed { pending.push(f) } else { sum.skipped += 1 }
        }
        (self.log)(&format!(
            "{}: {} files on the drone, {} new",
            src.model,
            files.len(),
            pending.len()
        ));
        if pending.is_empty() {
            return Ok(sum);
        }

        // 1. Flights: reuse earlier assignments, assign the rest.
        let dates: BTreeSet<NaiveDate> = pending.iter().map(|f| f.date()).collect();
        let mut known: Vec<Flight> = Vec::new();
        let mut places: Vec<Place> = Vec::new();
        for d in &dates {
            for (f, p) in self.ledger.flights_on(*d)? {
                known.push(f);
                places.push(p);
            }
        }
        let mut assigned: BTreeMap<String, FlightKey> = BTreeMap::new();
        let mut fresh: Vec<&MediaFile> = Vec::new();
        for f in &pending {
            match self.ledger.file_flight(&f.key())? {
                Some(k) => {
                    assigned.insert(f.key(), k);
                }
                None => fresh.push(f),
            }
        }
        let times: Vec<_> = fresh.iter().map(|f| f.taken).collect();
        let idx = flights::assign(&mut known, &times, Duration::minutes(self.cfg.flight_gap_minutes));
        places.resize(known.len(), Place::default());

        // 2. Where was each flight? Only the new files add information.
        let mut fixes: BTreeMap<usize, Vec<(f64, f64)>> = BTreeMap::new();
        for (f, &i) in fresh.iter().zip(&idx) {
            if let Some(g) = media::gps(f) {
                fixes.entry(i).or_default().push(g);
            }
        }
        let touched: BTreeSet<usize> = idx.iter().copied().collect();
        if !self.dry_run {
            self.ledger.begin()?;
        }
        for &i in &touched {
            let pts = fixes.get(&i).cloned().unwrap_or_default();
            let newer = match self.lookup {
                Some(l) => l.place(&pts)?,
                None => Place { centre: crate::geo::centre(&pts), geotagged: pts.len(), ..Place::default() },
            };
            places[i] = places[i].merge(&newer);
            if known[i].folder.is_none() {
                known[i].folder = Some(report::folder_name(&known[i], &places[i]));
            }
            if !self.dry_run {
                self.ledger.save_flight(&known[i], &places[i])?;
            }
        }
        for (f, &i) in fresh.iter().zip(&idx) {
            let k = (known[i].date, known[i].num);
            assigned.insert(f.key(), k);
            if !self.dry_run {
                self.ledger.set_file_flight(&f.key(), k.0, k.1)?;
            }
        }
        if !self.dry_run {
            self.ledger.commit()?;
        }

        let by_key: BTreeMap<FlightKey, usize> =
            known.iter().enumerate().map(|(i, f)| ((f.date, f.num), i)).collect();
        let folder_of = |k: &FlightKey| -> Option<(PathBuf, usize)> {
            let i = *by_key.get(k)?;
            let f = &known[i];
            Some((PathBuf::from(f.date.to_string()).join(f.folder.clone()?), i))
        };

        // 3. Copy and verify, destination by destination.
        let mut written: BTreeSet<(usize, usize)> = BTreeSet::new(); // (dest, flight)
        for (di, dest) in self.dests.iter().enumerate() {
            let mut needed = 0u64;
            for f in &pending {
                if !self.ledger.is_copied(&dest_keys[di], &f.key())? {
                    needed += f.size;
                }
            }
            if !self.dry_run
                && let Some(msg) = crate::space::shortfall(dest, needed) {
                    sum.errors.push(msg);
                    continue;
                }
            for f in &pending {
                if self.ledger.is_copied(&dest_keys[di], &f.key())? {
                    continue;
                }
                let Some((rel_dir, fi)) = assigned.get(&f.key()).and_then(&folder_of) else {
                    sum.errors.push(format!("{}: no flight assigned", f.name));
                    continue;
                };
                let rel = rel_dir.join(&f.name);
                sum.folders.insert(rel_dir.display().to_string());
                if self.dry_run {
                    (self.log)(&format!("would copy {} -> {}", f.name, dest.join(&rel).display()));
                    continue;
                }
                match copy_verified(&f.path, &dest.join(&rel)) {
                    Ok(out) => {
                        let hash = match out {
                            Outcome::Copied(h) => {
                                sum.copied += 1;
                                sum.copied_bytes += f.size;
                                h
                            }
                            Outcome::AlreadyThere(h) => h,
                        };
                        self.ledger.record_copy(&dest_keys[di], &f.key(), &rel.display().to_string(), &hash)?;
                        written.insert((di, fi));
                    }
                    Err(e) => sum.errors.push(format!("{}: {e:#}", f.name)),
                }
            }
        }

        for dest in &self.dests {
            if let Some(w) = crate::space::low(dest, self.cfg.low_space_warn_gb) {
                sum.warnings.push(w);
            }
        }

        // 4. Summaries.
        let source_note = self.lookup.and_then(|l| l.updated()).map(|u| u.chars().take(10).collect::<String>());
        for &(di, fi) in &written {
            if di == 0 {
                let f = &known[fi];
                let dir = self.dests[0].join(f.date.to_string()).join(f.folder.clone().unwrap_or_default());
                sum.touched.push((f.clone(), places[fi].clone(), dir));
            }
        }
        for (di, fi) in written {
            let f = &known[fi];
            let dir = self.dests[di].join(f.date.to_string()).join(f.folder.clone().unwrap_or_default());
            if let Err(e) = report::write(&dir, f, &places[fi], &src.model, source_note.as_deref()) {
                sum.errors.push(format!("shoot-info.txt in {}: {e:#}", dir.display()));
            }
        }
        Ok(sum)
    }
}

/// The main destination plus any marked external drives that are mounted.
pub fn destinations(cfg: &Config) -> Vec<PathBuf> {
    let mut v = vec![cfg.destination.clone()];
    for drive in crate::detect::marked_destinations(&cfg.volumes_root) {
        v.push(drive.join(&cfg.external_folder));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn drone(root: &std::path::Path, files: &[(&str, &[u8])]) -> Source {
        let v = root.join("Untitled");
        fs::create_dir_all(v.join("DCIM/DJI_001")).unwrap();
        fs::create_dir_all(v.join("MISC")).unwrap();
        fs::write(v.join("MISC/mavic4_edcf.db"), b"").unwrap();
        for (n, b) in files {
            fs::write(v.join("DCIM/DJI_001").join(n), b).unwrap();
        }
        crate::detect::inspect(&v).unwrap()
    }

    fn srt(lat: f64, lon: f64) -> Vec<u8> {
        format!("1\n00:00:00,000 --> 00:00:00,033\n[latitude: {lat}] [longitude: {lon}]\n").into_bytes()
    }

    fn parcel_db(dir: &std::path::Path) -> Lookup {
        let path = dir.join("p.db");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE meta(k TEXT PRIMARY KEY, v TEXT);
             CREATE TABLE parcels(id INTEGER PRIMARY KEY, apn TEXT NOT NULL, situs TEXT, acres REAL, geom BLOB NOT NULL);
             CREATE VIRTUAL TABLE parcels_rt USING rtree(id, minx, maxx, miny, maxy);
             CREATE TABLE cities(id INTEGER PRIMARY KEY, name TEXT NOT NULL, geom BLOB NOT NULL);
             CREATE VIRTUAL TABLE cities_rt USING rtree(id, minx, maxx, miny, maxy);",
        )
        .unwrap();
        let page = serde_json::json!({"features": [{"properties": {"apn": "99912345A", "situs_address": "100 EXAMPLE RD", "acres": 4.0},
            "geometry": {"type": "Polygon", "coordinates": [[[-110.02, 31.90], [-110.00, 31.90], [-110.00, 31.92], [-110.02, 31.92], [-110.02, 31.90]]]}}]});
        crate::geo::insert_parcels(&db, &page).unwrap();
        db.execute("INSERT INTO meta VALUES ('updated', '2026-11-24T00:00:00-07:00')", []).unwrap();
        drop(db);
        Lookup::open(&path).unwrap()
    }

    #[test]
    fn full_offload_twice_then_new_drive() {
        let tmp = tempfile::tempdir().unwrap();
        let vols = tmp.path().join("Volumes");
        let home = tmp.path().join("Drone");
        let cfg = Config { destination: home.clone(), volumes_root: vols.clone(), ..Config::default() };
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let lookup = parcel_db(tmp.path());
        let mut log = |_: &str| {};

        // Flight 1 (video with GPS over the parcel) and flight 2 (photos, no GPS).
        let src = drone(
            &vols,
            &[
                ("DJI_20260428182706_0001_D.MP4", b"video-1"),
                ("DJI_20260428182706_0001_D.SRT", &srt(31.912345, -110.012345)),
                ("DJI_20260428182706_0001_D.LRF", b"preview"),
                ("DJI_20260428183000_0002_D.JPG", b"photo-2"),
                ("DJI_20260428190500_0003_D.JPG", b"photo-3"),
            ],
        );
        let mut off = Offloader { cfg: &cfg, ledger: &ledger, lookup: Some(&lookup), dests: vec![home.clone()], dry_run: false, log: &mut log };
        let s = off.run(&src).unwrap();
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.copied, 4); // LRF skipped
        let f1 = home.join("2026-04-28/Flight-1_1827_APN-999-12-345A");
        assert!(f1.join("DJI_20260428182706_0001_D.MP4").exists());
        assert!(f1.join("DJI_20260428183000_0002_D.JPG").exists());
        assert!(!f1.join("DJI_20260428182706_0001_D.LRF").exists());
        assert!(home.join("2026-04-28/Flight-2_1905/DJI_20260428190500_0003_D.JPG").exists());
        let info = fs::read_to_string(f1.join("shoot-info.txt")).unwrap();
        assert!(info.contains("999-12-345A") && info.contains("100 Example Rd") && info.contains("Unincorporated"), "{info}");

        // Plug in again after another flight: only the new file moves, into Flight-3.
        fs::write(src.mount.join("DCIM/DJI_001/DJI_20260428194000_0004_D.JPG"), b"photo-4").unwrap();
        let mut off = Offloader { cfg: &cfg, ledger: &ledger, lookup: Some(&lookup), dests: vec![home.clone()], dry_run: false, log: &mut log };
        let s = off.run(&src).unwrap();
        assert_eq!((s.copied, s.skipped), (1, 4));
        assert!(home.join("2026-04-28/Flight-3_1940/DJI_20260428194000_0004_D.JPG").exists());

        // A backup drive marked later receives everything, in the same folders.
        let t7 = vols.join("T7");
        fs::create_dir_all(&t7).unwrap();
        fs::write(t7.join(crate::detect::DEST_MARKER), b"").unwrap();
        let dests = destinations(&cfg);
        assert_eq!(dests.len(), 2);
        let mut off = Offloader { cfg: &cfg, ledger: &ledger, lookup: Some(&lookup), dests, dry_run: false, log: &mut log };
        let s = off.run(&src).unwrap();
        assert_eq!(s.copied, 5);
        assert!(t7.join("Drone/2026-04-28/Flight-1_1827_APN-999-12-345A/DJI_20260428182706_0001_D.SRT").exists());
        assert!(t7.join("Drone/2026-04-28/Flight-3_1940/shoot-info.txt").exists());
        // The drone itself is never modified.
        assert_eq!(fs::read_dir(src.mount.join("DCIM/DJI_001")).unwrap().count(), 6);
    }
}
