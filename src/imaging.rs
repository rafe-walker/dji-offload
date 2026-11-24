//! Photo work for deliveries: a fast blur/exposure check run right after an
//! offload, web-size copies and watermarked samples.
//!
//! Everything is pure Rust. JPEGs are decoded with `jpeg-decoder`, which can
//! decode at 1/2, 1/4 or 1/8 scale straight from the DCT coefficients; that
//! is what makes the field check fast. Resizing and JPEG encoding use
//! `image`, watermark text is drawn with `ab_glyph` and a bundled subset of
//! Roboto Bold, and the PDF is written directly (base-14 Helvetica, thumbnails
//! embedded as JPEG), so no system libraries or font files are needed.
//!
//! Every JPEG written here is re-encoded from pixels: EXIF (including GPS),
//! XMP, maker notes and the DJI debug segments are never copied across.

use std::fs;
use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use anyhow::{Context, Result, anyhow, bail};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};
use image::{Rgb, RgbImage};

/// Roboto Bold (Apache-2.0), subset to Latin-1 plus common punctuation.
/// See `assets/LICENSE-Roboto.txt` and NOTICE.
const FONT: &[u8] = include_bytes!("../assets/Roboto-Bold-subset.ttf");

// ---------------------------------------------------------------------------
// Quality check
// ---------------------------------------------------------------------------

/// The field check decodes so that the long edge is at least this many
/// pixels: 1/4 scale for 20–25 MP frames (6144 → 1536), 1/8 for 100 MP.
const CHECK_LONG_EDGE: u32 = 1000;
/// "soft" below this fraction of the flight's median sharpness...
const SOFT_RELATIVE: f64 = 0.35;
/// ...or below this absolute Laplacian variance (at check scale).
pub const SOFT_FLOOR: f64 = 25.0;
/// Relative sharpness needs at least this many measured photos.
const MIN_FOR_RELATIVE: usize = 5;
/// "overexposed" when more than this fraction of pixels has luma >= 250.
const OVER_CLIPPED: f64 = 0.08;
/// "underexposed" when mean luma is below this AND more than
/// `UNDER_CLIPPED` of pixels have luma <= 5.
const UNDER_MEAN: f64 = 40.0;
const UNDER_CLIPPED: f64 = 0.10;

#[derive(Debug, Clone)]
pub struct Quality {
    /// Variance of the 4-neighbour Laplacian of luma (0–255 scale), measured
    /// at the reduced check scale. Higher is sharper.
    pub sharpness: f64,
    /// Fraction (0–1) of pixels with luma >= 250.
    pub clipped_high: f64,
    /// Fraction (0–1) of pixels with luma <= 5.
    pub clipped_low: f64,
    /// Mean luma, 0–255.
    pub mean_luma: f64,
}

#[derive(Debug, Clone)]
pub struct Flagged {
    pub path: PathBuf,
    /// Any of "soft", "overexposed", "underexposed", or "unreadable" when
    /// a `.JPG` could not be decoded (e.g. truncated on the drone).
    pub reasons: Vec<String>,
}

/// Measure sharpness and exposure of one JPEG, decoded at reduced scale.
pub fn quality(path: &Path) -> Result<Quality> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let raw = decode(&bytes, CHECK_LONG_EDGE).with_context(|| format!("decoding {}", path.display()))?;
    let luma = raw.luma();
    Ok(measure(&luma, raw.w as usize, raw.h as usize))
}

/// Check every JPEG in `paths` (one flight) and return the ones worth a look,
/// in input order. Non-JPEG files (DNG, MP4, SRT, …) are skipped. Files are
/// processed in parallel.
pub fn assess(paths: &[PathBuf]) -> Vec<Flagged> {
    let jpegs: Vec<&PathBuf> = paths.iter().filter(|p| is_jpeg_name(p)).collect();
    let results = par_map(&jpegs, |p| {
        catch_unwind(AssertUnwindSafe(|| quality(p))).unwrap_or_else(|_| Err(anyhow!("decoder panicked")))
    });

    let sharp: Vec<f64> = results.iter().filter_map(|r| r.as_ref().ok().map(|q| q.sharpness)).collect();
    let soft_below =
        if sharp.len() >= MIN_FOR_RELATIVE { (median(&sharp) * SOFT_RELATIVE).max(SOFT_FLOOR) } else { SOFT_FLOOR };

    let mut out = Vec::new();
    for (path, r) in jpegs.into_iter().zip(results) {
        let reasons = match r {
            Ok(q) => reasons(&q, soft_below),
            Err(_) => vec!["unreadable".to_string()],
        };
        if !reasons.is_empty() {
            out.push(Flagged { path: path.clone(), reasons });
        }
    }
    out
}

fn reasons(q: &Quality, soft_below: f64) -> Vec<String> {
    let mut r = Vec::new();
    if q.sharpness < soft_below {
        r.push("soft".to_string());
    }
    if q.clipped_high > OVER_CLIPPED {
        r.push("overexposed".to_string());
    }
    if q.mean_luma < UNDER_MEAN && q.clipped_low > UNDER_CLIPPED {
        r.push("underexposed".to_string());
    }
    r
}

fn measure(y: &[u8], w: usize, h: usize) -> Quality {
    let n = y.len().max(1) as f64;
    let (mut hi, mut lo, mut sum) = (0u64, 0u64, 0u64);
    for &v in y {
        sum += v as u64;
        hi += (v >= 250) as u64;
        lo += (v <= 5) as u64;
    }

    let mut sharpness = 0.0;
    if w >= 3 && h >= 3 {
        let (mut s, mut s2) = (0i64, 0i64);
        for row in 1..h - 1 {
            let up = &y[(row - 1) * w..row * w];
            let mid = &y[row * w..(row + 1) * w];
            let dn = &y[(row + 1) * w..(row + 2) * w];
            let (mut rs, mut rs2) = (0i64, 0i64);
            for x in 1..w - 1 {
                let l = 4 * mid[x] as i32 - mid[x - 1] as i32 - mid[x + 1] as i32 - up[x] as i32 - dn[x] as i32;
                rs += l as i64;
                rs2 += (l * l) as i64;
            }
            s += rs;
            s2 += rs2;
        }
        let cnt = ((w - 2) * (h - 2)) as f64;
        let mean = s as f64 / cnt;
        sharpness = (s2 as f64 / cnt - mean * mean).max(0.0);
    }

    Quality { sharpness, clipped_high: hi as f64 / n, clipped_low: lo as f64 / n, mean_luma: sum as f64 / n }
}

fn median(v: &[f64]) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 }
}

fn is_jpeg_name(p: &Path) -> bool {
    p.extension().map(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg")).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

struct Raw {
    w: u32,
    h: u32,
    /// 1 (grey) or 3 (RGB) bytes per pixel.
    channels: usize,
    data: Vec<u8>,
    exif: Option<Vec<u8>>,
    icc: Option<Vec<u8>>,
}

impl Raw {
    fn luma(&self) -> Vec<u8> {
        if self.channels == 1 {
            return self.data.clone();
        }
        self.data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| ((p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29 + 128) >> 8) as u8)
            .collect()
    }
}

/// Decode a JPEG at the cheapest DCT scale (1, 1/2, 1/4, 1/8) whose long
/// edge is still at least `min_long` pixels.
fn decode(bytes: &[u8], min_long: u32) -> Result<Raw> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        bail!("not a JPEG file");
    }
    let mut d = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    d.read_info()?;
    let info = d.info().context("JPEG has no frame header")?;
    let want = min_long.clamp(1, u16::MAX as u32) as u16;
    if u32::from(info.width.max(info.height)) > min_long {
        // `scale` stops at the first factor that satisfies EITHER axis, so the
        // short axis gets an unreachable request.
        if info.width >= info.height {
            d.scale(want, u16::MAX)?;
        } else {
            d.scale(u16::MAX, want)?;
        }
    }
    let data = d.decode()?;
    let info = d.info().context("JPEG has no frame header")?;
    let channels = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => 1,
        jpeg_decoder::PixelFormat::RGB24 => 3,
        other => bail!("unsupported JPEG pixel format {other:?}"),
    };
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    if data.len() != (w * h) as usize * channels {
        bail!("JPEG decoded to an unexpected size");
    }
    Ok(Raw { w, h, channels, data, exif: d.exif_data().map(<[u8]>::to_vec), icc: d.icc_profile() })
}

/// Decode `src` with its long edge at least `min_long` (or full size),
/// convert to sRGB, and apply the EXIF orientation.
pub(crate) fn load_rgb(src: &Path, min_long: u32) -> Result<RgbImage> {
    let bytes = fs::read(src).with_context(|| format!("reading {}", src.display()))?;
    let raw = decode(&bytes, min_long).with_context(|| format!("decoding {}", src.display()))?;
    let orientation = raw.exif.as_deref().map(exif_orientation).unwrap_or(1);
    let mut rgb = if raw.channels == 3 { raw.data } else { raw.data.iter().flat_map(|&v| [v, v, v]).collect() };
    if raw.channels == 3
        && let Some(icc) = raw.icc.as_deref()
    {
        to_srgb(icc, &mut rgb);
    }
    let img = RgbImage::from_raw(raw.w, raw.h, rgb).context("image buffer size")?;
    Ok(orient(img, orientation))
}

fn exif_orientation(tiff: &[u8]) -> u32 {
    exif::Reader::new()
        .read_raw(tiff.to_vec())
        .ok()
        .and_then(|e| e.get_field(exif::Tag::Orientation, exif::In::PRIMARY).and_then(|f| f.value.get_uint(0)))
        .unwrap_or(1)
}

fn orient(img: RgbImage, o: u32) -> RgbImage {
    match o {
        2 => imageops::flip_horizontal(&img),
        3 => imageops::rotate180(&img),
        4 => imageops::flip_vertical(&img),
        5 => imageops::flip_horizontal(&imageops::rotate90(&img)),
        6 => imageops::rotate90(&img),
        7 => imageops::flip_horizontal(&imageops::rotate270(&img)),
        8 => imageops::rotate270(&img),
        _ => img,
    }
}

/// Convert RGB pixels tagged with an ICC profile to sRGB. An unreadable
/// profile is ignored (the pixels are then treated as sRGB already).
fn to_srgb(icc: &[u8], rgb: &mut [u8]) {
    let Ok(src) = moxcms::ColorProfile::new_from_slice(icc) else { return };
    let dst = moxcms::ColorProfile::new_srgb();
    let Ok(t) = src.create_transform_8bit(moxcms::Layout::Rgb, &dst, moxcms::Layout::Rgb, Default::default()) else {
        return;
    };
    let mut out = vec![0u8; rgb.len()];
    if t.transform(rgb, &mut out).is_ok() {
        rgb.copy_from_slice(&out);
    }
}

/// Resize so the long edge is `long_edge`; never upscales.
pub(crate) fn fit_long_edge(img: RgbImage, long_edge: u32) -> RgbImage {
    let (w, h) = img.dimensions();
    if w.max(h) <= long_edge {
        return img;
    }
    let s = long_edge as f64 / w.max(h) as f64;
    let (nw, nh) = if w >= h {
        (long_edge, ((h as f64 * s).round() as u32).max(1))
    } else {
        (((w as f64 * s).round() as u32).max(1), long_edge)
    };
    imageops::resize(&img, nw, nh, FilterType::Lanczos3)
}

pub(crate) fn encode_jpeg(img: &RgbImage, quality_pct: u8) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity((img.width() * img.height()) as usize / 4);
    JpegEncoder::new_with_quality(&mut buf, quality_pct.clamp(1, 100)).encode_image(img)?;
    Ok(buf)
}

/// Write via a hidden `.partial` file and rename, so a crash never leaves a
/// half-written deliverable under the real name.
pub(crate) fn write_atomic(dst: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dst.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let name = dst.file_name().context("destination has no file name")?.to_string_lossy();
    let tmp = dst.with_file_name(format!(".{name}.partial"));
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, dst).with_context(|| format!("renaming into {}", dst.display()))?;
    Ok(())
}

/// Refuse to overwrite the original.
fn ensure_distinct(src: &Path, dst: &Path) -> Result<()> {
    if let (Ok(a), Ok(b)) = (fs::canonicalize(src), fs::canonicalize(dst))
        && a == b
    {
        bail!("refusing to overwrite the original {}", src.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Web copy and watermark
// ---------------------------------------------------------------------------

/// Write a web-size sRGB JPEG: long edge `long_edge` (aspect kept, never
/// upscaled), Lanczos3, quality `quality_pct`, no metadata (no GPS).
pub fn web_copy(src: &Path, dst: &Path, long_edge: u32, quality_pct: u8) -> Result<()> {
    if long_edge == 0 {
        bail!("long_edge must be > 0");
    }
    ensure_distinct(src, dst)?;
    let img = fit_long_edge(load_rgb(src, long_edge)?, long_edge);
    write_atomic(dst, &encode_jpeg(&img, quality_pct)?)
}

/// JPEG quality used for watermarked samples.
const WATERMARK_QUALITY: u8 = 82;

/// Write a watermarked sample: downscaled to `long_edge`, `text` tiled
/// diagonally across the whole frame plus a solid band along the bottom,
/// no metadata.
pub fn watermark(src: &Path, dst: &Path, long_edge: u32, text: &str) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        bail!("watermark text is empty");
    }
    if long_edge == 0 {
        bail!("long_edge must be > 0");
    }
    ensure_distinct(src, dst)?;
    let mut img = fit_long_edge(load_rgb(src, long_edge)?, long_edge);
    stamp(&mut img, text);
    write_atomic(dst, &encode_jpeg(&img, WATERMARK_QUALITY)?)
}

/// Anti-aliased coverage mask (0–1) of one line of text.
struct Mask {
    w: usize,
    h: usize,
    a: Vec<f32>,
}

impl Mask {
    fn render(font: &FontRef, text: &str, px: f32) -> Mask {
        let scale = PxScale::from(px);
        let sf = font.as_scaled(scale);
        let pad = 2.0;
        let fallback = font.glyph_id('?');
        let mut x = pad;
        let mut prev = None;
        let mut glyphs = Vec::new();
        for c in text.chars() {
            let c = if c.is_control() { ' ' } else { c };
            let mut id = font.glyph_id(c);
            if id.0 == 0 {
                id = fallback;
            }
            if let Some(p) = prev {
                x += sf.kern(p, id);
            }
            glyphs.push(id.with_scale_and_position(scale, point(x, pad + sf.ascent())));
            x += sf.h_advance(id);
            prev = Some(id);
        }
        let w = (x + pad).ceil().max(1.0) as usize;
        let h = (sf.ascent() - sf.descent() + 2.0 * pad).ceil().max(1.0) as usize;
        let mut a = vec![0f32; w * h];
        for g in glyphs {
            let Some(og) = font.outline_glyph(g) else { continue };
            let b = og.px_bounds();
            og.draw(|gx, gy, c| {
                let px = b.min.x as i64 + gx as i64;
                let py = b.min.y as i64 + gy as i64;
                if px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h {
                    let i = py as usize * w + px as usize;
                    a[i] = (a[i] + c).min(1.0);
                }
            });
        }
        Mask { w, h, a }
    }

    /// Bilinear sample; 0 outside the mask.
    fn sample(&self, x: f32, y: f32) -> f32 {
        let (x, y) = (x - 0.5, y - 0.5);
        if x <= -1.0 || y <= -1.0 || x >= self.w as f32 || y >= self.h as f32 {
            return 0.0;
        }
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let at = |xi: i64, yi: i64| -> f32 {
            if xi < 0 || yi < 0 || xi as usize >= self.w || yi as usize >= self.h {
                0.0
            } else {
                self.a[yi as usize * self.w + xi as usize]
            }
        };
        let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1, y0) * fx;
        let bot = at(x0, y0 + 1) * (1.0 - fx) + at(x0 + 1, y0 + 1) * fx;
        top * (1.0 - fy) + bot * fy
    }
}

fn blend(p: &mut Rgb<u8>, c: [f32; 3], a: f32) {
    if a <= 0.0 {
        return;
    }
    for (ch, cv) in p.0.iter_mut().zip(c) {
        *ch = (*ch as f32 * (1.0 - a) + cv * a).round().clamp(0.0, 255.0) as u8;
    }
}

// Watermark look. Tuned on 1600 px real-estate aerials: readable over both
// sky and dark ground, without hiding the photo.
const TILE_TEXT_SIZE: f32 = 0.040; // × long edge
const TILE_ANGLE_DEG: f32 = -30.0; // rising left to right
const TILE_TEXT_ALPHA: f32 = 0.38; // white text
const TILE_SHADOW_ALPHA: f32 = 0.30; // dark offset copy, so it shows on white sky
const BAND_HEIGHT: f32 = 0.075; // × image height
const BAND_ALPHA: f32 = 0.78;

fn stamp(img: &mut RgbImage, text: &str) {
    let font = FontRef::try_from_slice(FONT).expect("bundled font is valid");
    let (w, h) = img.dimensions();
    let long = w.max(h) as f32;

    // Diagonal tiles over the whole frame, alternate rows staggered by half
    // a period so there is no clean strip to crop between them.
    let size = (long * TILE_TEXT_SIZE).max(10.0);
    let tile = Mask::render(&font, text, size);
    let pitch_x = tile.w as f32 + size * 2.5;
    let pitch_y = size * 3.4;
    let y_off = (pitch_y - tile.h as f32) / 2.0;
    let shadow = (size / 16.0).max(1.0);
    let (sin, cos) = TILE_ANGLE_DEG.to_radians().sin_cos();
    for (x, y, p) in img.enumerate_pixels_mut() {
        let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
        let u = fx * cos + fy * sin;
        let v = -fx * sin + fy * cos;
        let row = (v / pitch_y).floor();
        let mu = (u + row * pitch_x * 0.5).rem_euclid(pitch_x);
        let mv = v.rem_euclid(pitch_y) - y_off;
        let s = tile.sample(mu - shadow, mv - shadow);
        blend(p, [0.0; 3], s * TILE_SHADOW_ALPHA);
        let a = tile.sample(mu, mv);
        blend(p, [255.0; 3], a * TILE_TEXT_ALPHA);
    }

    // Solid band along the bottom with the text centred.
    let band_h = (h as f32 * BAND_HEIGHT).max(18.0).min(h as f32).round() as u32;
    let mut bsize = band_h as f32 * 0.50;
    let mut label = Mask::render(&font, text, bsize);
    let max_w = w as f32 * 0.92;
    if label.w as f32 > max_w {
        bsize *= max_w / label.w as f32;
        label = Mask::render(&font, text, bsize);
    }
    let band_top = h - band_h;
    let lx = (w as f32 - label.w as f32) / 2.0;
    let ly = band_top as f32 + (band_h as f32 - label.h as f32) / 2.0;
    for y in band_top..h {
        for x in 0..w {
            let p = img.get_pixel_mut(x, y);
            blend(p, [12.0, 12.0, 12.0], BAND_ALPHA);
            let a = label.sample(x as f32 + 0.5 - lx, y as f32 + 0.5 - ly);
            blend(p, [255.0; 3], a * 0.95);
        }
    }
}

// ---------------------------------------------------------------------------
// Parallel map
// ---------------------------------------------------------------------------

/// Map `f` over `items` on all cores, keeping order.
pub(crate) fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let threads = std::thread::available_parallelism().map_or(4, |c| c.get()).min(n);
    let next = AtomicUsize::new(0);
    let done: Vec<(usize, R)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut got = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        got.push((i, f(&items[i])));
                    }
                    got
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().expect("imaging worker panicked")).collect()
    });
    let mut out: Vec<Option<R>> = (0..n).map(|_| None).collect();
    for (i, r) in done {
        out[i] = Some(r);
    }
    out.into_iter().map(|r| r.expect("every item processed")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    fn save_jpeg(path: &Path, img: &RgbImage, q: u8) {
        fs::write(path, encode_jpeg(img, q).unwrap()).unwrap();
    }

    /// High-contrast checkerboard with a little deterministic texture.
    fn checker(w: u32, h: u32, sq: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| {
            let on = ((x / sq) + (y / sq)).is_multiple_of(2);
            let n = ((x * 7 + y * 13) % 17) as u8;
            if on { Rgb([220 - n, 210 - n, 200 - n]) } else { Rgb([30 + n, 40 + n, 50 + n]) }
        })
    }

    fn flat(w: u32, h: u32, v: u8) -> RgbImage {
        RgbImage::from_pixel(w, h, Rgb([v, v, v]))
    }

    /// Segment markers between SOI and SOS.
    fn markers(j: &[u8]) -> Vec<u8> {
        assert_eq!(&j[..2], &[0xFF, 0xD8], "not a JPEG");
        let mut i = 2;
        let mut out = Vec::new();
        while i + 4 <= j.len() && j[i] == 0xFF {
            let m = j[i + 1];
            out.push(m);
            if m == 0xDA {
                break;
            }
            i += 2 + u16::from_be_bytes([j[i + 2], j[i + 3]]) as usize;
        }
        out
    }

    /// Insert an EXIF APP1 (big-endian TIFF, IFD0 = Orientation + a fake
    /// GPS-ish ASCII tag) right after SOI.
    fn with_exif(jpeg: &[u8], orientation: u16) -> Vec<u8> {
        let mut tiff = b"MM\x00\x2a\x00\x00\x00\x08".to_vec();
        tiff.extend_from_slice(&2u16.to_be_bytes());
        tiff.extend_from_slice(&[0x01, 0x12, 0x00, 0x03, 0, 0, 0, 1]);
        tiff.extend_from_slice(&orientation.to_be_bytes());
        tiff.extend_from_slice(&[0, 0]);
        // ImageDescription, ASCII, 4 bytes inline: "GPS"
        tiff.extend_from_slice(&[0x01, 0x0E, 0x00, 0x02, 0, 0, 0, 4, b'G', b'P', b'S', 0]);
        tiff.extend_from_slice(&[0, 0, 0, 0]);
        let mut app1 = b"Exif\x00\x00".to_vec();
        app1.extend_from_slice(&tiff);
        let mut out = jpeg[..2].to_vec();
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    fn dims(p: &Path) -> (u32, u32) {
        let r = decode(&fs::read(p).unwrap(), u32::MAX).unwrap();
        (r.w, r.h)
    }

    #[test]
    fn sharp_beats_blurred() {
        let dir = tempfile::tempdir().unwrap();
        let sharp = checker(2400, 1600, 12);
        let blurred = imageops::blur(&sharp, 8.0);
        let (ps, pb) = (dir.path().join("sharp.jpg"), dir.path().join("blur.jpg"));
        save_jpeg(&ps, &sharp, 92);
        save_jpeg(&pb, &blurred, 92);
        let (qs, qb) = (quality(&ps).unwrap(), quality(&pb).unwrap());
        assert!(qs.sharpness > SOFT_FLOOR * 10.0, "sharp {qs:?}");
        assert!(qb.sharpness < SOFT_FLOOR, "blurred {qb:?}");
        assert!(qs.sharpness > qb.sharpness * 20.0);

        let flagged = assess(&[ps.clone(), pb.clone()]);
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].path, pb);
        assert_eq!(flagged[0].reasons, vec!["soft"]);
    }

    #[test]
    fn relative_rule_needs_five_photos() {
        let dir = tempfile::tempdir().unwrap();
        let sharp = checker(1600, 1200, 10);
        // Softened, but still above the absolute floor.
        let soft = imageops::blur(&sharp, 1.6);
        let mut paths = Vec::new();
        for i in 0..4 {
            let p = dir.path().join(format!("s{i}.JPG"));
            save_jpeg(&p, &sharp, 92);
            paths.push(p);
        }
        let ps = dir.path().join("soft.JPG");
        save_jpeg(&ps, &soft, 92);
        let (qs, qm) = (quality(&paths[0]).unwrap(), quality(&ps).unwrap());
        assert!(qm.sharpness > SOFT_FLOOR, "test image must clear the floor: {qm:?}");
        assert!(qm.sharpness < qs.sharpness * SOFT_RELATIVE, "{qm:?} vs {qs:?}");

        // 3 sharp + soft = 4 photos: absolute floor only, nothing flagged.
        let few: Vec<PathBuf> = paths[..3].iter().cloned().chain([ps.clone()]).collect();
        assert!(assess(&few).is_empty());

        // 4 sharp + soft = 5 photos: relative rule applies.
        paths.push(ps.clone());
        let f = assess(&paths);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].path, ps);
        assert_eq!(f[0].reasons, vec!["soft"]);
    }

    #[test]
    fn exposure_flags() {
        let dir = tempfile::tempdir().unwrap();
        let white = dir.path().join("white.jpg");
        let grey = dir.path().join("grey.jpg");
        let black = dir.path().join("black.jpg");
        save_jpeg(&white, &flat(1200, 800, 255), 90);
        save_jpeg(&grey, &flat(1200, 800, 128), 90);
        save_jpeg(&black, &flat(1200, 800, 2), 90);

        let qw = quality(&white).unwrap();
        assert!(qw.clipped_high > 0.99 && qw.mean_luma > 250.0, "{qw:?}");
        let qg = quality(&grey).unwrap();
        assert!(qg.clipped_high == 0.0 && qg.clipped_low == 0.0, "{qg:?}");
        assert!((qg.mean_luma - 128.0).abs() < 3.0, "{qg:?}");

        let f = assess(&[white.clone(), grey.clone(), black.clone()]);
        let get = |p: &Path| f.iter().find(|x| x.path == p).map(|x| x.reasons.clone()).unwrap_or_default();
        assert!(get(&white).contains(&"overexposed".to_string()));
        assert!(!get(&white).contains(&"underexposed".to_string()));
        assert!(get(&black).contains(&"underexposed".to_string()));
        // Flat frames have no detail, so they are also "soft"; grey is not
        // mis-exposed.
        assert_eq!(get(&grey), vec!["soft"]);
    }

    #[test]
    fn non_jpegs_skipped_and_broken_jpegs_reported() {
        let dir = tempfile::tempdir().unwrap();
        let dng = dir.path().join("DJI_0001.DNG");
        let mp4 = dir.path().join("DJI_0002.MP4");
        let srt = dir.path().join("DJI_0002.SRT");
        for p in [&dng, &mp4, &srt] {
            fs::write(p, b"not an image").unwrap();
        }
        let bad = dir.path().join("DJI_0003.JPG");
        fs::write(&bad, b"\xFF\xD8\xFF\xE0 truncated").unwrap();
        let f = assess(&[dng, mp4, srt, bad.clone()]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].path, bad);
        assert_eq!(f[0].reasons, vec!["unreadable"]);
        assert!(assess(&[]).is_empty());
    }

    #[test]
    fn web_copy_resizes_and_strips_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.JPG");
        fs::write(&src, with_exif(&encode_jpeg(&checker(3000, 2000, 16), 90).unwrap(), 1)).unwrap();
        assert!(markers(&fs::read(&src).unwrap()).contains(&0xE1));

        let dst = dir.path().join("web/web.jpg");
        web_copy(&src, &dst, 1000, 85).unwrap();
        assert_eq!(dims(&dst), (1000, 667));
        let m = markers(&fs::read(&dst).unwrap());
        assert!(!m.contains(&0xE1), "APP1 present: {m:x?}");

        // Never upscale.
        let big = dir.path().join("big.jpg");
        web_copy(&src, &big, 5000, 85).unwrap();
        assert_eq!(dims(&big), (3000, 2000));

        // Refuse to overwrite the original.
        assert!(web_copy(&src, &src, 1000, 85).is_err());
        assert!(
            fs::read_dir(dir.path()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".partial"))
        );
    }

    #[test]
    fn web_copy_applies_orientation() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("rot.jpg");
        fs::write(&src, with_exif(&encode_jpeg(&checker(600, 400, 10), 90).unwrap(), 6)).unwrap();
        let dst = dir.path().join("rot-web.jpg");
        web_copy(&src, &dst, 300, 85).unwrap();
        assert_eq!(dims(&dst), (200, 300));
    }

    #[test]
    fn grey_jpeg_input_works() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("grey.jpg");
        let img = image::GrayImage::from_fn(800, 600, |x, y| Luma([((x ^ y) & 0xFF) as u8]));
        let mut buf = Vec::new();
        JpegEncoder::new_with_quality(&mut buf, 90).encode_image(&img).unwrap();
        fs::write(&src, buf).unwrap();
        assert!(quality(&src).unwrap().sharpness > 0.0);
        let dst = dir.path().join("grey-web.jpg");
        web_copy(&src, &dst, 400, 80).unwrap();
        assert_eq!(dims(&dst), (400, 300));
    }

    #[test]
    fn watermark_dims_metadata_and_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jpg");
        fs::write(&src, with_exif(&encode_jpeg(&flat(3000, 2000, 128), 92).unwrap(), 1)).unwrap();
        let dst = dir.path().join("sample.jpg");
        watermark(&src, &dst, 1600, "© Example Aerials — SAMPLE").unwrap();
        let bytes = fs::read(&dst).unwrap();
        assert!(!markers(&bytes).contains(&0xE1));
        let raw = decode(&bytes, u32::MAX).unwrap();
        assert_eq!((raw.w, raw.h), (1600, 1067));
        let l = raw.luma();
        let w = raw.w as usize;

        // Bottom band: dark.
        let band: Vec<u8> = l[(raw.h as usize - 20) * w..].to_vec();
        let band_mean = band.iter().map(|&v| v as f64).sum::<f64>() / band.len() as f64;
        assert!(band_mean < 90.0, "band mean {band_mean}");

        // Tiled text reaches every quarter of the frame above the band.
        let h_top = (raw.h as usize * 9) / 10;
        for (qx, qy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let mut changed = 0;
            let mut total = 0;
            for y in (qy * h_top / 2)..((qy + 1) * h_top / 2) {
                for x in (qx * w / 2)..((qx + 1) * w / 2) {
                    total += 1;
                    changed += ((l[y * w + x] as i32 - 128).abs() > 12) as usize;
                }
            }
            let frac = changed as f64 / total as f64;
            assert!(frac > 0.03, "quarter ({qx},{qy}) only {frac:.3} covered");
        }

        assert!(watermark(&src, &dst, 1600, "   ").is_err());
    }

    #[test]
    fn median_helper() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
    }

    /// Timing on real photos (never committed):
    /// `DJI_IMAGING_BENCH=/path/to/flight cargo test --release bench_real -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_real() {
        let Ok(dir) = std::env::var("DJI_IMAGING_BENCH") else { return };
        let mut photos: Vec<PathBuf> =
            fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).filter(|p| is_jpeg_name(p)).collect();
        photos.sort();
        let n = photos.len();
        assert!(n > 0, "no JPEGs in {dir}");
        let threads = std::thread::available_parallelism().map_or(1, |c| c.get());

        let t = std::time::Instant::now();
        let q = quality(&photos[0]).unwrap();
        let one = t.elapsed();
        println!(
            "quality, 1 photo single-threaded: {:.0} ms (sharpness {:.1}, mean luma {:.0})",
            one.as_secs_f64() * 1e3,
            q.sharpness,
            q.mean_luma
        );

        let t = std::time::Instant::now();
        let flagged = assess(&photos);
        let el = t.elapsed().as_secs_f64();
        println!(
            "assess: {n} photos on {threads} threads in {el:.2} s = {:.1} ms/photo wall; {} flagged",
            el * 1e3 / n as f64,
            flagged.len()
        );

        let out = tempfile::tempdir().unwrap();
        let t = std::time::Instant::now();
        web_copy(&photos[0], &out.path().join("w.jpg"), 2048, 85).unwrap();
        println!("web_copy 2048: {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
        let t = std::time::Instant::now();
        watermark(&photos[0], &out.path().join("m.jpg"), 1600, "© Sample — not for use").unwrap();
        println!("watermark 1600: {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
    }
}
