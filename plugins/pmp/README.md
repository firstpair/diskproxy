# PMP incremental-ingest plugin

Photo Mechanic Plus (PMP) records a camera card's ingested paths, byte sizes, and
filesystem modification times in a per-card incremental history. On inspected
Leica M10-R exFAT cards, modification times had no valid timezone offset. macOS
therefore interpreted the same recorded wall-clock timestamp differently after
mounting in another timezone. PMP saw a different epoch mtime and copied already
ingested photos again.

The plugin gives PMP a stable read-only card view while leaving the source photo
bytes and EXIF intact. The generic diskproxy engine supplies a generated exFAT
disk with valid UTC offset metadata; the plugin chooses which epoch mtime each
photo should present. The disk is selected under **Disks** in PMP. An optional
history-filtering policy hides exact previously ingested path/size/mtime matches.

## Configure once, launch normally

Install with `scripts/install.sh --config configs/pmp.json`. For an already
registered card:

```sh
diskproxy start
# In PMP, ingest the mounted volume under Disks.
diskproxy stop
```

`diskproxy` without a subcommand also starts; `diskproxy status` shows the session.
`pmp-card` and its start/status/stop subcommands continue to work, selecting
`~/.config/diskproxy/pmp.json` explicitly. Another source can be specified as a
positional path. No manual server launch or URL copying is required.

## First registration

Finish and review any intended EXIF corrections before sampling the card. For
an unregistered card mounted while macOS is in Tokyo (UTC+09:00):

```sh
diskproxy-server --config ~/.config/diskproxy/pmp.json \
  --source-offset=+09:00 --target-offset=+00:00 --register
```

This writes `.pmingest.dat` if absent and `.pmp-ingest-proxy.json` at the physical
card root. The existing names and version-1 profile schema are retained for
compatibility. The GUID identifies its history; never replace it merely to
avoid re-ingest. A lock file coordinates setup. Previously configured cards need
no registration or conversion when migrating to diskproxy.

`source_offset` is the offset used **when this physical card was mounted**, not
necessarily the computer's timezone after a subsequent change. Do not guess it
from EXIF. The profile stores three selected photo fingerprints and filesystem
wall-clock anchors, letting later launches detect a changed mount offset from
unchanged source samples. Changed samples are ignored; inability to verify a
single offset fails and requires explicit reviewed re-registration. The profile
travels with the card between Macs.

`target_offset=+00:00` establishes the stable UTC ingest convention for a new
card. This is filesystem normalization, not a conversion of local EXIF capture
times into actual UTC capture instants. Existing cards with Pacific history must
keep that convention; infer it from consistent history or specify it during
first registration. A saved profile rejects a conflicting target override.
The visible volume label remains `PMP Pacific` by default for compatibility,
including UTC cards; a label is not the card's time convention.

After registration, ordinary launches read the profile. First serving an
unregistered card with a known explicit source offset can also register it, for
legacy compatibility; explicit `--register` is the recommended setup path.
`--check` is read-only and never registers or mounts a card.

## Configuration and history filtering

All plugin settings can be supplied in JSON or overridden on the command line:

```json
{
  "source": "/Volumes/LEICA M",
  "plugin": "pmp",
  "plugins": {
    "pmp": {
      "hide_ingested": true
    }
  }
}
```

```sh
diskproxy start --set plugins.pmp.hide_ingested=true
# Equivalent to selecting configs/pmp-filtered.json.

diskproxy-server --config configs/pmp.json --check \
  --set plugins.pmp.hide_ingested=true
```

Supported plugin keys:

| Key | Meaning |
| --- | --- |
| `source_offset` | Known initial mount offset, e.g. `+09:00`; normally omitted after registration. |
| `target_offset` | Fixed ingest convention, e.g. `+00:00`; omit to reuse the saved profile. |
| `target_zone` | Alternative IANA convention with date-specific DST; mutually exclusive with target_offset. Ambiguous/nonexistent times fail. |
| `hide_ingested` | Hide exact history matches; defaults false. |
| `history` | Override the local incremental-history filename. Supplying it enables filtering. |
| `profiles_dir` | Override card-root profile storage, primarily for isolated testing. |

PMP history defaults to `~/Library/Caches/Photo Mechanic/ingest/incremental/<GUID>`.
It is opened read-only, validated, and copied into memory. Rows are card-relative
path, size, and epoch mtime separated by tabs. Leading slashes are normalized for
comparison because disk and folder ingest have produced different forms.
An established card cannot be filtered when its local history is missing; use
normal disk ingest or an explicitly supplied trusted history. No catalog is read
or changed by this plugin. Ingest progress is tracked by PMP, not diskproxy.

The history snapshot is fixed for a running server. Stop/restart before expecting
new ingest progress to hide more files. Disk mode without filtering uses PMP's
own incremental history normally and was the successful original workaround.
Filtered WebDAV is retained for folder-based workflows; unfiltered folder ingest
can repeat old files because its history path form differs from disk ingest.

The view exposes only `.pmingest.dat` and `DCIM` at the root. It excludes
AppleDouble `._` companions and `.PM_lock` files, and does not sample/count them
as images. Current image timestamp normalization/counting/history filtering
covers DNG and JPG; other file types under DCIM are passed through without those
image-specific policies.

## Observed use case

The original card resumed at the first genuinely new frame after its timezone
normalization matched all 7,777 previously ingested file entries. A later new
card was registered with a UTC convention and ingested successfully. A hidden
AppleDouble companion counting bug was fixed before this extraction. These are
local workflow observations, not a claim of vendor endorsement or complete
compatibility with every camera/application version.

Photos are never renamed or edited here. Perform intended capture-time corrections
before a proxy session, or on ingested destinations through the user's chosen
catalog workflow. If card samples are edited, review and refresh registration
before resuming, rather than treating edited bytes as a timezone change.
