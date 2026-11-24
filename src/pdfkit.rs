//! A tiny vector PDF writer, just enough for designed documents.
//!
//! - Pages of any size (US Letter by default), drawn with a **top-left**
//!   origin and y growing downwards; points (1/72 in) throughout.
//! - Text in the standard-14 Helvetica family (no font files embedded), in
//!   WinAnsi encoding with the Adobe AFM widths and kerning pairs, character
//!   spacing (letter-spaced labels) and left/centre/right alignment.
//!   Characters outside WinAnsi are transliterated where possible
//!   (`ł` → `l`, `−` → `-`), otherwise shown as `?`.
//! - Paths: rect, rounded rect, circle, polygon and Béziers; RGB fill and
//!   stroke, line width, dashes, round caps, and opacity via ExtGState.
//! - Images: JPEG passed through as DCTDecode; PNG (or any RGBA buffer)
//!   stored as Flate RGB with a Flate soft mask for alpha. Cover-fit (crop
//!   through a clip path) and contain-fit placement.
//! - URI link annotations, QR codes drawn as vector rectangles.
//! - Document info (Title, Author, Subject, Creator, Producer): only what is
//!   set is written, so nothing names this tool unless asked to.
//!
//! Content streams are Flate-compressed. The file is a classic PDF 1.4 with
//! an xref table.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

pub const LETTER_W: f32 = 612.0;
pub const LETTER_H: f32 = 792.0;

// ---------------------------------------------------------------------------
// Colour, fonts, text style
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(pub u8, pub u8, pub u8);

impl Color {
    pub const WHITE: Color = Color(255, 255, 255);
    pub const BLACK: Color = Color(0, 0, 0);

    /// `Color::hex(0xC9A84C)`.
    pub const fn hex(v: u32) -> Color {
        Color((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    /// Linear mix: `t` = 0 gives `self`, 1 gives `other`.
    pub fn mix(self, other: Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Color(m(self.0, other.0), m(self.1, other.1), m(self.2, other.2))
    }

    fn ops(self) -> String {
        format!("{} {} {}", unit(self.0), unit(self.1), unit(self.2))
    }
}

impl From<[u8; 3]> for Color {
    fn from(c: [u8; 3]) -> Color {
        Color(c[0], c[1], c[2])
    }
}

fn unit(v: u8) -> String {
    num(v as f32 / 255.0)
}

/// Compact number: at most 3 decimals, trailing zeros trimmed.
fn num(v: f32) -> String {
    let v = if v.is_finite() { v } else { 0.0 };
    let mut s = format!("{v:.3}");
    while s.contains('.') && (s.ends_with('0') || s.ends_with('.')) {
        s.pop();
    }
    if s == "-0" { "0".into() } else { s }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Font {
    Regular,
    Bold,
    Oblique,
    BoldOblique,
}

impl Font {
    const ALL: [Font; 4] = [Font::Regular, Font::Bold, Font::Oblique, Font::BoldOblique];

    fn idx(self) -> usize {
        match self {
            Font::Regular => 0,
            Font::Bold => 1,
            Font::Oblique => 2,
            Font::BoldOblique => 3,
        }
    }

    fn base(self) -> &'static str {
        match self {
            Font::Regular => "Helvetica",
            Font::Bold => "Helvetica-Bold",
            Font::Oblique => "Helvetica-Oblique",
            Font::BoldOblique => "Helvetica-BoldOblique",
        }
    }

    fn bold(self) -> bool {
        matches!(self, Font::Bold | Font::BoldOblique)
    }

    fn widths(self) -> &'static [u16; 224] {
        if self.bold() { &HELVETICA_BOLD_W } else { &HELVETICA_W }
    }

    fn kerns(self) -> &'static [(u8, u8, i16)] {
        if self.bold() { &HELVETICA_BOLD_K } else { &HELVETICA_K }
    }

    /// Cap height and descender, 1/1000 em (same for the whole family).
    pub const CAP_HEIGHT: f32 = 0.718;
    pub const DESCENT: f32 = 0.207;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug)]
pub struct TextStyle {
    pub font: Font,
    pub size: f32,
    pub color: Color,
    /// Extra space after every character, in points (PDF `Tc`).
    pub spacing: f32,
    pub align: Align,
}

impl TextStyle {
    pub fn new(font: Font, size: f32, color: Color) -> TextStyle {
        TextStyle { font, size, color, spacing: 0.0, align: Align::Left }
    }
    pub fn spacing(mut self, s: f32) -> TextStyle {
        self.spacing = s;
        self
    }
    pub fn align(mut self, a: Align) -> TextStyle {
        self.align = a;
        self
    }
    pub fn size(mut self, s: f32) -> TextStyle {
        self.size = s;
        self
    }
    pub fn color(mut self, c: Color) -> TextStyle {
        self.color = c;
        self
    }
}

// ---------------------------------------------------------------------------
// Text encoding and measuring
// ---------------------------------------------------------------------------

/// Latin Extended-A (U+0100..=U+017F) folded to ASCII base letters.
const LATIN_EXT_A: &[u8; 128] =
    b"AaAaAaCcCcCcCcDdDdEeEeEeEeEeGgGgGgGgHhHhIiIiIiIiIiIiJjKkkLlLlLlLlLlNnNnNnnNnOoOoOoOoRrRrRrSsSsSsSsTtTtTtUuUuUuUuUuUuWwYyYZzZzZzs";

/// Map one char to WinAnsiEncoding; `None` when there is no sensible
/// stand-in.
fn win_ansi(c: char) -> Option<u8> {
    let u = c as u32;
    Some(match u {
        0x20..=0x7E | 0xA0..=0xFF => u as u8,
        0x100..=0x17F => match c {
            'Œ' => 0x8C,
            'œ' => 0x9C,
            'Š' => 0x8A,
            'š' => 0x9A,
            'Ž' => 0x8E,
            'ž' => 0x9E,
            'Ÿ' => 0x9F,
            _ => LATIN_EXT_A[(u - 0x100) as usize],
        },
        _ => match c {
            '€' => 0x80,
            '‚' => 0x82,
            'ƒ' => 0x83,
            '„' => 0x84,
            '…' => 0x85,
            '†' => 0x86,
            '‡' => 0x87,
            'ˆ' => 0x88,
            '‰' => 0x89,
            '‹' => 0x8B,
            '‘' | '′' => 0x91,
            '’' | 'ʼ' => 0x92,
            '“' | '″' => 0x93,
            '”' => 0x94,
            '•' | '∙' | '●' => 0x95,
            '–' => 0x96,
            '—' | '―' => 0x97,
            '˜' => 0x98,
            '™' => 0x99,
            '›' => 0x9B,
            '\t' | '\n' | '\r' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => b' ',
            '‐' | '‑' | '‒' | '−' => b'-',
            '≈' | '∼' => b'~',
            '⁄' | '∕' => b'/',
            '\u{200B}'..='\u{200D}' | '\u{FEFF}' | '\u{00AD}' => return None,
            _ => b'?',
        },
    })
}

/// Encode text for a WinAnsi Helvetica font. Zero-width characters are
/// dropped; anything else that cannot be shown becomes `?`.
pub fn encode(s: &str) -> Vec<u8> {
    s.chars().filter_map(win_ansi).collect()
}

fn kern(font: Font, a: u8, b: u8) -> i16 {
    let k = font.kerns();
    k.binary_search_by(|&(x, y, _)| (x, y).cmp(&(a, b))).map(|i| k[i].2).unwrap_or(0)
}

fn glyph_w(font: Font, b: u8) -> u32 {
    if b < 0x20 { 0 } else { font.widths()[(b - 0x20) as usize] as u32 }
}

/// Visual width in points of encoded bytes: advance widths plus kerning,
/// plus `spacing` between characters (not after the last one).
fn measure_bytes(b: &[u8], font: Font, size: f32, spacing: f32) -> f32 {
    if b.is_empty() {
        return 0.0;
    }
    let mut units: i32 = b.iter().map(|&c| glyph_w(font, c) as i32).sum();
    for w in b.windows(2) {
        units += kern(font, w[0], w[1]) as i32;
    }
    units as f32 * size / 1000.0 + spacing * (b.len() - 1) as f32
}

/// Width of `s` in points as drawn with `style`.
pub fn measure(s: &str, style: &TextStyle) -> f32 {
    measure_bytes(&encode(s), style.font, style.size, style.spacing)
}

/// The largest size ≤ `style.size` (and ≥ `min_size`) at which `s` fits in
/// `max_w`. Character spacing scales with the size.
pub fn fit_size(s: &str, style: &TextStyle, max_w: f32, min_size: f32) -> f32 {
    let w = measure(s, style);
    if w <= max_w || w <= 0.0 {
        return style.size;
    }
    let mut size = (style.size * max_w / w).max(min_size);
    // Rounding: step down until it really fits.
    while size > min_size {
        let st = TextStyle { size, spacing: style.spacing * size / style.size, ..*style };
        if measure(s, &st) <= max_w {
            break;
        }
        size -= 0.1;
    }
    size.max(min_size)
}

/// Greedy word wrap to `max_w`. Words wider than a line are broken.
pub fn wrap(s: &str, style: &TextStyle, max_w: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in s.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let cand = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if measure(&cand, style) <= max_w {
                line = cand;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            if measure(word, style) <= max_w {
                line = word.to_string();
            } else {
                for c in word.chars() {
                    let cand = format!("{line}{c}");
                    if !line.is_empty() && measure(&cand, style) > max_w {
                        lines.push(std::mem::take(&mut line));
                        line.push(c);
                    } else {
                        line = cand;
                    }
                }
            }
        }
        lines.push(line);
    }
    while lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// Truncate at the end with an ellipsis to fit `max_w`.
pub fn ellipsize(s: &str, style: &TextStyle, max_w: f32) -> String {
    if measure(s, style) <= max_w {
        return s.to_string();
    }
    let mut v: Vec<char> = s.chars().collect();
    while !v.is_empty() {
        v.pop();
        let t: String = v.iter().collect::<String>().trim_end().to_string() + "…";
        if measure(&t, style) <= max_w {
            return t;
        }
    }
    "…".into()
}

/// Truncate in the middle (keeps a file extension visible).
pub fn ellipsize_middle(s: &str, style: &TextStyle, max_w: f32) -> String {
    if measure(s, style) <= max_w {
        return s.to_string();
    }
    let v: Vec<char> = s.chars().collect();
    let (mut a, mut b) = (v.len() / 2, v.len() / 2);
    while a > 0 || b < v.len() {
        if (a > 0 && a >= v.len() - b) || b >= v.len() {
            a -= 1;
        } else {
            b += 1;
        }
        let t: String = v[..a].iter().chain(['…'].iter()).chain(v[b..].iter()).collect();
        if measure(&t, style) <= max_w {
            return t;
        }
    }
    "…".into()
}

/// PDF literal string body (no parentheses) for encoded bytes.
fn literal(b: &[u8], out: &mut Vec<u8>) {
    for &c in b {
        if matches!(c, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        out.push(c);
    }
}

/// PDF text string for the info dictionary: UTF-16BE hex.
fn utf16_hex(s: &str) -> String {
    let mut out = String::from("<FEFF");
    for u in s.encode_utf16() {
        let _ = write!(out, "{u:04X}");
    }
    out.push('>');
    out
}

// ---------------------------------------------------------------------------
// Fitting
// ---------------------------------------------------------------------------

/// Rectangle `[x, y, w, h]`, top-left origin.
pub type Rect = [f32; 4];

/// Scale an `iw`×`ih` image to fill the box (cropping the overflow),
/// centred. Returns the image's placement, which overhangs the box.
pub fn cover_rect(iw: f32, ih: f32, b: Rect) -> Rect {
    let s = (b[2] / iw.max(1e-6)).max(b[3] / ih.max(1e-6));
    let (w, h) = (iw * s, ih * s);
    [b[0] + (b[2] - w) / 2.0, b[1] + (b[3] - h) / 2.0, w, h]
}

/// Scale an `iw`×`ih` image to fit inside the box, centred.
pub fn contain_rect(iw: f32, ih: f32, b: Rect) -> Rect {
    let s = (b[2] / iw.max(1e-6)).min(b[3] / ih.max(1e-6));
    let (w, h) = (iw * s, ih * s);
    [b[0] + (b[2] - w) / 2.0, b[1] + (b[3] - h) / 2.0, w, h]
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// Handle to an image added to a `Doc`; copy it freely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageRef {
    id: usize,
    pub w: u32,
    pub h: u32,
}

enum Pixels {
    Jpeg { gray: bool },
    Flate { gray: bool },
}

struct ImageObj {
    w: u32,
    h: u32,
    pixels: Pixels,
    data: Vec<u8>,
    /// Flate-compressed 8-bit alpha.
    smask: Option<Vec<u8>>,
}

/// Width, height and component count from a JPEG's SOF segment.
fn jpeg_info(j: &[u8]) -> Result<(u32, u32, u8)> {
    if j.len() < 4 || j[0] != 0xFF || j[1] != 0xD8 {
        bail!("not a JPEG");
    }
    let mut i = 2;
    while i + 4 <= j.len() {
        if j[i] != 0xFF {
            bail!("corrupt JPEG marker");
        }
        let m = j[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        if matches!(m, 0x01 | 0xD0..=0xD9) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([j[i + 2], j[i + 3]]) as usize;
        if matches!(m, 0xC0..=0xCF) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
            if i + 10 > j.len() {
                break;
            }
            let h = u16::from_be_bytes([j[i + 5], j[i + 6]]) as u32;
            let w = u16::from_be_bytes([j[i + 7], j[i + 8]]) as u32;
            return Ok((w, h, j[i + 9]));
        }
        i += 2 + len;
    }
    bail!("JPEG has no frame header")
}

fn deflate(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

pub struct Page {
    w: f32,
    h: f32,
    ops: Vec<u8>,
    fonts: [bool; 4],
    images: BTreeSet<usize>,
    gstates: BTreeSet<(u16, u16)>,
    links: Vec<(Rect, String)>,
}

impl Page {
    fn new(w: f32, h: f32) -> Page {
        Page { w, h, ops: Vec::new(), fonts: [false; 4], images: BTreeSet::new(), gstates: BTreeSet::new(), links: Vec::new() }
    }

    pub fn width(&self) -> f32 {
        self.w
    }
    pub fn height(&self) -> f32 {
        self.h
    }

    fn op(&mut self, s: &str) {
        self.ops.extend_from_slice(s.as_bytes());
        self.ops.push(b'\n');
    }

    fn pt(&self, x: f32, y: f32) -> String {
        format!("{} {}", num(x), num(self.h - y))
    }

    // --- graphics state ---

    pub fn save(&mut self) {
        self.op("q");
    }
    pub fn restore(&mut self) {
        self.op("Q");
    }
    pub fn fill_color(&mut self, c: Color) {
        self.op(&format!("{} rg", c.ops()));
    }
    pub fn stroke_color(&mut self, c: Color) {
        self.op(&format!("{} RG", c.ops()));
    }
    pub fn line_width(&mut self, w: f32) {
        self.op(&format!("{} w", num(w)));
    }
    /// Dash pattern in points; an empty slice is a solid line.
    pub fn dash(&mut self, pattern: &[f32], phase: f32) {
        let p: Vec<String> = pattern.iter().map(|&v| num(v)).collect();
        self.op(&format!("[{}] {} d", p.join(" "), num(phase)));
    }
    /// Round (true) or butt/miter (false) line caps and joins.
    pub fn round_caps(&mut self, round: bool) {
        self.op(if round { "1 J 1 j" } else { "0 J 0 j" });
    }
    /// Fill and stroke opacity, 0–1 (until the next `restore`).
    pub fn alpha(&mut self, fill: f32, stroke: f32) {
        let key = ((fill.clamp(0.0, 1.0) * 1000.0).round() as u16, (stroke.clamp(0.0, 1.0) * 1000.0).round() as u16);
        self.gstates.insert(key);
        self.op(&format!("/{} gs", gs_name(key)));
    }

    // --- paths ---

    pub fn move_to(&mut self, x: f32, y: f32) {
        let p = self.pt(x, y);
        self.op(&format!("{p} m"));
    }
    pub fn line_to(&mut self, x: f32, y: f32) {
        let p = self.pt(x, y);
        self.op(&format!("{p} l"));
    }
    pub fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (a, b, c) = (self.pt(x1, y1), self.pt(x2, y2), self.pt(x, y));
        self.op(&format!("{a} {b} {c} c"));
    }
    pub fn close(&mut self) {
        self.op("h");
    }
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let p = self.pt(x, y + h);
        self.op(&format!("{p} {} {} re", num(w), num(h)));
    }
    pub fn rounded_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        if r <= 0.0 {
            return self.rect(x, y, w, h);
        }
        let k = r * (1.0 - KAPPA);
        self.move_to(x + r, y);
        self.line_to(x + w - r, y);
        self.curve_to(x + w - k, y, x + w, y + k, x + w, y + r);
        self.line_to(x + w, y + h - r);
        self.curve_to(x + w, y + h - k, x + w - k, y + h, x + w - r, y + h);
        self.line_to(x + r, y + h);
        self.curve_to(x + k, y + h, x, y + h - k, x, y + h - r);
        self.line_to(x, y + r);
        self.curve_to(x, y + k, x + k, y, x + r, y);
        self.close();
    }
    pub fn circle(&mut self, cx: f32, cy: f32, r: f32) {
        let k = r * KAPPA;
        self.move_to(cx + r, cy);
        self.curve_to(cx + r, cy + k, cx + k, cy + r, cx, cy + r);
        self.curve_to(cx - k, cy + r, cx - r, cy + k, cx - r, cy);
        self.curve_to(cx - r, cy - k, cx - k, cy - r, cx, cy - r);
        self.curve_to(cx + k, cy - r, cx + r, cy - k, cx + r, cy);
        self.close();
    }
    /// A polyline; closed when `close` is true.
    pub fn polygon(&mut self, pts: &[(f32, f32)], close: bool) {
        let Some(&(x0, y0)) = pts.first() else { return };
        self.move_to(x0, y0);
        for &(x, y) in &pts[1..] {
            self.line_to(x, y);
        }
        if close {
            self.close();
        }
    }
    pub fn fill(&mut self) {
        self.op("f");
    }
    /// Fill with the even-odd rule (holes in polygons).
    pub fn fill_evenodd(&mut self) {
        self.op("f*");
    }
    pub fn stroke(&mut self) {
        self.op("S");
    }
    pub fn fill_stroke(&mut self) {
        self.op("B");
    }
    pub fn fill_stroke_evenodd(&mut self) {
        self.op("B*");
    }
    /// Intersect the clip with the current path (and end it).
    pub fn clip(&mut self) {
        self.op("W n");
    }

    // --- conveniences ---

    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        self.save();
        self.fill_color(c);
        self.rect(x, y, w, h);
        self.fill();
        self.restore();
    }

    /// A straight line in the current stroke state.
    pub fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) {
        self.move_to(x1, y1);
        self.line_to(x2, y2);
        self.stroke();
    }

    /// A horizontal rule of width `w` points and colour `c`.
    pub fn hrule(&mut self, x1: f32, x2: f32, y: f32, w: f32, c: Color) {
        self.save();
        self.stroke_color(c);
        self.line_width(w);
        self.line(x1, y, x2, y);
        self.restore();
    }

    // --- text ---

    /// Draw one line of text with its baseline at `y`; `x` is the left
    /// edge, centre or right edge depending on `style.align`. Returns the
    /// drawn width.
    pub fn text(&mut self, x: f32, y: f32, s: &str, style: &TextStyle) -> f32 {
        let b = encode(s);
        let w = measure_bytes(&b, style.font, style.size, style.spacing);
        if b.is_empty() {
            return 0.0;
        }
        let x = match style.align {
            Align::Left => x,
            Align::Center => x - w / 2.0,
            Align::Right => x - w,
        };
        self.fonts[style.font.idx()] = true;
        let mut tj = Vec::with_capacity(b.len() + 8);
        tj.push(b'(');
        for (i, &c) in b.iter().enumerate() {
            if i > 0 {
                let k = kern(style.font, b[i - 1], c);
                if k != 0 {
                    tj.extend_from_slice(format!(") {} (", -k).as_bytes());
                }
            }
            literal(&[c], &mut tj);
        }
        tj.push(b')');
        let head = format!(
            "q {} rg BT /F{} {} Tf {} Tc {} Td [",
            style.color.ops(),
            style.font.idx() + 1,
            num(style.size),
            num(style.spacing),
            self.pt(x, y)
        );
        self.ops.extend_from_slice(head.as_bytes());
        self.ops.extend_from_slice(&tj);
        self.ops.extend_from_slice(b"] TJ ET Q\n");
        w
    }

    /// Wrap `s` to `max_w` and draw it from baseline `y` with `leading`
    /// between baselines. Returns the baseline after the last line.
    pub fn paragraph(&mut self, x: f32, y: f32, max_w: f32, s: &str, style: &TextStyle, leading: f32) -> f32 {
        let mut y = y;
        for l in wrap(s, style, max_w) {
            self.text(x, y, &l, style);
            y += leading;
        }
        y
    }

    // --- images ---

    /// Draw an image stretched to the rectangle.
    pub fn image(&mut self, img: &ImageRef, x: f32, y: f32, w: f32, h: f32) {
        self.images.insert(img.id);
        let p = self.pt(x, y + h);
        self.op(&format!("q {} 0 0 {} {p} cm /Im{} Do Q", num(w), num(h), img.id));
    }

    /// Fill the box with the image, cropping what overhangs.
    pub fn image_cover(&mut self, img: &ImageRef, x: f32, y: f32, w: f32, h: f32) {
        let r = cover_rect(img.w as f32, img.h as f32, [x, y, w, h]);
        self.save();
        self.rect(x, y, w, h);
        self.clip();
        self.image(img, r[0], r[1], r[2], r[3]);
        self.restore();
    }

    /// Fit the whole image inside the box, centred. Returns where it went.
    pub fn image_contain(&mut self, img: &ImageRef, x: f32, y: f32, w: f32, h: f32) -> Rect {
        let r = contain_rect(img.w as f32, img.h as f32, [x, y, w, h]);
        self.image(img, r[0], r[1], r[2], r[3]);
        r
    }

    // --- links and QR ---

    /// A clickable area opening `uri` (http(s):, mailto:, tel:).
    pub fn link(&mut self, x: f32, y: f32, w: f32, h: f32, uri: &str) {
        self.links.push(([x, y, w, h], uri.to_string()));
    }

    /// Draw a QR code for `data` as a `size`×`size` square of dark modules
    /// (no quiet zone: put it on a light tile with ≥ 2 modules of margin).
    /// Returns the module count per side.
    pub fn qr(&mut self, x: f32, y: f32, size: f32, data: &str, c: Color) -> Result<usize> {
        let code = qrcode::QrCode::with_error_correction_level(data.as_bytes(), qrcode::EcLevel::M)
            .map_err(|e| anyhow!("QR code: {e:?}"))?;
        let n = code.width();
        let colors = code.to_colors();
        let m = size / n as f32;
        self.save();
        self.fill_color(c);
        for r in 0..n {
            let mut col = 0;
            while col < n {
                if colors[r * n + col] == qrcode::Color::Dark {
                    let start = col;
                    while col < n && colors[r * n + col] == qrcode::Color::Dark {
                        col += 1;
                    }
                    // A hair of overlap so no seams show between modules.
                    self.rect(x + start as f32 * m, y + r as f32 * m, (col - start) as f32 * m + 0.02, m + 0.02);
                } else {
                    col += 1;
                }
            }
        }
        self.fill();
        self.restore();
        Ok(n)
    }
}

const KAPPA: f32 = 0.552_284_8;

fn gs_name(k: (u16, u16)) -> String {
    format!("Ga{}s{}", k.0, k.1)
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Info {
    title: Option<String>,
    author: Option<String>,
    subject: Option<String>,
    creator: Option<String>,
    producer: Option<String>,
}

#[derive(Default)]
pub struct Doc {
    pages: Vec<Page>,
    images: Vec<ImageObj>,
    info: Info,
}

impl Doc {
    pub fn new() -> Doc {
        Doc::default()
    }

    pub fn set_title(&mut self, s: &str) {
        self.info.title = Some(s.to_string());
    }
    pub fn set_author(&mut self, s: &str) {
        self.info.author = Some(s.to_string());
    }
    pub fn set_subject(&mut self, s: &str) {
        self.info.subject = Some(s.to_string());
    }
    pub fn set_creator(&mut self, s: &str) {
        self.info.creator = Some(s.to_string());
    }
    pub fn set_producer(&mut self, s: &str) {
        self.info.producer = Some(s.to_string());
    }

    /// Append a US Letter page.
    pub fn page(&mut self) -> &mut Page {
        self.page_sized(LETTER_W, LETTER_H)
    }

    pub fn page_sized(&mut self, w: f32, h: f32) -> &mut Page {
        self.pages.push(Page::new(w, h));
        self.pages.last_mut().expect("just pushed")
    }

    pub fn page_mut(&mut self, i: usize) -> &mut Page {
        &mut self.pages[i]
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Add a baseline or progressive JPEG (greyscale or RGB), embedded
    /// as-is. Strip metadata before handing it over: the bytes are copied
    /// verbatim.
    pub fn add_jpeg(&mut self, bytes: Vec<u8>) -> Result<ImageRef> {
        let (w, h, comps) = jpeg_info(&bytes)?;
        let gray = match comps {
            1 => true,
            3 => false,
            n => bail!("unsupported JPEG with {n} components"),
        };
        Ok(self.push(ImageObj { w, h, pixels: Pixels::Jpeg { gray }, data: bytes, smask: None }))
    }

    /// Add a PNG (any colour type; alpha becomes a soft mask).
    pub fn add_png(&mut self, bytes: &[u8]) -> Result<ImageRef> {
        let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).context("decoding PNG")?;
        Ok(self.add_rgba(&img.to_rgba8()))
    }

    /// Add RGBA pixels, Flate-compressed; the alpha channel becomes a soft
    /// mask unless the image is fully opaque.
    pub fn add_rgba(&mut self, img: &image::RgbaImage) -> ImageRef {
        let (w, h) = img.dimensions();
        let raw = img.as_raw();
        let rgb: Vec<u8> = raw.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        let alpha: Vec<u8> = raw.chunks_exact(4).map(|p| p[3]).collect();
        let smask = if alpha.iter().all(|&a| a == 255) { None } else { Some(deflate(&alpha)) };
        self.push(ImageObj { w, h, pixels: Pixels::Flate { gray: false }, data: deflate(&rgb), smask })
    }

    /// Add RGB pixels, Flate-compressed (lossless; for small graphics).
    pub fn add_rgb(&mut self, img: &image::RgbImage) -> ImageRef {
        let (w, h) = img.dimensions();
        self.push(ImageObj { w, h, pixels: Pixels::Flate { gray: false }, data: deflate(img.as_raw()), smask: None })
    }

    fn push(&mut self, o: ImageObj) -> ImageRef {
        let r = ImageRef { id: self.images.len(), w: o.w, h: o.h };
        self.images.push(o);
        r
    }

    /// Write atomically (`.name.partial`, then rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        crate::imaging::write_atomic(path, &self.to_bytes())
    }

    /// Serialise the document.
    pub fn to_bytes(&self) -> Vec<u8> {
        // Object numbering: 1 catalog, 2 page tree, 3 info, then fonts,
        // images (+ soft masks), graphics states, and per page: the page,
        // its content stream and its link annotations.
        let mut next = 4usize;
        let mut alloc = |n: usize| {
            let first = next;
            next += n;
            first
        };
        let mut font_ids = [0usize; 4];
        for f in Font::ALL {
            if self.pages.iter().any(|p| p.fonts[f.idx()]) {
                font_ids[f.idx()] = alloc(1);
            }
        }
        let img_ids: Vec<(usize, Option<usize>)> =
            self.images.iter().map(|im| (alloc(1), im.smask.as_ref().map(|_| alloc(1)))).collect();
        let all_gs: BTreeSet<(u16, u16)> = self.pages.iter().flat_map(|p| p.gstates.iter().copied()).collect();
        let gs_ids: BTreeMap<(u16, u16), usize> = all_gs.iter().map(|&k| (k, alloc(1))).collect();
        let page_ids: Vec<(usize, usize, usize)> =
            self.pages.iter().map(|p| (alloc(1), alloc(1), alloc(p.links.len()))).collect();
        let total = next;

        let mut w = Writer::new(total);
        w.obj(1, b"<< /Type /Catalog /Pages 2 0 R /ViewerPreferences << /DisplayDocTitle true >> >>");
        let kids: Vec<String> = page_ids.iter().map(|(p, _, _)| format!("{p} 0 R")).collect();
        w.obj(2, format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids.join(" "), self.pages.len()).as_bytes());

        let mut info = String::from("<<");
        for (k, v) in [
            ("Title", &self.info.title),
            ("Author", &self.info.author),
            ("Subject", &self.info.subject),
            ("Creator", &self.info.creator),
            ("Producer", &self.info.producer),
        ] {
            if let Some(v) = v {
                let _ = write!(info, " /{k} {}", utf16_hex(v));
            }
        }
        let _ = write!(info, " /CreationDate (D:{}) >>", chrono::Local::now().format("%Y%m%d%H%M%S"));
        w.obj(3, info.as_bytes());

        for f in Font::ALL {
            let id = font_ids[f.idx()];
            if id != 0 {
                let body =
                    format!("<< /Type /Font /Subtype /Type1 /BaseFont /{} /Encoding /WinAnsiEncoding >>", f.base());
                w.obj(id, body.as_bytes());
            }
        }

        for (im, &(id, mask)) in self.images.iter().zip(&img_ids) {
            let (filter, gray) = match im.pixels {
                Pixels::Jpeg { gray } => ("DCTDecode", gray),
                Pixels::Flate { gray } => ("FlateDecode", gray),
            };
            let cs = if gray { "DeviceGray" } else { "DeviceRGB" };
            let mut dict = format!(
                "/Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /{cs} /BitsPerComponent 8 /Filter /{filter}",
                im.w, im.h
            );
            if let Some(m) = mask {
                let _ = write!(dict, " /SMask {m} 0 R");
            }
            w.stream(id, &dict, &im.data);
            if let (Some(m), Some(data)) = (mask, &im.smask) {
                let d = format!(
                    "/Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
                    im.w, im.h
                );
                w.stream(m, &d, data);
            }
        }

        for (&k, &id) in &gs_ids {
            let body = format!("<< /Type /ExtGState /ca {} /CA {} >>", num(k.0 as f32 / 1000.0), num(k.1 as f32 / 1000.0));
            w.obj(id, body.as_bytes());
        }

        // Compress content streams in parallel: a map or QR page can be big.
        let compressed = crate::imaging::par_map(&self.pages, |p| deflate(&p.ops));
        for ((p, &(pid, cid, lid)), content) in self.pages.iter().zip(&page_ids).zip(compressed) {
            let mut res = String::from("<<");
            let fonts: Vec<String> = Font::ALL
                .iter()
                .filter(|f| p.fonts[f.idx()])
                .map(|f| format!("/F{} {} 0 R", f.idx() + 1, font_ids[f.idx()]))
                .collect();
            if !fonts.is_empty() {
                let _ = write!(res, " /Font << {} >>", fonts.join(" "));
            }
            if !p.images.is_empty() {
                let xo: Vec<String> = p.images.iter().map(|&i| format!("/Im{i} {} 0 R", img_ids[i].0)).collect();
                let _ = write!(res, " /XObject << {} >>", xo.join(" "));
            }
            if !p.gstates.is_empty() {
                let gs: Vec<String> = p.gstates.iter().map(|k| format!("/{} {} 0 R", gs_name(*k), gs_ids[k])).collect();
                let _ = write!(res, " /ExtGState << {} >>", gs.join(" "));
            }
            res.push_str(" >>");
            let mut page = format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources {res} /Contents {cid} 0 R",
                num(p.w),
                num(p.h)
            );
            if !p.links.is_empty() {
                let a: Vec<String> = (0..p.links.len()).map(|i| format!("{} 0 R", lid + i)).collect();
                let _ = write!(page, " /Annots [{}]", a.join(" "));
            }
            page.push_str(" >>");
            w.obj(pid, page.as_bytes());
            w.stream(cid, "/Filter /FlateDecode", &content);
            for (i, (r, uri)) in p.links.iter().enumerate() {
                let (x1, y1, x2, y2) = (r[0], p.h - r[1] - r[3], r[0] + r[2], p.h - r[1]);
                let mut body = format!(
                    "<< /Type /Annot /Subtype /Link /Rect [{} {} {} {}] /Border [0 0 0] /A << /S /URI /URI (",
                    num(x1),
                    num(y1),
                    num(x2),
                    num(y2)
                )
                .into_bytes();
                literal(uri_ascii(uri).as_bytes(), &mut body);
                body.extend_from_slice(b") >> >>");
                w.obj(lid + i, &body);
            }
        }
        w.finish(total)
    }
}

/// URIs must be 7-bit: percent-encode anything else.
fn uri_ascii(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_graphic() {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Numbered objects and a classic xref table.
struct Writer {
    buf: Vec<u8>,
    offsets: Vec<usize>,
}

impl Writer {
    fn new(objects: usize) -> Writer {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
        Writer { buf, offsets: vec![0; objects] }
    }

    fn obj(&mut self, id: usize, body: &[u8]) {
        self.offsets[id] = self.buf.len();
        self.buf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        self.buf.extend_from_slice(body);
        self.buf.extend_from_slice(b"\nendobj\n");
    }

    fn stream(&mut self, id: usize, dict: &str, data: &[u8]) {
        let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\nendstream");
        self.obj(id, &body);
    }

    fn finish(mut self, n: usize) -> Vec<u8> {
        let xref = self.buf.len();
        let mut t = format!("xref\n0 {n}\n0000000000 65535 f \n");
        for &off in &self.offsets[1..] {
            let _ = writeln!(t, "{off:010} 00000 n ");
        }
        let _ = write!(t, "trailer\n<< /Size {n} /Root 1 0 R /Info 3 0 R >>\nstartxref\n{xref}\n%%EOF\n");
        self.buf.extend_from_slice(t.as_bytes());
        self.buf
    }
}

// ---------------------------------------------------------------------------
// Adobe Helvetica metrics (AFM), indexed by WinAnsi code - 0x20. Oblique
// faces share their upright widths and kerning. Kerning pairs are
// (left, right, adjustment) in WinAnsi codes, sorted.
// ---------------------------------------------------------------------------

const HELVETICA_W: [u16; 224] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556,
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556,
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556,
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584, 350,
    556, 350, 222, 556, 333, 1000, 556, 556, 333, 1000, 667, 333, 1000, 350, 611, 350,
    350, 222, 222, 333, 333, 350, 556, 1000, 333, 1000, 500, 333, 944, 350, 500, 667,
    278, 333, 556, 556, 556, 556, 260, 556, 333, 737, 370, 556, 584, 333, 737, 333,
    400, 584, 333, 333, 333, 556, 537, 278, 333, 333, 365, 556, 834, 834, 834, 611,
    667, 667, 667, 667, 667, 667, 1000, 722, 667, 667, 667, 667, 278, 278, 278, 278,
    722, 722, 778, 778, 778, 778, 778, 584, 778, 722, 722, 722, 722, 667, 667, 611,
    556, 556, 556, 556, 556, 556, 889, 500, 556, 556, 556, 556, 278, 278, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 584, 611, 556, 556, 556, 556, 500, 556, 500,
];
const HELVETICA_K: [(u8, u8, i16); 264] = [
    (32, 84, -50), (32, 86, -50), (32, 87, -40), (32, 89, -90), (32, 145, -60), (32, 147, -30),
    (44, 146, -100), (44, 148, -100), (46, 32, -60), (46, 146, -100), (46, 148, -100), (46, 160, -60),
    (58, 32, -50), (58, 160, -50), (59, 32, -50), (59, 160, -50), (65, 67, -30), (65, 71, -30),
    (65, 79, -30), (65, 81, -30), (65, 84, -120), (65, 85, -50), (65, 86, -70), (65, 87, -50),
    (65, 89, -100), (65, 117, -30), (65, 118, -40), (65, 119, -40), (65, 121, -40), (66, 44, -20),
    (66, 46, -20), (66, 85, -10), (67, 44, -30), (67, 46, -30), (68, 44, -70), (68, 46, -70),
    (68, 65, -40), (68, 86, -70), (68, 87, -40), (68, 89, -90), (70, 44, -150), (70, 46, -150),
    (70, 65, -80), (70, 97, -50), (70, 101, -30), (70, 111, -30), (70, 114, -45), (74, 44, -30),
    (74, 46, -30), (74, 65, -20), (74, 97, -20), (74, 117, -20), (75, 79, -50), (75, 101, -40),
    (75, 111, -40), (75, 117, -30), (75, 121, -50), (76, 84, -110), (76, 86, -110), (76, 87, -70),
    (76, 89, -140), (76, 121, -30), (76, 146, -160), (76, 148, -140), (79, 44, -40), (79, 46, -40),
    (79, 65, -20), (79, 84, -40), (79, 86, -50), (79, 87, -30), (79, 88, -60), (79, 89, -70),
    (80, 44, -180), (80, 46, -180), (80, 65, -120), (80, 97, -40), (80, 101, -50), (80, 111, -50),
    (81, 85, -10), (82, 79, -20), (82, 84, -30), (82, 85, -40), (82, 86, -50), (82, 87, -30),
    (82, 89, -50), (83, 44, -20), (83, 46, -20), (84, 44, -120), (84, 45, -140), (84, 46, -120),
    (84, 58, -20), (84, 59, -20), (84, 65, -120), (84, 79, -40), (84, 97, -120), (84, 101, -120),
    (84, 111, -120), (84, 114, -120), (84, 117, -120), (84, 119, -120), (84, 121, -120), (84, 173, -140),
    (85, 44, -40), (85, 46, -40), (85, 65, -40), (86, 44, -125), (86, 45, -80), (86, 46, -125),
    (86, 58, -40), (86, 59, -40), (86, 65, -80), (86, 71, -40), (86, 79, -40), (86, 97, -70),
    (86, 101, -80), (86, 111, -80), (86, 117, -70), (86, 173, -80), (87, 44, -80), (87, 45, -40),
    (87, 46, -80), (87, 65, -50), (87, 79, -20), (87, 97, -40), (87, 101, -30), (87, 111, -30),
    (87, 117, -30), (87, 121, -20), (87, 173, -40), (89, 44, -140), (89, 45, -140), (89, 46, -140),
    (89, 58, -60), (89, 59, -60), (89, 65, -110), (89, 79, -85), (89, 97, -140), (89, 101, -140),
    (89, 105, -20), (89, 111, -140), (89, 117, -110), (89, 173, -140), (97, 118, -20), (97, 119, -20),
    (97, 121, -30), (98, 44, -40), (98, 46, -40), (98, 98, -10), (98, 108, -20), (98, 117, -20),
    (98, 118, -20), (98, 121, -20), (99, 44, -15), (99, 107, -20), (101, 44, -15), (101, 46, -15),
    (101, 118, -30), (101, 119, -20), (101, 120, -30), (101, 121, -20), (102, 44, -30), (102, 46, -30),
    (102, 97, -30), (102, 101, -30), (102, 111, -30), (102, 146, 50), (102, 148, 60), (103, 114, -10),
    (104, 121, -30), (107, 101, -20), (107, 111, -20), (109, 117, -10), (109, 121, -15), (110, 117, -10),
    (110, 118, -20), (110, 121, -15), (111, 44, -40), (111, 46, -40), (111, 118, -15), (111, 119, -15),
    (111, 120, -30), (111, 121, -30), (112, 44, -35), (112, 46, -35), (112, 121, -30), (114, 44, -50),
    (114, 46, -50), (114, 58, 30), (114, 59, 30), (114, 97, -10), (114, 105, 15), (114, 107, 15),
    (114, 108, 15), (114, 109, 25), (114, 110, 25), (114, 112, 30), (114, 116, 40), (114, 117, 15),
    (114, 118, 30), (114, 121, 30), (115, 44, -15), (115, 46, -15), (115, 119, -30), (118, 44, -80),
    (118, 46, -80), (118, 97, -25), (118, 101, -25), (118, 111, -25), (119, 44, -60), (119, 46, -60),
    (119, 97, -15), (119, 101, -10), (119, 111, -10), (120, 101, -30), (121, 44, -100), (121, 46, -100),
    (121, 97, -20), (121, 101, -20), (121, 111, -20), (122, 101, -15), (122, 111, -15), (145, 145, -57),
    (146, 32, -70), (146, 100, -50), (146, 114, -50), (146, 115, -50), (146, 146, -57), (146, 160, -70),
    (148, 32, -40), (148, 160, -40), (160, 84, -50), (160, 86, -50), (160, 87, -40), (160, 89, -90),
    (160, 145, -60), (160, 147, -30), (248, 44, -95), (248, 46, -95), (248, 97, -55), (248, 98, -55),
    (248, 99, -55), (248, 100, -55), (248, 101, -55), (248, 102, -55), (248, 103, -55), (248, 104, -55),
    (248, 105, -55), (248, 106, -55), (248, 107, -55), (248, 108, -55), (248, 109, -55), (248, 110, -55),
    (248, 111, -55), (248, 112, -55), (248, 113, -55), (248, 114, -55), (248, 115, -55), (248, 116, -55),
    (248, 117, -55), (248, 118, -70), (248, 119, -70), (248, 120, -85), (248, 121, -70), (248, 122, -55),
];
const HELVETICA_BOLD_W: [u16; 224] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611,
    975, 722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 333, 278, 333, 584, 556,
    333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611,
    611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584, 350,
    556, 350, 278, 556, 500, 1000, 556, 556, 333, 1000, 667, 333, 1000, 350, 611, 350,
    350, 278, 278, 500, 500, 350, 556, 1000, 333, 1000, 556, 333, 944, 350, 500, 667,
    278, 333, 556, 556, 556, 556, 280, 556, 333, 737, 370, 556, 584, 333, 737, 333,
    400, 584, 333, 333, 333, 611, 556, 278, 333, 333, 365, 556, 834, 834, 834, 611,
    722, 722, 722, 722, 722, 722, 1000, 722, 667, 667, 667, 667, 278, 278, 278, 278,
    722, 722, 778, 778, 778, 778, 778, 584, 778, 722, 722, 722, 722, 667, 667, 611,
    556, 556, 556, 556, 556, 556, 889, 556, 556, 556, 556, 556, 278, 278, 278, 278,
    611, 611, 611, 611, 611, 611, 611, 584, 611, 611, 611, 611, 611, 556, 611, 556,
];
const HELVETICA_BOLD_K: [(u8, u8, i16); 225] = [
    (32, 84, -100), (32, 86, -80), (32, 87, -80), (32, 89, -120), (32, 145, -60), (32, 147, -80),
    (44, 32, -40), (44, 146, -120), (44, 148, -120), (44, 160, -40), (46, 32, -40), (46, 146, -120),
    (46, 148, -120), (46, 160, -40), (58, 32, -40), (58, 160, -40), (59, 32, -40), (59, 160, -40),
    (65, 67, -40), (65, 71, -50), (65, 79, -40), (65, 81, -40), (65, 84, -90), (65, 85, -50),
    (65, 86, -80), (65, 87, -60), (65, 89, -110), (65, 117, -30), (65, 118, -40), (65, 119, -30),
    (65, 121, -30), (66, 65, -30), (66, 85, -10), (68, 44, -30), (68, 46, -30), (68, 65, -40),
    (68, 86, -40), (68, 87, -40), (68, 89, -70), (70, 44, -100), (70, 46, -100), (70, 65, -80),
    (70, 97, -20), (74, 44, -20), (74, 46, -20), (74, 65, -20), (74, 117, -20), (75, 79, -30),
    (75, 101, -15), (75, 111, -35), (75, 117, -30), (75, 121, -40), (76, 84, -90), (76, 86, -110),
    (76, 87, -80), (76, 89, -120), (76, 121, -30), (76, 146, -140), (76, 148, -140), (79, 44, -40),
    (79, 46, -40), (79, 65, -50), (79, 84, -40), (79, 86, -50), (79, 87, -50), (79, 88, -50),
    (79, 89, -70), (80, 44, -120), (80, 46, -120), (80, 65, -100), (80, 97, -30), (80, 101, -30),
    (80, 111, -40), (81, 44, 20), (81, 46, 20), (81, 85, -10), (82, 79, -20), (82, 84, -20),
    (82, 85, -20), (82, 86, -50), (82, 87, -40), (82, 89, -50), (84, 44, -80), (84, 45, -120),
    (84, 46, -80), (84, 58, -40), (84, 59, -40), (84, 65, -90), (84, 79, -40), (84, 97, -80),
    (84, 101, -60), (84, 111, -80), (84, 114, -80), (84, 117, -90), (84, 119, -60), (84, 121, -60),
    (84, 173, -120), (85, 44, -30), (85, 46, -30), (85, 65, -50), (86, 44, -120), (86, 45, -80),
    (86, 46, -120), (86, 58, -40), (86, 59, -40), (86, 65, -80), (86, 71, -50), (86, 79, -50),
    (86, 97, -60), (86, 101, -50), (86, 111, -90), (86, 117, -60), (86, 173, -80), (87, 44, -80),
    (87, 45, -40), (87, 46, -80), (87, 58, -10), (87, 59, -10), (87, 65, -60), (87, 79, -20),
    (87, 97, -40), (87, 101, -35), (87, 111, -60), (87, 117, -45), (87, 121, -20), (87, 173, -40),
    (89, 44, -100), (89, 46, -100), (89, 58, -50), (89, 59, -50), (89, 65, -110), (89, 79, -70),
    (89, 97, -90), (89, 101, -80), (89, 111, -100), (89, 117, -100), (97, 103, -10), (97, 118, -15),
    (97, 119, -15), (97, 121, -20), (98, 108, -10), (98, 117, -20), (98, 118, -20), (98, 121, -20),
    (99, 104, -10), (99, 107, -20), (99, 108, -20), (99, 121, -10), (100, 100, -10), (100, 118, -15),
    (100, 119, -15), (100, 121, -15), (101, 44, 10), (101, 46, 20), (101, 118, -15), (101, 119, -15),
    (101, 120, -15), (101, 121, -15), (102, 44, -10), (102, 46, -10), (102, 101, -10), (102, 111, -20),
    (102, 146, 30), (102, 148, 30), (103, 101, 10), (103, 103, -10), (104, 121, -20), (107, 111, -15),
    (108, 119, -15), (108, 121, -15), (109, 117, -20), (109, 121, -30), (110, 117, -10), (110, 118, -40),
    (110, 121, -20), (111, 118, -20), (111, 119, -15), (111, 120, -30), (111, 121, -20), (112, 121, -15),
    (114, 44, -60), (114, 45, -20), (114, 46, -60), (114, 99, -20), (114, 100, -20), (114, 103, -15),
    (114, 111, -20), (114, 113, -20), (114, 115, -15), (114, 116, 20), (114, 118, 10), (114, 121, 10),
    (114, 173, -20), (115, 119, -15), (118, 44, -80), (118, 46, -80), (118, 97, -20), (118, 111, -30),
    (119, 44, -40), (119, 46, -40), (119, 111, -20), (120, 101, -10), (121, 44, -80), (121, 46, -80),
    (121, 97, -30), (121, 101, -10), (121, 111, -25), (122, 101, 10), (145, 145, -46), (146, 32, -80),
    (146, 100, -80), (146, 108, -20), (146, 114, -40), (146, 115, -60), (146, 118, -20), (146, 146, -46),
    (146, 160, -80), (148, 32, -80), (148, 160, -80), (160, 84, -100), (160, 86, -80), (160, 87, -80),
    (160, 89, -120), (160, 145, -60), (160, 147, -80),
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Inflate every FlateDecode stream that is not an image and return
    /// the concatenated text, plus the raw (uncompressed) object text.
    pub(crate) fn readable(pdf: &[u8]) -> String {
        let mut out = String::new();
        let mut i = 0;
        while let Some(p) = find(&pdf[i..], b">>\nstream\n") {
            let head_end = i + p + 2;
            let start = head_end + 8;
            let obj_at = pdf[..head_end].windows(5).rposition(|w| w == b" obj\n").unwrap_or(0);
            let dict = String::from_utf8_lossy(&pdf[obj_at..head_end]).into_owned();
            let len: usize = dict
                .rsplit("/Length ")
                .next()
                .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|s| s.parse().ok())
                .expect("stream length");
            out.push_str(&String::from_utf8_lossy(&pdf[i..head_end]));
            out.push('\n');
            let data = &pdf[start..start + len];
            if !dict.contains("/Subtype /Image")
                && dict.contains("FlateDecode")
                && let Ok(d) = miniz_oxide::inflate::decompress_to_vec_zlib(data)
            {
                out.push_str(&String::from_utf8_lossy(&d));
            }
            i = start + len;
        }
        out.push_str(&String::from_utf8_lossy(&pdf[i..]));
        out
    }

    /// Join kerned TJ pieces back together: `(Pre) 15 (view)` → `(Preview)`.
    pub(crate) fn unkern(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b')' && (i == 0 || b[i - 1] != b'\\') {
                let mut j = i + 1;
                if j < b.len() && b[j] == b' ' {
                    j += 1;
                    if j < b.len() && b[j] == b'-' {
                        j += 1;
                    }
                    let d = j;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                    if j > d && j + 1 < b.len() && b[j] == b' ' && b[j + 1] == b'(' {
                        i = j + 2;
                        continue;
                    }
                }
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    pub(crate) fn find(h: &[u8], n: &[u8]) -> Option<usize> {
        h.windows(n.len()).position(|w| w == n)
    }

    /// Run `qpdf --check` when qpdf is installed; otherwise check the xref
    /// table by hand.
    pub(crate) fn check_pdf(pdf: &[u8]) {
        assert!(pdf.starts_with(b"%PDF-1.4"));
        let tail = String::from_utf8_lossy(&pdf[pdf.len().saturating_sub(64)..]).into_owned();
        let sx = tail.rfind("startxref\n").expect("startxref");
        let xref: usize = tail[sx + 10..].lines().next().unwrap().parse().unwrap();
        let table = std::str::from_utf8(&pdf[xref..]).unwrap();
        assert!(table.starts_with("xref\n0 "));
        for (id, line) in table.lines().skip(3).take_while(|l| l.ends_with(" n ")).enumerate() {
            let off: usize = line[..10].parse().unwrap();
            assert!(pdf[off..].starts_with(format!("{} 0 obj\n", id + 1).as_bytes()), "object {}", id + 1);
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("check.pdf");
        std::fs::write(&p, pdf).unwrap();
        if let Ok(out) = std::process::Command::new("qpdf").arg("--check").arg(&p).output() {
            let so = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "qpdf --check failed:\n{so}\n{}", String::from_utf8_lossy(&out.stderr));
            assert!(so.contains("No syntax or stream encoding errors"), "{so}");
        }
    }

    #[test]
    fn widths_kerning_and_encoding() {
        let st = TextStyle::new(Font::Regular, 10.0, Color::BLACK);
        // "AV" is kerned tighter than "A" + "V".
        assert!(measure("AV", &st) < measure("A", &st) + measure("V", &st));
        assert!((measure("Hello", &st) - 22.78).abs() < 0.01, "{}", measure("Hello", &st));
        let bold = TextStyle::new(Font::Bold, 10.0, Color::BLACK);
        assert!(measure("Hello", &bold) > measure("Hello", &st));
        // Letter spacing adds between characters only.
        let sp = st.spacing(2.0);
        assert!((measure("ABC", &sp) - measure("ABC", &st) - 4.0).abs() < 1e-4);
        // Latin-1 widths are real AFM values: é is 556 in Helvetica.
        assert!((measure("é", &st) - 5.56).abs() < 1e-4);

        assert_eq!(encode("é—ł−漢"), vec![0xE9, 0x97, b'l', b'-', b'?']);
        assert_eq!(encode("Œuvre ž"), vec![0x8C, b'u', b'v', b'r', b'e', b' ', 0x9E]);
        assert_eq!(encode("a\u{200B}b"), b"ab".to_vec());
    }

    #[test]
    fn fitting_helpers() {
        let st = TextStyle::new(Font::Bold, 30.0, Color::BLACK);
        let s = "A rather long property address that will not fit";
        let size = fit_size(s, &st, 300.0, 12.0);
        assert!((12.0..30.0).contains(&size));
        assert!(measure(s, &st.size(size)) <= 300.0);
        assert_eq!(fit_size("Short", &st, 300.0, 12.0), 30.0);

        let lines = wrap("one two three four five six seven", &TextStyle::new(Font::Regular, 10.0, Color::BLACK), 60.0);
        assert!(lines.len() > 1);
        for l in &lines {
            assert!(measure(l, &TextStyle::new(Font::Regular, 10.0, Color::BLACK)) <= 60.0, "{l}");
        }
        let e = ellipsize_middle("client_photo_0001_long_name.jpg", &st.size(7.0), 60.0);
        assert!(e.ends_with(".jpg") && e.contains('…'), "{e}");

        assert_eq!(cover_rect(300.0, 200.0, [0.0, 0.0, 100.0, 100.0]), [-25.0, 0.0, 150.0, 100.0]);
        assert_eq!(contain_rect(300.0, 200.0, [0.0, 0.0, 150.0, 150.0]), [0.0, 25.0, 150.0, 100.0]);
    }

    #[test]
    fn document_structure() {
        let mut doc = Doc::new();
        doc.set_title("Tëst — “document”");
        doc.set_author("Example Aerials");
        let png = {
            let img = image::RgbaImage::from_fn(20, 10, |x, _| image::Rgba([200, 160, 80, (x * 12) as u8]));
            let mut buf = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png).unwrap();
            buf
        };
        let logo = doc.add_png(&png).unwrap();
        let jpg = crate::imaging::encode_jpeg(&image::RgbImage::from_pixel(30, 20, image::Rgb([10, 20, 30])), 80).unwrap();
        let photo = doc.add_jpeg(jpg).unwrap();
        assert_eq!((photo.w, photo.h), (30, 20));
        assert!(doc.add_jpeg(b"nope".to_vec()).is_err());

        let p = doc.page();
        p.fill_rect(0.0, 0.0, LETTER_W, 100.0, Color::hex(0x0C0F0A));
        p.save();
        p.alpha(0.5, 0.5);
        p.stroke_color(Color::hex(0xC9A84C));
        p.dash(&[2.0, 1.0], 0.0);
        p.rounded_rect(10.0, 10.0, 100.0, 50.0, 8.0);
        p.stroke();
        p.restore();
        p.text(306.0, 50.0, "Centred (paren) \\ back", &TextStyle::new(Font::Bold, 12.0, Color::WHITE).align(Align::Center));
        p.image(&logo, 20.0, 120.0, 40.0, 20.0);
        p.image_cover(&photo, 100.0, 120.0, 50.0, 50.0);
        p.link(100.0, 200.0, 80.0, 12.0, "https://example.com/a b");
        let n = p.qr(400.0, 400.0, 72.0, "https://example.com", Color::BLACK).unwrap();
        assert!(n >= 21);
        doc.page().text(10.0, 10.0, "Two", &TextStyle::new(Font::Oblique, 9.0, Color::BLACK));

        let bytes = doc.to_bytes();
        check_pdf(&bytes);
        let s = readable(&bytes);
        assert!(s.contains("/Count 2"));
        assert!(s.contains("/SMask"));
        assert!(s.contains("/DCTDecode"));
        assert!(s.contains("/URI (https://example.com/a%20b)"));
        assert!(s.contains("/BaseFont /Helvetica-Oblique"));
        assert!(!s.contains("/BaseFont /Helvetica-BoldOblique"), "unused fonts are not emitted");
        assert!(s.contains("/ca 0.5"));
        assert!(s.contains("[(Centred \\(paren\\) \\\\ bac) 20 (k)] TJ"), "escaped, kerned text");
        assert!(unkern(&s).contains("[(Centred \\(paren\\) \\\\ back)] TJ"));
        assert!(!s.contains("/Producer"));
        assert!(s.contains(" re"));
    }
}
