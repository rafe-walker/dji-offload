//! Client-facing names and captions for photos, worked out from where each
//! photo was taken relative to the parcel, which way the camera looked, how
//! steeply, how high, and where the sun was.
//!
//! `DJI_20260428183754_0217_D.JPG` becomes, for example,
//! `100-Example-Rd_014_aerial-from-sw-looking-ne_golden-hour.jpg`, with the
//! caption "Aerial from the southwest, looking northeast" and the meta line
//! "Looking NE · 62 m above take-off · 35° down · 6:31 PM · golden hour".
//! No aircraft or camera brand appears anywhere.

use std::f64::consts::PI;
use std::path::PathBuf;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};

use crate::coverage::{Shot, footprint_size};
use crate::geo::Ring;

const R: f64 = 6_371_008.8;

/// Local metres around an origin (equirectangular; fine at parcel scale).
#[derive(Clone, Copy)]
pub struct Proj {
    lat0: f64,
    lon0: f64,
    k: f64,
}

impl Proj {
    pub fn new(lat0: f64, lon0: f64) -> Self {
        Proj { lat0, lon0, k: (lat0 * PI / 180.0).cos() }
    }
    pub fn xy(&self, lon: f64, lat: f64) -> (f64, f64) {
        ((lon - self.lon0) * PI / 180.0 * R * self.k, (lat - self.lat0) * PI / 180.0 * R)
    }
    pub fn lonlat(&self, x: f64, y: f64) -> (f64, f64) {
        (self.lon0 + x / (R * self.k) * 180.0 / PI, self.lat0 + y / R * 180.0 / PI)
    }
}

/// Sun elevation and azimuth (degrees, azimuth clockwise from north).
/// Low-precision solar position (about 0.1°), plenty for "golden hour".
pub fn sun(lat: f64, lon: f64, t: DateTime<Utc>) -> (f64, f64) {
    let rad = PI / 180.0;
    let jd = t.timestamp() as f64 / 86_400.0 + 2_440_587.5;
    let n = jd - 2_451_545.0;
    let l = (280.460 + 0.985_647_4 * n).rem_euclid(360.0);
    let g = ((357.528 + 0.985_600_3 * n).rem_euclid(360.0)) * rad;
    let lambda = (l + 1.915 * g.sin() + 0.020 * (2.0 * g).sin()) * rad;
    let eps = (23.439 - 0.000_000_4 * n) * rad;
    let ra = (eps.cos() * lambda.sin()).atan2(lambda.cos());
    let dec = (eps.sin() * lambda.sin()).asin();
    let gmst_h = (18.697_374_558 + 24.065_709_824_419_08 * n).rem_euclid(24.0);
    let ha = (gmst_h * 15.0 + lon) * rad - ra;
    let la = lat * rad;
    let elev = (la.sin() * dec.sin() + la.cos() * dec.cos() * ha.cos()).asin();
    let az = (-ha.sin()).atan2(dec.tan() * la.cos() - la.sin() * ha.cos());
    (elev / rad, (az / rad).rem_euclid(360.0))
}

/// "golden hour", "twilight" or None for ordinary daylight / night.
pub fn light(elev: f64) -> Option<&'static str> {
    match elev {
        e if (-0.833..=6.0).contains(&e) => Some("golden hour"),
        e if (-6.0..-0.833).contains(&e) => Some("twilight"),
        _ => None,
    }
}

const DIR8: [&str; 8] = ["north", "northeast", "east", "southeast", "south", "southwest", "west", "northwest"];
const DIR8_SHORT: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];

fn dir8(bearing: f64) -> usize {
    (((bearing.rem_euclid(360.0) + 22.5) / 45.0) as usize) % 8
}

/// Bearing (clockwise from north) of vector (dx east, dy north).
fn bearing(dx: f64, dy: f64) -> f64 {
    dx.atan2(dy).to_degrees().rem_euclid(360.0)
}

fn contains(rings: &[Vec<(f64, f64)>], x: f64, y: f64) -> bool {
    let mut inside = false;
    for r in rings {
        let n = r.len();
        if n < 3 {
            continue;
        }
        let mut j = n - 1;
        for i in 0..n {
            let ((xi, yi), (xj, yj)) = (r[i], r[j]);
            if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                inside = !inside;
            }
            j = i;
        }
    }
    inside
}

fn seg_dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0) };
    ((p.0 - a.0 - t * dx).powi(2) + (p.1 - a.1 - t * dy).powi(2)).sqrt()
}

/// The parcel in local metres, for describing positions.
pub struct Site {
    pub proj: Proj,
    rings: Vec<Vec<(f64, f64)>>,
    bbox: (f64, f64, f64, f64),
    centre: (f64, f64),
}

impl Site {
    pub fn new(parcel: &[Ring]) -> Option<Site> {
        let pts: Vec<&(f64, f64)> = parcel.iter().flatten().collect();
        if pts.len() < 3 {
            return None;
        }
        let n = pts.len() as f64;
        let (lon0, lat0) = pts.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + x / n, b + y / n));
        let proj = Proj::new(lat0, lon0);
        let rings: Vec<Vec<(f64, f64)>> = parcel.iter().map(|r| r.iter().map(|&(x, y)| proj.xy(x, y)).collect()).collect();
        let all: Vec<&(f64, f64)> = rings.iter().flatten().collect();
        let bbox = all.iter().fold((f64::MAX, f64::MIN, f64::MAX, f64::MIN), |b, &&(x, y)| {
            (b.0.min(x), b.1.max(x), b.2.min(y), b.3.max(y))
        });
        Some(Site { proj, rings, bbox, centre: ((bbox.0 + bbox.1) / 2.0, (bbox.2 + bbox.3) / 2.0) })
    }

    pub fn inside(&self, x: f64, y: f64) -> bool {
        contains(&self.rings, x, y)
    }

    /// "northwest corner", "north edge", "centre", ... of the parcel.
    pub fn sector(&self, x: f64, y: f64) -> &'static str {
        let (x0, x1, y0, y1) = self.bbox;
        let col = (((x - x0) / (x1 - x0).max(1e-9)) * 3.0).clamp(0.0, 2.999) as usize;
        let row = (((y1 - y) / (y1 - y0).max(1e-9)) * 3.0).clamp(0.0, 2.999) as usize;
        [
            ["northwest corner", "north edge", "northeast corner"],
            ["west edge", "centre", "east edge"],
            ["southwest corner", "south edge", "southeast corner"],
        ][row][col]
    }

    /// Distance (m) from a point outside the parcel to its boundary.
    pub fn distance(&self, x: f64, y: f64) -> f64 {
        let mut best = f64::MAX;
        for r in &self.rings {
            for i in 0..r.len() {
                best = best.min(seg_dist((x, y), r[i], r[(i + 1) % r.len()]));
            }
        }
        best
    }

    pub fn centre(&self) -> (f64, f64) {
        self.centre
    }
}

/// Everything a client sees about one photo.
#[derive(Debug, Clone)]
pub struct Described {
    pub path: PathBuf,
    pub shot: Shot,
    pub number: usize,
    pub file_name: String,
    pub title: String,
    /// A compact caption for thumbnails: "Top-down · NW corner".
    pub short: String,
    pub meta: String,
    pub position: String,
    pub nadir: bool,
    /// The camera points at (or over) the parcel.
    pub facing_parcel: bool,
    pub light: Option<&'static str>,
    pub sun_elev: Option<f64>,
}

/// "2931 N GOLDEN RULE RD" -> "2931-N-Golden-Rule-Rd"; falls back to the APN.
pub fn slug(address: Option<&str>, apn: Option<&str>) -> String {
    let base = match (address, apn) {
        (Some(a), _) if !a.trim().is_empty() => crate::geo::title_case(a),
        (_, Some(p)) => format!("APN-{p}"),
        _ => "Aerial".into(),
    };
    let s: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    s.chars().take(48).collect()
}

fn clock(t: NaiveDateTime) -> String {
    t.format("%-I:%M %p").to_string()
}

/// Describe one photo. `taken` is the drone's local clock time.
pub fn describe(path: PathBuf, shot: Shot, taken: NaiveDateTime, site: Option<&Site>, number: usize, prefix: &str) -> Described {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "jpg".into());
    let pitch = shot.gimbal_pitch;
    let nadir = pitch.is_some_and(|p| p <= -75.0);
    let utc = Local.from_local_datetime(&taken).single().map(|t| t.with_timezone(&Utc));
    let (sun_elev, light) = match utc {
        Some(u) => {
            let (e, _) = sun(shot.lat, shot.lon, u);
            (Some(e), light(e))
        }
        None => (None, None),
    };

    let mut title;
    let mut slug_parts: Vec<String>;
    let position;
    let mut facing = false;
    let look = shot.gimbal_yaw.map(dir8);
    match site {
        Some(site) => {
            let (x, y) = site.proj.xy(shot.lon, shot.lat);
            let inside = site.inside(x, y);
            let (cx, cy) = site.centre();
            let from = dir8(bearing(x - cx, y - cy));
            if nadir {
                if inside {
                    let s = site.sector(x, y);
                    title = format!("Top-down over the {s}");
                    slug_parts = vec![format!("top-down-{}", s.replace(' ', "-"))];
                    position = format!("Taken over the {s} of the parcel");
                    facing = true;
                } else {
                    let d = site.distance(x, y);
                    title = format!("Top-down, {} of the parcel", DIR8[from]);
                    slug_parts = vec![format!("top-down-{}-of-parcel", DIR8_SHORT[from].to_lowercase())];
                    position = format!("Taken {:.0} m {} of the parcel", d, DIR8[from]);
                    facing = d < 30.0;
                }
            } else {
                // Aimed at the property: camera heading within 35° of the
                // direction to the parcel centre, from within 1 km.
                if let Some(yaw) = shot.gimbal_yaw {
                    let to_parcel = bearing(cx - x, cy - y);
                    let off = ((yaw - to_parcel + 540.0).rem_euclid(360.0) - 180.0).abs();
                    facing = inside || (off <= 35.0 && site.distance(x, y) <= 1000.0);
                }
                if inside {
                    let s = site.sector(x, y);
                    title = format!("Aerial over the {s}");
                    slug_parts = vec![format!("aerial-over-{}", s.replace(' ', "-"))];
                    position = format!("Taken above the {s} of the parcel");
                } else {
                    let d = site.distance(x, y);
                    title = format!("Aerial from the {}", DIR8[from]);
                    slug_parts = vec![format!("aerial-from-{}", DIR8_SHORT[from].to_lowercase())];
                    position = format!("Taken {:.0} m {} of the parcel", d, DIR8[from]);
                }
                if let Some(l) = look {
                    title += &format!(", looking {}", DIR8[l]);
                    slug_parts.push(format!("looking-{}", DIR8_SHORT[l].to_lowercase()));
                }
            }
        }
        None => {
            title = if nadir { "Top-down aerial".into() } else { "Aerial view".into() };
            slug_parts = vec![if nadir { "top-down".into() } else { "aerial".into() }];
            if let (false, Some(l)) = (nadir, look) {
                title += &format!(", looking {}", DIR8[l]);
                slug_parts.push(format!("looking-{}", DIR8_SHORT[l].to_lowercase()));
            }
            position = String::new();
        }
    }
    // Light is worth naming on angled views (it sells them); on mapping
    // frames it is noise. The title leaves it to the details line.
    if let (false, Some(l)) = (nadir, light) {
        slug_parts.push(l.replace(' ', "-"));
    }
    let short = shorten(&title);

    let mut meta = Vec::new();
    if let (false, Some(l)) = (nadir, look) {
        meta.push(format!("Looking {}", DIR8_SHORT[l]));
    }
    if let Some(a) = shot.rel_alt_m {
        meta.push(format!("{:.0} m ({:.0} ft) above take-off", a, a * 3.28084));
    }
    match pitch {
        Some(_) if nadir => meta.push("straight down".into()),
        Some(p) if p > -2.0 => meta.push("level".into()),
        Some(p) => meta.push(format!("{:.0}° down", -p)),
        None => {}
    }
    meta.push(clock(taken));
    if let Some(l) = light {
        meta.push(l.to_string());
    }

    Described {
        file_name: format!("{prefix}_{number:03}_{}.{ext}", slug_parts.join("_")),
        path,
        shot,
        number,
        title,
        short,
        meta: meta.join(" · "),
        position,
        nadir,
        facing_parcel: facing,
        light,
        sun_elev,
    }
}

/// "Aerial over the northwest corner, looking north" ->
/// "Aerial · NW corner · looking N".
fn shorten(title: &str) -> String {
    let t = title
        .replace("Top-down over the ", "Top-down · ")
        .replace("Aerial over the ", "Aerial · ")
        .replace("Aerial from the ", "Aerial from ")
        .replace("Top-down, ", "Top-down · ")
        .replace(" of the parcel", " of parcel")
        .replace(", looking ", " · looking ");
    // Longest names first so "northwest" is not read as "north" + "west".
    let mut out = t;
    for (long, short) in [
        ("northwest", "NW"),
        ("northeast", "NE"),
        ("southwest", "SW"),
        ("southeast", "SE"),
        ("north", "N"),
        ("south", "S"),
        ("east", "E"),
        ("west", "W"),
    ] {
        out = out.replace(long, short);
    }
    out
}

/// Ground footprint of a straight-down photo, as a (lon, lat) ring.
pub fn footprint(shot: &Shot) -> Option<Vec<(f64, f64)>> {
    let alt = shot.rel_alt_m.filter(|a| *a > 1.0)?;
    if !shot.gimbal_pitch.is_some_and(|p| p <= -75.0) {
        return None;
    }
    let (across, along) = footprint_size(alt, shot.focal_35mm, shot.width, shot.height);
    let yaw = shot.gimbal_yaw.unwrap_or(0.0).to_radians();
    let p = Proj::new(shot.lat, shot.lon);
    let (fx, fy) = (yaw.sin(), yaw.cos()); // forward
    let (rx, ry) = (yaw.cos(), -yaw.sin()); // right
    let corners = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];
    Some(
        corners
            .iter()
            .map(|(a, b)| {
                let x = rx * a * across / 2.0 + fx * b * along / 2.0;
                let y = ry * a * across / 2.0 + fy * b * along / 2.0;
                p.lonlat(x, y)
            })
            .collect(),
    )
}

/// Ground sample distance (cm per pixel) of a straight-down photo.
pub fn gsd_cm(shot: &Shot) -> Option<f64> {
    let alt = shot.rel_alt_m.filter(|a| *a > 1.0)?;
    let w = shot.width.max(shot.height)? as f64;
    let (across, along) = footprint_size(alt, shot.focal_35mm, shot.width, shot.height);
    Some(across.max(along) / w * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shot(lat: f64, lon: f64, alt: f64, pitch: f64, yaw: f64) -> Shot {
        Shot {
            name: "x".into(),
            lat,
            lon,
            rel_alt_m: Some(alt),
            abs_alt_m: None,
            gimbal_pitch: Some(pitch),
            gimbal_yaw: Some(yaw),
            focal_35mm: Some(24.0),
            width: Some(6000),
            height: Some(4000),
        }
    }

    fn parcel() -> Vec<Ring> {
        // ~190 m square around 31.91, -110.01 (fictional)
        vec![vec![(-110.011, 31.909), (-110.009, 31.909), (-110.009, 31.911), (-110.011, 31.911), (-110.011, 31.909)]]
    }

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn sun_position_is_sane() {
        // Equinox noon UTC at 0,0: sun near overhead.
        let (e, _) = sun(0.0, 0.0, Utc.with_ymd_and_hms(2026, 3, 20, 12, 0, 0).unwrap());
        assert!(e > 85.0, "{e}");
        // Southern Arizona, 2026-04-28 01:30 UTC (18:30 MST): low in the west-northwest.
        let (e, az) = sun(31.91, -110.01, Utc.with_ymd_and_hms(2026, 4, 29, 1, 30, 0).unwrap());
        assert!((0.0..10.0).contains(&e) && (280.0..300.0).contains(&az), "{e} {az}");
        assert_eq!(light(3.0), Some("golden hour"));
        assert_eq!(light(-3.0), Some("twilight"));
        assert_eq!(light(40.0), None);
    }

    #[test]
    fn names_nadir_and_oblique_shots() {
        let site = Site::new(&parcel()).unwrap();
        // Straight down over the NW part of the parcel.
        let d = describe("a.JPG".into(), shot(31.9108, -110.0108, 60.0, -90.0, 0.0), at("2026-04-28 12:00:00"), Some(&site), 7, "100-Example-Rd");
        assert_eq!(d.title, "Top-down over the northwest corner");
        assert!(d.file_name.starts_with("100-Example-Rd_007_top-down-northwest-corner."), "{}", d.file_name);
        assert!(d.facing_parcel);
        // From ~150 m south-west, looking north-east at the parcel.
        let d = describe("b.JPG".into(), shot(31.9077, -110.0125, 60.0, -25.0, 45.0), at("2026-04-28 12:00:00"), Some(&site), 8, "P");
        assert_eq!(d.title, "Aerial from the southwest, looking northeast");
        assert!(d.file_name.contains("aerial-from-sw_looking-ne"), "{}", d.file_name);
        assert!(d.facing_parcel);
        assert_eq!(d.short, "Aerial from SW · looking NE");
        assert!(d.position.contains("southwest of the parcel"), "{}", d.position);
        assert!(!d.file_name.to_uppercase().contains("DJI"));
        // Looking away from the parcel.
        let d = describe("c.JPG".into(), shot(31.9077, -110.0125, 60.0, -25.0, 225.0), at("2026-04-28 12:00:00"), Some(&site), 9, "P");
        assert!(!d.facing_parcel);
    }

    #[test]
    fn slugs_and_footprints() {
        assert_eq!(slug(Some("100  EXAMPLE RD"), Some("999-12-345A")), "100-Example-Rd");
        assert_eq!(slug(None, Some("999-12-345A")), "APN-999-12-345A");
        let fp = footprint(&shot(31.91, -110.01, 100.0, -90.0, 0.0)).unwrap();
        let p = Proj::new(31.91, -110.01);
        let (x0, y0) = p.xy(fp[0].0, fp[0].1);
        let (x2, y2) = p.xy(fp[2].0, fp[2].1);
        assert!(((x2 - x0).abs() - 150.0).abs() < 8.0 && ((y2 - y0).abs() - 100.0).abs() < 8.0, "{x0} {x2} {y0} {y2}");
        let g = gsd_cm(&shot(31.91, -110.01, 100.0, -90.0, 0.0)).unwrap();
        assert!((2.0..3.0).contains(&g), "{g}");
    }
}
