//! Did the flight actually cover the parcel?
//!
//! Each photo or video becomes a [`Shot`] (position, altitude, gimbal angles,
//! lens). [`coverage`] then checks it against the parcel outline:
//! - **Mapping** (nadir grid): every photo's ground footprint is painted onto
//!   a ~2 m raster of the parcel; uncovered areas are named by compass sector.
//! - **Oblique** (real-estate stills/video): shots are binned by which side of
//!   the parcel the drone stood on; an empty side is a gap.
//!
//! The check runs in the field, so a gap can be re-flown before leaving.
//! Footprints assume flat ground at the take-off height (`RelativeAltitude`
//! is height above the take-off point, not above the ground below).

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use crate::geo::Ring;
use crate::media;

/// Camera position and pointing for one photo or video.
#[derive(Debug, Clone)]
pub struct Shot {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// Height above the take-off point (DJI `RelativeAltitude`).
    pub rel_alt_m: Option<f64>,
    pub abs_alt_m: Option<f64>,
    /// Degrees; -90 is straight down.
    pub gimbal_pitch: Option<f64>,
    /// Degrees clockwise from north. Gimbal yaw, or the aircraft's yaw when
    /// the gimbal yaw is missing.
    pub gimbal_yaw: Option<f64>,
    pub focal_35mm: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Lens assumed when a photo has no `FocalLengthIn35mmFilm`: the Mavic 4 Pro
/// Hasselblad wide camera writes 28 mm, 6144x4096 (3:2).
pub const DEFAULT_FOCAL_35MM: f64 = 28.0;
pub const DEFAULT_ASPECT: f64 = 1.5;

const XMP_SCAN_BYTES: u64 = 256 * 1024;
const XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

// ---------- reading shots ----------

/// Read position, altitude, gimbal and lens data for a photo (EXIF + DJI XMP)
/// or a video (its `.SRT` sidecar). `None` when the file has no GPS fix.
pub fn read_shot(path: &Path) -> Option<Shot> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let ext = path.extension()?.to_string_lossy().to_uppercase();
    match ext.as_str() {
        "JPG" | "JPEG" | "DNG" => read_photo(path, name),
        "MP4" | "MOV" => read_video(path, name),
        _ => None,
    }
}

fn valid_fix(lat: f64, lon: f64) -> Option<(f64, f64)> {
    let ok = lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0;
    (ok && (lat != 0.0 || lon != 0.0)).then_some((lat, lon))
}

fn read_photo(path: &Path, name: String) -> Option<Shot> {
    let mut head = Vec::new();
    File::open(path).ok()?.take(XMP_SCAN_BYTES).read_to_end(&mut head).ok()?;
    let xmp = xmp_packet(&head).map(|p| String::from_utf8_lossy(p).into_owned());
    let x = |k: &str| xmp.as_deref().and_then(|p| xmp_number(p, k));
    let ex = File::open(path)
        .ok()
        .and_then(|f| exif::Reader::new().read_from_container(&mut BufReader::new(f)).ok());

    let (lat, lon) = ex.as_ref().and_then(exif_gps).or_else(|| {
        let lon = x("drone-dji:GpsLongitude").or_else(|| x("drone-dji:GpsLongtitude"))?; // older DJI spelling
        valid_fix(x("drone-dji:GpsLatitude")?, lon)
    })?;

    let uint = |tag| -> Option<u32> {
        ex.as_ref()?.get_field(tag, exif::In::PRIMARY)?.value.get_uint(0).filter(|v| *v > 0)
    };
    let mut width = uint(exif::Tag::PixelXDimension).or_else(|| uint(exif::Tag::ImageWidth));
    let mut height = uint(exif::Tag::PixelYDimension).or_else(|| uint(exif::Tag::ImageLength));
    if matches!(uint(exif::Tag::Orientation), Some(5..=8)) {
        std::mem::swap(&mut width, &mut height);
    }

    Some(Shot {
        name,
        lat,
        lon,
        rel_alt_m: x("drone-dji:RelativeAltitude"),
        abs_alt_m: x("drone-dji:AbsoluteAltitude").or_else(|| ex.as_ref().and_then(exif_altitude)),
        gimbal_pitch: x("drone-dji:GimbalPitchDegree"),
        gimbal_yaw: x("drone-dji:GimbalYawDegree").or_else(|| x("drone-dji:FlightYawDegree")),
        focal_35mm: uint(exif::Tag::FocalLengthIn35mmFilm).map(f64::from),
        width,
        height,
    })
}

fn exif_gps(ex: &exif::Exif) -> Option<(f64, f64)> {
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
    valid_fix(
        coord(exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef)?,
        coord(exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef)?,
    )
}

fn exif_altitude(ex: &exif::Exif) -> Option<f64> {
    let f = ex.get_field(exif::Tag::GPSAltitude, exif::In::PRIMARY)?;
    let exif::Value::Rational(ref v) = f.value else { return None };
    let alt = v.first()?.to_f64();
    let below = ex
        .get_field(exif::Tag::GPSAltitudeRef, exif::In::PRIMARY)
        .and_then(|r| r.value.get_uint(0))
        == Some(1);
    alt.is_finite().then_some(if below { -alt } else { alt })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The XMP packet: from a JPEG APP1 segment (bounded by the segment length),
/// or a bare `<x:xmpmeta>` packet anywhere in the buffer (DNG/TIFF).
fn xmp_packet(buf: &[u8]) -> Option<&[u8]> {
    if let Some(i) = find(buf, XMP_HEADER) {
        let start = i + XMP_HEADER.len();
        let mut end = buf.len();
        if i >= 4 && buf[i - 4] == 0xFF && buf[i - 3] == 0xE1 {
            // The length counts its own two bytes, which start at i - 2.
            let len = u16::from_be_bytes([buf[i - 2], buf[i - 1]]) as usize;
            end = (i - 2 + len).clamp(start, buf.len());
        }
        if start < end {
            return Some(&buf[start..end]);
        }
    }
    let s = find(buf, b"<x:xmpmeta")?;
    let e = find(&buf[s..], b"</x:xmpmeta>").map_or(buf.len(), |e| s + e + b"</x:xmpmeta>".len());
    Some(&buf[s..e])
}

/// Value of `key` (e.g. `drone-dji:RelativeAltitude`) written either as an
/// attribute (`key="+52.300"`) or as an element (`<key>+52.300</key>`).
fn xmp_value<'a>(packet: &'a str, key: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(off) = packet[from..].find(key) {
        let i = from + off;
        from = i + key.len();
        let rest = &packet[from..];
        match packet[..i].chars().next_back() {
            Some('<') => {
                // Element: <key ...>value</key>
                if !rest.starts_with(|c: char| c == '>' || c.is_whitespace()) {
                    continue;
                }
                let Some(gt) = rest.find('>') else { continue };
                if rest[..gt].ends_with('/') {
                    continue;
                }
                let body = &rest[gt + 1..];
                let end = body.find('<').unwrap_or(body.len());
                return Some(body[..end].trim());
            }
            Some(c) if c.is_whitespace() => {
                // Attribute: key = "value"
                let Some(t) = rest.trim_start().strip_prefix('=') else { continue };
                let t = t.trim_start();
                let Some(q) = t.chars().next().filter(|q| *q == '"' || *q == '\'') else { continue };
                let body = &t[1..];
                let end = body.find(q)?;
                return Some(body[..end].trim());
            }
            _ => continue,
        }
    }
    None
}

fn xmp_number(packet: &str, key: &str) -> Option<f64> {
    let v = xmp_value(packet, key)?;
    let v = v.strip_prefix('+').unwrap_or(v);
    v.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// Videos: position from the `.SRT` sidecar (`media::srt_gps`). Altitude and
/// gimbal angles are best effort: read from the same telemetry block as the
/// first fix (`[rel_alt: 52.300 abs_alt: 1400.2]`, `gb_pitch:`/`gb_yaw:`),
/// when the model writes them. Many models write no gimbal angles at all.
fn read_video(path: &Path, name: String) -> Option<Shot> {
    let srt = [path.with_extension("SRT"), path.with_extension("srt")]
        .into_iter()
        .find(|p| p.is_file())?;
    let (lat, lon) = media::srt_gps(&srt)?;
    let block = first_fix_block(&srt).unwrap_or_default();
    let f = |keys: &[&str]| keys.iter().find_map(|k| srt_field(&block, k));
    Some(Shot {
        name,
        lat,
        lon,
        rel_alt_m: f(&["rel_alt"]),
        abs_alt_m: f(&["abs_alt"]),
        gimbal_pitch: f(&["gb_pitch", "gimbal_pitch"]),
        gimbal_yaw: f(&["gb_yaw", "gimbal_yaw"]),
        focal_35mm: None,
        width: None,
        height: None,
    })
}

/// The first subtitle block (blank-line separated) with a non-zero fix.
fn first_fix_block(path: &Path) -> Option<String> {
    let mut block = String::new();
    let mut lines = BufReader::new(File::open(path).ok()?).lines().map_while(Result::ok);
    loop {
        let line = lines.next();
        let end = line.as_deref().is_none_or(|l| l.trim().is_empty());
        if let Some(l) = &line {
            block.push_str(l);
            block.push('\n');
        }
        if end {
            if let (Some(lat), Some(lon)) = (srt_field(&block, "latitude"), srt_field(&block, "longitude"))
                && valid_fix(lat, lon).is_some()
            {
                return Some(block);
            }
            block.clear();
            line.as_ref()?;
        }
    }
}

/// `key: number` with `key` not preceded by a letter, digit or `_`.
fn srt_field(text: &str, key: &str) -> Option<f64> {
    let pat = format!("{key}:");
    let mut from = 0;
    while let Some(off) = text[from..].find(&pat) {
        let i = from + off;
        from = i + pat.len();
        if text[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let num: String = text[from..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+'))
            .collect();
        if let Ok(v) = num.trim_start_matches('+').parse::<f64>()
            && v.is_finite()
        {
            return Some(v);
        }
    }
    None
}

// ---------- coverage ----------

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Mapping,
    Oblique,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Coverage {
    pub kind: Kind,
    /// Mapping only: share of the parcel inside at least one photo footprint.
    pub covered_pct: Option<f64>,
    /// Mapping gaps are compass sectors ("NE corner", "south edge", "centre");
    /// oblique gaps are full sentences ("No shots from the west side.").
    pub gaps: Vec<String>,
    /// Oblique shots by the side of the parcel the drone stood on: N, E, S, W.
    pub sides: [usize; 4],
    /// Shots that took part: nadir photos whose footprint reached the parcel
    /// area, plus oblique shots within 1 km.
    pub shots_used: usize,
}

const SIDE_NAMES: [&str; 4] = ["north", "east", "south", "west"];

impl Coverage {
    /// One or two plain-English lines for `shoot-info.txt` and a notification.
    pub fn summary(&self) -> String {
        let (sector_gaps, notes): (Vec<&String>, Vec<&String>) = self.gaps.iter().partition(|g| !g.ends_with('.'));
        let oblique_n: usize = self.sides.iter().sum();
        let mapping = || match self.covered_pct {
            Some(p) => {
                let n = self.shots_used.saturating_sub(oblique_n);
                let mut s = format!("Mapping: {}% of the parcel covered ({n} photos).", pct(p));
                if !sector_gaps.is_empty() {
                    let names: Vec<&str> = sector_gaps.iter().map(|g| g.as_str()).collect();
                    s.push_str(&format!(" Not covered: {}.", names.join(", ")));
                }
                s
            }
            None => "Mapping: coverage could not be checked (no altitude data).".to_string(),
        };
        let oblique = || {
            let [n, e, s, w] = self.sides;
            let mut line = format!("Oblique: {oblique_n} shots (N {n}, E {e}, S {s}, W {w}).");
            for g in &notes {
                line.push(' ');
                line.push_str(g);
            }
            line
        };
        match self.kind {
            Kind::Unknown => "Coverage could not be checked: no gimbal or altitude data.".to_string(),
            Kind::Mapping => mapping(),
            Kind::Oblique => oblique(),
            Kind::Mixed => format!("{}\n{}", mapping(), oblique()),
        }
    }

    pub fn has_gaps(&self) -> bool {
        !self.gaps.is_empty()
    }
}

/// Never round a gap up to 100%.
fn pct(p: f64) -> String {
    if (99.5..100.0).contains(&p) { "99".into() } else { format!("{p:.0}") }
}

const R_EARTH: f64 = 6_371_008.8;
const NADIR_TOL_DEG: f64 = 15.0;
/// Fewer nadir photos than this, next to oblique shots, are top-down stills,
/// not a mapping grid.
const MIN_GRID_SHOTS: usize = 10;
/// A mapping grid with oblique shots making up at least this share is Mixed.
const MIXED_OBLIQUE_SHARE: f64 = 0.2;
const CELL_M: f64 = 2.0;
const MAX_CELLS: usize = 400_000;
const OBLIQUE_RADIUS_M: f64 = 1000.0;
const LOOK_AT_MAX_M: f64 = 500.0;
const SECTOR_OK: f64 = 0.9;
const SECTOR_NAMES: [[&str; 3]; 3] = [
    ["NW corner", "north edge", "NE corner"],
    ["west edge", "centre", "east edge"],
    ["SW corner", "south edge", "SE corner"],
];

/// Equirectangular projection to metres around a reference point. x east, y north.
#[derive(Clone, Copy)]
struct Proj {
    lon0: f64,
    lat0: f64,
    kx: f64,
    ky: f64,
}

impl Proj {
    fn new(lon0: f64, lat0: f64) -> Self {
        let ky = R_EARTH * std::f64::consts::PI / 180.0;
        Proj { lon0, lat0, kx: ky * lat0.to_radians().cos(), ky }
    }
    fn xy(&self, lon: f64, lat: f64) -> (f64, f64) {
        ((lon - self.lon0) * self.kx, (lat - self.lat0) * self.ky)
    }
}

/// Ground footprint (across, along) in metres of a nadir photo: across is the
/// image width, along is the image height (the heading direction).
///
/// The 35 mm-equivalent focal length is defined on the 43.27 mm diagonal, so
/// the equivalent frame is 36x24 mm for 3:2 images and 34.6x26.0 mm for 4:3.
/// With no focal length the Mavic 4 Pro wide camera (28 mm, 3:2) is assumed.
pub fn footprint_size(rel_alt_m: f64, focal_35mm: Option<f64>, width: Option<u32>, height: Option<u32>) -> (f64, f64) {
    let f = focal_35mm.filter(|f| *f > 0.0).unwrap_or(DEFAULT_FOCAL_35MM);
    let (long_over_short, portrait) = match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w.max(h) as f64 / w.min(h) as f64, h > w),
        _ => (DEFAULT_ASPECT, false),
    };
    let diag = (36.0f64 * 36.0 + 24.0 * 24.0).sqrt();
    let short = diag / (1.0 + long_over_short * long_over_short).sqrt();
    let long = short * long_over_short;
    let (fw, fh) = if portrait { (short, long) } else { (long, short) };
    // HFOV = 2 atan(fw / 2f), so the half-width on the ground is alt * fw / 2f.
    (rel_alt_m * fw / f, rel_alt_m * fh / f)
}

/// A raster of the parcel in local metres. Row 0 is the southern edge.
struct Grid {
    x0: f64,
    y0: f64,
    cell: f64,
    nx: usize,
    ny: usize,
    inside: Vec<bool>,
    covered: Vec<bool>,
    n_inside: usize,
}

impl Grid {
    fn new(rings: &[Vec<(f64, f64)>]) -> Option<Grid> {
        let pts = rings.iter().filter(|r| r.len() >= 3).flatten();
        let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for &(x, y) in pts {
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        let (w, h) = (x1 - x0, y1 - y0);
        if !(w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite()) {
            return None;
        }
        let cell = CELL_M.max((w * h / MAX_CELLS as f64).sqrt());
        let nx = ((w / cell).ceil() as usize).max(1);
        let ny = ((h / cell).ceil() as usize).max(1);
        let mut g = Grid {
            x0,
            y0,
            cell,
            nx,
            ny,
            inside: vec![false; nx * ny],
            covered: vec![false; nx * ny],
            n_inside: 0,
        };
        let mut inside = std::mem::take(&mut g.inside);
        g.scan(rings, |row, a, b| inside[row * nx + a..row * nx + b].fill(true));
        g.n_inside = inside.iter().filter(|c| **c).count();
        g.inside = inside;
        (g.n_inside > 0).then_some(g)
    }

    fn centre_of(&self, i: usize, j: usize) -> (f64, f64) {
        (self.x0 + (i as f64 + 0.5) * self.cell, self.y0 + (j as f64 + 0.5) * self.cell)
    }

    fn cell_at(&self, x: f64, y: f64) -> Option<usize> {
        let i = ((x - self.x0) / self.cell).floor();
        let j = ((y - self.y0) / self.cell).floor();
        (i >= 0.0 && j >= 0.0 && (i as usize) < self.nx && (j as usize) < self.ny)
            .then(|| j as usize * self.nx + i as usize)
    }

    /// Even-odd scanline fill: calls `span(row, first, end)` for each run of
    /// cells whose centres lie inside the rings.
    fn scan(&self, rings: &[Vec<(f64, f64)>], mut span: impl FnMut(usize, usize, usize)) {
        let (mut ry0, mut ry1) = (f64::MAX, f64::MIN);
        for &(_, y) in rings.iter().flatten() {
            ry0 = ry0.min(y);
            ry1 = ry1.max(y);
        }
        let to_row = |y: f64| ((y - self.y0) / self.cell - 0.5).clamp(0.0, self.ny as f64);
        let (j0, j1) = (to_row(ry0).floor() as usize, (to_row(ry1).ceil() as usize + 1).min(self.ny));
        let mut xs = Vec::new();
        for j in j0..j1 {
            let yc = self.y0 + (j as f64 + 0.5) * self.cell;
            xs.clear();
            for r in rings.iter().filter(|r| r.len() >= 3) {
                let mut k = r.len() - 1;
                for i in 0..r.len() {
                    let ((xi, yi), (xk, yk)) = (r[i], r[k]);
                    if (yi > yc) != (yk > yc) {
                        xs.push(xi + (yc - yi) * (xk - xi) / (yk - yi));
                    }
                    k = i;
                }
            }
            xs.sort_by(f64::total_cmp);
            for pair in xs.chunks_exact(2) {
                let col = |x: f64| ((x - self.x0) / self.cell - 0.5).ceil().clamp(0.0, self.nx as f64) as usize;
                let (a, b) = (col(pair[0]), col(pair[1]));
                if a < b {
                    span(j, a, b);
                }
            }
        }
    }

    /// Mean of the parcel cells: a centroid that respects holes and parts.
    fn centroid(&self) -> (f64, f64) {
        let (mut sx, mut sy) = (0.0, 0.0);
        for j in 0..self.ny {
            for i in 0..self.nx {
                if self.inside[j * self.nx + i] {
                    let (x, y) = self.centre_of(i, j);
                    sx += x;
                    sy += y;
                }
            }
        }
        (sx / self.n_inside as f64, sy / self.n_inside as f64)
    }
}

fn is_nadir(pitch: f64) -> bool {
    (pitch + 90.0).abs() <= NADIR_TOL_DEG
}

fn is_video(s: &Shot) -> bool {
    let n = s.name.to_ascii_uppercase();
    n.ends_with(".MP4") || n.ends_with(".MOV")
}

/// Check how well `shots` cover `parcel` (rings of (lon, lat)).
///
/// Shots with a gimbal pitch within 15° of -90 are nadir. A flight is Mapping
/// when it has nadir photos and (at least 10 of them, or no oblique shots)
/// oblique shots stay under 20% of the total; Mixed when a grid of 10+ nadir
/// photos sits next to more oblique shots than that; Oblique otherwise. Videos
/// with no gimbal data in their `.SRT` count as oblique. Unknown when nothing
/// usable is left (no pitch, no altitude for a grid, or an empty outline).
pub fn coverage(shots: &[Shot], parcel: &[Ring]) -> Coverage {
    let unknown = Coverage { kind: Kind::Unknown, covered_pct: None, gaps: vec![], sides: [0; 4], shots_used: 0 };
    let mut nadir: Vec<&Shot> = Vec::new();
    let mut oblique: Vec<&Shot> = Vec::new();
    for s in shots {
        match s.gimbal_pitch.filter(|p| p.is_finite()) {
            Some(p) if is_nadir(p) => nadir.push(s),
            Some(_) => oblique.push(s),
            None if is_video(s) => oblique.push(s),
            None => {}
        }
    }
    let total = nadir.len() + oblique.len();
    if total == 0 {
        return unknown;
    }
    let grid_like = nadir.len() >= MIN_GRID_SHOTS || (oblique.is_empty() && !nadir.is_empty());
    let mut kind = if !grid_like {
        Kind::Oblique
    } else if (oblique.len() as f64) < MIXED_OBLIQUE_SHARE * total as f64 {
        Kind::Mapping
    } else {
        Kind::Mixed
    };

    // Project around the parcel's bounding-box centre.
    let pts: Vec<&(f64, f64)> = parcel.iter().filter(|r| r.len() >= 3).flatten().collect();
    if pts.is_empty() {
        return unknown;
    }
    let (lo0, lo1, la0, la1) = pts.iter().fold((f64::MAX, f64::MIN, f64::MAX, f64::MIN), |(a, b, c, d), &&(x, y)| {
        (a.min(x), b.max(x), c.min(y), d.max(y))
    });
    let proj = Proj::new((lo0 + lo1) / 2.0, (la0 + la1) / 2.0);
    let rings: Vec<Vec<(f64, f64)>> =
        parcel.iter().map(|r| r.iter().map(|&(lon, lat)| proj.xy(lon, lat)).collect()).collect();
    let Some(mut grid) = Grid::new(&rings) else { return unknown };

    let mut out = Coverage { kind: kind.clone(), covered_pct: None, gaps: vec![], sides: [0; 4], shots_used: 0 };
    if kind == Kind::Mapping {
        // A stray oblique or two in a grid flight is ignored.
        oblique.clear();
    }
    if matches!(kind, Kind::Mapping | Kind::Mixed) {
        let (pct, gaps, used) = mapping(&nadir, &mut grid, &proj);
        out.covered_pct = pct;
        out.gaps.extend(gaps);
        out.shots_used += used;
        if pct.is_none() {
            kind = if kind == Kind::Mixed { Kind::Oblique } else { Kind::Unknown };
        }
    }
    if matches!(kind, Kind::Oblique | Kind::Mixed) {
        let (sides, gaps) = side_counts(&oblique, &grid, &proj);
        out.sides = sides;
        out.gaps.extend(gaps);
        out.shots_used += sides.iter().sum::<usize>();
    }
    if kind == Kind::Unknown {
        return unknown;
    }
    out.kind = kind;
    out
}

/// Unit vector of a compass heading (degrees clockwise from north).
fn heading(deg: f64) -> (f64, f64) {
    let r = deg.to_radians();
    (r.sin(), r.cos())
}

fn mapping(nadir: &[&Shot], grid: &mut Grid, proj: &Proj) -> (Option<f64>, Vec<String>, usize) {
    let mut used = 0;
    let (gx1, gy1) = (grid.x0 + grid.nx as f64 * grid.cell, grid.y0 + grid.ny as f64 * grid.cell);
    let mut covered = std::mem::take(&mut grid.covered);
    for s in nadir {
        let Some(alt) = s.rel_alt_m.filter(|a| a.is_finite() && *a > 1.0) else { continue };
        let (w, h) = footprint_size(alt, s.focal_35mm, s.width, s.height);
        let (mut cx, mut cy) = proj.xy(s.lon, s.lat);
        let poly: Vec<(f64, f64)> = match s.gimbal_yaw.filter(|y| y.is_finite()) {
            Some(yaw) => {
                let (ax, ay) = heading(yaw); // along = image "up"
                let (bx, by) = (ay, -ax); // across = image "right"
                // A camera a few degrees off nadir looks ahead of the drone.
                let tilt = (s.gimbal_pitch.unwrap_or(-90.0) + 90.0).to_radians();
                let ahead = alt * tilt.tan();
                cx += ax * ahead;
                cy += ay * ahead;
                [(1.0, 1.0), (1.0, -1.0), (-1.0, -1.0), (-1.0, 1.0)]
                    .iter()
                    .map(|(u, v)| (cx + bx * u * w / 2.0 + ax * v * h / 2.0, cy + by * u * w / 2.0 + ay * v * h / 2.0))
                    .collect()
            }
            None => {
                // Unknown heading: only the circle every rotation covers.
                let r = w.min(h) / 2.0;
                (0..16)
                    .map(|k| {
                        let a = k as f64 * std::f64::consts::TAU / 16.0;
                        (cx + r * a.cos(), cy + r * a.sin())
                    })
                    .collect()
            }
        };
        let (px0, px1, py0, py1) = poly.iter().fold((f64::MAX, f64::MIN, f64::MAX, f64::MIN), |(a, b, c, d), &(x, y)| {
            (a.min(x), b.max(x), c.min(y), d.max(y))
        });
        if px1 < grid.x0 || px0 > gx1 || py1 < grid.y0 || py0 > gy1 {
            continue;
        }
        used += 1;
        let nx = grid.nx;
        grid.scan(std::slice::from_ref(&poly), |row, a, b| covered[row * nx + a..row * nx + b].fill(true));
    }
    grid.covered = covered;
    if used == 0 {
        return (None, vec![], 0);
    }

    let mut hit = 0;
    let mut sec = [[(0usize, 0usize); 3]; 3]; // (parcel cells, covered cells)
    for j in 0..grid.ny {
        let row = 2 - (j * 3 / grid.ny).min(2); // 0 = north
        for i in 0..grid.nx {
            let k = j * grid.nx + i;
            if !grid.inside[k] {
                continue;
            }
            let s = &mut sec[row][(i * 3 / grid.nx).min(2)];
            s.0 += 1;
            if grid.covered[k] {
                s.1 += 1;
                hit += 1;
            }
        }
    }
    let min_cells = (grid.n_inside / 100).max(4);
    let mut gaps = Vec::new();
    for (r, names) in SECTOR_NAMES.iter().enumerate() {
        for (c, name) in names.iter().enumerate() {
            let (n, h) = sec[r][c];
            if n >= min_cells && (h as f64) < SECTOR_OK * n as f64 {
                gaps.push(name.to_string());
            }
        }
    }
    (Some(100.0 * hit as f64 / grid.n_inside as f64), gaps, used)
}

fn side_counts(oblique: &[&Shot], grid: &Grid, proj: &Proj) -> ([usize; 4], Vec<String>) {
    let (ox, oy) = grid.centroid();
    let mut sides = [0usize; 4];
    let (mut aimed, mut away) = (0, 0);
    for s in oblique {
        let (x, y) = proj.xy(s.lon, s.lat);
        let (dx, dy) = (x - ox, y - oy);
        let dist = dx.hypot(dy);
        if dist.is_nan() || dist > OBLIQUE_RADIUS_M {
            continue;
        }
        let bearing = dx.atan2(dy).to_degrees().rem_euclid(360.0);
        sides[(((bearing + 45.0) / 90.0) as usize) % 4] += 1;

        // Where is the camera looking? Ground hit from height and pitch,
        // clamped (level or upward shots look at the horizon).
        if let Some(yaw) = s.gimbal_yaw.filter(|y| y.is_finite()) {
            let down = -s.gimbal_pitch.unwrap_or(0.0);
            let reach = match s.rel_alt_m {
                Some(alt) if alt > 0.0 && down > 0.5 => (alt / down.to_radians().tan()).min(LOOK_AT_MAX_M),
                _ => LOOK_AT_MAX_M,
            };
            let (hx, hy) = heading(yaw);
            let (lx, ly) = (x + hx * reach, y + hy * reach);
            let on_parcel = grid.cell_at(lx, ly).is_some_and(|k| grid.inside[k]);
            let closer = (lx - ox).hypot(ly - oy) < dist;
            aimed += 1;
            if !on_parcel && !closer {
                away += 1;
            }
        }
    }
    let mut gaps = Vec::new();
    if sides.iter().sum::<usize>() == 0 {
        gaps.push("No oblique shots within 1 km of the parcel.".to_string());
        return (sides, gaps);
    }
    for (n, name) in sides.iter().zip(SIDE_NAMES) {
        if *n == 0 {
            gaps.push(format!("No shots from the {name} side."));
        }
    }
    if aimed >= 4 && away * 2 > aimed {
        gaps.push(format!("Most shots ({away} of {aimed}) point away from the parcel."));
    }
    (sides, gaps)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fictional test site near 31.91, -110.01.
    const LAT0: f64 = 31.91;
    const LON0: f64 = -110.01;

    fn ll(x: f64, y: f64) -> (f64, f64) {
        let p = Proj::new(LON0, LAT0);
        (LON0 + x / p.kx, LAT0 + y / p.ky)
    }

    /// Square parcel of `side` metres centred on the test site, as (lon, lat).
    fn square(side: f64) -> Vec<Ring> {
        let h = side / 2.0;
        vec![[(-h, -h), (h, -h), (h, h), (-h, h), (-h, -h)].iter().map(|&(x, y)| ll(x, y)).collect()]
    }

    fn shot(x: f64, y: f64, alt: f64, pitch: f64, yaw: f64) -> Shot {
        let (lon, lat) = ll(x, y);
        Shot {
            name: format!("DJI_{x:.0}_{y:.0}.JPG"),
            lat,
            lon,
            rel_alt_m: Some(alt),
            abs_alt_m: Some(1400.0 + alt),
            gimbal_pitch: Some(pitch),
            gimbal_yaw: Some(yaw),
            focal_35mm: Some(24.0),
            width: Some(6000),
            height: Some(4000),
        }
    }

    /// Nadir grid at 60 m (24 mm: 90 x 60 m footprints) over a square of `side` m.
    fn grid_flight(side: f64, skip: impl Fn(f64, f64) -> bool) -> Vec<Shot> {
        let mut v = Vec::new();
        let h = side / 2.0;
        let mut y = -h;
        while y <= h + 1.0 {
            let mut x = -h;
            while x <= h + 1.0 {
                if !skip(x, y) {
                    v.push(shot(x, y, 60.0, -90.0, 0.0));
                }
                x += 40.0;
            }
            y += 30.0;
        }
        v
    }

    fn jpeg_with_xmp(xmp: &str) -> Vec<u8> {
        let mut payload = XMP_HEADER.to_vec();
        payload.extend_from_slice(xmp.as_bytes());
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xE1];
        b.extend(((payload.len() + 2) as u16).to_be_bytes());
        b.extend(payload);
        b.extend([0xFF, 0xD9]);
        b
    }

    #[test]
    fn parses_dji_xmp() {
        let xmp = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
<rdf:Description rdf:about="DJI Meta Data" xmlns:drone-dji="http://www.dji.com/drone-dji/1.0/"
   drone-dji:GpsLatitude="+31.912000"
   drone-dji:GpsLongtitude="-110.012000"
   drone-dji:AbsoluteAltitude="+1452.300"
   drone-dji:RelativeAltitude = '+52.300'
   drone-dji:GimbalPitchDegree="-90.00"
   drone-dji:FlightYawDegree="+12.50">
  <drone-dji:GimbalYawDegreeX>1</drone-dji:GimbalYawDegreeX>
  <drone-dji:GimbalYawDegree>-45.10</drone-dji:GimbalYawDegree>
</rdf:Description></rdf:RDF></x:xmpmeta>
<?xpacket end="w"?>"#;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("DJI_20260101120000_0001_D.JPG");
        std::fs::write(&p, jpeg_with_xmp(xmp)).unwrap();

        let s = read_shot(&p).unwrap();
        assert_eq!(s.name, "DJI_20260101120000_0001_D.JPG");
        assert!((s.lat - 31.912).abs() < 1e-9 && (s.lon + 110.012).abs() < 1e-9);
        assert_eq!(s.rel_alt_m, Some(52.3));
        assert_eq!(s.abs_alt_m, Some(1452.3));
        assert_eq!(s.gimbal_pitch, Some(-90.0));
        assert_eq!(s.gimbal_yaw, Some(-45.1)); // element form, not the ...X decoy
        assert_eq!(s.focal_35mm, None);

        // Flight yaw stands in for a missing gimbal yaw.
        let p2 = dir.path().join("b.jpg");
        std::fs::write(&p2, jpeg_with_xmp(&xmp.replace("GimbalYawDegree>", "Other>"))).unwrap();
        assert_eq!(read_shot(&p2).unwrap().gimbal_yaw, Some(12.5));

        // No fix anywhere: no shot.
        let p3 = dir.path().join("c.JPG");
        std::fs::write(&p3, jpeg_with_xmp("<x:xmpmeta drone-dji:RelativeAltitude=\"+1\"/>")).unwrap();
        assert!(read_shot(&p3).is_none());
    }

    #[test]
    fn xmp_value_forms() {
        let p = r#"<a drone-dji:Pitch="+1.5" drone-dji:PitchX="9"/><drone-dji:Yaw rdf:x="1"> -3 </drone-dji:Yaw>"#;
        assert_eq!(xmp_number(p, "drone-dji:Pitch"), Some(1.5));
        assert_eq!(xmp_number(p, "drone-dji:Yaw"), Some(-3.0));
        assert_eq!(xmp_number(p, "drone-dji:Roll"), None);
    }

    #[test]
    fn reads_video_srt() {
        let dir = tempfile::tempdir().unwrap();
        let v = dir.path().join("DJI_0002.MP4");
        std::fs::write(&v, b"").unwrap();
        std::fs::write(
            dir.path().join("DJI_0002.SRT"),
            "1\n00:00:00,000 --> 00:00:00,033\n[iso: 100] [latitude: 0.000000] [longitude: 0.000000] [rel_alt: 0.000 abs_alt: 1390.0]\n\n\
             2\n00:00:00,033 --> 00:00:00,066\n<font size=\"28\">FrameCnt: 2</font>\n[iso: 100] [latitude: 31.912345] [longitude: -110.012345] [rel_alt: 52.300 abs_alt: 1442.5] [gb_yaw: 10.0 gb_pitch: -30.5 gb_roll: 0.0]\n",
        )
        .unwrap();
        let s = read_shot(&v).unwrap();
        assert_eq!((s.lat, s.lon), (31.912345, -110.012345));
        assert_eq!(s.rel_alt_m, Some(52.3));
        assert_eq!(s.abs_alt_m, Some(1442.5));
        assert_eq!(s.gimbal_pitch, Some(-30.5));
        assert_eq!(s.gimbal_yaw, Some(10.0));
        assert!(read_shot(&dir.path().join("DJI_0003.MP4")).is_none());
    }

    #[test]
    fn nadir_footprint() {
        let (w, h) = footprint_size(100.0, Some(24.0), Some(6000), Some(4000));
        assert!((w - 150.0).abs() < 0.01 && (h - 100.0).abs() < 0.01, "{w} x {h}");
        let (w, h) = footprint_size(100.0, Some(24.0), Some(4000), Some(6000));
        assert!((w - 100.0).abs() < 0.01 && (h - 150.0).abs() < 0.01, "portrait {w} x {h}");
        // 4:3 keeps the diagonal: 34.6 x 26.0 mm equivalent frame.
        let (w, h) = footprint_size(100.0, Some(24.0), Some(4000), Some(3000));
        assert!((w - 144.2).abs() < 0.1 && (h - 108.2).abs() < 0.1, "4:3 {w} x {h}");
        // Default lens: Mavic 4 Pro wide, 28 mm 3:2.
        let (w, h) = footprint_size(100.0, None, None, None);
        assert!((w - 128.57).abs() < 0.01 && (h - 85.71).abs() < 0.01);
    }

    #[test]
    fn full_grid_then_missing_ne_corner() {
        let parcel = square(200.0);
        let full = coverage(&grid_flight(200.0, |_, _| false), &parcel);
        assert_eq!(full.kind, Kind::Mapping);
        assert!(full.covered_pct.unwrap() > 99.9, "{full:?}");
        assert!(!full.has_gaps(), "{full:?}");
        assert!(full.summary().starts_with("Mapping: 100% of the parcel covered"), "{}", full.summary());

        let holed = coverage(&grid_flight(200.0, |x, y| x > 30.0 && y > 30.0), &parcel);
        assert_eq!(holed.kind, Kind::Mapping);
        let p = holed.covered_pct.unwrap(); // 35 x 50 m of 200 x 200 m missing
        assert!((95.0..96.5).contains(&p), "{holed:?}");
        assert_eq!(holed.gaps, vec!["NE corner".to_string()]);
        assert!(holed.summary().contains("Not covered: NE corner."), "{}", holed.summary());
    }

    #[test]
    fn tilted_footprint_moves_ahead() {
        // Drone 10.6 m south of a 12 m parcel, camera pitched to -80 looking
        // north: the 20 x 13 m footprint lands on the parcel.
        let parcel = square(12.0);
        let mut s = shot(0.0, -10.6, 60.0, -80.0, 0.0);
        s.focal_35mm = Some(108.0);
        let c = coverage(std::slice::from_ref(&s), &parcel);
        assert!(c.covered_pct.unwrap() > 99.0 && !c.has_gaps(), "{c:?}");
        s.gimbal_pitch = Some(-90.0); // straight down: misses the north half
        let c = coverage(&[s], &parcel);
        assert!(c.covered_pct.unwrap() < 50.0, "{c:?}");
        assert!(c.gaps.contains(&"north edge".to_string()), "{c:?}");
    }

    #[test]
    fn oblique_sides() {
        let parcel = square(60.0);
        // Orbit at 80 m out, camera aimed at the centre, nothing from the west.
        let shots: Vec<Shot> = [(0.0, 80.0, 180.0), (5.0, 80.0, 180.0), (80.0, 0.0, 270.0), (0.0, -80.0, 0.0)]
            .iter()
            .map(|&(x, y, yaw)| shot(x, y, 40.0, -25.0, yaw))
            .collect();
        let c = coverage(&shots, &parcel);
        assert_eq!(c.kind, Kind::Oblique);
        assert_eq!(c.sides, [2, 1, 1, 0]);
        assert_eq!(c.gaps, vec!["No shots from the west side.".to_string()]);
        assert_eq!(c.shots_used, 4);
        assert!(c.summary().contains("N 2, E 1, S 1, W 0"), "{}", c.summary());

        // Same positions, camera turned away from the parcel.
        let away: Vec<Shot> = shots.iter().cloned().map(|mut s| {
            s.gimbal_yaw = s.gimbal_yaw.map(|y| y + 180.0);
            s
        }).collect();
        let c = coverage(&away, &parcel);
        assert!(c.gaps.iter().any(|g| g.starts_with("Most shots (4 of 4) point away")), "{c:?}");

        // Far away (2 km): nothing counts.
        let c = coverage(&[shot(2000.0, 0.0, 40.0, -20.0, 270.0)], &parcel);
        assert_eq!(c.sides, [0; 4]);
        assert!(c.has_gaps());
    }

    #[test]
    fn mixed_and_unknown() {
        let parcel = square(200.0);
        let mut shots = grid_flight(200.0, |_, _| false);
        let n_grid = shots.len();
        for (x, y, yaw) in [(0.0, 150.0, 180.0), (150.0, 0.0, 270.0), (0.0, -150.0, 0.0), (-150.0, 0.0, 90.0)] {
            for _ in 0..n_grid / 3 {
                shots.push(shot(x, y, 50.0, -20.0, yaw));
            }
        }
        let c = coverage(&shots, &parcel);
        assert_eq!(c.kind, Kind::Mixed);
        assert!(!c.has_gaps(), "{c:?}");
        assert_eq!(c.summary().lines().count(), 2);

        let mut blind = shot(0.0, 0.0, 50.0, -90.0, 0.0);
        blind.gimbal_pitch = None;
        let c = coverage(&[blind.clone()], &parcel);
        assert_eq!(c.kind, Kind::Unknown);
        assert!(c.summary().contains("could not be checked"));
        assert!(!c.has_gaps());

        // Nadir grid with no altitude at all is Unknown too.
        let no_alt: Vec<Shot> = grid_flight(200.0, |_, _| false).into_iter().map(|mut s| {
            s.rel_alt_m = None;
            s
        }).collect();
        assert_eq!(coverage(&no_alt, &parcel).kind, Kind::Unknown);
        assert_eq!(coverage(&shots, &[]).kind, Kind::Unknown);
    }

    #[test]
    fn big_parcel_is_fast() {
        // ~160 acres (805 m square), 600 nadir photos at 100 m.
        let parcel = square(805.0);
        let mut shots = Vec::new();
        for j in 0..20 {
            for i in 0..30 {
                shots.push(shot(-400.0 + i as f64 * 28.0, -400.0 + j as f64 * 42.0, 100.0, -90.0, 90.0));
            }
        }
        let t = std::time::Instant::now();
        let c = coverage(&shots, &parcel);
        let dt = t.elapsed();
        assert_eq!(c.kind, Kind::Mapping);
        assert!(c.covered_pct.unwrap() > 99.0, "{c:?}");
        // Generous for unoptimised test builds; release runs in a few ms.
        assert!(dt.as_secs_f64() < 5.0, "{dt:?}");
    }
}
