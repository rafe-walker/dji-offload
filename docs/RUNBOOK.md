# dji-offload runbook

How the tool works, how to operate it in the field, and how to fix it when
something looks wrong.

## 1. Field procedure

1. Land, power the drone off (or leave it on; both work), and plug it into the
   Mac with a **data** USB-C cable.
2. Within a few seconds a notification says *"Mavic 4 found, copying…"*.
3. Wait for *"done: N files (X GB) copied and verified"*. The drone is ejected
   automatically.
4. Unplug, swap batteries, fly again. The next plug-in copies only the new
   footage into the next flight folder.

If the notification says **"problems"**, do **not** format the drone. Read the
log (section 6), fix the cause, and plug in again. Files that already verified
are not copied again.

Formatting the drone after an offload is a manual choice. The tool never
deletes anything, on the drone or on the Mac.

## 2. How it works

```
mount event (launchd StartOnMount)
  └─ dji-offload run --auto
       ├─ lock (one run at a time; launchd fires once per mount)
       ├─ detect: /Volumes/* with DCIM/DJI_* (or *MEDIA) folders
       │          skip drives carrying .dji-offload-destination
       ├─ scan DCIM → files + capture time
       ├─ ledger: drop files already verified at every destination
       ├─ flights: assign new files to flights (10-min gap rule)
       ├─ place: GPS → parcel + city from the offline database
       ├─ copy: hash-while-copy → re-read → compare → rename
       ├─ shoot-info.txt per flight folder, per destination
       ├─ notify, eject
       └─ refresh the parcel data if older than 30 days and online
```

### Detection
Volume names are useless (the Mavic 4 Pro mounts as "Untitled"). A volume
counts as DJI media when `DCIM/` holds a `DJI_###` or `###MEDIA` folder. DJI
drones also leave a model database in `MISC/` (e.g. `mavic4_*.db`), which is
used to name the model in notifications. A drive marked as a backup
destination is never treated as a source.

### Capture time
In order of preference:
1. the DJI file name (`DJI_YYYYMMDDHHMMSS_NNNN_*`),
2. EXIF `DateTimeOriginal`,
3. the file's modified time.

The time is the drone's local clock. Sidecars (`.SRT`) share the video's name,
so they land in the same flight.

### Flights
Files are processed in time order. A file joins an existing flight on the same
date if it falls within 10 minutes (`flight_gap_minutes`) of that flight's
start or end; otherwise it starts the next flight number for that day. Flights
and each file's assignment are stored in the ledger, so:
- downloading after every flight gives Flight-1, Flight-2, … rather than duplicates;
- footage still being recorded at the previous plug-in extends its flight;
- a backup drive plugged in later gets the same folder names.

A folder name is fixed when the flight is first seen:
`Flight-<n>_<HHMM>[_APN-<apn>]`. If a later download changes which parcel has
most files, `shoot-info.txt` is updated but the folder name is not.

### Parcel and city tagging
- **GPS:** photo EXIF; for videos, the first non-zero fix in the `.SRT`
  telemetry sidecar. Files with no fix are still copied; they just don't
  contribute to the location.
- **Parcel:** each fix is looked up in an R-tree index of parcel bounding
  boxes, then confirmed with a point-in-polygon test (holes and multi-part
  parcels handled). The parcel with the most fixes names the folder; every
  parcel touched is listed in `shoot-info.txt`.
- **City:** the mean position of the flight is tested against incorporated
  city/town limits. Outside every boundary it reads "Unincorporated Cochise
  County".
- **Address:** the county's situs address. Roughly half of Cochise parcels
  (mostly rural land) have none on file; the APN is always present.

The county's `city` field in the parcel layer is the **owner's mailing city**,
not the parcel's location, so it is deliberately not used.

### Copy safety
- The file streams through a BLAKE3 hasher into `.<name>.partial`, then `fsync`.
- The partial file is re-read and hashed. If the hashes differ, the partial
  file is deleted and the error is reported.
- Only a verified file is renamed to its real name. A file that already exists
  with the same hash is accepted; one that exists with **different** content
  is left alone and reported.
- The ledger records `(destination, file name + size) → hash` only after
  verification.

## 3. Files and settings

| Path | What |
| --- | --- |
| `~/Library/Application Support/dji-offload/config.toml` | Settings (below) |
| `…/dji-offload/ledger.db` | Copied files, flights, file → flight map |
| `…/dji-offload/parcels.db` | Offline parcels + city limits (≈ 50–100 MB) |
| `…/dji-offload/bin/dji-offload` | Binary used by the agent |
| `~/Library/LaunchAgents/io.github.rafe-walker.dji-offload.plist` | Auto-run agent |
| `~/Library/Logs/dji-offload.log` | Output of every automatic run |

`config.toml` keys and defaults:

```toml
destination = "/Users/<you>/Drone"   # main copy
external_folder = "Drone"            # folder created on marked drives
flight_gap_minutes = 10
eject_when_done = true
skip_extensions = ["LRF"]            # DJI low-res previews
parcels = true
parcel_refresh_days = 30
volumes_root = "/Volumes"
```

Set `DJI_OFFLOAD_HOME` to use a different state folder (handy for testing).

## 4. Parcel data

- Source: Cochise County Assessor public ArcGIS feature service
  `Cad_Parcel_TaxInfo` (layer 0). About 123,000 parcels, fetched 2,000 per
  request with `outSR=4326`.
- City limits: Arizona incorporated city boundaries (`CITIES_AGOL`, Arizona
  Department of Revenue via ADEQ).
- `dji-offload parcels update` writes to `parcels.db.partial` and swaps it in
  only when the download completes and holds more than 1,000 parcels. A failed
  refresh never breaks the working copy.
- It refreshes automatically after an offload when the copy is older than
  `parcel_refresh_days` and the Mac is online. Offline, the refresh is skipped
  quietly.

Other counties: the lookup code is generic. Adding a county means adding its
feature-service URL and field names in `src/geo.rs`.

## 5. Install, upgrade, remove

```sh
./dji-offload install      # copy binary into place, register agent, fetch parcels
./dji-offload status       # confirm "Auto-run: installed" and parcel date
./dji-offload uninstall    # remove the agent; footage and settings stay
```

To upgrade, download the new release and run `install` again.

First-run prompts on macOS:
- **Removable volume access:** allow it. Otherwise the agent sees an empty drive.
- **Notifications:** they are posted through AppleScript. If none appear,
  enable notifications for *Script Editor* in System Settings → Notifications.

## 6. Troubleshooting

| Symptom | Check |
| --- | --- |
| Nothing happens on plug-in | `dji-offload status`: is the agent installed and the drone listed? Does the drone appear in Finder? Try another USB-C cable (many only charge). |
| "No DJI drone or card found" | `ls /Volumes/*/DCIM`. The drone must show a `DCIM/DJI_###` folder. |
| Drone shows an empty folder to the agent | Removable-volume permission was denied: System Settings → Privacy & Security → Files and Folders. |
| Folder has no `_APN-` | Look at `shoot-info.txt`: no GPS, no parcel database, or outside Cochise County. |
| "problems" notification | `tail -50 ~/Library/Logs/dji-offload.log`, then plug in again. Verified files are skipped. |
| Want to re-copy everything | Delete `ledger.db`. Existing identical files are recognised by hash and not duplicated. |

Manual run with full output:

```sh
dji-offload run --dry-run     # plan only
dji-offload run --no-eject    # copy, keep the drone mounted
```

## 7. Development

```sh
cargo test        # unit + end-to-end tests (synthetic drone, fictional parcels)
cargo clippy
```

CI runs the tests on Linux and macOS. Tagged releases (`v*`) build a
universal (arm64 + x86_64) macOS binary with signed build provenance.

Test fixtures use fictional APNs, addresses and coordinates only.
