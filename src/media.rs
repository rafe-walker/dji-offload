//! Finding media on a DJI volume, and reading capture time and GPS from it.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{NaiveDate, NaiveDateTime};

/// One file found under `DCIM/` on the drone.
#[derive(Debug, Clone)]
pub struct MediaFile {
    pub path: PathBuf,
    /// File name only, e.g. `DJI_20260428182706_0001_D.JPG`.
    pub name: String,
    pub size: u64,
    /// Local capture time (drone clock).
    pub taken: NaiveDateTime,
    /// Upper-case extension without the dot.
    pub ext: String,
}

impl MediaFile {
    pub fn date(&self) -> NaiveDate {
        self.taken.date()
    }
    /// Stable identity used by the ledger: name + size.
    pub fn key(&self) -> String {
        format!("{}:{}", self.name, self.size)
    }
    pub fn is_photo(&self) -> bool {
        matches!(self.ext.as_str(), "JPG" | "JPEG" | "DNG")
    }
    pub fn is_video(&self) -> bool {
        matches!(self.ext.as_str(), "MP4" | "MOV")
    }
}

/// Parse the capture time DJI embeds in its file names:
/// `DJI_20260428182706_0001_D.JPG` -> 2026-04-28 18:27:06.
pub fn time_from_name(name: &str) -> Option<NaiveDateTime> {
    let rest = name.strip_prefix("DJI_")?;
    let stamp: String = rest.chars().take(14).collect();
    if stamp.len() != 14 || !stamp.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    NaiveDateTime::parse_from_str(&stamp, "%Y%m%d%H%M%S").ok()
}

/// Walk `DCIM/` and return every media file, oldest first.
/// Hidden files and the extensions in `skip_ext` are ignored.
pub fn scan(volume: &Path, skip_ext: &[String]) -> Result<Vec<MediaFile>> {
    let mut out = Vec::new();
    let dcim = volume.join("DCIM");
    walk(&dcim, skip_ext, &mut out)?;
    out.sort_by(|a, b| a.taken.cmp(&b.taken).then(a.name.cmp(&b.name)));
    Ok(out)
}

fn walk(dir: &Path, skip_ext: &[String], out: &mut Vec<MediaFile>) -> Result<()> {
    let Ok(rd) = fs::read_dir(dir) else { return Ok(()) };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk(&path, skip_ext, out)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_uppercase())
            .unwrap_or_default();
        if ext.is_empty() || skip_ext.iter().any(|s| s.eq_ignore_ascii_case(&ext)) {
            continue;
        }
        let meta = entry.metadata()?;
        let taken = time_from_name(&name)
            .or_else(|| exif_time(&path))
            .or_else(|| {
                meta.modified()
                    .ok()
                    .map(|t| chrono::DateTime::<chrono::Local>::from(t).naive_local())
            })
            .unwrap_or_default();
        out.push(MediaFile { path, name, size: meta.len(), taken, ext });
    }
    Ok(())
}

fn read_exif(path: &Path) -> Option<exif::Exif> {
    let file = fs::File::open(path).ok()?;
    exif::Reader::new().read_from_container(&mut BufReader::new(file)).ok()
}

fn exif_time(path: &Path) -> Option<NaiveDateTime> {
    let ext = path.extension()?.to_string_lossy().to_uppercase();
    if !matches!(ext.as_str(), "JPG" | "JPEG" | "DNG") {
        return None;
    }
    let ex = read_exif(path)?;
    let f = ex.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)?;
    if let exif::Value::Ascii(ref v) = f.value {
        let s = String::from_utf8_lossy(v.first()?);
        return NaiveDateTime::parse_from_str(s.trim(), "%Y:%m:%d %H:%M:%S").ok();
    }
    None
}

/// GPS position (lat, lon) of a file: EXIF for photos, the `.SRT` sidecar for videos.
pub fn gps(file: &MediaFile) -> Option<(f64, f64)> {
    if file.is_photo() {
        return exif_gps(&file.path);
    }
    if file.is_video() {
        return srt_gps(&file.path.with_extension("SRT"))
            .or_else(|| srt_gps(&file.path.with_extension("srt")));
    }
    None
}

fn exif_gps(path: &Path) -> Option<(f64, f64)> {
    let ex = read_exif(path)?;
    let coord = |tag, reftag| -> Option<f64> {
        let f = ex.get_field(tag, exif::In::PRIMARY)?;
        let exif::Value::Rational(ref v) = f.value else { return None };
        if v.len() < 3 {
            return None;
        }
        let deg = v[0].to_f64() + v[1].to_f64() / 60.0 + v[2].to_f64() / 3600.0;
        let r = ex.get_field(reftag, exif::In::PRIMARY)?.display_value().to_string();
        Some(if r.contains('S') || r.contains('W') { -deg } else { deg })
    };
    let lat = coord(exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef)?;
    let lon = coord(exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef)?;
    if lat == 0.0 && lon == 0.0 {
        return None; // no fix
    }
    Some((lat, lon))
}

/// DJI `.SRT` telemetry lines look like `[latitude: 31.912345] [longitude: -110.012345]`.
/// Returns the first non-zero fix in the file.
pub fn srt_gps(path: &Path) -> Option<(f64, f64)> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let lat = field(&line, "latitude");
        let lon = field(&line, "longitude");
        if let (Some(lat), Some(lon)) = (lat, lon)
            && (lat != 0.0 || lon != 0.0) {
                return Some((lat, lon));
            }
    }
    None
}

fn field(line: &str, key: &str) -> Option<f64> {
    let i = line.find(&format!("{key}:"))?;
    let rest = &line[i + key.len() + 1..];
    let num: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    num.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dji_names() {
        let t = time_from_name("DJI_20260428182706_0001_D.JPG").unwrap();
        assert_eq!(t.to_string(), "2026-04-28 18:27:06");
        assert!(time_from_name("IMG_0001.JPG").is_none());
        assert!(time_from_name("DJI_0001.JPG").is_none());
    }

    #[test]
    fn parses_srt_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.SRT");
        fs::write(
            &p,
            "1\n00:00:00,000 --> 00:00:00,033\n[iso: 100] [latitude: 0.000000] [longitude: 0.000000]\n\n2\n[iso: 100] [latitude: 31.912345] [longitude: -110.012345] [rel_alt: 50.0]\n",
        )
        .unwrap();
        assert_eq!(srt_gps(&p), Some((31.912345, -110.012345)));
    }
}
