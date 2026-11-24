# dji-offload

Plug your DJI drone into your Mac and walk away. Every new photo and video is
copied, **verified byte-for-byte**, and sorted into dated, per-flight folders,
each tagged with the **land parcel (APN), street address and city** it was shot
over. The parcel lookup works **fully offline**, so it still works at a rural
shoot with no signal.

```
~/Drone/
  2026-04-28/
    Flight-1_1827_APN-999-12-345A/
      DJI_20260428182706_0001_D.JPG
      ...
      shoot-info.txt      ← APN, address, acres, city or "Unincorporated", map link
    Flight-2_1905/
```

Built for real-estate and land work in **Cochise County, Arizona**. Parcel
tagging uses the county's own published parcel data; everything else works
for any DJI drone anywhere.

## What it does

- **Detects the drone automatically.** A macOS launchd agent runs on every
  mount. A drive counts as DJI media only if it holds `DCIM/` with a DJI
  folder, so other USB drives are ignored. The Mavic 4 Pro mounts as
  "Untitled"; it is recognised by its contents, not its name.
- **Copies safely.** Each file is hashed (BLAKE3) while it is copied, then the
  copy is read back and hashed again. It only gets its real name once the two
  match. The drone is never written to and nothing is ever deleted.
- **Never copies twice.** A small ledger remembers every verified file, so you
  can plug in after every flight and only the new footage moves.
- **Splits the day into flights.** A gap of more than 10 minutes between files
  (landing, battery swap) starts a new flight folder. Folder names stay stable
  across downloads.
- **Tags each flight with its parcel.** It reads GPS from photo EXIF and from
  video `.SRT` telemetry, then looks up the parcel and city limits in a local
  copy of the public data. Flights that cross several parcels list them all.
- **Backs up to an external drive too.** Mark a drive once and every offload
  also lands there whenever it is plugged in. A drive plugged in later catches
  up on everything it missed.
- **Tells you when it's done.** A notification shows how many files and GB were
  verified, then the drone is ejected so it's safe to unplug.

## Install (macOS 11+, Apple Silicon or Intel)

```sh
curl -fsSL https://github.com/rafe-walker/dji-offload/releases/latest/download/dji-offload-macos-universal.tar.gz | tar xz
./dji-offload install        # registers the auto-run agent, downloads parcel data once
```

Then plug in the drone. The first time, macOS asks whether `dji-offload` may
access files on a removable volume: choose **Allow**.

Optional: `dji-offload mark-drive /Volumes/YourBackupDrive` to copy every
offload there as well.

## Commands

| Command | What it does |
| --- | --- |
| `dji-offload run` | Offload any connected drone now (`--dry-run` to preview) |
| `dji-offload status` | Settings, destinations, parcel data age, connected drones |
| `dji-offload parcels update` | Refresh the offline parcel and city-limits data |
| `dji-offload parcels lookup LAT LON` | Look up the parcel at a point |
| `dji-offload mark-drive PATH` / `unmark-drive PATH` | Add or remove an external backup drive |
| `dji-offload install` / `uninstall` | Turn the automatic run on or off |

Settings live in `~/Library/Application Support/dji-offload/config.toml`.
How it works, operations and troubleshooting: [docs/RUNBOOK.md](docs/RUNBOOK.md).

## Data sources

- Parcels: Cochise County Assessor, `Cad_Parcel_TaxInfo` public feature
  service. Fields used: APN, situs address, acres.
- City limits: Arizona incorporated city boundaries, published by the Arizona
  Department of Revenue (via ADEQ).

Parcel data is for reference only and is not a survey.

## Credits

The plug-in-and-walk-away idea was also explored by
[dji-auto-upload](https://github.com/szilvasolutions/dji-auto-upload) (MIT),
which focuses on cloud upload.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
