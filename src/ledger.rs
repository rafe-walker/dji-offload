//! What has already been copied, and which flight each file belongs to.
//! Lives in a small SQLite file so it survives reboots and partial runs.

use std::path::Path;

use anyhow::Result;
use chrono::{NaiveDate, NaiveDateTime};
use rusqlite::{Connection, OptionalExtension, params};

use crate::flights::Flight;
use crate::geo::Place;

pub struct Ledger {
    db: Connection,
}

const DT: &str = "%Y-%m-%d %H:%M:%S";

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS copied(dest TEXT, key TEXT, rel TEXT, hash TEXT, at TEXT, PRIMARY KEY(dest, key));
             CREATE TABLE IF NOT EXISTS flights(date TEXT, num INTEGER, start TEXT, end TEXT, folder TEXT, place TEXT, PRIMARY KEY(date, num));
             CREATE TABLE IF NOT EXISTS file_flight(key TEXT PRIMARY KEY, date TEXT, num INTEGER);
             CREATE TABLE IF NOT EXISTS hash_cache(path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, hash TEXT);",
        )?;
        Ok(Self { db })
    }

    pub fn is_copied(&self, dest: &str, key: &str) -> Result<bool> {
        Ok(self
            .db
            .query_row("SELECT 1 FROM copied WHERE dest=?1 AND key=?2", params![dest, key], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// (relative path, hash) of a verified copy at `dest`.
    pub fn copy_record(&self, dest: &str, key: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .db
            .query_row("SELECT rel, hash FROM copied WHERE dest=?1 AND key=?2", params![dest, key], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?)
    }

    /// Hash of `path`, reusing the cached value while size and mtime are unchanged.
    pub fn cached_hash(&self, path: &Path) -> Result<String> {
        let meta = std::fs::metadata(path)?;
        let size = meta.len() as i64;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let key = path.display().to_string();
        let hit: Option<String> = self
            .db
            .query_row(
                "SELECT hash FROM hash_cache WHERE path=?1 AND size=?2 AND mtime=?3",
                params![key, size, mtime],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(h) = hit {
            return Ok(h);
        }
        let h = crate::copy::hash_hex(path)?;
        self.db.execute(
            "INSERT OR REPLACE INTO hash_cache VALUES (?1, ?2, ?3, ?4)",
            params![key, size, mtime, h],
        )?;
        Ok(h)
    }

    pub fn record_copy(&self, dest: &str, key: &str, rel: &str, hash: &str) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO copied VALUES (?1, ?2, ?3, ?4, ?5)",
            params![dest, key, rel, hash, chrono::Local::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn flights_on(&self, date: NaiveDate) -> Result<Vec<(Flight, Place)>> {
        let mut st = self
            .db
            .prepare("SELECT num, start, end, folder, place FROM flights WHERE date=?1 ORDER BY num")?;
        let rows = st.query_map([date.to_string()], |r| {
            Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get(3)?, r.get::<_, Option<String>>(4)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (num, start, end, folder, place) = row?;
            out.push((
                Flight {
                    date,
                    num,
                    start: NaiveDateTime::parse_from_str(&start, DT)?,
                    end: NaiveDateTime::parse_from_str(&end, DT)?,
                    folder,
                },
                place.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default(),
            ));
        }
        Ok(out)
    }

    pub fn save_flight(&self, f: &Flight, place: &Place) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO flights VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                f.date.to_string(),
                f.num,
                f.start.format(DT).to_string(),
                f.end.format(DT).to_string(),
                f.folder,
                serde_json::to_string(place)?
            ],
        )?;
        Ok(())
    }

    pub fn file_flight(&self, key: &str) -> Result<Option<(NaiveDate, u32)>> {
        let row: Option<(String, u32)> = self
            .db
            .query_row("SELECT date, num FROM file_flight WHERE key=?1", [key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        Ok(match row {
            Some((d, n)) => Some((d.parse()?, n)),
            None => None,
        })
    }

    pub fn set_file_flight(&self, key: &str, date: NaiveDate, num: u32) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO file_flight VALUES (?1, ?2, ?3)",
            params![key, date.to_string(), num],
        )?;
        Ok(())
    }

    pub fn begin(&self) -> Result<()> {
        self.db.execute_batch("BEGIN")?;
        Ok(())
    }
    pub fn commit(&self) -> Result<()> {
        self.db.execute_batch("COMMIT")?;
        Ok(())
    }
}
