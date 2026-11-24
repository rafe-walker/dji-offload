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
    /// After an offload, ask who each new flight is for (client, library,
    /// promo): "ask" or "never". Tag later with `dji-offload tag`.
    pub ask_job: String,
    /// Long edge in pixels of the web-sized JPEGs in a client Delivery folder.
    pub delivery_long_edge: u32,
    /// Text stamped on library samples sent to prospective buyers.
    pub watermark_text: String,
    /// How many watermarked samples to make per library flight.
    pub library_sample_count: usize,
    /// Run the blur/exposure and coverage checks after each offload.
    pub field_checks: bool,
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
            ask_job: "ask".into(),
            delivery_long_edge: 2560,
            watermark_text: "SAMPLE".into(),
            library_sample_count: 6,
            field_checks: true,
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
    pub fn save(&self) -> Result<()> {
        fs::write(config_path()?, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Change one setting by a friendly name. Returns a confirmation line.
    pub fn set(&mut self, key: &str, value: &str) -> Result<String> {
        let yes = |v: &str| -> Result<bool> {
            match v.to_lowercase().as_str() {
                "on" | "yes" | "true" => Ok(true),
                "off" | "no" | "false" => Ok(false),
                _ => anyhow::bail!("use on or off"),
            }
        };
        let msg = match key {
            "wipe" => {
                let v = value.to_lowercase();
                if !matches!(v.as_str(), "never" | "ask" | "always") {
                    anyhow::bail!("wipe must be never, ask or always");
                }
                self.wipe_after_offload = v.clone();
                format!("Drone wipe after offload: {v}")
            }
            "gap" => {
                self.flight_gap_minutes = value.parse().context("gap is a number of minutes")?;
                format!("New flight after a gap of {} min", self.flight_gap_minutes)
            }
            "low-space" => {
                self.low_space_warn_gb = value.parse().context("low-space is a number of GB")?;
                format!("Low-space warning under {} GB", self.low_space_warn_gb)
            }
            "eject" => {
                self.eject_when_done = yes(value)?;
                format!("Eject after offload: {}", if self.eject_when_done { "on" } else { "off" })
            }
            "parcels" => {
                self.parcels = yes(value)?;
                format!("Parcel tagging: {}", if self.parcels { "on" } else { "off" })
            }
            "ask-job" => {
                let v = value.to_lowercase();
                if !matches!(v.as_str(), "ask" | "never") {
                    anyhow::bail!("ask-job must be ask or never");
                }
                self.ask_job = v.clone();
                format!("Ask who each flight is for: {v}")
            }
            "watermark" => {
                self.watermark_text = value.to_string();
                format!("Library sample watermark: {value}")
            }
            "checks" => {
                self.field_checks = yes(value)?;
                format!("Blur/exposure and coverage checks: {}", if self.field_checks { "on" } else { "off" })
            }
            "destination" => {
                self.destination = PathBuf::from(value);
                format!("Main copy goes to {}", self.destination.display())
            }
            _ => anyhow::bail!("unknown setting `{key}`; try wipe, ask-job, watermark, checks, gap, low-space, eject, parcels or destination"),
        };
        self.save()?;
        Ok(msg)
    }

    pub fn load() -> Result<Self> {
        let p = config_path()?;
        if !p.exists() {
            let c = Config::default();
            fs::write(&p, toml::to_string_pretty(&c)?)?;
            return Ok(c);
        }
        let text = fs::read_to_string(&p)?;
        let c: Config = toml::from_str(&text).with_context(|| format!("reading {}", p.display()))?;
        // After an upgrade, write any new settings into the file (with their
        // defaults) so they are visible and editable.
        let full = toml::to_string_pretty(&c)?;
        let missing = full
            .lines()
            .filter_map(|l| l.split_once(" = ").map(|(k, _)| k.trim()))
            .any(|k| !text.lines().any(|l| l.trim_start().starts_with(&format!("{k} ")) || l.trim_start().starts_with(&format!("{k}="))));
        if missing {
            let _ = fs::write(&p, full);
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_fills_in_new_settings() {
        let d = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("DJI_OFFLOAD_HOME", d.path()) };
        fs::write(d.path().join("config.toml"), "destination = \"/tmp/x\"\nflight_gap_minutes = 12\n").unwrap();
        let c = Config::load().unwrap();
        assert_eq!((c.flight_gap_minutes, c.wipe_after_offload.as_str()), (12, "never"));
        let text = fs::read_to_string(d.path().join("config.toml")).unwrap();
        assert!(text.contains("wipe_after_offload") && text.contains("flight_gap_minutes = 12"), "{text}");

        let mut c = Config::load().unwrap();
        c.set("wipe", "Ask").unwrap();
        assert!(c.set("wipe", "sometimes").is_err());
        c.set("eject", "off").unwrap();
        let c = Config::load().unwrap();
        assert_eq!((c.wipe_after_offload.as_str(), c.eject_when_done), ("ask", false));
    }
}
