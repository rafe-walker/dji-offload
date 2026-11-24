//! What gets made from a flight once it is safely on the Mac:
//!
//! - **Field checks** (every flight): blur/exposure flags, coverage of the
//!   main parcel, and `map.kml` with photo positions and parcel outlines.
//! - **Client**: `Delivery - <address or APN> - <date>/` with web-sized
//!   JPEGs renamed by content, the branded Aerial Report PDF, a photo index
//!   and a client map.
//! - **Library**: `Library Sample/` with watermarked previews to send to a
//!   prospective buyer.
//!
//! Originals are never modified. Everything here can be re-run safely.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::Config;
use crate::coverage::{self, Coverage, Shot};
use crate::flights::Flight;
use crate::geo::{Lookup, Place, title_case};
use crate::imaging::{self, Flagged};
use crate::naming::{self, Described};
use crate::report_pdf;
use crate::jobs::Job;

pub struct FieldReport {
    pub coverage: Option<Coverage>,
    pub flagged: Vec<Flagged>,
    pub photos: Vec<PathBuf>,
}

impl FieldReport {
    /// One line for a notification, e.g. "coverage OK · 2 soft photos (0042, 0043)".
    pub fn headline(&self) -> String {
        let mut parts = Vec::new();
        if let Some(c) = &self.coverage {
            parts.push(if c.has_gaps() { c.summary() } else { "coverage OK".into() });
        }
        if self.flagged.is_empty() {
            if !self.photos.is_empty() {
                parts.push("all photos look sharp and well exposed".into());
            }
        } else {
            let names: Vec<String> = self.flagged.iter().take(4).map(|f| short_name(&f.path)).collect();
            let more = if self.flagged.len() > 4 { ", …" } else { "" };
            parts.push(format!("{} to check ({}{more})", plural(self.flagged.len(), "photo", "photos"), names.join(", ")));
        }
        parts.join(" · ")
    }

    pub fn needs_attention(&self) -> bool {
        !self.flagged.is_empty() || self.coverage.as_ref().is_some_and(Coverage::has_gaps)
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Run `f` over `items` on all cores; returns the first error.
fn par_each<T: Sync>(items: &[T], f: impl Fn(&T) -> Result<()> + Sync) -> Result<()> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let first_err = std::sync::Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(item) = items.get(i) else { break };
                    if let Err(e) = f(item) {
                        first_err.lock().unwrap().get_or_insert(e);
                        break;
                    }
                }
            });
        }
    });
    match first_err.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `DJI_20260428182706_0042_D.JPG` -> `0042`.
fn short_name(p: &Path) -> String {
    let n = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
    n.split('_').nth(2).map(str::to_string).unwrap_or(n)
}

fn media_in(dir: &Path, exts: &[&str]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let n = p.file_name().unwrap_or_default().to_string_lossy().to_uppercase();
                    !n.starts_with('.') && exts.iter().any(|x| n.ends_with(x))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Blur/exposure, coverage and the KML map for one flight folder.
pub fn field_checks(dir: &Path, title: &str, place: &Place, lookup: Option<&Lookup>) -> Result<FieldReport> {
    let photos = media_in(dir, &[".JPG", ".JPEG"]);
    let mut shots: Vec<Shot> = photos.iter().filter_map(|p| coverage::read_shot(p)).collect();
    shots.extend(media_in(dir, &[".MP4", ".MOV"]).iter().filter_map(|p| coverage::read_shot(p)));

    let mut parcels = Vec::new();
    if let Some(l) = lookup {
        for (p, _) in place.parcels.iter().take(8) {
            if let Some(rings) = l.geometry(&p.apn)? {
                let label = p.situs.as_deref().map(title_case).unwrap_or_else(|| "no address on file".into());
                parcels.push((p.apn.clone(), label, rings));
            }
        }
    }
    let cov = parcels.first().filter(|_| !shots.is_empty()).map(|(_, _, rings)| coverage::coverage(&shots, rings));
    let flagged = imaging::assess(&photos);
    if !shots.is_empty() {
        crate::kml::write_kml(&dir.join("map.kml"), title, &shots, &parcels, cov.as_ref())?;
    }

    let mut txt = format!("Field checks for {title}\n\n");
    match &cov {
        Some(c) => txt += &format!("Coverage: {}\n", c.summary()),
        None => txt += "Coverage: not checked (needs GPS, altitude and the parcel database)\n",
    }
    if flagged.is_empty() {
        txt += "Photos: none flagged\n";
    } else {
        txt += &format!("Photos to check ({}):\n", flagged.len());
        for f in &flagged {
            txt += &format!("  {}  {}\n", f.path.file_name().unwrap_or_default().to_string_lossy(), f.reasons.join(", "));
        }
    }
    fs::write(dir.join("field-checks.txt"), txt)?;
    Ok(FieldReport { coverage: cov, flagged, photos })
}

/// Library samples must be at least this high above take-off.
const SAMPLE_MIN_ALT_M: f64 = 20.0;

/// Most thumbnails in the report's photo index (Photo index.csv lists all).
const INDEX_MAX: usize = 300;


fn safe(s: &str) -> String {
    s.chars().map(|c| if matches!(c, '/' | ':' | '\\') { '-' } else { c }).collect()
}

/// Everything `build` needs about a flight.
pub struct BuildCtx<'a> {
    pub dir: &'a Path,
    pub flight: &'a Flight,
    pub place: &'a Place,
    pub job: &'a Job,
    pub cfg: &'a Config,
    pub lookup: Option<&'a Lookup>,
    pub checks: Option<&'a FieldReport>,
    /// BLAKE3 over the verified originals, for the report's integrity page.
    pub fingerprint: Option<String>,
    /// When the parcel data was downloaded (YYYY-MM-DD).
    pub parcel_date: Option<String>,
    /// Videos captured on this flight (not delivered as files yet).
    pub videos: usize,
}

fn taken(p: &Path) -> chrono::NaiveDateTime {
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    crate::media::time_from_name(&name).unwrap_or_else(|| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|t| chrono::DateTime::<chrono::Local>::from(t).naive_local())
            .unwrap_or_default()
    })
}

/// Describe the photos that will be delivered, numbered in time order.
fn describe_all(good: &[PathBuf], site: Option<&naming::Site>, prefix: &str) -> Vec<Described> {
    let mut v: Vec<(chrono::NaiveDateTime, PathBuf)> = good.iter().map(|p| (taken(p), p.clone())).collect();
    v.sort();
    v.into_iter()
        .enumerate()
        .map(|(i, (t, p))| {
            let shot = coverage::read_shot(&p).unwrap_or(Shot {
                name: String::new(),
                lat: 0.0,
                lon: 0.0,
                rel_alt_m: None,
                abs_alt_m: None,
                gimbal_pitch: None,
                gimbal_yaw: None,
                focal_35mm: None,
                width: None,
                height: None,
            });
            let has_fix = shot.lat != 0.0 || shot.lon != 0.0;
            naming::describe(p, shot, t, site.filter(|_| has_fix), i + 1, prefix)
        })
        .collect()
}

/// The cover photo: the sharpest angled shot aimed at the property,
/// preferring golden hour; falls back to any delivered photo.
fn pick_hero(described: &[Described]) -> Option<PathBuf> {
    let candidates: Vec<&Described> = described
        .iter()
        .filter(|d| !d.nadir && d.facing_parcel && d.shot.rel_alt_m.is_none_or(|a| a >= 20.0))
        .collect();
    let pool: Vec<&Described> = if candidates.is_empty() { described.iter().collect() } else { candidates };
    if pool.is_empty() {
        return None;
    }
    let step = (pool.len() / 12).max(1);
    pool.iter()
        .step_by(step)
        .filter_map(|d| {
            let q = imaging::quality(&d.path).ok()?;
            let bonus = if d.light == Some("golden hour") { 1.3 } else { 1.0 };
            Some((q.sharpness * bonus, d.path.clone()))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, p)| p)
        .or_else(|| pool.first().map(|d| d.path.clone()))
}

/// Up to `n` photos for the report: distinct angled views first (from at
/// least `SAMPLE_MIN_ALT_M` up, near-repeats dropped), then straight-down
/// frames spread across the flight.
fn feature(described: &[Described], n: usize) -> Vec<usize> {
    let spread = |idx: Vec<usize>, k: usize| -> Vec<usize> {
        if idx.len() <= k || k == 0 {
            return idx.into_iter().take(k).collect();
        }
        (0..k).map(|i| idx[i * (idx.len() - 1) / (k - 1).max(1)]).collect()
    };
    // Highest first, so of two near-repeats the loftier view wins.
    let mut oblique: Vec<usize> = (0..described.len())
        .filter(|&i| !described[i].nadir && described[i].shot.rel_alt_m.is_none_or(|a| a >= SAMPLE_MIN_ALT_M))
        .collect();
    oblique.sort_by(|&a, &b| alt(&described[b]).total_cmp(&alt(&described[a])));
    let mut kept: Vec<usize> = Vec::new();
    for i in oblique {
        if !kept.iter().any(|&j| near_repeat(&described[i], &described[j])) {
            kept.push(i);
        }
    }
    kept.sort();
    let nadir: Vec<usize> = (0..described.len())
        .filter(|&i| described[i].nadir && described[i].shot.rel_alt_m.is_none_or(|a| a >= SAMPLE_MIN_ALT_M))
        .collect();
    let mut v = spread(kept, n);
    let room = n.saturating_sub(v.len());
    v.extend(spread(nadir, room));
    v.sort();
    v.dedup();
    v
}

fn alt(d: &Described) -> f64 {
    d.shot.rel_alt_m.unwrap_or(0.0)
}

/// Two angled views from about the same spot, height and heading.
fn near_repeat(a: &Described, b: &Described) -> bool {
    let (dy, dx) = ((a.shot.lat - b.shot.lat) * 111_320.0, (a.shot.lon - b.shot.lon) * 111_320.0 * a.shot.lat.to_radians().cos());
    let close = dx.hypot(dy) < 15.0 && (alt(a) - alt(b)).abs() < 25.0;
    let heading = match (a.shot.gimbal_yaw, b.shot.gimbal_yaw) {
        (Some(x), Some(y)) => ((x - y + 540.0).rem_euclid(360.0) - 180.0).abs() < 30.0,
        _ => true,
    };
    close && heading
}

fn compass(bearing: f64) -> &'static str {
    ["north", "northeast", "east", "southeast", "south", "southwest", "west", "northwest"]
        [(((bearing.rem_euclid(360.0) + 22.5) / 45.0) as usize) % 8]
}

fn brand(cfg: &Config) -> report_pdf::Brand {
    let b = &cfg.brand;
    let opt = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    report_pdf::Brand {
        name: b.name.clone(),
        tagline: opt(&b.tagline),
        url: opt(&b.url),
        phone: opt(&b.phone),
        email: opt(&b.email),
        logo: opt(&b.logo).map(PathBuf::from).filter(|p| p.is_file()),
        dark: crate::config::hex_rgb(&b.dark).unwrap_or([27, 31, 36]),
        accent: crate::config::hex_rgb(&b.accent).unwrap_or([176, 141, 87]),
    }
}

/// Files earlier versions put in a Delivery folder that named the camera.
const LEGACY: [&str; 3] = ["Contact Sheet.pdf", "shoot-info.txt", "map.kml"];

/// Make the client and library outputs for a flight. Returns short notes.
pub fn build(ctx: &BuildCtx) -> Result<Vec<String>> {
    let (dir, flight, place, job, cfg) = (ctx.dir, ctx.flight, ctx.place, ctx.job, ctx.cfg);
    let mut notes = Vec::new();
    let flagged: &[Flagged] = ctx.checks.map(|c| c.flagged.as_slice()).unwrap_or(&[]);
    let photos = media_in(dir, &[".JPG", ".JPEG"]);
    let bad: std::collections::BTreeSet<&PathBuf> = flagged.iter().map(|f| &f.path).collect();
    let good: Vec<PathBuf> = photos.iter().filter(|p| !bad.contains(p)).cloned().collect();
    let main = place.main();
    let address = main.and_then(|p| p.situs.as_deref()).map(title_case);
    let name = address.clone().or_else(|| main.map(|p| format!("APN {}", p.apn))).unwrap_or_else(|| "Aerial photography".into());

    let mut parcels = Vec::new();
    if let Some(l) = ctx.lookup {
        for (p, _) in place.parcels.iter().take(8) {
            if let Some(rings) = l.geometry(&p.apn)? {
                parcels.push((p.clone(), rings));
            }
        }
    }
    let site = parcels.first().and_then(|(_, rings)| naming::Site::new(rings));
    let prefix = naming::slug(address.as_deref(), main.map(|p| p.apn.as_str()));
    let described = describe_all(&good, site.as_ref(), &prefix);
    let file_name = |d: &Described| -> String {
        if cfg.rename_delivery { d.file_name.clone() } else { d.path.file_name().unwrap_or_default().to_string_lossy().to_string() }
    };

    if job.client.is_some() {
        let out = dir.join(format!("Delivery - {} - {}", safe(&name), flight.date));
        for f in LEGACY {
            let _ = fs::remove_file(out.join(f));
        }
        let web = out.join("Photos (web)");
        fs::create_dir_all(&web)?;
        // Remove web copies from an earlier naming scheme.
        let want: std::collections::BTreeSet<String> = described.iter().map(&file_name).collect();
        for e in fs::read_dir(&web)?.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !want.contains(&n) {
                let _ = fs::remove_file(e.path());
            }
        }
        let jobs: Vec<(PathBuf, PathBuf)> = described.iter().map(|d| (d.path.clone(), web.join(file_name(d)))).collect();
        par_each(&jobs, |(src, dst)| {
            if dst.exists() { Ok(()) } else { imaging::web_copy(src, dst, cfg.delivery_long_edge, 90) }
        })?;

        // Index of what each delivered photo shows.
        let mut idx = String::from("number,file,title,position,details,latitude,longitude\n");
        let mut names = String::from("delivered,original\n");
        for d in &described {
            let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
            idx += &format!(
                "{},{},{},{},{},{:.6},{:.6}\n",
                d.number,
                q(&file_name(d)),
                q(&d.title),
                q(&d.position),
                q(&d.meta),
                d.shot.lat,
                d.shot.lon
            );
            names += &format!("{},{}\n", file_name(d), d.path.file_name().unwrap_or_default().to_string_lossy());
        }
        fs::write(out.join("Photo index.csv"), idx)?;
        fs::write(dir.join("delivery-names.csv"), names)?; // internal: stays out of Delivery

        // Client map with delivered names only.
        let shots: Vec<Shot> = described
            .iter()
            .filter(|d| d.shot.lat != 0.0 || d.shot.lon != 0.0)
            .map(|d| Shot { name: file_name(d), ..d.shot.clone() })
            .collect();
        let kml_parcels: Vec<(String, String, Vec<crate::geo::Ring>)> = parcels
            .iter()
            .map(|(p, r)| (p.apn.clone(), p.situs.as_deref().map(title_case).unwrap_or_default(), r.clone()))
            .collect();
        let cov = ctx.checks.and_then(|c| c.coverage.as_ref());
        if !shots.is_empty() {
            crate::kml::write_kml(&out.join(format!("{} - Map.kml", safe(&name))), &name, &shots, &kml_parcels, cov)?;
        }

        let report = build_report(ctx, &name, &described, &parcels, &file_name);
        imaging_report(&out.join(format!("{} - Aerial Report.pdf", safe(&name))), &report)?;
        let skipped = if bad.is_empty() { String::new() } else { format!(" ({} left out)", plural(bad.len(), "flagged photo", "flagged photos")) };
        notes.push(format!("client delivery ready: {} and the report{skipped}", plural(described.len(), "photo", "photos")));
    }

    if job.library {
        let out = dir.join("Library Sample");
        fs::create_dir_all(&out)?;
        // The same distinct views the report features: angled first.
        let picks: Vec<&Described> = feature(&described, cfg.library_sample_count).into_iter().map(|i| &described[i]).collect();
        let want: std::collections::BTreeSet<String> = picks.iter().map(|d| format!("SAMPLE_{}", d.file_name)).collect();
        // Drop samples from earlier runs (older names, or named after the
        // camera files) so only the current set is offered.
        for e in fs::read_dir(&out)?.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if (n.starts_with("SAMPLE_") || n.to_uppercase().contains("DJI")) && !want.contains(&n) {
                let _ = fs::remove_file(e.path());
            }
        }
        let jobs: Vec<(PathBuf, PathBuf)> =
            picks.iter().map(|d| (d.path.clone(), out.join(format!("SAMPLE_{}", d.file_name)))).collect();
        par_each(&jobs, |(src, dst)| {
            if dst.exists() { Ok(()) } else { imaging::watermark(src, dst, 1600, &cfg.watermark_text) }
        })?;
        notes.push(plural(picks.len(), "watermarked library sample", "watermarked library samples"));
    }
    Ok(notes)
}

fn imaging_report(dst: &Path, r: &report_pdf::Report) -> Result<()> {
    report_pdf::write_report(dst, r)
}

fn build_report(
    ctx: &BuildCtx,
    name: &str,
    described: &[Described],
    parcels: &[(crate::geo::Parcel, Vec<crate::geo::Ring>)],
    file_name: &dyn Fn(&Described) -> String,
) -> report_pdf::Report {
    let (flight, place, job, cfg) = (ctx.flight, ctx.place, ctx.job, ctx.cfg);
    let featured = feature(described, cfg.report_photos);
    let cov = ctx.checks.and_then(|c| c.coverage.as_ref());
    let mapping = cov.is_some_and(|c| c.kind == coverage::Kind::Mapping);
    let main = place.main();

    // Map.
    let fixes: Vec<&Described> = described.iter().filter(|d| d.shot.lat != 0.0 || d.shot.lon != 0.0).collect();
    let map = report_pdf::MapData {
        parcels: parcels
            .iter()
            .enumerate()
            .map(|(i, (p, rings))| report_pdf::MapParcel { apn: p.apn.clone(), rings: rings.clone(), main: i == 0 })
            .collect(),
        path: fixes.iter().map(|d| (d.shot.lon, d.shot.lat)).collect(),
        shots: fixes
            .iter()
            .map(|d| report_pdf::MapShot {
                lon: d.shot.lon,
                lat: d.shot.lat,
                number: featured.contains(&(d.number - 1)).then_some(d.number),
                nadir: d.nadir,
            })
            .collect(),
        footprints: fixes.iter().filter_map(|d| naming::footprint(&d.shot)).take(600).collect(),
    };

    // Light at mid-flight.
    let mid = fixes.get(fixes.len() / 2);
    let light_line = mid.and_then(|d| {
        let e = d.sun_elev?;
        let t = chrono::TimeZone::from_local_datetime(&chrono::Local, &taken(&d.path)).single()?;
        let (_, az) = naming::sun(d.shot.lat, d.shot.lon, t.with_timezone(&chrono::Utc));
        Some(match d.light {
            Some(l) => format!("{}: sun {:.0}° above the {} horizon", capitalise(l), e.max(0.0), compass(az)),
            None if e > 0.0 => format!("Daylight: sun {:.0}° high in the {}", e, compass(az)),
            None => "After dark".to_string(),
        })
    });
    let date_long = flight.date.format("%A, %B %-d, %Y").to_string();
    let window = format!(
        "{}–{} ({} min)",
        flight.start.format("%-I:%M %p"),
        flight.end.format("%-I:%M %p"),
        ((flight.end - flight.start).num_seconds() as f64 / 60.0).round().max(1.0)
    );
    let fact = |l: &str, v: String| report_pdf::Fact { label: l.into(), value: v };
    let mut facts = Vec::new();
    if let Some(p) = main {
        if let Some(a) = &p.situs {
            facts.push(fact("Address", title_case(a)));
        }
        facts.push(fact("Parcel (APN)", p.apn.clone()));
        if let Some(a) = p.acres {
            facts.push(fact("Acreage", format!("{a:.2} acres")));
        }
        facts.push(fact("Jurisdiction", place.city_label()));
    }
    facts.push(fact("Date flown", date_long.clone()));
    facts.push(fact("Capture window", window.clone()));
    facts.push(fact("Photos delivered", described.len().to_string()));
    if ctx.videos > 0 {
        facts.push(fact("Video clips captured", ctx.videos.to_string()));
    }
    if let Some(c) = cov.and_then(|c| c.covered_pct) {
        facts.push(fact("Parcel coverage", format!("{c:.0}% of the parcel photographed from above")));
    }
    if let Some(l) = &light_line {
        facts.push(fact("Light", l.clone()));
    }

    // Technical appendix.
    let alts: Vec<f64> = fixes.iter().filter_map(|d| d.shot.rel_alt_m).collect();
    let mut technical = vec![fact("Capture window", format!("{date_long}, {window}"))];
    if !alts.is_empty() {
        let (lo, hi) = alts.iter().fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
        technical.push(fact("Altitude", format!("{lo:.0}–{hi:.0} m ({:.0}–{:.0} ft) above take-off", lo * 3.28084, hi * 3.28084)));
    }
    let mut gsds: Vec<f64> = fixes.iter().filter(|d| d.nadir).filter_map(|d| naming::gsd_cm(&d.shot)).collect();
    if !gsds.is_empty() {
        gsds.sort_by(f64::total_cmp);
        technical.push(fact("Ground resolution", format!("≈{:.1} cm per pixel (straight-down frames)", gsds[gsds.len() / 2])));
    }
    if let Some(c) = cov {
        technical.push(fact("Coverage check", c.summary()));
    }
    let oblique = described.iter().filter(|d| !d.nadir).count();
    technical.push(fact(
        "Frames",
        format!("{} delivered ({} angled, {} straight-down)", described.len(), oblique, described.len() - oblique),
    ));
    if let Some(l) = light_line {
        technical.push(fact("Sun", l));
    }
    technical.push(fact("Coordinates", "WGS 84 (EPSG:4326), from the aircraft's GNSS".into()));
    if let Some(d) = &ctx.parcel_date {
        technical.push(fact("Parcel data", format!("Cochise County Assessor public records, retrieved {d}")));
    }

    let mut integrity = vec![
        "Every original file was verified byte-for-byte (BLAKE3 checksum) as it was copied off the aircraft, and archived unaltered.".to_string(),
        "Delivered photos are colour-managed sRGB copies with location and device metadata removed; positions are listed in Photo index.csv.".to_string(),
    ];
    if let Some(f) = &ctx.fingerprint {
        integrity.push(format!("Archive fingerprint: {f}"));
    }

    let hero = pick_hero(described);
    let photos = featured
        .iter()
        .map(|&i| {
            let d = &described[i];
            report_pdf::PhotoPage {
                path: d.path.clone(),
                number: d.number,
                title: d.title.clone(),
                meta: d.meta.clone(),
                position: d.position.clone(),
                file_name: file_name(d),
                wide: !d.nadir,
            }
        })
        .collect();
    let index = described
        .iter()
        .take(INDEX_MAX)
        .map(|d| report_pdf::IndexItem { path: d.path.clone(), label: format!("{:03}  {}", d.number, d.short) })
        .collect();
    let mut subtitle = Vec::new();
    if let Some(p) = main {
        subtitle.push(format!("APN {}", p.apn));
        if let Some(a) = p.acres {
            subtitle.push(format!("{a:.2} acres"));
        }
        subtitle.push(place.city_label());
    }
    let b = brand(cfg);
    report_pdf::Report {
        qr_url: b.url.clone(),
        brand: b,
        kicker: if mapping { "AERIAL MAPPING REPORT".into() } else { "AERIAL PHOTOGRAPHY REPORT".into() },
        title: name.to_string(),
        subtitle: subtitle.join("  ·  "),
        prepared_for: job.client.clone(),
        date_line: format!("Flown {date_long}"),
        hero,
        facts,
        map,
        photos,
        index,
        index_total: described.len(),
        technical,
        integrity,
        disclaimer: "Parcel boundaries, addresses and acreage come from public county records and are shown for reference only; this report is not a survey. Positions are from satellite navigation and are typically accurate to a few metres.".into(),
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
