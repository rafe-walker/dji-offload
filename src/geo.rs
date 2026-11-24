//! Offline parcel and city-limits lookup.
//!
//! `update` downloads every Cochise County parcel (from the county's own
//! ArcGIS service) and every Arizona incorporated city boundary into one local
//! SQLite file with R-tree indexes. Lookups never touch the network, so the
//! tool works at a rural shoot with no signal.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde_json::Value;

/// Cochise County Assessor parcels (tax info layer), updated by the county.
pub const PARCELS_URL: &str = "https://services6.arcgis.com/Yxem0VOcqSy8T6TE/arcgis/rest/services/Cad_Parcel_TaxInfo/FeatureServer/0";
/// Arizona incorporated city/town boundaries (AZ Dept. of Revenue via ADEQ).
pub const CITIES_URL: &str = "https://services.arcgis.com/SzoH1oFM2apCSkx3/ArcGIS/rest/services/CITIES_AGOL/FeatureServer/0";

const PAGE: usize = 2000;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Parcel {
    pub apn: String,
    pub situs: Option<String>,
    pub acres: Option<f64>,
}

/// What we know about where a flight happened.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Place {
    /// (parcel, number of files that fell inside it), most files first.
    pub parcels: Vec<(Parcel, usize)>,
    /// Incorporated city/town, if the flight centre is inside one.
    pub city: Option<String>,
    /// Mean position of all geotagged files.
    pub centre: Option<(f64, f64)>,
    pub geotagged: usize,
}

impl Place {
    pub fn main(&self) -> Option<&Parcel> {
        self.parcels.first().map(|(p, _)| p)
    }
    pub fn city_label(&self) -> String {
        match &self.city {
            Some(c) => c.clone(),
            None => "Unincorporated Cochise County".into(),
        }
    }
}

/// Cochise APNs come back as `99912345A`; the county writes them `999-12-345A`.
pub fn format_apn(raw: &str) -> String {
    let s = raw.trim();
    if s.len() < 6 || !s[..5].chars().all(|c| c.is_ascii_digit()) {
        return s.to_string();
    }
    format!("{}-{}-{}", &s[..3], &s[3..5], &s[5..])
}

pub fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let lower = w.to_lowercase();
            let mut c = lower.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Collapse the padding the county leaves in addresses: `"601  ARIZONA ST  "`.
fn clean(s: &str) -> Option<String> {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() { None } else { Some(t) }
}

// ---------- geometry ----------

type Ring = Vec<(f64, f64)>; // (lon, lat)

fn rings_of(geom: &Value) -> Vec<Ring> {
    let ring = |r: &Value| -> Ring {
        r.as_array()
            .map(|pts| {
                pts.iter()
                    .filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
                    .collect()
            })
            .unwrap_or_default()
    };
    let coords = &geom["coordinates"];
    match geom["type"].as_str() {
        Some("Polygon") => coords.as_array().map(|rs| rs.iter().map(ring).collect()).unwrap_or_default(),
        Some("MultiPolygon") => coords
            .as_array()
            .map(|ps| ps.iter().flat_map(|p| p.as_array().into_iter().flatten().map(ring)).collect())
            .unwrap_or_default(),
        _ => vec![],
    }
}

fn encode(rings: &[Ring]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend((rings.len() as u32).to_le_bytes());
    for r in rings {
        b.extend((r.len() as u32).to_le_bytes());
        for (x, y) in r {
            b.extend(x.to_le_bytes());
            b.extend(y.to_le_bytes());
        }
    }
    b
}

fn decode(b: &[u8]) -> Vec<Ring> {
    let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as usize;
    let f64_at = |i: usize| f64::from_le_bytes(b[i..i + 8].try_into().unwrap());
    let mut out = Vec::new();
    if b.len() < 4 {
        return out;
    }
    let n = u32_at(0);
    let mut i = 4;
    for _ in 0..n {
        let m = u32_at(i);
        i += 4;
        let mut r = Vec::with_capacity(m);
        for _ in 0..m {
            r.push((f64_at(i), f64_at(i + 8)));
            i += 16;
        }
        out.push(r);
    }
    out
}

fn bbox(rings: &[Ring]) -> Option<(f64, f64, f64, f64)> {
    let mut it = rings.iter().flatten();
    let &(x0, y0) = it.next()?;
    Some(it.fold((x0, x0, y0, y0), |(a, b, c, d), &(x, y)| (a.min(x), b.max(x), c.min(y), d.max(y))))
}

/// Even-odd point-in-polygon across all rings: handles holes and multi-part parcels.
pub fn contains(rings: &[Ring], lon: f64, lat: f64) -> bool {
    let mut inside = false;
    for r in rings {
        let n = r.len();
        if n < 3 {
            continue;
        }
        let mut j = n - 1;
        for i in 0..n {
            let (xi, yi) = r[i];
            let (xj, yj) = r[j];
            if (yi > lat) != (yj > lat) && lon < (xj - xi) * (lat - yi) / (yj - yi) + xi {
                inside = !inside;
            }
            j = i;
        }
    }
    inside
}

// ---------- database ----------

fn schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT);
         CREATE TABLE IF NOT EXISTS parcels(id INTEGER PRIMARY KEY, apn TEXT NOT NULL, situs TEXT, acres REAL, geom BLOB NOT NULL);
         CREATE VIRTUAL TABLE IF NOT EXISTS parcels_rt USING rtree(id, minx, maxx, miny, maxy);
         CREATE TABLE IF NOT EXISTS cities(id INTEGER PRIMARY KEY, name TEXT NOT NULL, geom BLOB NOT NULL);
         CREATE VIRTUAL TABLE IF NOT EXISTS cities_rt USING rtree(id, minx, maxx, miny, maxy);",
    )?;
    Ok(())
}

/// Insert one page of GeoJSON features. Returns how many were stored.
pub fn insert_parcels(db: &Connection, page: &Value) -> Result<usize> {
    let mut n = 0;
    let tx = db.unchecked_transaction()?;
    for f in page["features"].as_array().into_iter().flatten() {
        let p = &f["properties"];
        let Some(apn) = p["apn"].as_str().map(str::trim).filter(|s| !s.is_empty()) else { continue };
        let rings = rings_of(&f["geometry"]);
        let Some((minx, maxx, miny, maxy)) = bbox(&rings) else { continue };
        tx.execute(
            "INSERT INTO parcels(apn, situs, acres, geom) VALUES (?1, ?2, ?3, ?4)",
            params![apn, p["situs_address"].as_str().and_then(clean), p["acres"].as_f64(), encode(&rings)],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO parcels_rt VALUES (?1, ?2, ?3, ?4, ?5)", params![id, minx, maxx, miny, maxy])?;
        n += 1;
    }
    tx.commit()?;
    Ok(n)
}

pub fn insert_cities(db: &Connection, page: &Value) -> Result<usize> {
    let mut n = 0;
    let tx = db.unchecked_transaction()?;
    for f in page["features"].as_array().into_iter().flatten() {
        let Some(name) = f["properties"]["NAME"].as_str().and_then(clean) else { continue };
        let rings = rings_of(&f["geometry"]);
        let Some((minx, maxx, miny, maxy)) = bbox(&rings) else { continue };
        tx.execute("INSERT INTO cities(name, geom) VALUES (?1, ?2)", params![title_case(&name), encode(&rings)])?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO cities_rt VALUES (?1, ?2, ?3, ?4, ?5)", params![id, minx, maxx, miny, maxy])?;
        n += 1;
    }
    tx.commit()?;
    Ok(n)
}

fn fetch_all(
    agent: &ureq::Agent,
    base: &str,
    fields: &str,
    mut store: impl FnMut(&Value) -> Result<usize>,
    progress: &mut dyn FnMut(usize),
) -> Result<usize> {
    let mut offset = 0;
    let mut total = 0;
    loop {
        let url = format!(
            "{base}/query?where=1%3D1&outFields={fields}&outSR=4326&orderByFields=OBJECTID\
             &resultOffset={offset}&resultRecordCount={PAGE}&f=geojson"
        );
        let mut tries = 0;
        let page: Value = loop {
            tries += 1;
            match agent.get(&url).call().and_then(|mut r| r.body_mut().with_config().limit(64 << 20).read_to_string()) {
                Ok(body) => break serde_json::from_str(&body).context("parsing parcel page")?,
                Err(e) if tries < 4 => {
                    std::thread::sleep(Duration::from_secs(2 * tries));
                    let _ = e;
                }
                Err(e) => return Err(e).with_context(|| format!("downloading {base} at offset {offset}")),
            }
        };
        if let Some(err) = page.get("error") {
            bail!("server error from {base}: {err}");
        }
        let got = page["features"].as_array().map_or(0, Vec::len);
        total += store(&page)?;
        progress(total);
        let more = page["properties"]["exceededTransferLimit"].as_bool().unwrap_or(false)
            || page["exceededTransferLimit"].as_bool().unwrap_or(false);
        if got == 0 || (!more && got < PAGE) {
            break;
        }
        offset += got;
    }
    Ok(total)
}

/// Download parcels and city limits into `db_path` (written to a temp file and
/// swapped in at the end, so a failed update never breaks the old copy).
pub fn update(db_path: &Path, progress: &mut dyn FnMut(&str, usize)) -> Result<(usize, usize)> {
    let tmp = db_path.with_extension("db.partial");
    let _ = std::fs::remove_file(&tmp);
    let db = Connection::open(&tmp)?;
    db.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;")?;
    schema(&db)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(180)))
        .build()
        .into();
    let parcels = fetch_all(&agent, PARCELS_URL, "apn,situs_address,acres", |p| insert_parcels(&db, p), &mut |n| {
        progress("parcels", n)
    })?;
    let cities = fetch_all(&agent, CITIES_URL, "NAME", |p| insert_cities(&db, p), &mut |n| progress("cities", n))?;
    if parcels < 1000 {
        bail!("only {parcels} parcels downloaded; keeping the previous copy");
    }
    let now = chrono::Local::now().to_rfc3339();
    db.execute("INSERT OR REPLACE INTO meta VALUES ('updated', ?1)", [&now])?;
    db.execute("INSERT OR REPLACE INTO meta VALUES ('parcels', ?1)", [parcels.to_string()])?;
    db.execute("INSERT OR REPLACE INTO meta VALUES ('cities', ?1)", [cities.to_string()])?;
    drop(db);
    std::fs::rename(&tmp, db_path)?;
    Ok((parcels, cities))
}

/// Read-only handle on the local parcel database.
pub struct Lookup {
    db: Connection,
}

impl Lookup {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self { db })
    }

    /// When the local copy was last refreshed (RFC 3339).
    pub fn updated(&self) -> Option<String> {
        self.db.query_row("SELECT v FROM meta WHERE k='updated'", [], |r| r.get(0)).ok()
    }

    pub fn parcel(&self, lat: f64, lon: f64) -> Result<Option<Parcel>> {
        let mut st = self.db.prepare_cached(
            "SELECT p.apn, p.situs, p.acres, p.geom FROM parcels_rt r JOIN parcels p ON p.id = r.id
             WHERE r.minx <= ?1 AND r.maxx >= ?1 AND r.miny <= ?2 AND r.maxy >= ?2",
        )?;
        let mut rows = st.query(params![lon, lat])?;
        while let Some(r) = rows.next()? {
            let geom: Vec<u8> = r.get(3)?;
            if contains(&decode(&geom), lon, lat) {
                let apn: String = r.get(0)?;
                return Ok(Some(Parcel { apn: format_apn(&apn), situs: r.get(1)?, acres: r.get(2)? }));
            }
        }
        Ok(None)
    }

    pub fn city(&self, lat: f64, lon: f64) -> Result<Option<String>> {
        let mut st = self.db.prepare_cached(
            "SELECT c.name, c.geom FROM cities_rt r JOIN cities c ON c.id = r.id
             WHERE r.minx <= ?1 AND r.maxx >= ?1 AND r.miny <= ?2 AND r.maxy >= ?2",
        )?;
        let mut rows = st.query(params![lon, lat])?;
        while let Some(r) = rows.next()? {
            let geom: Vec<u8> = r.get(1)?;
            if contains(&decode(&geom), lon, lat) {
                return Ok(Some(r.get(0)?));
            }
        }
        Ok(None)
    }

    /// Summarise where a set of GPS fixes landed.
    pub fn place(&self, fixes: &[(f64, f64)]) -> Result<Place> {
        let mut counts: BTreeMap<String, (Parcel, usize)> = BTreeMap::new();
        for &(lat, lon) in fixes {
            if let Some(p) = self.parcel(lat, lon)? {
                counts.entry(p.apn.clone()).or_insert((p, 0)).1 += 1;
            }
        }
        let mut parcels: Vec<(Parcel, usize)> = counts.into_values().collect();
        parcels.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.apn.cmp(&b.0.apn)));
        let centre = centre(fixes);
        let city = match centre {
            Some((lat, lon)) => self.city(lat, lon)?,
            None => None,
        };
        Ok(Place { parcels, city, centre, geotagged: fixes.len() })
    }
}

impl Place {
    /// Fold a later download's findings into what we stored for the flight.
    pub fn merge(&self, newer: &Place) -> Place {
        let mut counts: BTreeMap<String, (Parcel, usize)> = BTreeMap::new();
        for (p, n) in self.parcels.iter().chain(newer.parcels.iter()) {
            counts.entry(p.apn.clone()).or_insert((p.clone(), 0)).1 += n;
        }
        let mut parcels: Vec<(Parcel, usize)> = counts.into_values().collect();
        parcels.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.apn.cmp(&b.0.apn)));
        let total = self.geotagged + newer.geotagged;
        let centre = match (self.centre, newer.centre) {
            (Some((a, b)), Some((c, d))) if total > 0 => {
                let (w1, w2) = (self.geotagged as f64, newer.geotagged as f64);
                Some(((a * w1 + c * w2) / (w1 + w2), (b * w1 + d * w2) / (w1 + w2)))
            }
            (x, y) => x.or(y),
        };
        Place { parcels, city: self.city.clone().or(newer.city.clone()), centre, geotagged: total }
    }
}

pub fn centre(fixes: &[(f64, f64)]) -> Option<(f64, f64)> {
    if fixes.is_empty() {
        return None;
    }
    let n = fixes.len() as f64;
    let (a, b) = fixes.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
    Some((a / n, b / n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn square(x0: f64, y0: f64, d: f64) -> Value {
        json!([[[x0, y0], [x0 + d, y0], [x0 + d, y0 + d], [x0, y0 + d], [x0, y0]]])
    }

    #[test]
    fn apn_format() {
        assert_eq!(format_apn("99912345A"), "999-12-345A");
        assert_eq!(format_apn("10102001"), "101-02-001");
        assert_eq!(format_apn("ABC"), "ABC");
    }

    #[test]
    fn polygon_with_hole() {
        let rings = vec![
            vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
            vec![(4.0, 4.0), (6.0, 4.0), (6.0, 6.0), (4.0, 6.0)],
        ];
        assert!(contains(&rings, 1.0, 1.0));
        assert!(!contains(&rings, 5.0, 5.0));
        assert!(!contains(&rings, 11.0, 5.0));
        assert_eq!(decode(&encode(&rings)), rings);
    }

    #[test]
    fn lookup_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.db");
        let db = Connection::open(&path).unwrap();
        schema(&db).unwrap();
        let parcels = json!({"features": [
            {"properties": {"apn": "99912345A", "situs_address": "100 EXAMPLE RD  ", "acres": 4.01},
             "geometry": {"type": "Polygon", "coordinates": square(-110.013, 31.911, 0.003)}},
            {"properties": {"apn": "99912346", "situs_address": "   ", "acres": 4.0},
             "geometry": {"type": "MultiPolygon", "coordinates": [square(-110.010, 31.911, 0.003)]}}
        ]});
        assert_eq!(insert_parcels(&db, &parcels).unwrap(), 2);
        let cities = json!({"features": [
            {"properties": {"NAME": "SIERRA VISTA"}, "geometry": {"type": "Polygon", "coordinates": square(-110.4, 31.5, 0.2)}}
        ]});
        insert_cities(&db, &cities).unwrap();
        drop(db);
        let l = Lookup::open(&path).unwrap();
        let p = l.parcel(31.912345, -110.012345).unwrap().unwrap();
        assert_eq!(p.apn, "999-12-345A");
        assert_eq!(p.situs.as_deref(), Some("100 EXAMPLE RD"));
        assert_eq!(l.parcel(31.912, -110.009).unwrap().unwrap().situs, None);
        assert_eq!(l.parcel(31.0, -109.0).unwrap(), None);
        assert_eq!(l.city(31.55, -110.28).unwrap().as_deref(), Some("Sierra Vista"));
        let place = l.place(&[(31.9123, -110.0123), (31.9120, -110.0121), (31.9121, -110.0091)]).unwrap();
        assert_eq!(place.main().unwrap().apn, "999-12-345A");
        assert_eq!(place.parcels.len(), 2);
        assert_eq!(place.city_label(), "Unincorporated Cochise County");
    }
}
