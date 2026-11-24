//! `shoot-info.txt`: a plain summary dropped into every flight folder.

use std::fs;
use std::path::Path;

use anyhow::Result;

use crate::flights::Flight;
use crate::geo::{Place, title_case};

pub fn folder_name(f: &Flight, place: &Place) -> String {
    match place.main() {
        Some(p) => format!("{}_APN-{}", f.default_folder(), p.apn),
        None => f.default_folder(),
    }
}

fn human(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 1.0 { format!("{gb:.1} GB") } else { format!("{:.0} MB", bytes as f64 / 1e6) }
}

/// Write (or rewrite) the summary from what is actually in the folder.
pub fn write(dir: &Path, f: &Flight, place: &Place, model: &str, parcel_source: Option<&str>) -> Result<()> {
    let (mut photos, mut videos, mut bytes) = (0usize, 0usize, 0u64);
    for e in fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().to_uppercase();
        if name.starts_with('.') || name == "SHOOT-INFO.TXT" {
            continue;
        }
        bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        if name.ends_with(".JPG") || name.ends_with(".JPEG") || name.ends_with(".DNG") {
            photos += 1;
        } else if name.ends_with(".MP4") || name.ends_with(".MOV") {
            videos += 1;
        }
    }

    let mut s = String::new();
    s += &format!(
        "Flight {} — {} {}–{} ({model})\n",
        f.num,
        f.date.format("%Y-%m-%d"),
        f.start.format("%H:%M"),
        f.end.format("%H:%M")
    );
    s += &format!("Photos: {photos}   Videos: {videos}   Size: {}\n\n", human(bytes));

    match place.main() {
        Some(p) => {
            s += &format!("Parcel (APN):  {}\n", p.apn);
            s += &format!(
                "Address:       {}\n",
                p.situs.as_deref().map(title_case).unwrap_or_else(|| "none on file with the county".into())
            );
            if let Some(a) = p.acres {
                s += &format!("Acres:         {a:.2}\n");
            }
        }
        None if place.geotagged == 0 => s += "Parcel (APN):  no GPS in these files\n",
        None if parcel_source.is_none() => s += "Parcel (APN):  parcel database not installed (run `dji-offload parcels update`)\n",
        None => s += "Parcel (APN):  outside the parcel data (not in Cochise County?)\n",
    }
    if parcel_source.is_some() && place.geotagged > 0 {
        s += &format!("Jurisdiction:  {}\n", place.city_label());
    } else {
        s += "Jurisdiction:  unknown (needs GPS and the parcel database)\n";
    }
    if let Some((lat, lon)) = place.centre {
        s += &format!("Centre:        {lat:.6}, {lon:.6}\n");
        s += &format!("Map:           https://maps.google.com/?q={lat:.6},{lon:.6}\n");
    }
    if place.parcels.len() > 1 {
        s += "\nThis flight covered more than one parcel:\n";
        for (p, n) in &place.parcels {
            let addr = p.situs.as_deref().map(title_case).unwrap_or_default();
            s += &format!("  {:<14} {:>4} files  {addr}\n", p.apn, n);
        }
    }
    s += &format!("\nGeotagged files: {}\n", place.geotagged);
    if let Some(src) = parcel_source {
        s += &format!("Parcel data: Cochise County Assessor, local copy from {src}\n");
    }
    s += "Written by dji-offload. Parcel data is for reference only; it is not a survey.\n";
    fs::write(dir.join("shoot-info.txt"), s)?;
    Ok(())
}
