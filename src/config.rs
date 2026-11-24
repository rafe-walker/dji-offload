//! Settings, stored as TOML next to the ledger and parcel database.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Main copy, always written. Dated folders go inside.
    pub destination: PathBuf,
    /// Folder name used on marked external drives (`<drive>/<this>/<date>/...`).
    pub external_folder: String,
    /// A gap longer than this between files starts a new flight.
    pub flight_gap_minutes: i64,
    /// Eject the drone after a clean offload so it is safe to unplug.
    pub eject_when_done: bool,
    /// Extensions not worth copying. LRF files are DJI's low-res previews.
    pub skip_extensions: Vec<String>,
    /// Tag flights with parcel/APN, address and city from the offline database.
    pub parcels: bool,
    /// Refresh the parcel database when it is older than this (needs internet).
    pub parcel_refresh_days: i64,
    /// Where drives mount.
    pub volumes_root: PathBuf,
    /// Clear the drone after a clean offload: "never", "ask" (dialog) or "always".
    /// Files are deleted only when every connected destination has a verified copy.
    pub wipe_after_offload: String,
    /// Warn when a destination drops below this much free space.
    pub low_space_warn_gb: f64,
    /// Where backup mode looks for DJI files on this Mac.
    pub backup_scan_roots: Vec<PathBuf>,
    /// Folder created on backup drives.
    pub backup_folder: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            destination: dirs::home_dir().unwrap_or_default().join("Drone"),
            external_folder: "Drone".into(),
            flight_gap_minutes: 10,
            eject_when_done: true,
            skip_extensions: vec!["LRF".into()],
            parcels: true,
            parcel_refresh_days: 30,
            volumes_root: PathBuf::from("/Volumes"),
            wipe_after_offload: "never".into(),
            low_space_warn_gb: 20.0,
            backup_scan_roots: vec![dirs::home_dir().unwrap_or_default()],
            backup_folder: "Drone Backup".into(),
        }
    }
}

/// `~/Library/Application Support/dji-offload` on macOS.
pub fn state_dir() -> Result<PathBuf> {
    let d = std::env::var_os("DJI_OFFLOAD_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|d| d.join("dji-offload")))
        .context("no data directory")?;
    fs::create_dir_all(&d)?;
    Ok(d)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("config.toml"))
}

impl Config {
    pub fn load() -> Result<Self> {
        let p = config_path()?;
        if !p.exists() {
            let c = Config::default();
            fs::write(&p, toml::to_string_pretty(&c)?)?;
            return Ok(c);
        }
        let text = fs::read_to_string(&p)?;
        toml::from_str(&text).with_context(|| format!("reading {}", p.display()))
    }
}
