//! Spreadsheets kept next to the footage, rebuilt from the ledger after every
//! offload or tag, so they never drift out of sync:
//!
//! - `flight-log.csv`: every flight (Part 107 records, invoicing).
//! - `Library/catalog.csv`: flights tagged for the already-flown library,
//!   one row per flight with APN, address and where the samples are.
//! - `Promo/promo.csv`: flights tagged for your own marketing.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::flights::Flight;
use crate::geo::{Place, title_case};
use crate::jobs::Job;

pub struct Row {
    pub flight: Flight,
    pub place: Place,
    pub job: Job,
    pub dir: PathBuf,
}

#[derive(Default)]
struct Counts {
    photos: usize,
    videos: usize,
    bytes: u64,
}

fn counts(dir: &Path) -> Counts {
    let mut c = Counts::default();
    let Ok(rd) = fs::read_dir(dir) else { return c };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_uppercase();
        if n.starts_with('.') || !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let is_photo = [".JPG", ".JPEG", ".DNG"].iter().any(|x| n.ends_with(x));
        let is_video = [".MP4", ".MOV"].iter().any(|x| n.ends_with(x));
        if is_photo || is_video {
            c.bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
        if is_photo {
            c.photos += 1;
        } else if is_video {
            c.videos += 1;
        }
    }
    c
}

fn csv(fields: &[String]) -> String {
    fields
        .iter()
        .map(|f| {
            if f.contains([',', '"', '\n']) { format!("\"{}\"", f.replace('"', "\"\"")) } else { f.clone() }
        })
        .collect::<Vec<_>>()
        .join(",")
        + "\n"
}

fn place_fields(p: &Place) -> [String; 4] {
    match p.main() {
        Some(m) => [
            m.apn.clone(),
            m.situs.as_deref().map(title_case).unwrap_or_default(),
            m.acres.map(|a| format!("{a:.2}")).unwrap_or_default(),
            if p.geotagged > 0 { p.city_label() } else { String::new() },
        ],
        None => Default::default(),
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("csv.partial");
    fs::write(&tmp, text)?;
    fs::rename(tmp, path)?;
    Ok(())
}

/// Rewrite all three spreadsheets under `root` (the main destination).
pub fn rebuild(root: &Path, rows: &[Row]) -> Result<()> {
    let mut log = csv(&[
        "date", "flight", "start", "end", "minutes", "apn", "address", "acres", "jurisdiction", "photos", "videos", "gb",
        "client", "library", "promo", "package", "folder",
    ]
    .map(String::from));
    let mut cat = csv(&[
        "apn", "address", "acres", "jurisdiction", "date", "flight", "photos", "videos", "samples", "folder",
    ]
    .map(String::from));
    let mut promo = csv(&["date", "flight", "apn", "address", "jurisdiction", "photos", "videos", "client", "folder"].map(String::from));

    for r in rows {
        let f = &r.flight;
        let c = counts(&r.dir);
        let [apn, addr, acres, city] = place_fields(&r.place);
        let minutes = (f.end - f.start).num_seconds() as f64 / 60.0;
        let folder = r.dir.display().to_string();
        log.push_str(&csv(&[
            f.date.to_string(),
            f.num.to_string(),
            f.start.format("%H:%M").to_string(),
            f.end.format("%H:%M").to_string(),
            format!("{minutes:.0}"),
            apn.clone(),
            addr.clone(),
            acres.clone(),
            city.clone(),
            c.photos.to_string(),
            c.videos.to_string(),
            format!("{:.2}", c.bytes as f64 / 1e9),
            r.job.client.clone().unwrap_or_default(),
            if r.job.library { "yes" } else { "" }.into(),
            if r.job.promo { "yes" } else { "" }.into(),
            r.job.package.clone().unwrap_or_default(),
            folder.clone(),
        ]));
        if r.job.library {
            let samples = r.dir.join("Library Sample");
            cat.push_str(&csv(&[
                apn.clone(),
                addr.clone(),
                acres.clone(),
                city.clone(),
                f.date.to_string(),
                f.num.to_string(),
                c.photos.to_string(),
                c.videos.to_string(),
                if samples.exists() { samples.display().to_string() } else { String::new() },
                folder.clone(),
            ]));
        }
        if r.job.promo {
            promo.push_str(&csv(&[
                f.date.to_string(),
                f.num.to_string(),
                apn,
                addr,
                city,
                c.photos.to_string(),
                c.videos.to_string(),
                r.job.client.clone().unwrap_or_default(),
                folder,
            ]));
        }
    }
    write_atomic(&root.join("flight-log.csv"), &log)?;
    write_atomic(&root.join("Library/catalog.csv"), &cat)?;
    write_atomic(&root.join("Promo/promo.csv"), &promo)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::Parcel;
    use chrono::NaiveDateTime;

    #[test]
    fn writes_log_catalog_and_promo() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("2026-04-28/Flight-1_1827");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("DJI_1.JPG"), vec![0u8; 1000]).unwrap();
        fs::write(dir.join("DJI_2.MP4"), vec![0u8; 3000]).unwrap();
        fs::write(dir.join("shoot-info.txt"), "x").unwrap();
        let t = |s| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        let row = Row {
            flight: Flight { date: t("2026-04-28 18:27:06").date(), num: 1, start: t("2026-04-28 18:27:06"), end: t("2026-04-28 18:37:54"), folder: None },
            place: Place {
                parcels: vec![(Parcel { apn: "999-12-345A".into(), situs: Some("100 EXAMPLE RD".into()), acres: Some(4.0) }, 3)],
                city: None,
                centre: Some((31.9, -110.0)),
                geotagged: 3,
            },
            job: Job { client: Some("Example Realty, LLC".into()), library: true, promo: false, package: None },
            dir: dir.clone(),
        };
        rebuild(d.path(), &[row]).unwrap();
        let log = fs::read_to_string(d.path().join("flight-log.csv")).unwrap();
        assert!(log.contains("2026-04-28,1,18:27,18:37,11,999-12-345A,100 Example Rd,4.00,Unincorporated Cochise County,1,1,0.00,\"Example Realty, LLC\",yes,,"), "{log}");
        let cat = fs::read_to_string(d.path().join("Library/catalog.csv")).unwrap();
        assert_eq!(cat.lines().count(), 2);
        let promo = fs::read_to_string(d.path().join("Promo/promo.csv")).unwrap();
        assert_eq!(promo.lines().count(), 1); // header only
    }
}
