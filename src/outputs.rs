//! What gets made from a flight once it is safely on the Mac:
//!
//! - **Field checks** (every flight): blur/exposure flags, coverage of the
//!   main parcel, and `map.kml` with photo positions and parcel outlines.
//! - **Client**: `Delivery - <address or APN> - <date>/` with web-sized
//!   JPEGs, a contact sheet PDF, the map and the shoot summary.
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
fn par_each(items: &[PathBuf], f: impl Fn(&PathBuf) -> Result<()> + Sync) -> Result<()> {
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

/// Most photos shown on a client contact sheet (spread across the flight).
const CONTACT_SHEET_MAX: usize = 60;

fn evenly(items: &[PathBuf], n: usize) -> Vec<PathBuf> {
    if items.len() <= n || n == 0 {
        return items.to_vec();
    }
    (0..n).map(|i| items[i * (items.len() - 1) / (n - 1).max(1)].clone()).collect()
}

fn place_lines(flight: &Flight, place: &Place, job: &Job, photos: usize, videos: usize) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(p) = place.main() {
        if let Some(a) = &p.situs {
            v.push(title_case(a));
        }
        v.push(format!("APN {}{}", p.apn, p.acres.map(|a| format!(" · {a:.2} acres")).unwrap_or_default()));
        v.push(place.city_label());
    }
    v.push(format!(
        "Flown {} {}–{} · {photos} photos, {videos} videos",
        flight.date,
        flight.start.format("%H:%M"),
        flight.end.format("%H:%M")
    ));
    if let Some(c) = &job.client {
        v.push(format!("Prepared for {c}"));
    }
    v
}

fn safe(s: &str) -> String {
    s.chars().map(|c| if matches!(c, '/' | ':' | '\\') { '-' } else { c }).collect()
}

/// Make the client and library outputs for a flight. Returns short notes.
pub fn build(dir: &Path, flight: &Flight, place: &Place, job: &Job, cfg: &Config, flagged: &[Flagged]) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let photos = media_in(dir, &[".JPG", ".JPEG"]);
    let videos = media_in(dir, &[".MP4", ".MOV"]);
    let bad: std::collections::BTreeSet<&PathBuf> = flagged.iter().map(|f| &f.path).collect();
    let good: Vec<PathBuf> = photos.iter().filter(|p| !bad.contains(p)).cloned().collect();
    let name = place
        .main()
        .map(|p| p.situs.as_deref().map(title_case).unwrap_or_else(|| format!("APN {}", p.apn)))
        .unwrap_or_else(|| "Flight".into());

    if job.client.is_some() {
        let out = dir.join(format!("Delivery - {} - {}", safe(&name), flight.date));
        let web = out.join("Photos (web)");
        fs::create_dir_all(&web)?;
        par_each(&good, |p| {
            let dst = web.join(p.file_name().unwrap_or_default());
            if dst.exists() { Ok(()) } else { imaging::web_copy(p, &dst, cfg.delivery_long_edge, 90) }
        })?;
        let title = format!("{name} — aerial photos");
        let sheet = evenly(&good, CONTACT_SHEET_MAX);
        let mut lines = place_lines(flight, place, job, photos.len(), videos.len());
        if sheet.len() < good.len() {
            lines.push(format!("Showing {} of {} photos; all are in Photos (web)", sheet.len(), good.len()));
        }
        imaging::contact_sheet_pdf(&out.join("Contact Sheet.pdf"), &title, &lines, &sheet)?;
        for extra in ["map.kml", "shoot-info.txt"] {
            if dir.join(extra).exists() {
                fs::copy(dir.join(extra), out.join(extra))?;
            }
        }
        let skipped = if bad.is_empty() { String::new() } else { format!(" ({} left out)", plural(bad.len(), "flagged photo", "flagged photos")) };
        notes.push(format!("client delivery ready: {}{skipped}", plural(good.len(), "photo", "photos")));
    }

    if job.library {
        let out = dir.join("Library Sample");
        fs::create_dir_all(&out)?;
        // Buyers want the angled "listing" views; straight-down mapping
        // frames only fill in when there aren't enough of those.
        // Take-off and landing frames (a few metres up) are angled too, so
        // a listing view must also be at least SAMPLE_MIN_ALT_M above take-off.
        let (oblique, rest): (Vec<PathBuf>, Vec<PathBuf>) = good.iter().cloned().partition(|p| {
            coverage::read_shot(p).is_some_and(|s| {
                s.gimbal_pitch.is_none_or(|pitch| pitch > -60.0) && s.rel_alt_m.is_none_or(|a| a >= SAMPLE_MIN_ALT_M)
            })
        });
        let mut picks = evenly(&oblique, cfg.library_sample_count);
        if picks.len() < cfg.library_sample_count {
            let high: Vec<PathBuf> = rest
                .into_iter()
                .filter(|p| coverage::read_shot(p).and_then(|s| s.rel_alt_m).is_none_or(|a| a >= SAMPLE_MIN_ALT_M))
                .collect();
            picks.extend(evenly(&high, cfg.library_sample_count - picks.len()));
        }
        par_each(&picks, |p| {
            let dst = out.join(format!("SAMPLE_{}", p.file_name().unwrap_or_default().to_string_lossy()));
            if dst.exists() { Ok(()) } else { imaging::watermark(p, &dst, 1600, &cfg.watermark_text) }
        })?;
        notes.push(plural(picks.len(), "watermarked library sample", "watermarked library samples"));
    }
    Ok(notes)
}
