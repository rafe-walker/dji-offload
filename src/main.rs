//! dji-offload: plug in a DJI drone, and its photos and videos are copied,
//! verified and sorted into dated, per-flight folders tagged with the parcel
//! they were shot over.

mod backup;
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
mod space;
mod wipe;

use std::fs;
use std::path::PathBuf;
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
    /// Mark a drive for backup mode: whenever it is plugged in, every DJI file
    /// found on this Mac is checked and any missing ones are copied to it.
    MarkBackup { path: PathBuf },
    /// Stop using a drive for backup mode.
    UnmarkBackup { path: PathBuf },
    /// Run a backup sweep now onto marked backup drives (or `--to PATH`).
    Backup {
        #[arg(long)]
        to: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete already-verified files from the connected drone.
    Wipe {
        #[arg(long)]
        source: Option<PathBuf>,
        /// Actually delete. Without it, only shows what would be removed.
        #[arg(long)]
        yes: bool,
    },
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
    /// Look up the parcel at a point, offline (e.g. `lookup 31.55 -110.27`).
    #[command(allow_negative_numbers = true)]
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

/// One offload at a time; launchd fires once per mount. An OS file lock is
/// released automatically if the process dies, so a crash never wedges it.
struct Lock(#[allow(dead_code)] fs::File);
impl Lock {
    fn take() -> Result<Option<Lock>> {
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(state_dir()?.join("run.lock"))?;
        match f.try_lock() {
            Ok(()) => Ok(Some(Lock(f))),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => Err(e.into()),
        }
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
        if !dry_run && !detect::marked_backups(&cfg.volumes_root).is_empty() {
            let ledger = ledger::Ledger::open(&state_dir()?.join("ledger.db"))?;
            run_backups(&cfg, &ledger, None, false)?;
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
        for w in &sum.warnings {
            macos::notify("Drone offload: storage low", w);
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
        let mode = cfg.wipe_after_offload.to_lowercase();
        if mode == "ask" || mode == "always" {
            let plan = wipe::plan(&ledger, &src.mount, &dests, &cfg.skip_extensions)?;
            if !plan.delete.is_empty() && (mode == "always" || wipe::ask(plan.delete.len(), plan.bytes)) {
                let (n, errs) = wipe::execute(&plan);
                for e in &errs {
                    eprintln!("wipe error: {e}");
                }
                macos::notify(
                    "Drone offload: drone cleared",
                    &format!("Deleted {n} verified files ({}) from the drone.", space::gb(plan.bytes)),
                );
            }
            if !plan.keep.is_empty() {
                println!("{} files left on the drone (not verified on every destination).", plan.keep.len());
            }
        }
        if cfg.eject_when_done && !no_eject {
            match macos::eject(&src.mount) {
                Ok(()) => println!("Ejected {}. Safe to unplug.", src.mount.display()),
                Err(e) => eprintln!("could not eject: {e:#}"),
            }
        }
    }

    if !dry_run {
        run_backups(&cfg, &ledger, None, false)?;
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

/// Backup sweep onto every marked backup drive (or just `to`).
fn run_backups(cfg: &Config, ledger: &ledger::Ledger, to: Option<PathBuf>, dry_run: bool) -> Result<()> {
    let drives = match to {
        Some(p) => vec![p],
        None => detect::marked_backups(&cfg.volumes_root),
    };
    if drives.is_empty() {
        return Ok(());
    }
    let mut sorted_roots = vec![cfg.destination.clone()];
    for d in detect::marked_destinations(&cfg.volumes_root) {
        sorted_roots.push(d.join(&cfg.external_folder));
    }
    let mut roots = cfg.backup_scan_roots.clone();
    for r in &sorted_roots {
        if !roots.iter().any(|x| r.starts_with(x)) {
            roots.push(r.clone());
        }
    }
    for drive in drives {
        let target = drive.join(&cfg.backup_folder);
        let mut exclude = vec![drive.clone()];
        exclude.extend(detect::sources(&cfg.volumes_root).into_iter().map(|s| s.mount));
        println!("Backup: scanning this Mac for DJI files…");
        let found = backup::find(&roots, &exclude);
        let needed = backup::estimate(&found, &target);
        if !dry_run
            && let Some(msg) = space::shortfall(&target, needed) {
                macos::notify("Drone backup: not enough space", &msg);
                continue;
            }
        let mut log = |s: &str| println!("{s}");
        let r = backup::sweep(ledger, &found, &target, &sorted_roots, dry_run, &mut log)?;
        for e in &r.errors {
            eprintln!("backup error: {e}");
        }
        let name = drive.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let msg = format!(
            "Checked {} files: {} new ({}) backed up to {name}, {} already there{}{}.",
            r.found,
            r.copied,
            space::gb(r.copied_bytes),
            r.already,
            if r.conflicts > 0 { format!(", {} name clashes kept side by side", r.conflicts) } else { String::new() },
            if r.errors.is_empty() { String::new() } else { format!(", {} errors (see log)", r.errors.len()) },
        );
        if dry_run {
            println!("{msg}");
        } else {
            macos::notify("Drone backup", &msg);
            if let Some(w) = space::low(&target, cfg.low_space_warn_gb) {
                macos::notify("Drone backup: storage low", &w);
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
        Cmd::MarkBackup { path } => {
            if !path.is_dir() {
                bail!("{} is not a mounted drive", path.display());
            }
            if path.join(detect::DEST_MARKER).exists() {
                bail!("{} is already an offload destination; unmark it first", path.display());
            }
            fs::write(path.join(detect::BACKUP_MARKER), "dji-offload backs up drone footage here\n")?;
            println!("Marked as a backup drive. Plugging it in will back up every DJI file on this Mac to {}.", path.join(Config::load()?.backup_folder).display());
            Ok(())
        }
        Cmd::UnmarkBackup { path } => {
            let m = path.join(detect::BACKUP_MARKER);
            if m.exists() {
                fs::remove_file(m)?;
            }
            println!("Unmarked {}.", path.display());
            Ok(())
        }
        Cmd::Backup { to, dry_run } => {
            let cfg = Config::load()?;
            let ledger = ledger::Ledger::open(&state_dir()?.join("ledger.db"))?;
            if to.is_none() && detect::marked_backups(&cfg.volumes_root).is_empty() {
                bail!("no backup drive marked; use `mark-backup PATH` or `backup --to PATH`");
            }
            run_backups(&cfg, &ledger, to, dry_run)
        }
        Cmd::Wipe { source, yes } => {
            let cfg = Config::load()?;
            let src = match source {
                Some(p) => detect::inspect(&p).with_context(|| format!("{} has no DJI DCIM folder", p.display()))?,
                None => detect::sources(&cfg.volumes_root).into_iter().next().context("no DJI drone connected")?,
            };
            let ledger = ledger::Ledger::open(&state_dir()?.join("ledger.db"))?;
            let dests = offload::destinations(&cfg);
            let plan = wipe::plan(&ledger, &src.mount, &dests, &cfg.skip_extensions)?;
            println!(
                "{} files ({}) are verified on every destination and can be deleted; {} would stay.",
                plan.delete.len(),
                space::gb(plan.bytes),
                plan.keep.len()
            );
            for (name, why) in plan.keep.iter().take(10) {
                println!("  keep {name}: {why}");
            }
            if !yes {
                println!("Nothing deleted. Re-run with --yes to delete.");
                return Ok(());
            }
            let (n, errs) = wipe::execute(&plan);
            for e in &errs {
                eprintln!("error: {e}");
            }
            println!("Deleted {n} files from {}.", src.mount.display());
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
            for d in detect::marked_backups(&cfg.volumes_root) {
                println!("Backup drive:  {}", d.join(&cfg.backup_folder).display());
            }
            if let Some(free) = space::available(&cfg.destination) {
                println!("Free space:    {} at {}", space::gb(free), cfg.destination.display());
            }
            println!("Flight gap:    {} min", cfg.flight_gap_minutes);
            println!("Wipe drone:    {}", cfg.wipe_after_offload);
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

