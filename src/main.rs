//! dji-offload: plug in a DJI drone, and its photos and videos are copied,
//! verified and sorted into dated, per-flight folders tagged with the parcel
//! they were shot over.

mod config;
mod copy;
mod detect;
mod flights;
mod geo;
mod ledger;
mod macos;
mod media;
mod offload;
mod report;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use config::{Config, state_dir};

#[derive(Parser)]
#[command(version, about = "Auto-offload DJI drone media into dated, per-flight, parcel-tagged folders")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Offload every mounted DJI drone or card now.
    Run {
        /// Use this volume instead of scanning /Volumes.
        #[arg(long)]
        source: Option<PathBuf>,
        /// Show what would be copied without copying.
        #[arg(long)]
        dry_run: bool,
        /// Don't eject the drone afterwards.
        #[arg(long)]
        no_eject: bool,
        /// Mode used by the launchd agent: quiet when no drone is present.
        #[arg(long, hide = true)]
        auto: bool,
    },
    /// Offline parcel database.
    Parcels {
        #[command(subcommand)]
        cmd: ParcelCmd,
    },
    /// Mark an external drive so every offload is also copied onto it.
    MarkDrive { path: PathBuf },
    /// Stop copying onto a previously marked drive.
    UnmarkDrive { path: PathBuf },
    /// Run automatically whenever the drone is plugged in (macOS launchd agent).
    Install,
    /// Remove the launchd agent. Copied footage is never touched.
    Uninstall,
    /// Show settings, destinations and parcel database age.
    Status,
}

#[derive(Subcommand)]
enum ParcelCmd {
    /// Download the latest Cochise County parcels and city limits (needs internet).
    Update,
    /// Look up the parcel at a point, offline.
    Lookup { lat: f64, lon: f64 },
}

fn parcel_db() -> Result<PathBuf> {
    Ok(state_dir()?.join("parcels.db"))
}

fn open_lookup(cfg: &Config) -> Option<geo::Lookup> {
    if !cfg.parcels {
        return None;
    }
    let p = parcel_db().ok()?;
    if !p.exists() {
        return None;
    }
    geo::Lookup::open(&p).ok()
}

/// One offload at a time; launchd fires once per mount.
struct Lock(PathBuf);
impl Lock {
    fn take() -> Result<Option<Lock>> {
        let p = state_dir()?.join("run.lock");
        if let Ok(pid) = fs::read_to_string(&p) {
            let alive = Command::new("kill")
                .args(["-0", pid.trim()])
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if alive {
                return Ok(None);
            }
        }
        fs::write(&p, std::process::id().to_string())?;
        Ok(Some(Lock(p)))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn run(source: Option<PathBuf>, dry_run: bool, no_eject: bool, auto: bool) -> Result<()> {
    let cfg = Config::load()?;
    let Some(_lock) = Lock::take()? else {
        if !auto {
            println!("Another offload is already running.");
        }
        return Ok(());
    };
    if auto {
        std::thread::sleep(Duration::from_secs(3)); // let the volume settle
    }
    let sources = match source {
        Some(p) => vec![detect::inspect(&p).with_context(|| format!("{} has no DJI DCIM folder", p.display()))?],
        None => detect::sources(&cfg.volumes_root),
    };
    if sources.is_empty() {
        if !auto {
            println!("No DJI drone or card found under {}.", cfg.volumes_root.display());
        }
        return Ok(());
    }
    let dests = offload::destinations(&cfg);
    let ledger = ledger::Ledger::open(&state_dir()?.join("ledger.db"))?;
    let lookup = open_lookup(&cfg);
    if cfg.parcels && lookup.is_none() {
        println!("Parcel database missing: run `dji-offload parcels update` once while online.");
    }

    for src in &sources {
        if !dry_run {
            macos::notify("Drone offload", &format!("{} found, copying…", src.model));
        }
        let mut log = |s: &str| println!("{s}");
        let mut off = offload::Offloader {
            cfg: &cfg,
            ledger: &ledger,
            lookup: lookup.as_ref(),
            dests: dests.clone(),
            dry_run,
            log: &mut log,
        };
        let sum = off.run(src)?;
        if dry_run {
            continue;
        }
        for e in &sum.errors {
            eprintln!("error: {e}");
        }
        let where_to = if dests.len() > 1 { format!(" to {} places", dests.len()) } else { String::new() };
        if !sum.errors.is_empty() {
            macos::notify(
                "Drone offload: problems",
                &format!("{} files failed. Do NOT format the drone. See ~/Library/Logs/dji-offload.log", sum.errors.len()),
            );
            continue;
        }
        let folders = sum.folders.iter().cloned().collect::<Vec<_>>().join(", ");
        if sum.copied == 0 {
            macos::notify("Drone offload", "Nothing new on the drone. Safe to unplug.");
        } else {
            macos::notify(
                "Drone offload: done",
                &format!(
                    "{} files ({:.1} GB) copied and verified{where_to}. {folders}",
                    sum.copied,
                    sum.copied_bytes as f64 / 1e9
                ),
            );
        }
        if cfg.eject_when_done && !no_eject {
            match macos::eject(&src.mount) {
                Ok(()) => println!("Ejected {}. Safe to unplug.", src.mount.display()),
                Err(e) => eprintln!("could not eject: {e:#}"),
            }
        }
    }

    // Refresh the parcel copy in the background of a successful run, if stale.
    if cfg.parcels && !dry_run {
        let stale = match &lookup {
            None => false, // first download is an explicit step
            Some(l) => l
                .updated()
                .and_then(|u| chrono::DateTime::parse_from_rfc3339(&u).ok())
                .map(|t| chrono::Local::now().signed_duration_since(t).num_days() >= cfg.parcel_refresh_days)
                .unwrap_or(true),
        };
        if stale {
            drop(lookup);
            println!("Parcel data is over {} days old; refreshing…", cfg.parcel_refresh_days);
            match geo::update(&parcel_db()?, &mut |_, _| {}) {
                Ok((p, c)) => println!("Parcel data refreshed: {p} parcels, {c} cities."),
                Err(e) => println!("Parcel refresh skipped (offline?): {e:#}"),
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Run { source, dry_run, no_eject, auto } => run(source, dry_run, no_eject, auto),
        Cmd::Parcels { cmd: ParcelCmd::Update } => {
            let db = parcel_db()?;
            println!("Downloading Cochise County parcels and Arizona city limits…");
            let (p, c) = geo::update(&db, &mut |what, n| {
                if what == "parcels" && n % 10_000 < 2000 {
                    println!("  {n} parcels");
                }
            })?;
            println!("Done: {p} parcels, {c} incorporated cities/towns -> {}", db.display());
            Ok(())
        }
        Cmd::Parcels { cmd: ParcelCmd::Lookup { lat, lon } } => {
            let l = geo::Lookup::open(&parcel_db()?).context("no parcel database; run `parcels update`")?;
            match l.parcel(lat, lon)? {
                Some(p) => {
                    println!("APN:     {}", p.apn);
                    println!("Address: {}", p.situs.as_deref().map(geo::title_case).unwrap_or_else(|| "none on file".into()));
                    if let Some(a) = p.acres {
                        println!("Acres:   {a:.2}");
                    }
                }
                None => println!("No parcel at that point."),
            }
            println!("City:    {}", l.city(lat, lon)?.unwrap_or_else(|| "Unincorporated Cochise County".into()));
            Ok(())
        }
        Cmd::MarkDrive { path } => {
            if !path.is_dir() {
                bail!("{} is not a mounted drive", path.display());
            }
            fs::write(path.join(detect::DEST_MARKER), "dji-offload copies drone footage here\n")?;
            println!("Marked. Offloads will also go to {}.", path.join(Config::load()?.external_folder).display());
            Ok(())
        }
        Cmd::UnmarkDrive { path } => {
            let m = path.join(detect::DEST_MARKER);
            if m.exists() {
                fs::remove_file(m)?;
            }
            println!("Unmarked {}.", path.display());
            Ok(())
        }
        Cmd::Install => {
            let plist = macos::install(&state_dir()?)?;
            println!("Installed launchd agent: {}", plist.display());
            if !parcel_db()?.exists() {
                println!("Fetching the parcel database (one time, ~50 MB download)…");
                if let Err(e) = geo::update(&parcel_db()?, &mut |_, _| {}) {
                    println!("Parcel download failed ({e:#}); run `dji-offload parcels update` when online.");
                }
            }
            println!("Plug in the drone to test. Copies go to {}.", Config::load()?.destination.display());
            Ok(())
        }
        Cmd::Uninstall => {
            macos::uninstall()?;
            println!("Removed the launchd agent. Your footage and settings were not touched.");
            Ok(())
        }
        Cmd::Status => {
            let cfg = Config::load()?;
            println!("Config:        {}", config::config_path()?.display());
            println!("Auto-run:      {}", if macos::installed() { "installed" } else { "not installed (dji-offload install)" });
            for d in offload::destinations(&cfg) {
                println!("Destination:   {}", d.display());
            }
            println!("Flight gap:    {} min", cfg.flight_gap_minutes);
            match open_lookup(&cfg) {
                Some(l) => println!("Parcels:       local copy from {}", l.updated().unwrap_or_default()),
                None => println!("Parcels:       not downloaded (dji-offload parcels update)"),
            }
            let s = detect::sources(&cfg.volumes_root);
            println!("Drones now:    {}", if s.is_empty() { "none".into() } else { s.iter().map(|s| format!("{} at {}", s.model, s.mount.display())).collect::<Vec<_>>().join(", ") });
            Ok(())
        }
    }
}

