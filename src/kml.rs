//! A KML 2.2 map of a flight for Google Earth: parcel outlines, one camera
//! marker per shot, the flight path, and the coverage summary.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::coverage::{Coverage, Shot};
use crate::geo::Ring;

/// Escape text for XML element content and attribute values.
pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            // Control characters other than tab/newline are not allowed in XML 1.0.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => o.push(' '),
            c => o.push(c),
        }
    }
    o
}

/// Google Earth renders `<description>` as HTML: escape each line for HTML,
/// join with `<br/>`, then escape the result for XML.
fn description(lines: &[String]) -> String {
    let html: Vec<String> = lines.iter().map(|l| escape(l)).collect();
    escape(&html.join("<br/>"))
}

fn coord(lon: f64, lat: f64, alt: f64) -> Option<String> {
    (lon.is_finite() && lat.is_finite() && alt.is_finite()).then(|| format!("{lon:.7},{lat:.7},{alt:.1}"))
}

fn shot_alt(s: &Shot) -> f64 {
    s.rel_alt_m.filter(|a| a.is_finite()).unwrap_or(0.0)
}

/// Write `out` as KML 2.2. Shots should arrive in time order: they are joined
/// into the flight path in the order given. Shot heights are drawn relative
/// to the ground using the height above take-off.
pub fn write_kml(
    out: &Path,
    title: &str,
    shots: &[Shot],
    parcels: &[(String, String, Vec<Ring>)],
    coverage: Option<&Coverage>,
) -> Result<()> {
    let mut k = String::new();
    let mut doc_desc = Vec::new();
    if let Some(c) = coverage {
        doc_desc.extend(c.summary().lines().map(str::to_string));
    }
    doc_desc.push(format!("{} geotagged shots.", shots.len()));

    k.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    k.push_str("<kml xmlns=\"http://www.opengis.net/kml/2.2\">\n<Document>\n");
    let _ = writeln!(k, "<name>{}</name>", escape(title));
    let _ = writeln!(k, "<description>{}</description>", description(&doc_desc));
    k.push_str(concat!(
        "<Style id=\"parcel\"><LineStyle><color>ff00ffff</color><width>3</width></LineStyle>",
        "<PolyStyle><color>00ffffff</color><fill>0</fill><outline>1</outline></PolyStyle></Style>\n",
        "<Style id=\"camera\"><IconStyle><scale>0.6</scale><Icon>",
        "<href>http://maps.google.com/mapfiles/kml/shapes/camera.png</href></Icon></IconStyle>",
        "<LabelStyle><scale>0</scale></LabelStyle></Style>\n",
        "<Style id=\"path\"><LineStyle><color>ffffff00</color><width>2</width></LineStyle></Style>\n",
    ));

    k.push_str("<Folder>\n<name>Parcels</name>\n");
    for (apn, label, rings) in parcels {
        let _ = writeln!(
            k,
            "<Placemark>\n<name>{}</name>\n<description>{}</description>\n<styleUrl>#parcel</styleUrl>\n<MultiGeometry>",
            escape(apn),
            description(std::slice::from_ref(label))
        );
        // Each ring is drawn as its own outline: rings are stored without
        // outer/hole roles and the fill is transparent, so this is exact.
        for r in rings {
            let mut pts: Vec<String> = r.iter().filter_map(|&(lon, lat)| coord(lon, lat, 0.0)).collect();
            if pts.len() < 3 {
                continue;
            }
            if pts.first() != pts.last() {
                pts.push(pts[0].clone());
            }
            let _ = writeln!(
                k,
                "<Polygon><tessellate>1</tessellate><outerBoundaryIs><LinearRing><coordinates>{}</coordinates></LinearRing></outerBoundaryIs></Polygon>",
                pts.join(" ")
            );
        }
        k.push_str("</MultiGeometry>\n</Placemark>\n");
    }
    k.push_str("</Folder>\n");

    k.push_str("<Folder>\n<name>Camera positions</name>\n");
    for s in shots {
        let Some(c) = coord(s.lon, s.lat, shot_alt(s)) else { continue };
        let mut d = Vec::new();
        match s.rel_alt_m {
            Some(a) => d.push(format!("Altitude: {a:.1} m above take-off")),
            None => d.push("Altitude: unknown".to_string()),
        }
        if let Some(a) = s.abs_alt_m {
            d.push(format!("Absolute altitude: {a:.1} m"));
        }
        match s.gimbal_pitch {
            Some(p) => d.push(format!("Gimbal pitch: {p:.1}°")),
            None => d.push("Gimbal pitch: unknown".to_string()),
        }
        if let Some(y) = s.gimbal_yaw {
            d.push(format!("Heading: {:.0}°", y.rem_euclid(360.0)));
        }
        let mode = if s.rel_alt_m.is_some() { "relativeToGround" } else { "clampToGround" };
        let _ = writeln!(
            k,
            "<Placemark>\n<name>{}</name>\n<description>{}</description>\n<styleUrl>#camera</styleUrl>\n\
             <Point><altitudeMode>{mode}</altitudeMode><coordinates>{c}</coordinates></Point>\n</Placemark>",
            escape(&s.name),
            description(&d)
        );
    }
    k.push_str("</Folder>\n");

    let path: Vec<String> = shots.iter().filter_map(|s| coord(s.lon, s.lat, shot_alt(s))).collect();
    if path.len() >= 2 {
        let _ = writeln!(
            k,
            "<Placemark>\n<name>Flight path</name>\n<styleUrl>#path</styleUrl>\n<LineString><tessellate>1</tessellate>\
             <altitudeMode>relativeToGround</altitudeMode><coordinates>{}</coordinates></LineString>\n</Placemark>",
            path.join(" ")
        );
    }
    k.push_str("</Document>\n</kml>\n");

    let tmp = out.with_extension("kml.partial");
    std::fs::write(&tmp, k).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, out).with_context(|| format!("writing {}", out.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::coverage;

    fn shot(name: &str, lat: f64, lon: f64, alt: Option<f64>, pitch: Option<f64>) -> Shot {
        Shot {
            name: name.into(),
            lat,
            lon,
            rel_alt_m: alt,
            abs_alt_m: None,
            gimbal_pitch: pitch,
            gimbal_yaw: Some(180.0),
            focal_35mm: Some(24.0),
            width: Some(6000),
            height: Some(4000),
        }
    }

    /// Minimal well-formedness check: every tag closes in order.
    fn balanced(xml: &str) -> Result<usize, String> {
        let mut stack: Vec<&str> = Vec::new();
        let mut elements = 0;
        let mut rest = xml;
        while let Some(i) = rest.find('<') {
            let after = &rest[i + 1..];
            let end = after.find('>').ok_or("unterminated tag")?;
            let tag = &after[..end];
            rest = &after[end + 1..];
            if tag.starts_with('?') || tag.starts_with('!') {
                continue;
            }
            if let Some(name) = tag.strip_prefix('/') {
                match stack.pop() {
                    Some(open) if open == name => {}
                    other => return Err(format!("</{name}> closes {other:?}")),
                }
            } else if !tag.ends_with('/') {
                stack.push(tag.split_whitespace().next().unwrap_or(""));
                elements += 1;
            }
        }
        if stack.is_empty() { Ok(elements) } else { Err(format!("unclosed: {stack:?}")) }
    }

    #[test]
    fn kml_is_well_formed() {
        // Fictional parcel and shots near 31.91, -110.01.
        let ring = vec![(-110.011, 31.909), (-110.009, 31.909), (-110.009, 31.911), (-110.011, 31.911)];
        let parcels = vec![("999-12-345A".to_string(), "100 Example Rd <test> & \"co\"".to_string(), vec![ring.clone()])];
        let shots = vec![
            shot("DJI_0001.JPG", 31.912, -110.010, Some(40.0), Some(-30.0)),
            shot("DJI_0002.JPG", 31.912, -110.0095, Some(41.5), Some(-30.0)),
            shot("A&B<1>.MP4", 31.9115, -110.0090, None, None),
        ];
        let cov = coverage(&shots, &[ring]);
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("flight.kml");
        write_kml(&out, "Flight 1 & <2>", &shots, &parcels, Some(&cov)).unwrap();
        let xml = std::fs::read_to_string(&out).unwrap();

        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<kml xmlns=\"http://www.opengis.net/kml/2.2\">"));
        balanced(&xml).unwrap();
        assert_eq!(xml.matches("<Placemark>").count(), 1 + 3 + 1);
        assert!(xml.contains("<name>999-12-345A</name>"));
        assert!(xml.contains("<name>Flight 1 &amp; &lt;2&gt;</name>"));
        assert!(xml.contains("<name>A&amp;B&lt;1&gt;.MP4</name>"));
        // HTML-escaped, then XML-escaped: shows literally in Google Earth.
        assert!(xml.contains("100 Example Rd &amp;lt;test&amp;gt; &amp;amp; &amp;quot;co&amp;quot;"));
        assert!(xml.contains("<LineString>"));
        assert!(xml.contains("-110.0100000,31.9120000,40.0 -110.0095000,31.9120000,41.5"));
        // Rings are closed.
        assert!(xml.contains("-110.0110000,31.9090000,0.0</coordinates>"));
        assert!(xml.contains("<description>Oblique: 3 shots"), "{}", cov.summary());
        assert!(!dir.path().join("flight.kml.partial").exists());
    }

    #[test]
    fn balanced_checker_catches_errors() {
        assert!(balanced("<a><b></a></b>").is_err());
        assert!(balanced("<a><b/>").is_err());
        assert_eq!(balanced("<?xml?><a x=\"1\"><b/></a>"), Ok(1));
    }
}
