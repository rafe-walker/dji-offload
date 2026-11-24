//! The client photo report: a designed, branded PDF for real-estate agents
//! and landowners.
//!
//! Pages, in order (sections with nothing to show are left out):
//! 1. **Cover**: full-bleed brand `dark`; hero photo over the top 58%,
//!    accent rule, kicker, title (the address), subtitle, "prepared for"
//!    and date; a bottom band with the mark, brand name and tagline,
//!    clickable contact links and a QR code on a white tile.
//! 2. **Site overview**: a vector map of the parcels, flight path, photo
//!    positions (numbered for featured photos) and mapping footprints, with
//!    north arrow, metric/imperial scale bar and legend; then a facts grid.
//! 3. **Photographs**: two landscape photos per page (portrait ones get a
//!    page each), each with a caption column: number badge matching the
//!    map, title, meta and position lines, delivered file name.
//! 4. **Photo index**: 5 × 6 thumbnails per page with short labels.
//! 5. **Capture & data integrity**: technical facts, integrity lines and the
//!    disclaimer.
//!
//! Interior pages carry a header (mark, brand name, report title) and a
//! footer (accent hairline, brand · url, "Page n of N").
//!
//! The mark: `brand.logo` (PNG with alpha, or JPEG) when given and readable;
//! otherwise a vector reticle (concentric rings, crosshair, centre dot) in
//! the brand colours is drawn, so a report never goes out without a mark.
//!
//! Client-safe by construction: photos are re-encoded from pixels (no EXIF,
//! GPS or maker notes), images carry no names, document info holds only
//! Title, Author (brand), Subject (kicker) and Creator (brand), and every
//! string is scrubbed of the aircraft maker's name and the best-known
//! product-family names (see `scrub`). Pass file names that are already
//! client-facing: a `DJI_…` prefix is removed here, so a name shown in the
//! report can then differ from the file on disk.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::imaging;
use crate::pdfkit::{
    Align, Color, Doc, Font, ImageRef, LETTER_H, LETTER_W, Page, TextStyle, ellipsize, ellipsize_middle, fit_size,
    measure, wrap,
};

// ---------------------------------------------------------------------------
// Public data model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Brand {
    pub name: String,
    pub tagline: Option<String>,
    pub url: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub logo: Option<PathBuf>,
    pub dark: [u8; 3],
    pub accent: [u8; 3],
}

#[derive(Clone, Debug)]
pub struct Fact {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug)]
pub struct MapParcel {
    pub apn: String,
    /// Rings of (lon, lat); holes are drawn with the even-odd rule.
    pub rings: Vec<Vec<(f64, f64)>>,
    pub main: bool,
}

#[derive(Clone, Debug)]
pub struct MapShot {
    pub lon: f64,
    pub lat: f64,
    /// Set for photos featured in the report (matches `PhotoPage::number`).
    pub number: Option<usize>,
    pub nadir: bool,
}

#[derive(Clone, Debug, Default)]
pub struct MapData {
    pub parcels: Vec<MapParcel>,
    /// Flight path, (lon, lat).
    pub path: Vec<(f64, f64)>,
    pub shots: Vec<MapShot>,
    /// Ground footprints of mapping frames, (lon, lat) rings.
    pub footprints: Vec<Vec<(f64, f64)>>,
}

#[derive(Clone, Debug)]
pub struct PhotoPage {
    pub path: PathBuf,
    pub number: usize,
    pub title: String,
    pub meta: String,
    pub position: String,
    pub file_name: String,
    /// A showcase view: its own page, across the full width. Otherwise
    /// photos share a page, three to a page beside their captions.
    pub wide: bool,
}

#[derive(Clone, Debug)]
pub struct IndexItem {
    pub path: PathBuf,
    /// Short caption under the thumbnail (number and description).
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub brand: Brand,
    pub kicker: String,
    pub title: String,
    pub subtitle: String,
    pub prepared_for: Option<String>,
    pub date_line: String,
    pub hero: Option<PathBuf>,
    pub facts: Vec<Fact>,
    pub map: MapData,
    pub photos: Vec<PhotoPage>,
    pub index: Vec<IndexItem>,
    /// Photos delivered in all; the index may show only the first ones.
    pub index_total: usize,
    pub technical: Vec<Fact>,
    pub integrity: Vec<String>,
    pub disclaimer: String,
    pub qr_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Layout constants (points; US Letter 612 × 792, top-left origin)
// ---------------------------------------------------------------------------

const W: f32 = LETTER_W;
const H: f32 = LETTER_H;
/// Side margin of interior pages.
const M: f32 = 54.0;
const CW: f32 = W - 2.0 * M;
/// Content band of interior pages.
const TOP: f32 = 86.0;
const BOTTOM: f32 = 724.0;
/// Cover hero height (58% of the page).
const HERO_H: f32 = H * 0.58;

/// Photo pages: up to three photos a page, caption column beside each.
const PER_PAGE: usize = 3;
const CAP_GAP: f32 = 22.0;
const ROW_GAP: f32 = 24.0;
/// Narrowest caption column; the image takes the rest of the measure.
const CAP_MIN: f32 = 150.0;

/// Photo index grid.
const IDX_COLS: usize = 5;
const IDX_ROWS: usize = 6;
const IDX_GUTTER: f32 = 10.0;

/// Embedded image sizes (long edge, px) and JPEG qualities.
const HERO_PX: u32 = 2400;
const HERO_Q: u8 = 82;
/// Height of the fade at the bottom of the hero, points.
const HERO_FADE: f32 = 110.0;
/// Full-width showcase photos.
const WIDE_PX: u32 = 1800;
const WIDE_Q: u8 = 82;
/// Photos that share a page.
const PHOTO_PX: u32 = 1200;
const PHOTO_Q: u8 = 78;
const THUMB_PX: u32 = 300;
const THUMB_Q: u8 = 72;

struct Pal {
    dark: Color,
    accent: Color,
    /// Gold for small text on white: darker, for contrast.
    accent_deep: Color,
    /// Headings and values on white.
    ink: Color,
    body: Color,
    muted: Color,
    hair: Color,
    tint: Color,
    /// Text on the dark cover.
    soft: Color,
    dim: Color,
}

impl Pal {
    fn new(b: &Brand) -> Pal {
        let dark = Color::from(b.dark);
        let accent = Color::from(b.accent);
        Pal {
            dark,
            accent,
            accent_deep: accent.mix(Color::BLACK, 0.2),
            ink: dark.mix(Color::hex(0x2A2A26), 0.3),
            body: Color::hex(0x4A4B45),
            muted: Color::hex(0x8D8A81),
            hair: Color::hex(0xDDD8CD),
            tint: Color::hex(0xF5F3EE),
            soft: Color::hex(0xD6D2C8),
            dim: Color::hex(0x949086),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Build the report and write it atomically to `dst`.
pub fn write_report(dst: &Path, r: &Report) -> Result<()> {
    let r = &scrub_report(r);
    let pal = Pal::new(&r.brand);
    let mut doc = Doc::new();
    doc.set_title(&r.title);
    if !r.brand.name.trim().is_empty() {
        doc.set_author(&r.brand.name);
        doc.set_creator(&r.brand.name);
    }
    if !r.kicker.trim().is_empty() {
        doc.set_subject(&r.kicker);
    }

    let imgs = prepare_images(&mut doc, r);
    let mark = imgs.logo.map(Mark::Image).unwrap_or(Mark::Vector);

    cover(&mut doc, r, &pal, &imgs, mark);
    let first_interior = doc.page_count();
    let mut section = 0;
    let mut next_section = || {
        section += 1;
        format!("{section:02}")
    };
    if !map_is_empty(&r.map) || !r.facts.is_empty() {
        site_overview(&mut doc, r, &pal, &next_section());
    }
    if !r.photos.is_empty() {
        photo_pages(&mut doc, r, &pal, &imgs, &next_section());
    }
    if !r.index.is_empty() {
        index_pages(&mut doc, r, &pal, &imgs, &next_section());
    }
    if !r.technical.is_empty() || !r.integrity.is_empty() || !r.disclaimer.trim().is_empty() {
        appendix(&mut doc, r, &pal, &next_section());
    }
    let total = doc.page_count();
    for i in first_interior..total {
        chrome(doc.page_mut(i), r, &pal, mark, i + 1, total);
    }
    doc.save(dst)
}

// ---------------------------------------------------------------------------
// Client-safety scrub
// ---------------------------------------------------------------------------

/// Words that would tell a client which aircraft flew the job. Matched
/// case-insensitively as whole words (`DJI` also as a `DJI_` prefix).
const AIRCRAFT_WORDS: [&str; 6] = ["dji", "mavic", "phantom", "matrice", "avata", "zenmuse"];

/// Remove aircraft maker/model words (with a joining `_`, `-` or space).
pub(crate) fn scrub(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'outer: while i < s.len() {
        let word_start = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        if word_start {
            for w in AIRCRAFT_WORDS {
                if lower[i..].starts_with(w) {
                    let end = i + w.len();
                    let word_end = end >= s.len() || !b[end].is_ascii_alphabetic();
                    if word_end {
                        i = end;
                        while i < s.len() && matches!(b[i], b'_' | b'-' | b' ') {
                            i += 1;
                        }
                        continue 'outer;
                    }
                }
            }
        }
        let c = s[i..].chars().next().expect("in bounds");
        out.push(c);
        i += c.len_utf8();
    }
    let out = out.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    if s.ends_with(' ') && !out.is_empty() { out + " " } else { out }
}

fn scrub_report(r: &Report) -> Report {
    let mut r = r.clone();
    let s = |v: &mut String| *v = scrub(v);
    let so = |v: &mut Option<String>| {
        if let Some(x) = v {
            *x = scrub(x);
        }
    };
    s(&mut r.brand.name);
    so(&mut r.brand.tagline);
    s(&mut r.kicker);
    s(&mut r.title);
    s(&mut r.subtitle);
    so(&mut r.prepared_for);
    s(&mut r.date_line);
    for f in r.facts.iter_mut().chain(r.technical.iter_mut()) {
        s(&mut f.label);
        s(&mut f.value);
    }
    for p in &mut r.photos {
        s(&mut p.title);
        s(&mut p.meta);
        s(&mut p.position);
        s(&mut p.file_name);
    }
    for i in &mut r.index {
        s(&mut i.label);
    }
    for l in &mut r.integrity {
        s(l);
    }
    s(&mut r.disclaimer);
    for m in &mut r.map.parcels {
        s(&mut m.apn);
    }
    r
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

struct Images {
    hero: Option<ImageRef>,
    photo: HashMap<PathBuf, ImageRef>,
    thumb: HashMap<PathBuf, ImageRef>,
    logo: Option<ImageRef>,
}

struct Prepared {
    jpeg: Vec<u8>,
}

/// The cover photo: cropped to the hero box, `HERO_PX` wide, with a soft
/// fade into `dark` over the bottom `HERO_FADE` points baked into the
/// pixels (smooth, unlike stacked translucent strips).
fn prepare_hero(path: &Path, dark: [u8; 3]) -> Option<Prepared> {
    let r = catch_unwind(AssertUnwindSafe(|| -> Result<Prepared> {
        let img = imaging::load_rgb(path, HERO_PX)?;
        let (w, h) = (img.width() as f32, img.height() as f32);
        // The largest centred crop with the hero box's aspect.
        let s = (w / W).min(h / HERO_H);
        let (crop_w, crop_h) = ((W * s).round().max(1.0) as u32, (HERO_H * s).round().max(1.0) as u32);
        let (x0, y0) = (((w - crop_w as f32) / 2.0).max(0.0) as u32, ((h - crop_h as f32) / 2.0).max(0.0) as u32);
        let crop = image::imageops::crop_imm(&img, x0, y0, crop_w.min(img.width()), crop_h.min(img.height())).to_image();
        let mut out = imaging::fit_long_edge(crop, HERO_PX);
        let (ow, oh) = out.dimensions();
        let fade_rows = (oh as f32 * HERO_FADE / HERO_H).round() as u32;
        for y in oh.saturating_sub(fade_rows)..oh {
            let t = (y + 1 + fade_rows - oh) as f32 / fade_rows.max(1) as f32;
            let a = 0.62 * t * t;
            for x in 0..ow {
                let p = out.get_pixel_mut(x, y);
                for (c, d) in p.0.iter_mut().zip(dark) {
                    *c = (*c as f32 * (1.0 - a) + d as f32 * a).round() as u8;
                }
            }
        }
        Ok(Prepared { jpeg: imaging::encode_jpeg(&out, HERO_Q)? })
    }));
    r.ok().and_then(Result::ok)
}

/// Decode (EXIF orientation, ICC → sRGB), downscale and re-encode. `None`
/// when the file cannot be read or decoded.
fn prepare(path: &Path, long_edge: u32, q: u8) -> Option<Prepared> {
    let r = catch_unwind(AssertUnwindSafe(|| -> Result<Prepared> {
        let img = imaging::fit_long_edge(imaging::load_rgb(path, long_edge)?, long_edge);
        Ok(Prepared { jpeg: imaging::encode_jpeg(&img, q)? })
    }));
    r.ok().and_then(Result::ok)
}

fn prepare_images(doc: &mut Doc, r: &Report) -> Images {
    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Hero,
        Photo,
        Thumb,
    }
    // One job per distinct (path, kind), all decoded in parallel.
    let mut jobs: Vec<(PathBuf, Kind, u32, u8)> = Vec::new();
    let mut push = |p: &Path, k: Kind, px: u32, q: u8| {
        if !jobs.iter().any(|(jp, jk, ..)| jp == p && *jk == k) {
            jobs.push((p.to_path_buf(), k, px, q));
        }
    };
    if let Some(h) = &r.hero {
        push(h, Kind::Hero, HERO_PX, HERO_Q);
    }
    for p in &r.photos {
        let (px, q) = if p.wide { (WIDE_PX, WIDE_Q) } else { (PHOTO_PX, PHOTO_Q) };
        push(&p.path, Kind::Photo, px, q);
    }
    for i in &r.index {
        push(&i.path, Kind::Thumb, THUMB_PX, THUMB_Q);
    }
    let done = imaging::par_map(&jobs, |(p, k, px, q)| {
        if *k == Kind::Hero { prepare_hero(p, r.brand.dark) } else { prepare(p, *px, *q) }
    });

    let mut out = Images { hero: None, photo: HashMap::new(), thumb: HashMap::new(), logo: None };
    for ((path, kind, ..), prep) in jobs.into_iter().zip(done) {
        let Some(prep) = prep else { continue };
        let Ok(img) = doc.add_jpeg(prep.jpeg) else { continue };
        match kind {
            Kind::Hero => out.hero = Some(img),
            Kind::Photo => {
                out.photo.insert(path, img);
            }
            Kind::Thumb => {
                out.thumb.insert(path, img);
            }
        }
    }
    out.logo = r.brand.logo.as_deref().and_then(|p| load_logo(doc, p));
    out
}

/// The brand logo: PNG (alpha kept) or JPEG, at most 600 px.
fn load_logo(doc: &mut Doc, p: &Path) -> Option<ImageRef> {
    let bytes = std::fs::read(p).ok()?;
    if bytes.starts_with(b"\x89PNG") {
        let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()?.to_rgba8();
        let img = if img.width().max(img.height()) > 600 {
            let s = 600.0 / img.width().max(img.height()) as f32;
            let (w, h) = (((img.width() as f32 * s) as u32).max(1), ((img.height() as f32 * s) as u32).max(1));
            image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };
        Some(doc.add_rgba(&img))
    } else {
        let prep = prepare(p, 600, 90)?;
        doc.add_jpeg(prep.jpeg).ok()
    }
}

#[derive(Clone, Copy)]
enum Mark {
    Image(ImageRef),
    Vector,
}

/// Draw the brand mark in a `size`-point square at (x, y).
fn draw_mark(p: &mut Page, mark: Mark, x: f32, y: f32, size: f32, pal: &Pal) {
    match mark {
        Mark::Image(img) => {
            p.image_contain(&img, x, y, size, size);
        }
        Mark::Vector => reticle(p, x, y, size, pal, true, 1.0),
    }
}

/// The vector reticle mark: rings, crosshair, centre dot and corner ticks,
/// in a 200-unit design square (optionally on a rounded `dark` tile), at
/// `opacity`.
fn reticle(p: &mut Page, x: f32, y: f32, size: f32, pal: &Pal, tile: bool, opacity: f32) {
    let u = size / 200.0;
    let (cx, cy) = (x + 100.0 * u, y + 100.0 * u);
    p.save();
    if tile {
        p.fill_color(pal.dark);
        p.rounded_rect(x, y, size, size, 40.0 * u);
        p.fill();
    }
    p.stroke_color(pal.accent);
    p.fill_color(pal.accent);
    p.alpha(opacity, opacity);
    for (r, w, a) in [(80.0, 3.0, 1.0), (60.0, 2.0, 0.6), (40.0, 2.0, 0.4)] {
        p.save();
        p.alpha(opacity, a * opacity);
        p.line_width(w * u);
        p.circle(cx, cy, r * u);
        p.stroke();
        p.restore();
    }
    p.line_width(3.0 * u);
    p.line(x + 20.0 * u, cy, x + 180.0 * u, cy);
    p.line(cx, y + 20.0 * u, cx, y + 180.0 * u);
    p.circle(cx, cy, 8.0 * u);
    p.fill();
    p.save();
    p.alpha(opacity, 0.7 * opacity);
    p.line_width(2.0 * u);
    for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        p.line(cx + sx * 40.0 * u, cy + sy * 40.0 * u, cx + sx * 25.0 * u, cy + sy * 25.0 * u);
    }
    p.restore();
    p.restore();
}

// ---------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------

fn label_style(pal: &Pal) -> TextStyle {
    TextStyle::new(Font::Bold, 6.6, pal.muted).spacing(1.1)
}

/// Section opener on an interior page: number and label in letter-spaced
/// gold, a large heading, a short accent rule. Returns the next y.
fn section_head(p: &mut Page, y: f32, num: &str, title: &str, pal: &Pal) -> f32 {
    let k = TextStyle::new(Font::Bold, 7.4, pal.accent_deep).spacing(2.2);
    let w = p.text(M, y + 7.0, num, &k);
    p.hrule(M + w + 8.0, M + w + 30.0, y + 4.6, 0.6, pal.accent);
    p.text(M + w + 38.0, y + 7.0, &title.to_uppercase(), &k);
    p.text(M, y + 36.0, title, &TextStyle::new(Font::Regular, 22.0, pal.ink));
    y + 58.0
}

/// Interior header and footer.
fn chrome(p: &mut Page, r: &Report, pal: &Pal, mark: Mark, n: usize, total: usize) {
    draw_mark(p, mark, M, 28.0, 15.0, pal);
    let brand = TextStyle::new(Font::Bold, 6.8, pal.ink).spacing(1.5);
    let bw = p.text(M + 22.0, 38.4, &ellipsize(&r.brand.name.to_uppercase(), &brand, 230.0), &brand);
    let t = TextStyle::new(Font::Regular, 7.4, pal.muted).align(Align::Right);
    let room = (CW - 22.0 - bw - 24.0).max(40.0);
    p.text(W - M, 38.4, &ellipsize(&r.title, &t, room), &t);
    p.hrule(M, W - M, 52.0, 0.4, pal.hair);

    p.hrule(M, W - M, 748.0, 0.5, pal.accent);
    let f = TextStyle::new(Font::Regular, 6.8, pal.muted);
    let mut left = r.brand.name.clone();
    if let Some(u) = &r.brand.url {
        left = format!("{left}  ·  {}", display_url(u));
    }
    let pg = format!("Page {n} of {total}");
    let pw = p.text(W - M, 763.0, &pg, &f.align(Align::Right));
    p.text(M, 763.0, &ellipsize(&left, &f, CW - pw - 20.0), &f);
}

/// Wrap "a · b · c" detail lines between parts where possible (falling
/// back to word wrap for a part wider than the line), at most `max_lines`.
fn wrap_parts(s: &str, style: &TextStyle, max_w: f32, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for part in s.split(" · ").map(str::trim).filter(|p| !p.is_empty()) {
        let cand = if cur.is_empty() { part.to_string() } else { format!("{cur} · {part}") };
        if measure(&cand, style) <= max_w {
            cur = cand;
            continue;
        }
        if !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        let mut w = wrap(part, style, max_w);
        cur = w.pop().unwrap_or_default();
        lines.extend(w);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max_lines && max_lines > 0 {
        lines.truncate(max_lines);
        let last = lines.pop().unwrap_or_default();
        lines.push(ellipsize(&format!("{last} …"), style, max_w));
    }
    lines
}

/// Draw pre-wrapped lines from baseline `y`; returns the next baseline.
fn draw_lines(p: &mut Page, x: f32, mut y: f32, lines: &[String], style: &TextStyle, leading: f32) -> f32 {
    for l in lines {
        p.text(x, y, l, style);
        y += leading;
    }
    y
}

fn display_url(u: &str) -> String {
    let u = u.trim();
    let u = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")).unwrap_or(u);
    u.trim_end_matches('/').to_string()
}

fn url_href(u: &str) -> String {
    let u = u.trim();
    if u.contains("://") { u.to_string() } else { format!("https://{u}") }
}

fn tel_href(t: &str) -> String {
    let digits: String = t.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect();
    format!("tel:{digits}")
}

/// Start a new interior page; returns its index.
fn new_page(doc: &mut Doc) -> usize {
    doc.page();
    doc.page_count() - 1
}

// ---------------------------------------------------------------------------
// Cover
// ---------------------------------------------------------------------------

fn cover(doc: &mut Doc, r: &Report, pal: &Pal, imgs: &Images, mark: Mark) {
    let p = doc.page();
    p.fill_rect(0.0, 0.0, W, H, pal.dark);

    match imgs.hero {
        Some(hero) => {
            // Already cropped to the box, with the fade baked in.
            p.image_cover(&hero, 0.0, 0.0, W, HERO_H);
        }
        None => {
            // No photo: a large, faint reticle fills the space instead.
            reticle(p, W - 360.0, -40.0, 470.0, pal, false, 0.07);
        }
    }
    p.hrule(0.0, W, HERO_H, 0.9, pal.accent);

    // Text block.
    let x = M + 6.0;
    let tw = W - 2.0 * x;
    let mut y = HERO_H + 44.0;
    if !r.kicker.trim().is_empty() {
        let k = TextStyle::new(Font::Bold, 7.8, pal.accent).spacing(2.6);
        p.text(x, y, &ellipsize(&r.kicker.to_uppercase(), &k, tw), &k);
        y += 38.0;
    } else {
        y += 22.0;
    }
    let t = TextStyle::new(Font::Regular, 30.0, Color::WHITE).spacing(-0.2);
    let size = fit_size(&r.title, &t, tw, 20.0);
    if measure(&r.title, &t.size(size)) <= tw {
        p.text(x, y, &r.title, &t.size(size));
    } else {
        let ts = t.size(21.0);
        let mut lines = wrap(&r.title, &ts, tw);
        if lines.len() > 2 {
            lines.truncate(2);
            lines[1] = ellipsize(&format!("{} …", lines[1]), &ts, tw);
        }
        y -= 9.0;
        for (i, l) in lines.iter().enumerate() {
            if i > 0 {
                y += 25.0;
            }
            p.text(x, y, l, &ts);
        }
    }
    if !r.subtitle.trim().is_empty() {
        y += 23.0;
        let s = TextStyle::new(Font::Regular, 10.5, pal.soft).spacing(0.25);
        p.text(x, y, &ellipsize(&r.subtitle, &s, tw), &s);
    }

    // Prepared for / date.
    y += 40.0;
    let lab = TextStyle::new(Font::Bold, 6.4, pal.accent).spacing(1.8);
    let val = TextStyle::new(Font::Regular, 9.6, Color::WHITE);
    let mut cx = x;
    if let Some(pf) = r.prepared_for.as_deref().filter(|s| !s.trim().is_empty()) {
        p.text(cx, y, "PREPARED FOR", &lab);
        p.text(cx, y + 15.0, &ellipsize(pf, &val, 240.0), &val);
        cx += 260.0;
    }
    if !r.date_line.trim().is_empty() {
        p.text(cx, y, "DATE", &lab);
        p.text(cx, y + 15.0, &ellipsize(&r.date_line, &val, W - x - cx), &val);
    }

    // Bottom band.
    let band = H - 106.0;
    p.save();
    p.alpha(1.0, 0.32);
    p.hrule(M, W - M, band, 0.5, pal.accent);
    p.restore();
    let mark_s = 46.0;
    let my = band + 26.0;

    // QR tile at the right, contacts to its left (right-aligned).
    let mut right = W - M;
    // A URL too long for a QR code just goes without one.
    let qr_ok = |q: &&str| qrcode::QrCode::with_error_correction_level(q.trim().as_bytes(), qrcode::EcLevel::M).is_ok();
    if let Some(q) = r.qr_url.as_deref().filter(|s| !s.trim().is_empty()).filter(qr_ok) {
        let tile = 66.0;
        let ty = band + 16.0;
        let tx = W - M - tile;
        p.save();
        p.fill_color(Color::WHITE);
        p.rounded_rect(tx, ty, tile, tile, 5.0);
        p.fill();
        p.restore();
        let pad = 6.0;
        let _ = p.qr(tx + pad, ty + pad, tile - 2.0 * pad, q.trim(), pal.dark);
        p.link(tx, ty, tile, tile, &url_href(q));
        right = tx - 18.0;
    }
    let c = TextStyle::new(Font::Regular, 8.2, pal.soft).spacing(0.3).align(Align::Right);
    let mut contacts: Vec<(String, String)> = Vec::new();
    if let Some(u) = r.brand.url.as_deref().filter(|s| !s.trim().is_empty()) {
        contacts.push((display_url(u), url_href(u)));
    }
    if let Some(ph) = r.brand.phone.as_deref().filter(|s| !s.trim().is_empty()) {
        contacts.push((ph.trim().to_string(), tel_href(ph)));
    }
    if let Some(e) = r.brand.email.as_deref().filter(|s| !s.trim().is_empty()) {
        contacts.push((e.trim().to_string(), format!("mailto:{}", e.trim())));
    }
    let n = contacts.len() as f32;
    let mut cy = my + 26.0 - (n - 1.0) * 14.0 / 2.0;
    let mut left = right;
    for (shown, href) in contacts {
        let s = ellipsize(&shown, &c, 200.0);
        let w = p.text(right, cy, &s, &c);
        p.link(right - w - 2.0, cy - 9.0, w + 4.0, 13.0, &href);
        left = left.min(right - w);
        cy += 14.0;
    }

    // Mark and wordmark at the left.
    draw_mark(p, mark, M, my, mark_s, pal);
    let tx = M + mark_s + 14.0;
    let room = (left - 24.0 - tx).max(80.0);
    let name_up = r.brand.name.trim().to_uppercase();
    let tag = r.brand.tagline.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(str::to_uppercase);
    let ts = TextStyle::new(Font::Regular, 6.4, pal.dim).spacing(1.5);
    let one = TextStyle::new(Font::Bold, 10.5, Color::WHITE).spacing(2.4);
    let words: Vec<&str> = name_up.split_whitespace().collect();
    if measure(&name_up, &one) <= room || words.len() < 2 {
        // One line: NAME, tagline under it.
        let size = fit_size(&name_up, &one, room, 7.0);
        let st = one.size(size).spacing(2.4 * size / 10.5);
        let name_y = if tag.is_some() { my + 21.0 } else { my + 27.0 };
        p.text(tx, name_y, &ellipsize(&name_up, &st, room), &st);
        if let Some(t) = &tag {
            p.text(tx, name_y + 13.0, &ellipsize(t, &ts, room), &ts);
        }
    } else {
        // Stacked wordmark: first word large, the rest small in the accent.
        let big = TextStyle::new(Font::Bold, 14.0, Color::WHITE).spacing(3.2);
        let size = fit_size(words[0], &big, room, 8.0);
        let big = big.size(size).spacing(3.2 * size / 14.0);
        let rest = words[1..].join(" ");
        let small = TextStyle::new(Font::Bold, 6.9, pal.accent).spacing(2.5);
        let y1 = if tag.is_some() { my + 17.0 } else { my + 22.0 };
        let w1 = p.text(tx, y1, words[0], &big);
        // Stretch the second line to the first line's width when it is
        // shorter, the way a set wordmark is justified.
        let natural = measure(&rest, &small);
        let n_chars = rest.chars().count().max(2) as f32;
        let small = if natural < w1 && w1 - natural < 60.0 {
            small.spacing(small.spacing + (w1 - natural) / (n_chars - 1.0))
        } else {
            small
        };
        p.text(tx, y1 + 12.0, &ellipsize(&rest, &small, room), &small);
        if let Some(t) = &tag {
            p.text(tx, y1 + 26.0, &ellipsize(t, &ts, room), &ts);
        }
    }
}

// ---------------------------------------------------------------------------
// Map maths
// ---------------------------------------------------------------------------

/// Mean Earth radius (m).
const R_EARTH: f64 = 6_371_008.8;
const FT_PER_M: f64 = 3.280_839_895;

/// Equirectangular projection centred on (lon0, lat0), scaled and centred
/// into a page box. North is up; distances are true near the centre.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Proj {
    lon0: f64,
    lat0: f64,
    /// Metres per degree of longitude / latitude at lat0.
    mx: f64,
    my: f64,
    /// Points per metre.
    pub(crate) scale: f64,
    /// Page position of (lon0, lat0).
    px: f64,
    py: f64,
}

impl Proj {
    /// Fit the points into `bx` with `margin` points on every side. `None`
    /// when there are no points.
    pub(crate) fn fit(pts: &[(f64, f64)], bx: [f32; 4], margin: f32, min_extent_m: f64) -> Option<Proj> {
        let (mut lo_x, mut hi_x, mut lo_y, mut hi_y) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for &(lon, lat) in pts.iter().filter(|(a, b)| a.is_finite() && b.is_finite()) {
            lo_x = lo_x.min(lon);
            hi_x = hi_x.max(lon);
            lo_y = lo_y.min(lat);
            hi_y = hi_y.max(lat);
        }
        if lo_x > hi_x {
            return None;
        }
        let (lon0, lat0) = ((lo_x + hi_x) / 2.0, (lo_y + hi_y) / 2.0);
        let my = R_EARTH * std::f64::consts::PI / 180.0;
        let mx = my * lat0.to_radians().cos();
        let ext_x = ((hi_x - lo_x) * mx).max(min_extent_m);
        let ext_y = ((hi_y - lo_y) * my).max(min_extent_m);
        let aw = (bx[2] - 2.0 * margin).max(1.0) as f64;
        let ah = (bx[3] - 2.0 * margin).max(1.0) as f64;
        let scale = (aw / ext_x).min(ah / ext_y);
        Some(Proj {
            lon0,
            lat0,
            mx,
            my,
            scale,
            px: (bx[0] + bx[2] / 2.0) as f64,
            py: (bx[1] + bx[3] / 2.0) as f64,
        })
    }

    /// Local metres east/north of the centre.
    pub(crate) fn metres(&self, lon: f64, lat: f64) -> (f64, f64) {
        ((lon - self.lon0) * self.mx, (lat - self.lat0) * self.my)
    }

    pub(crate) fn page(&self, lon: f64, lat: f64) -> (f32, f32) {
        let (e, n) = self.metres(lon, lat);
        ((self.px + e * self.scale) as f32, (self.py - n * self.scale) as f32)
    }

    /// Inverse of `page` in local metres.
    fn page_to_metres(&self, x: f32, y: f32) -> (f64, f64) {
        ((x as f64 - self.px) / self.scale, (self.py - y as f64) / self.scale)
    }
}

/// The largest 1, 2 or 5 × 10ⁿ that is ≤ `x` (x > 0).
pub(crate) fn nice_floor(x: f64) -> f64 {
    if x.is_nan() || x <= 0.0 {
        return 0.0;
    }
    let p = 10f64.powf(x.log10().floor());
    let f = x / p;
    // Guard against 4.9999 from floating point.
    let f = if (f - f.round()).abs() < 1e-9 { f.round() } else { f };
    let m = if f >= 5.0 {
        5.0
    } else if f >= 2.0 {
        2.0
    } else {
        1.0
    };
    m * p
}

#[derive(Debug, PartialEq)]
pub(crate) struct ScaleBar {
    pub metres: f64,
    pub metres_pt: f32,
    pub feet: f64,
    pub feet_pt: f32,
}

/// Round-number scale bars no longer than `max_pt`, at `pt_per_m`.
pub(crate) fn scale_bar(pt_per_m: f64, max_pt: f32) -> ScaleBar {
    let max_m = max_pt as f64 / pt_per_m;
    let metres = nice_floor(max_m);
    let feet = nice_floor(max_m * FT_PER_M);
    ScaleBar {
        metres,
        metres_pt: (metres * pt_per_m) as f32,
        feet,
        feet_pt: (feet / FT_PER_M * pt_per_m) as f32,
    }
}

/// "0.5", "20", "1.5 km".
fn len_label(v: f64, unit: &str) -> String {
    let (v, unit) = if unit == "m" && v >= 1000.0 { (v / 1000.0, "km") } else { (v, unit) };
    let s = if (v - v.round()).abs() < 1e-9 { format!("{}", v.round() as i64) } else { format!("{v:.1}") };
    if unit.is_empty() { s } else { format!("{s} {unit}") }
}

fn map_is_empty(m: &MapData) -> bool {
    m.parcels.iter().all(|p| p.rings.iter().all(Vec::is_empty)) && m.path.is_empty() && m.shots.is_empty()
}

/// Clip a page-space polygon to a rectangle (Sutherland–Hodgman).
fn clip_ring(pts: &[(f32, f32)], b: [f32; 4]) -> Vec<(f32, f32)> {
    let mut out: Vec<(f32, f32)> = pts.to_vec();
    let (x0, y0, x1, y1) = (b[0], b[1], b[0] + b[2], b[1] + b[3]);
    // (inside test, intersection) for each of the four edges.
    type Edge = (fn(&(f32, f32), f32) -> bool, bool, f32);
    let edges: [Edge; 4] = [
        (|p, v| p.0 >= v, true, x0),
        (|p, v| p.0 <= v, true, x1),
        (|p, v| p.1 >= v, false, y0),
        (|p, v| p.1 <= v, false, y1),
    ];
    for (inside, vertical, v) in edges {
        let input = std::mem::take(&mut out);
        let n = input.len();
        for i in 0..n {
            let (a, c) = (input[i], input[(i + 1) % n]);
            let cut = |a: (f32, f32), c: (f32, f32)| {
                if vertical {
                    let t = (v - a.0) / (c.0 - a.0);
                    (v, a.1 + t * (c.1 - a.1))
                } else {
                    let t = (v - a.1) / (c.1 - a.1);
                    (a.0 + t * (c.0 - a.0), v)
                }
            };
            match (inside(&a, v), inside(&c, v)) {
                (true, true) => out.push(c),
                (true, false) => out.push(cut(a, c)),
                (false, true) => {
                    out.push(cut(a, c));
                    out.push(c);
                }
                (false, false) => {}
            }
        }
    }
    out
}

/// Area-weighted centroid of a page-space ring (falls back to the mean).
fn centroid(pts: &[(f32, f32)]) -> (f32, f32) {
    let n = pts.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let (mut a, mut cx, mut cy) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..n {
        let (x0, y0) = (pts[i].0 as f64, pts[i].1 as f64);
        let (x1, y1) = (pts[(i + 1) % n].0 as f64, pts[(i + 1) % n].1 as f64);
        let c = x0 * y1 - x1 * y0;
        a += c;
        cx += (x0 + x1) * c;
        cy += (y0 + y1) * c;
    }
    if a.abs() < 1e-6 {
        let sx: f32 = pts.iter().map(|p| p.0).sum();
        let sy: f32 = pts.iter().map(|p| p.1).sum();
        return (sx / n as f32, sy / n as f32);
    }
    ((cx / (3.0 * a)) as f32, (cy / (3.0 * a)) as f32)
}

// ---------------------------------------------------------------------------
// Site overview
// ---------------------------------------------------------------------------

fn site_overview(doc: &mut Doc, r: &Report, pal: &Pal, num: &str) {
    let pi = new_page(doc);
    let p = doc.page_mut(pi);
    let mut y = section_head(p, TOP, num, "Site overview", pal);
    if !map_is_empty(&r.map) {
        // The map takes whatever the legend and facts leave, within reason.
        let legend_h = legend(None, 0.0, &r.map, pal);
        let facts_h = if r.facts.is_empty() { 0.0 } else { facts_height(&r.facts, pal) + 22.0 };
        let map_h = (BOTTOM - y - 12.0 - legend_h - facts_h).clamp(250.0, 420.0);
        draw_map(p, [M, y, CW, map_h], &r.map, pal);
        y += map_h + 12.0;
        y = legend(Some(p), y, &r.map, pal) + 22.0;
    }
    if !r.facts.is_empty() {
        facts_grid(doc, pi, y, &r.facts, pal);
    }
}

fn draw_map(p: &mut Page, bx: [f32; 4], map: &MapData, pal: &Pal) {
    let [x, y, w, h] = bx;
    p.save();
    p.fill_color(pal.tint);
    p.rect(x, y, w, h);
    p.fill();
    p.restore();

    // Fit: the main parcel plus where we flew; everything else is context.
    let main_pts: Vec<(f64, f64)> =
        map.parcels.iter().filter(|m| m.main).flat_map(|m| m.rings.iter().flatten().copied()).collect();
    let mut fit_pts = main_pts.clone();
    fit_pts.extend(map.path.iter().copied());
    fit_pts.extend(map.shots.iter().map(|s| (s.lon, s.lat)));
    if fit_pts.is_empty() {
        fit_pts.extend(map.parcels.iter().flat_map(|m| m.rings.iter().flatten().copied()));
    }
    let Some(proj) = Proj::fit(&fit_pts, bx, 46.0, 40.0) else { return };

    p.save();
    p.rect(x, y, w, h);
    p.clip();

    // Faint survey grid at the scale-bar step.
    let bar = scale_bar(proj.scale, (w * 0.22).min(150.0));
    if bar.metres > 0.0 {
        let step = bar.metres / 2.0;
        let (e0, n1) = proj.page_to_metres(x, y);
        let (e1, n0) = proj.page_to_metres(x + w, y + h);
        p.save();
        p.stroke_color(pal.hair);
        p.alpha(1.0, 0.7);
        p.line_width(0.3);
        let mut e = (e0 / step).ceil() * step;
        while e <= e1 {
            let gx = (proj.px + e * proj.scale) as f32;
            p.line(gx, y, gx, y + h);
            e += step;
        }
        let mut n = (n0 / step).ceil() * step;
        while n <= n1 {
            let gy = (proj.py - n * proj.scale) as f32;
            p.line(x, gy, x + w, gy);
            n += step;
        }
        p.restore();
    }

    let ring_pts = |ring: &[(f64, f64)]| -> Vec<(f32, f32)> { ring.iter().map(|&(a, b)| proj.page(a, b)).collect() };

    // Mapping footprints: barely there.
    if !map.footprints.is_empty() {
        p.save();
        p.fill_color(pal.accent);
        p.stroke_color(pal.accent);
        p.alpha(0.035, 0.14);
        p.line_width(0.3);
        for f in &map.footprints {
            p.polygon(&ring_pts(f), true);
            p.fill_stroke();
        }
        p.restore();
    }

    // Neighbouring parcels: grey hairlines with small APN labels.
    let lab = TextStyle::new(Font::Regular, 5.8, Color::hex(0x7F7B71)).spacing(0.25).align(Align::Center);
    p.save();
    p.stroke_color(Color::hex(0xA9A498));
    p.line_width(0.45);
    for m in map.parcels.iter().filter(|m| !m.main) {
        for ring in &m.rings {
            p.polygon(&ring_pts(ring), true);
        }
        p.stroke();
    }
    p.restore();
    for m in map.parcels.iter().filter(|m| !m.main) {
        let Some(outer) = m.rings.first() else { continue };
        // Label the part of the parcel that is on the map.
        let pts = clip_ring(&ring_pts(outer), [x + 6.0, y + 6.0, w - 12.0, h - 12.0]);
        if pts.len() < 3 {
            continue;
        }
        let (cx, cy) = centroid(&pts);
        let (minx, maxx) = pts.iter().fold((f32::MAX, f32::MIN), |a, p| (a.0.min(p.0), a.1.max(p.0)));
        let (miny, maxy) = pts.iter().fold((f32::MAX, f32::MIN), |a, p| (a.0.min(p.1), a.1.max(p.1)));
        let tw = measure(&m.apn, &lab);
        if maxx - minx > tw + 10.0 && maxy - miny > 14.0 {
            p.text(cx, cy + 2.0, &m.apn, &lab);
        }
    }

    // The subject parcel.
    for m in map.parcels.iter().filter(|m| m.main) {
        p.save();
        p.fill_color(pal.accent);
        p.alpha(0.13, 1.0);
        for ring in &m.rings {
            p.polygon(&ring_pts(ring), true);
        }
        p.fill_evenodd();
        p.restore();
        p.save();
        p.stroke_color(pal.accent);
        p.line_width(1.5);
        p.round_caps(true);
        for ring in &m.rings {
            p.polygon(&ring_pts(ring), true);
        }
        p.stroke();
        p.restore();
    }

    // Flight path.
    if map.path.len() > 1 {
        p.save();
        p.stroke_color(pal.dark);
        p.alpha(1.0, 0.7);
        p.line_width(0.6);
        p.round_caps(true);
        p.polygon(&ring_pts(&map.path), false);
        p.stroke();
        p.restore();
    }

    // Photo positions: solid dots; mapping frames: small rings.
    p.save();
    p.fill_color(pal.dark);
    p.alpha(0.6, 1.0);
    for s in map.shots.iter().filter(|s| s.number.is_none() && !s.nadir) {
        let (sx, sy) = proj.page(s.lon, s.lat);
        p.circle(sx, sy, 1.5);
    }
    p.fill();
    p.restore();
    p.save();
    p.stroke_color(pal.dark);
    p.alpha(1.0, 0.55);
    p.line_width(0.45);
    for s in map.shots.iter().filter(|s| s.number.is_none() && s.nadir) {
        let (sx, sy) = proj.page(s.lon, s.lat);
        p.circle(sx, sy, 1.6);
    }
    p.stroke();
    p.restore();

    // Featured photos: numbered gold badges, drawn last.
    let mut featured: Vec<&MapShot> = map.shots.iter().filter(|s| s.number.is_some()).collect();
    featured.sort_by_key(|s| std::cmp::Reverse(s.number));
    for s in featured {
        let (sx, sy) = proj.page(s.lon, s.lat);
        badge(p, sx, sy, 6.6, s.number.unwrap_or(0), pal, true);
    }
    p.restore();

    // North arrow (top right).
    let (nx, ny) = (x + w - 24.0, y + 28.0);
    p.save();
    p.fill_color(Color::WHITE);
    p.alpha(0.85, 1.0);
    p.circle(nx, ny, 15.0);
    p.fill();
    p.restore();
    p.save();
    p.stroke_color(pal.hair);
    p.line_width(0.5);
    p.circle(nx, ny, 15.0);
    p.stroke();
    p.fill_color(pal.dark);
    p.polygon(&[(nx, ny - 10.5), (nx - 4.6, ny + 6.5), (nx, ny + 3.2)], true);
    p.fill();
    p.fill_color(pal.accent);
    p.polygon(&[(nx, ny - 10.5), (nx + 4.6, ny + 6.5), (nx, ny + 3.2)], true);
    p.fill();
    p.restore();
    p.text(nx, ny - 18.5, "N", &TextStyle::new(Font::Bold, 6.6, pal.ink).align(Align::Center));

    // Scale bar (bottom left): metres above, feet below.
    if bar.metres > 0.0 && bar.feet > 0.0 {
        let bx0 = x + 16.0;
        let by = y + h - 30.0;
        let len = bar.metres_pt.max(bar.feet_pt);
        p.save();
        p.fill_color(Color::WHITE);
        p.alpha(0.86, 1.0);
        p.rounded_rect(bx0 - 8.0, by - 15.0, len + 34.0, 36.0, 3.0);
        p.fill();
        p.restore();
        let t = TextStyle::new(Font::Regular, 5.6, pal.body);
        let bar_h = 2.6;
        for (i, (val, pt, unit)) in [(bar.metres, bar.metres_pt, "m"), (bar.feet, bar.feet_pt, "ft")].into_iter().enumerate() {
            let top = if i == 0 { by - bar_h } else { by };
            let segs = 4;
            for s in 0..segs {
                let sx = bx0 + pt * s as f32 / segs as f32;
                let c = if (s + i) % 2 == 0 { pal.dark } else { Color::WHITE };
                p.fill_rect(sx, top, pt / segs as f32, bar_h, c);
            }
            p.save();
            p.stroke_color(pal.dark);
            p.line_width(0.35);
            p.rect(bx0, top, pt, bar_h);
            p.stroke();
            p.restore();
            let ly = if i == 0 { top - 3.2 } else { top + bar_h + 6.8 };
            p.text(bx0, ly, "0", &t.align(Align::Center));
            p.text(bx0 + pt / 2.0, ly, &len_label(val / 2.0, ""), &t.align(Align::Center));
            p.text(bx0 + pt - 2.0, ly, &len_label(val, unit), &t);
        }
    }

    // Hairline frame.
    p.save();
    p.stroke_color(pal.hair);
    p.line_width(0.5);
    p.rect(x, y, w, h);
    p.stroke();
    p.restore();
}

/// A gold disc with a dark number; `ring` adds a white outline (on maps).
fn badge(p: &mut Page, cx: f32, cy: f32, r: f32, n: usize, pal: &Pal, ring: bool) {
    p.save();
    p.fill_color(pal.accent);
    if ring {
        p.stroke_color(Color::WHITE);
        p.line_width(r * 0.16);
        p.circle(cx, cy, r);
        p.fill_stroke();
    } else {
        p.circle(cx, cy, r);
        p.fill();
    }
    p.restore();
    let s = n.to_string();
    let size = if s.len() > 2 { r * 0.95 } else { r * 1.12 };
    let st = TextStyle::new(Font::Bold, size, pal.dark).align(Align::Center);
    p.text(cx, cy + size * Font::CAP_HEIGHT / 2.0, &s, &st);
}

/// Legend entries under the map, wrapping when needed. Returns the y
/// below it; with no page, only measures (returns the height from 0).
fn legend(mut page: Option<&mut Page>, y: f32, map: &MapData, pal: &Pal) -> f32 {
    #[derive(Clone, Copy)]
    enum Sw {
        Main,
        Other,
        Foot,
        Path,
        Dot,
        Ring,
        Badge,
    }
    let mut items: Vec<(Sw, &str)> = Vec::new();
    if map.parcels.iter().any(|m| m.main) {
        items.push((Sw::Main, "Subject parcel"));
    }
    if map.parcels.iter().any(|m| !m.main) {
        items.push((Sw::Other, "Adjacent parcels"));
    }
    if map.path.len() > 1 {
        items.push((Sw::Path, "Flight path"));
    }
    if map.shots.iter().any(|s| s.number.is_none() && !s.nadir) {
        items.push((Sw::Dot, "Photo position"));
    }
    if map.shots.iter().any(|s| s.number.is_none() && s.nadir) {
        items.push((Sw::Ring, "Mapping frame"));
    }
    if !map.footprints.is_empty() {
        items.push((Sw::Foot, "Mapping coverage"));
    }
    if map.shots.iter().any(|s| s.number.is_some()) {
        items.push((Sw::Badge, "Featured photo"));
    }
    let t = TextStyle::new(Font::Regular, 6.5, pal.body);
    let gap = 13.0;
    let (mut lx, mut ly) = (M, y + 7.0);
    for (sw, label) in items {
        let lw = measure(label, &t);
        if lx > M && lx + 15.0 + lw > W - M {
            lx = M;
            ly += 14.0;
        }
        let Some(p) = page.as_deref_mut() else {
            lx += 15.0 + lw + gap;
            continue;
        };
        let (sx, sy) = (lx, ly - 5.5);
        match sw {
            Sw::Main => {
                p.save();
                p.fill_color(pal.accent.mix(Color::WHITE, 0.85));
                p.stroke_color(pal.accent);
                p.line_width(1.0);
                p.rect(sx, sy, 11.0, 7.0);
                p.fill_stroke();
                p.restore();
            }
            Sw::Other => {
                p.save();
                p.stroke_color(Color::hex(0xA9A498));
                p.line_width(0.5);
                p.rect(sx, sy, 11.0, 7.0);
                p.stroke();
                p.restore();
            }
            Sw::Foot => {
                p.save();
                p.fill_color(pal.accent);
                p.stroke_color(pal.accent);
                p.alpha(0.08, 0.35);
                p.line_width(0.4);
                p.rect(sx, sy, 11.0, 7.0);
                p.fill_stroke();
                p.restore();
            }
            Sw::Path => {
                p.save();
                p.stroke_color(pal.dark);
                p.line_width(0.7);
                p.line(sx, sy + 3.5, sx + 11.0, sy + 3.5);
                p.restore();
            }
            Sw::Dot => {
                p.save();
                p.fill_color(pal.dark);
                p.alpha(0.6, 1.0);
                p.circle(sx + 5.5, sy + 3.5, 1.7);
                p.fill();
                p.restore();
            }
            Sw::Ring => {
                p.save();
                p.stroke_color(pal.dark);
                p.line_width(0.5);
                p.circle(sx + 5.5, sy + 3.5, 1.8);
                p.stroke();
                p.restore();
            }
            Sw::Badge => badge(p, sx + 5.5, sy + 3.5, 4.6, 1, pal, false),
        }
        p.text(lx + 15.0, ly, label, &t);
        lx += 15.0 + lw + gap;
    }
    ly + 4.0
}

const FACT_COL_GAP: f32 = 28.0;
const FACT_COL_W: f32 = (CW - FACT_COL_GAP) / 2.0;

fn fact_value_style(pal: &Pal) -> TextStyle {
    TextStyle::new(Font::Regular, 9.6, pal.ink)
}

/// Value lines per fact in one grid row, and the row height.
fn fact_row(row: &[Fact], pal: &Pal) -> (Vec<Vec<String>>, f32) {
    let val = fact_value_style(pal);
    let lines: Vec<Vec<String>> = row
        .iter()
        .map(|f| {
            let mut l = wrap(&f.value, &val, FACT_COL_W);
            if l.len() > 3 {
                l.truncate(3);
                l[2] = ellipsize(&format!("{} …", l[2]), &val, FACT_COL_W);
            }
            l
        })
        .collect();
    let n = lines.iter().map(Vec::len).max().unwrap_or(1).max(1);
    (lines, 38.0 + 12.5 * (n as f32 - 1.0))
}

fn facts_height(facts: &[Fact], pal: &Pal) -> f32 {
    facts.chunks(2).map(|row| fact_row(row, pal).1).sum()
}

/// Facts in a two-column grid: small letter-spaced label over the value,
/// hairline between rows. Flows onto new pages when needed.
fn facts_grid(doc: &mut Doc, mut pi: usize, mut y: f32, facts: &[Fact], pal: &Pal) -> (usize, f32) {
    let (col_gap, col_w) = (FACT_COL_GAP, FACT_COL_W);
    let lab = label_style(pal);
    let val = fact_value_style(pal);
    for row in facts.chunks(2) {
        let (lines, row_h) = fact_row(row, pal);
        if y + row_h > BOTTOM {
            pi = new_page(doc);
            y = TOP;
        }
        let p = doc.page_mut(pi);
        for (i, (f, ls)) in row.iter().zip(&lines).enumerate() {
            let x = M + i as f32 * (col_w + col_gap);
            p.hrule(x, x + col_w, y, 0.5, pal.hair);
            p.text(x, y + 13.0, &ellipsize(&f.label.to_uppercase(), &lab, col_w), &lab);
            for (j, l) in ls.iter().enumerate() {
                p.text(x, y + 27.0 + 12.5 * j as f32, l, &val);
            }
        }
        y += row_h;
    }
    (pi, y)
}

// ---------------------------------------------------------------------------
// Photo pages
// ---------------------------------------------------------------------------

fn photo_pages(doc: &mut Doc, r: &Report, pal: &Pal, imgs: &Images, num: &str) {
    // Showcase and portrait photos get a page each; the rest share, up to
    // three a page, in order.
    let is_portrait = |ph: &PhotoPage| imgs.photo.get(&ph.path).is_some_and(|i| i.h > i.w);
    let alone = |ph: &PhotoPage| ph.wide || is_portrait(ph);
    let mut pages: Vec<Vec<&PhotoPage>> = Vec::new();
    for ph in &r.photos {
        match pages.last_mut() {
            Some(last) if last.len() < PER_PAGE && !alone(last[0]) && !alone(ph) => last.push(ph),
            _ => pages.push(vec![ph]),
        }
    }
    for (k, group) in pages.iter().enumerate() {
        let pi = new_page(doc);
        let p = doc.page_mut(pi);
        let y = if k == 0 { section_head(p, TOP, num, "Photographs", pal) } else { TOP };
        let avail = BOTTOM - y;
        if let [one] = group.as_slice()
            && !is_portrait(one)
        {
            // A landscape photo on its own gets the full width.
            photo_wide(p, y, avail, one, imgs.photo.get(&one.path).copied(), pal);
            continue;
        }
        // Rows are sized for a full page, so a short last page keeps the
        // same photo size, top-aligned.
        let rows = if group.len() == 1 { 1 } else { PER_PAGE };
        let row_h = (avail - ROW_GAP * (rows as f32 - 1.0)) / rows as f32;
        let mut y = y;
        for ph in group {
            let h = photo_block(p, y, row_h, ph, imgs.photo.get(&ph.path).copied(), pal);
            y += h + ROW_GAP;
        }
    }
}

/// One landscape photo across the full measure, caption underneath:
/// badge and title on the left, details on the right.
fn photo_wide(p: &mut Page, y: f32, avail: f32, ph: &PhotoPage, img: Option<ImageRef>, pal: &Pal) {
    let max_h = avail - 96.0;
    let ih = match img {
        Some(img) => {
            let s = (CW / img.w as f32).min(max_h / img.h as f32);
            let (w, h) = (img.w as f32 * s, img.h as f32 * s);
            p.image(&img, M + (CW - w) / 2.0, y, w, h);
            h
        }
        None => {
            let h = (CW * 2.0 / 3.0).min(max_h);
            p.fill_rect(M, y, CW, h, pal.tint);
            let t = TextStyle::new(Font::Regular, 8.0, pal.muted).align(Align::Center);
            p.text(M + CW / 2.0, y + h / 2.0 + 3.0, "Preview unavailable", &t);
            h
        }
    };
    let cy = y + ih + 20.0;
    badge(p, M + 11.0, cy + 11.0, 11.0, ph.number, pal, false);
    let title = TextStyle::new(Font::Bold, 12.0, pal.ink);
    let col2 = M + 262.0;
    let tw = col2 - M - 34.0 - 24.0;
    draw_lines(p, M + 34.0, cy + 15.0, &wrap_parts(&ph.title, &title, tw, 3), &title, 15.0);
    let w2 = W - M - col2;
    let mut ty = cy + 9.0;
    if !ph.meta.trim().is_empty() {
        let st = TextStyle::new(Font::Regular, 7.8, pal.body);
        ty = draw_lines(p, col2, ty, &wrap_parts(&ph.meta, &st, w2, 3), &st, 11.0) + 1.0;
    }
    if !ph.position.trim().is_empty() {
        let st = TextStyle::new(Font::Regular, 7.8, pal.muted);
        ty = draw_lines(p, col2, ty, &wrap_parts(&ph.position, &st, w2, 2), &st, 11.0);
    }
    if !ph.file_name.trim().is_empty() {
        let f = TextStyle::new(Font::Regular, 6.6, pal.muted).spacing(0.35);
        let lab = label_style(pal).size(5.6);
        let lw = p.text(col2, ty + 6.0, "FILE", &lab);
        let lines = wrap_file_name(&ph.file_name, &f, w2 - lw - 8.0, 2);
        draw_lines(p, col2 + lw + 8.0, ty + 6.0, &lines, &f, 9.0);
    }
}

/// One photo, right-aligned and as large as `max_h` allows, with its
/// caption in the column to its left. Returns the block height.
fn photo_block(p: &mut Page, y: f32, max_h: f32, ph: &PhotoPage, img: Option<ImageRef>, pal: &Pal) -> f32 {
    let max_w = CW - CAP_MIN - CAP_GAP;
    let (iw, ih) = match img {
        Some(img) => {
            let s = (max_w / img.w as f32).min(max_h / img.h as f32);
            let (w, h) = (img.w as f32 * s, img.h as f32 * s);
            p.image(&img, W - M - w, y, w, h);
            (w, h)
        }
        None => {
            let h = max_h.min(max_w * 2.0 / 3.0);
            let w = h * 1.5;
            p.fill_rect(W - M - w, y, w, h, pal.tint);
            let t = TextStyle::new(Font::Regular, 8.0, pal.muted).align(Align::Center);
            p.text(W - M - w / 2.0, y + h / 2.0 + 3.0, "Preview unavailable", &t);
            (w, h)
        }
    };
    let cap_w = CW - iw - CAP_GAP;

    // Caption column.
    let x = M;
    badge(p, x + 11.0, y + 11.0, 11.0, ph.number, pal, false);
    p.hrule(x + 30.0, x + cap_w, y + 11.0, 0.5, pal.hair);
    let mut cy = y + 42.0;
    let title = TextStyle::new(Font::Bold, 11.0, pal.ink);
    if !ph.title.trim().is_empty() {
        cy = draw_lines(p, x, cy, &wrap_parts(&ph.title, &title, cap_w, 3), &title, 14.0) + 3.0;
    }
    let meta = TextStyle::new(Font::Regular, 7.8, pal.body);
    if !ph.meta.trim().is_empty() {
        cy = draw_lines(p, x, cy, &wrap_parts(&ph.meta, &meta, cap_w, 4), &meta, 11.0) + 2.0;
    }
    let pos = TextStyle::new(Font::Regular, 7.8, pal.muted);
    if !ph.position.trim().is_empty() {
        draw_lines(p, x, cy, &wrap_parts(&ph.position, &pos, cap_w, 3), &pos, 11.0);
    }
    // File name sits on the image's bottom edge, wrapped rather than cut.
    if !ph.file_name.trim().is_empty() {
        let f = TextStyle::new(Font::Regular, 6.4, pal.muted).spacing(0.3);
        let lines = wrap_file_name(&ph.file_name, &f, cap_w, 2);
        let lab = label_style(pal).size(5.6);
        let base = y + ih - 1.5 - 9.0 * (lines.len() as f32 - 1.0);
        p.text(x, base - 10.5, "FILE", &lab);
        draw_lines(p, x, base, &lines, &f, 9.0);
    }
    ih
}

/// Break a file name at `_` (then `-`) so it fits in `max_lines` lines of
/// `max_w`; the last line is shortened in the middle if it still overflows.
fn wrap_file_name(name: &str, style: &TextStyle, max_w: f32, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    // Pieces keep their trailing separator so the name reads back exactly.
    let mut pieces = Vec::new();
    let mut start = 0;
    for (i, c) in name.char_indices() {
        if c == '_' || c == '-' {
            pieces.push(&name[start..=i]);
            start = i + 1;
        }
    }
    pieces.push(&name[start..]);
    for piece in pieces {
        let trial = format!("{cur}{piece}");
        if !cur.is_empty() && measure(&trial, style) > max_w && lines.len() + 1 < max_lines {
            lines.push(std::mem::take(&mut cur));
            cur = piece.to_string();
        } else {
            cur = trial;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if let Some(last) = lines.last_mut() {
        *last = ellipsize_middle(last, style, max_w);
    }
    lines
}

// ---------------------------------------------------------------------------
// Photo index
// ---------------------------------------------------------------------------

fn index_pages(doc: &mut Doc, r: &Report, pal: &Pal, imgs: &Images, num: &str) {
    let featured: HashMap<&Path, usize> = r.photos.iter().map(|p| (p.path.as_path(), p.number)).collect();
    let cell_w = (CW - IDX_GUTTER * (IDX_COLS as f32 - 1.0)) / IDX_COLS as f32;
    let box_h = (cell_w * 2.0 / 3.0).round();
    let pitch = box_h + 24.0;
    let name = TextStyle::new(Font::Regular, 6.0, pal.body).spacing(0.2);
    for (k, chunk) in r.index.chunks(IDX_COLS * IDX_ROWS).enumerate() {
        let pi = new_page(doc);
        let p = doc.page_mut(pi);
        let y0 = if k == 0 {
            let y = section_head(p, TOP, num, "Photo index", pal);
            let total = r.index_total.max(r.index.len());
            let note = if total > r.index.len() {
                format!("First {} of {} photographs · all are listed in Photo index.csv", r.index.len(), total)
            } else {
                format!("{total} photographs in this delivery")
            };
            p.text(W - M, y - 22.0, &note, &TextStyle::new(Font::Regular, 7.4, pal.muted).align(Align::Right));
            y
        } else {
            TOP
        };
        for (i, item) in chunk.iter().enumerate() {
            let (row, col) = (i / IDX_COLS, i % IDX_COLS);
            let x = M + col as f32 * (cell_w + IDX_GUTTER);
            let y = y0 + row as f32 * pitch;
            p.fill_rect(x, y, cell_w, box_h, pal.tint);
            match imgs.thumb.get(&item.path) {
                Some(img) => {
                    p.image_contain(img, x, y, cell_w, box_h);
                }
                None => {
                    let t = TextStyle::new(Font::Regular, 6.4, pal.muted).align(Align::Center);
                    p.text(x + cell_w / 2.0, y + box_h / 2.0 + 2.0, "No preview", &t);
                }
            }
            if let Some(&n) = featured.get(item.path.as_path()) {
                badge(p, x + 9.0, y + 9.0, 5.6, n, pal, true);
            }
            let s = ellipsize(&item.label, &name, cell_w);
            p.text(x, y + box_h + 10.0, &s, &name);
        }
    }
}

// ---------------------------------------------------------------------------
// Appendix
// ---------------------------------------------------------------------------

fn appendix(doc: &mut Doc, r: &Report, pal: &Pal, num: &str) {
    let mut pi = new_page(doc);
    let mut y = section_head(doc.page_mut(pi), TOP, num, "Capture & data integrity", pal);

    // Technical table: label column, value column, hairlines.
    let lab_w = 150.0;
    let lab = label_style(pal);
    let val = TextStyle::new(Font::Regular, 8.8, pal.ink);
    for f in &r.technical {
        let lines = wrap(&f.value, &val, CW - lab_w);
        let row_h = 10.0 + 12.0 * lines.len().max(1) as f32 + 6.0;
        if y + row_h > BOTTOM {
            pi = new_page(doc);
            y = TOP;
        }
        let p = doc.page_mut(pi);
        p.hrule(M, W - M, y, 0.5, pal.hair);
        p.text(M, y + 16.0, &ellipsize(&f.label.to_uppercase(), &lab, lab_w - 12.0), &lab);
        for (j, l) in lines.iter().enumerate() {
            p.text(M + lab_w, y + 16.5 + 12.0 * j as f32, l, &val);
        }
        y += row_h;
    }
    if !r.technical.is_empty() {
        doc.page_mut(pi).hrule(M, W - M, y, 0.5, pal.hair);
        y += 30.0;
    }

    if !r.integrity.is_empty() {
        if y + 40.0 > BOTTOM {
            pi = new_page(doc);
            y = TOP;
        }
        let k = TextStyle::new(Font::Bold, 7.0, pal.accent_deep).spacing(2.0);
        doc.page_mut(pi).text(M, y, "DATA INTEGRITY", &k);
        y += 20.0;
        let t = TextStyle::new(Font::Regular, 8.6, pal.body);
        for line in &r.integrity {
            let lines = wrap(line, &t, CW - 20.0);
            let h = 12.5 * lines.len().max(1) as f32 + 6.0;
            if y + h > BOTTOM {
                pi = new_page(doc);
                y = TOP;
            }
            let p = doc.page_mut(pi);
            check_mark(p, M, y - 6.4, pal);
            for (j, l) in lines.iter().enumerate() {
                p.text(M + 20.0, y + 12.5 * j as f32, l, &t);
            }
            y += h;
        }
        y += 20.0;
    }

    if !r.disclaimer.trim().is_empty() {
        let t = TextStyle::new(Font::Regular, 6.8, pal.muted);
        let lines = wrap(&r.disclaimer, &t, CW);
        let h = 18.0 + 9.6 * lines.len() as f32;
        // Sits at the foot of the page when there is room; otherwise flows.
        let mut top = (BOTTOM - h).max(y);
        if top + h > BOTTOM {
            pi = new_page(doc);
            top = (BOTTOM - h).max(TOP);
        }
        let p = doc.page_mut(pi);
        p.hrule(M, M + 40.0, top, 0.6, pal.accent);
        let mut ly = top + 15.0;
        for l in &lines {
            if ly > BOTTOM + 8.0 {
                break;
            }
            p.text(M, ly, l, &t);
            ly += 9.6;
        }
    }
}

/// A small gold tick in a 10-point box.
fn check_mark(p: &mut Page, x: f32, y: f32, pal: &Pal) {
    p.save();
    p.stroke_color(pal.accent);
    p.line_width(1.3);
    p.round_caps(true);
    p.polygon(&[(x + 1.0, y + 4.8), (x + 3.8, y + 7.6), (x + 9.0, y + 1.0)], false);
    p.stroke();
    p.restore();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdfkit::tests::{check_pdf, find, readable, unkern};
    use image::{Rgb, RgbImage};
    use std::fs;

    const LON: f64 = -110.01;
    const LAT: f64 = 31.91;

    fn synth(path: &Path, w: u32, h: u32, seed: u32) {
        let img = RgbImage::from_fn(w, h, |x, y| {
            let n = ((x * 7 + y * 13 + seed * 31) % 23) as u8;
            Rgb([(80 + (x * 120 / w) as u8).saturating_add(n), (90 + (y * 100 / h) as u8), ((60 + seed * 9) % 255) as u8])
        });
        fs::write(path, imaging::encode_jpeg(&img, 85).unwrap()).unwrap();
    }

    fn square(lon: f64, lat: f64, d: f64) -> Vec<(f64, f64)> {
        vec![(lon - d, lat - d), (lon + d, lat - d), (lon + d, lat + d), (lon - d, lat + d), (lon - d, lat - d)]
    }

    fn brand() -> Brand {
        Brand {
            name: "Example Aerials".into(),
            tagline: Some("Independent & insured".into()),
            url: Some("example.com".into()),
            phone: Some("(555) 010-0199".into()),
            email: Some("hello@example.com".into()),
            logo: None,
            dark: [0x0C, 0x0F, 0x0A],
            accent: [0xC9, 0xA8, 0x4C],
        }
    }

    fn report(dir: &Path, photos: usize, portrait_at: Option<usize>, index: usize) -> Report {
        let hero = dir.join("hero.jpg");
        synth(&hero, 1200, 800, 99);
        let mut pages = Vec::new();
        let mut shots = Vec::new();
        for i in 0..photos {
            let p = dir.join(format!("DJI_20260428183754_{i:04}_D.JPG"));
            if Some(i) == portrait_at { synth(&p, 400, 600, i as u32) } else { synth(&p, 600, 400, i as u32) }
            pages.push(PhotoPage {
                path: p,
                number: i + 1,
                title: format!("View {} from the north-east", i + 1),
                meta: "Captured 28 Apr 2026, 18:37 · 24 mm · f/2.8 · 1/1000 s · ISO 100".into(),
                position: "31.91012° N, 110.00988° W · 61 m above take-off".into(),
                file_name: format!("100-Example-Rd_{:04}.jpg", i + 1),
                wide: i % 3 == 0,
            });
            shots.push(MapShot { lon: LON + 0.0003 * i as f64, lat: LAT + 0.0002, number: Some(i + 1), nadir: false });
        }
        let mut idx = Vec::new();
        for i in 0..index {
            let p = dir.join(format!("idx_{i:03}.jpg"));
            synth(&p, 240, 160, i as u32);
            idx.push(IndexItem { path: p, label: format!("{:03}  DJI_20260428_{i:04}", i + 1) });
        }
        for i in 0..30 {
            let t = i as f64 / 30.0;
            shots.push(MapShot { lon: LON - 0.001 + 0.002 * t, lat: LAT - 0.0006, number: None, nadir: i % 2 == 0 });
        }
        Report {
            brand: brand(),
            kicker: "Aerial photography report".into(),
            title: "100 Example Rd, Exampleville AZ 85000".into(),
            subtitle: "APN 999-12-345A · 4.12 acres · Unincorporated Example County".into(),
            prepared_for: Some("Pat Example, Example Realty".into()),
            date_line: "28 April 2026".into(),
            hero: Some(hero),
            facts: vec![
                Fact { label: "APN".into(), value: "999-12-345A".into() },
                Fact { label: "Lot size".into(), value: "4.12 acres".into() },
                Fact { label: "Jurisdiction".into(), value: "Unincorporated Example County".into() },
                Fact { label: "Photographs".into(), value: format!("{photos} featured, {index} delivered") },
            ],
            map: MapData {
                parcels: vec![
                    MapParcel { apn: "999-12-345A".into(), rings: vec![square(LON, LAT, 0.0012)], main: true },
                    MapParcel { apn: "999-12-346".into(), rings: vec![square(LON + 0.0026, LAT, 0.0012)], main: false },
                ],
                path: (0..20).map(|i| (LON - 0.001 + 0.0001 * i as f64, LAT + 0.0004 * ((i % 4) as f64 - 1.5))).collect(),
                shots,
                footprints: vec![square(LON, LAT - 0.0006, 0.0003)],
            },
            photos: pages,
            index: idx,
            index_total: index,
            technical: vec![
                Fact { label: "Capture".into(), value: "Mavic 4 Pro, 24 mm".into() },
                Fact { label: "Coordinate system".into(), value: "WGS 84".into() },
            ],
            integrity: vec!["Every file verified with BLAKE3 after copying.".into()],
            disclaimer: "Parcel boundaries are approximate and for illustration only.".into(),
            qr_url: Some("https://example.com/r/999-12-345A".into()),
        }
    }

    /// A realistic site: a 3 × 3 block of parcels around the subject, a
    /// mapping grid with frame footprints, and an orbit of featured photos.
    fn rich_map(featured: usize) -> MapData {
        let (dx, dy) = (0.0021, 0.0018);
        let mut parcels = Vec::new();
        for (i, j) in (-1..=1).flat_map(|i| (-1..=1).map(move |j| (i, j))) {
            let (cx, cy) = (LON + i as f64 * dx, LAT + j as f64 * dy);
            let main = i == 0 && j == 0;
            let ring = if main {
                vec![
                    (cx - dx / 2.0, cy - dy / 2.0),
                    (cx + dx / 2.0, cy - dy / 2.0),
                    (cx + dx / 2.0, cy + dy * 0.18),
                    (cx + dx * 0.12, cy + dy / 2.0),
                    (cx - dx / 2.0, cy + dy / 2.0),
                    (cx - dx / 2.0, cy - dy / 2.0),
                ]
            } else {
                let (hx, hy) = (dx / 2.0, dy / 2.0);
                vec![(cx - hx, cy - hy), (cx + hx, cy - hy), (cx + hx, cy + hy), (cx - hx, cy + hy), (cx - hx, cy - hy)]
            };
            parcels.push(MapParcel { apn: format!("999-12-{:03}", 340 + (i + 1) * 3 + (j + 1)), rings: vec![ring], main });
        }
        parcels[4].apn = "999-12-345A".into();
        let mut path = Vec::new();
        let mut shots = Vec::new();
        let mut footprints = Vec::new();
        let lanes = 6;
        for l in 0..lanes {
            let y = LAT - dy * 0.42 + dy * 0.84 * l as f64 / (lanes - 1) as f64;
            let xs: Vec<f64> = (0..8).map(|k| LON - dx * 0.42 + dx * 0.84 * k as f64 / 7.0).collect();
            let xs: Vec<f64> = if l % 2 == 0 { xs } else { xs.into_iter().rev().collect() };
            for x in xs {
                path.push((x, y));
                shots.push(MapShot { lon: x, lat: y, number: None, nadir: true });
                footprints.push(vec![(x - 0.00026, y - 0.00018), (x + 0.00026, y - 0.00018), (x + 0.00026, y + 0.00018), (x - 0.00026, y + 0.00018)]);
            }
        }
        for k in 0..featured {
            let a = std::f64::consts::TAU * k as f64 / featured.max(1) as f64;
            let (x, y) = (LON + dx * 0.62 * a.cos(), LAT + dy * 0.62 * a.sin());
            path.push((x, y));
            shots.push(MapShot { lon: x, lat: y, number: Some(k + 1), nadir: false });
        }
        for k in 0..12 {
            let a = std::f64::consts::TAU * (k as f64 + 0.5) / 12.0;
            shots.push(MapShot { lon: LON + dx * 0.62 * a.cos(), lat: LAT + dy * 0.62 * a.sin(), number: None, nadir: false });
        }
        MapData { parcels, path, shots, footprints }
    }

    fn pages(pdf: &[u8]) -> usize {
        String::from_utf8_lossy(pdf).matches("/Type /Page ").count()
    }

    #[test]
    fn full_report_structure_links_and_no_aircraft_names() {
        let dir = tempfile::tempdir().unwrap();
        // 6 photos, every third wide, a portrait 4th: [W] [L L] [P] [L L]
        // = 4 pages; 25 index items = 1 page; cover, overview, appendix = 3.
        let r = report(dir.path(), 6, Some(3), 25);
        let out = dir.path().join("out/Report.pdf");
        write_report(&out, &r).unwrap();
        let pdf = fs::read(&out).unwrap();
        check_pdf(&pdf);
        assert_eq!(pages(&pdf), 8);
        assert!(String::from_utf8_lossy(&pdf).contains("/Count 8"));
        assert!(fs::read_dir(out.parent().unwrap()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("partial")));

        // Nothing names the aircraft maker or model: not in the raw bytes,
        // not in any decompressed content stream, not in the metadata.
        assert!(find(&pdf, b"DJI").is_none(), "raw bytes mention DJI");
        let text = readable(&pdf);
        let lower = text.to_lowercase();
        for w in ["dji", "mavic", "dji-offload"] {
            assert!(!lower.contains(w), "PDF mentions {w}");
        }
        assert!(!text.contains("/Producer"));
        assert!(text.contains("/Creator <FEFF"));

        // Links and QR.
        for uri in ["(https://example.com)", "(tel:5550100199)", "(mailto:hello@example.com)", "(https://example.com/r/999-12-345A)"] {
            assert!(text.contains(&format!("/URI {uri}")), "missing link {uri}");
        }
        // The QR code is many filled module rectangles on the cover.
        assert!(text.matches(" re\n").count() > 150, "QR modules missing");

        // Images: 1 hero + 6 photos + 25 thumbnails, all JPEG.
        assert_eq!(text.matches("/Subtype /Image").count(), 32);
        assert_eq!(text.matches("/DCTDecode").count(), 32);
        // Featured photo numbers appear (map badges and captions).
        assert!(text.contains("(6)"));
    }

    #[test]
    fn sparse_report_skips_sections() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = report(dir.path(), 0, None, 0);
        r.hero = None;
        r.map = MapData::default();
        r.facts.clear();
        r.technical.clear();
        r.integrity.clear();
        r.disclaimer.clear();
        r.qr_url = Some(format!("https://example.com/{}", "x".repeat(5000)));
        r.prepared_for = None;
        r.brand.tagline = None;
        r.title = "Ōkubo 漢字 Road — a very long property title that certainly will not fit on one line of the cover".into();
        let out = dir.path().join("sparse.pdf");
        write_report(&out, &r).unwrap();
        let pdf = fs::read(&out).unwrap();
        check_pdf(&pdf);
        assert_eq!(pages(&pdf), 1, "cover only");

        // A missing photo file still gets its page, with a placeholder.
        let mut r2 = report(dir.path(), 1, None, 0);
        fs::remove_file(&r2.photos[0].path).unwrap();
        r2.hero = Some(dir.path().join("missing.jpg"));
        let out2 = dir.path().join("missing.pdf");
        write_report(&out2, &r2).unwrap();
        let pdf2 = fs::read(&out2).unwrap();
        check_pdf(&pdf2);
        assert_eq!(pages(&pdf2), 4);
        assert!(unkern(&readable(&pdf2)).contains("(Preview unavailable)"));
    }

    #[test]
    fn map_projection_and_scale_bar() {
        // Two points 0.001° of longitude apart at 31.91° N are ~94.4 m apart.
        let pts = [(LON, LAT), (LON + 0.001, LAT)];
        let p = Proj::fit(&pts, [0.0, 0.0, 200.0, 100.0], 0.0, 1.0).unwrap();
        let (e, n) = p.metres(LON + 0.001, LAT);
        let (e0, _) = p.metres(LON, LAT);
        let d = e - e0;
        assert!((d - 94.39).abs() < 0.05, "{d}");
        assert!(n.abs() < 1e-9);
        // Fitted to the 200 pt width, centred on the box.
        assert!((p.scale * d - 200.0).abs() < 1e-6);
        let (x0, y0) = p.page(LON, LAT);
        let (x1, _) = p.page(LON + 0.001, LAT);
        assert!((x0 - 0.0).abs() < 1e-3 && (x1 - 200.0).abs() < 1e-3 && (y0 - 50.0).abs() < 1e-3);
        // North is up: a point to the north has a smaller page y.
        assert!(p.page(LON, LAT + 0.0001).1 < y0);
        // True aspect: 100 m east and 100 m north are the same length on paper.
        let q = Proj::fit(&[(LON, LAT), (LON + 0.002, LAT + 0.002)], [0.0, 0.0, 300.0, 300.0], 10.0, 1.0).unwrap();
        let (a, _) = q.page(LON, LAT);
        let (b, _) = q.page(LON + 100.0 / q.mx, LAT);
        let (_, c) = q.page(LON, LAT);
        let (_, d2) = q.page(LON, LAT + 100.0 / q.my);
        assert!(((b - a) - (c - d2)).abs() < 1e-3);
        // Degenerate input gets a minimum extent instead of infinite scale.
        let one = Proj::fit(&[(LON, LAT)], [0.0, 0.0, 100.0, 100.0], 0.0, 50.0).unwrap();
        assert!((one.scale - 2.0).abs() < 1e-9);
        assert!(Proj::fit(&[], [0.0, 0.0, 1.0, 1.0], 0.0, 1.0).is_none());

        for (x, want) in [(137.0, 100.0), (0.3, 0.2), (4999.0, 2000.0), (5000.0, 5000.0), (1.0, 1.0), (19.9, 10.0), (20.0, 20.0)] {
            assert_eq!(nice_floor(x), want, "nice_floor({x})");
        }
        // 1 pt per metre, at most 120 pt: 100 m (100 pt) and 200 ft (60.96 pt).
        let b = scale_bar(1.0, 120.0);
        assert_eq!((b.metres, b.feet), (100.0, 200.0));
        assert!((b.metres_pt - 100.0).abs() < 1e-4 && (b.feet_pt - 60.96).abs() < 1e-3);
        assert_eq!(len_label(2000.0, "m"), "2 km");
        assert_eq!(len_label(25.0, ""), "25");
        assert_eq!(len_label(2.5, "ft"), "2.5 ft");
    }

    #[test]
    fn clipping_and_detail_wrapping() {
        // A square half outside the box is cut at the box edge.
        let sq = [(-50.0, 10.0), (50.0, 10.0), (50.0, 60.0), (-50.0, 60.0)];
        let c = clip_ring(&sq, [0.0, 0.0, 100.0, 100.0]);
        assert!(c.iter().all(|p| p.0 >= 0.0 && p.0 <= 50.0));
        let (cx, cy) = centroid(&c);
        assert!((cx - 25.0).abs() < 1e-3 && (cy - 35.0).abs() < 1e-3, "{cx} {cy}");
        assert!(clip_ring(&sq, [200.0, 200.0, 10.0, 10.0]).is_empty());

        let st = TextStyle::new(Font::Regular, 7.8, Color::BLACK);
        let lines = wrap_parts("Captured 28 Apr 2026, 18:37 · 24 mm · f/2.8 · 1/1000 s · ISO 100", &st, 128.0, 4);
        assert_eq!(lines, vec!["Captured 28 Apr 2026, 18:37", "24 mm · f/2.8 · 1/1000 s · ISO 100"]);
    }

    #[test]
    fn scrub_removes_aircraft_names() {
        assert_eq!(scrub("DJI_20260428183754_0217_D.JPG"), "20260428183754_0217_D.JPG");
        assert_eq!(scrub("Shot on a DJI Mavic 4 Pro, 24 mm"), "Shot on a 4 Pro, 24 mm");
        assert_eq!(scrub("dji-offload"), "offload");
        // Words that merely contain the letters are kept.
        assert_eq!(scrub("Adji Road · Mavica Ln"), "Adji Road · Mavica Ln");
        assert_eq!(scrub("Example Rd"), "Example Rd");
    }

    /// Real-photo build, never committed:
    /// `REPORT_PDF_PHOTOS=/dir/of/jpgs [REPORT_PDF_LOGO=logo.png] [REPORT_PDF_OUT=out.pdf]
    ///  cargo test --release bench_report -- --ignored --nocapture`
    /// Builds a 24-photo report with a 60-thumbnail index (cycling through
    /// the photos found) and prints size and timing.
    #[test]
    #[ignore]
    fn bench_report() {
        let Ok(src) = std::env::var("REPORT_PDF_PHOTOS") else { return };
        let mut photos: Vec<PathBuf> = fs::read_dir(&src)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg")))
            .collect();
        photos.sort();
        assert!(!photos.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let mut r = report(dir.path(), 24, Some(5), 60);
        r.brand = Brand {
            name: std::env::var("REPORT_PDF_BRAND").unwrap_or_else(|_| "Example Aerials".into()),
            tagline: std::env::var("REPORT_PDF_TAGLINE").ok(),
            logo: std::env::var("REPORT_PDF_LOGO").ok().map(PathBuf::from),
            ..brand()
        };
        r.hero = Some(photos[photos.len() - 1].clone());
        r.map = rich_map(24);
        for (i, p) in r.photos.iter_mut().enumerate() {
            if i != 5 {
                p.path = photos[i % photos.len()].clone();
            }
        }
        for (i, it) in r.index.iter_mut().enumerate() {
            it.path = photos[i % photos.len()].clone();
        }
        if std::env::var("REPORT_PDF_VARIANT").is_ok_and(|v| v == "sparse") {
            r.hero = None;
            r.brand.logo = None;
            r.brand.name = "Northfield Precision Aerial Imaging".into();
            r.title = "Lot 14, 12500 East Very Long Ranch Road Name, Exampleville Junction AZ 85000".into();
            r.photos.truncate(3);
            r.index.truncate(5);
            r.map.footprints.clear();
            r.facts.truncate(3);
        }
        let out = std::env::var("REPORT_PDF_OUT").map(PathBuf::from).unwrap_or_else(|_| dir.path().join("bench.pdf"));
        let t = std::time::Instant::now();
        write_report(&out, &r).unwrap();
        let el = t.elapsed().as_secs_f64();
        let size = fs::metadata(&out).unwrap().len();
        println!(
            "report: 24 photos + 60 thumbs on {} threads: {el:.2} s, {:.2} MB",
            std::thread::available_parallelism().map_or(1, |c| c.get()),
            size as f64 / 1e6
        );
    }
}
