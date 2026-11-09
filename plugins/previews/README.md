# Previews plugin

Serve a folder of stand-in files with better embedded previews, without writing
or copying any image data. Typical use: browse and export from a photo library
whose originals are offline, using small DNG stand-ins (such as Lightroom Smart
Previews) plus preview JPEGs that already exist elsewhere (such as an image
browser's preview cache).

## What it does

The source tree is served as usual. A map file names the files to change:

- **DNG/TIFF with `preview=`:** the file is served with the named JPEG added as
  an extra reduced-resolution preview (a new SubIFD). Every original byte stays
  at its original offset; only the 8-byte TIFF header is repointed at a copy of
  IFD0 appended after the original data, followed by the JPEG. Applications that
  show embedded previews (Photo Mechanic, Quick Look, `exiftool -PreviewImage`)
  then see the large JPEG instead of a small thumbnail, while raw-capable
  applications still read the original image data.
- **JPEG with `metadata=`:** the JPEG is served with the EXIF and XMP of the
  named TIFF or DNG: a photo's Lightroom Smart Preview, or an EXIF-only TIFF
  such as `exiftool -n -tagsfromfile ORIGINAL -exif:all --makernotes -o X.exif`
  writes (big- or little-endian). That gives camera, lens, exposure, serial
  number, GPS and, if the file has one, its XMP packet. The EXIF is rebuilt in a
  new segment, without the DNG's image-structure tags and maker note, and gains
  this JPEG's pixel size. The XMP segment's contents are read from the DNG in
  place on every read, so it costs no memory.
- **A JPEG that already has EXIF:** its EXIF segments are dropped and replaced by
  the rebuilt one, which merges both: the JPEG's own tags (Orientation, color
  space, resolution) win over the DNG's, because they describe the preview's
  pixels; the DNG's ExifVersion wins, because it describes the tag set. The
  JPEG's own XMP is kept, unless the DNG has XMP, which then replaces it so there
  is one packet.
- **JPEG with `orientation=` and/or `datetime=`:** these set Orientation and
  DateTimeOriginal in the rebuilt EXIF, overriding both the JPEG's and the DNG's
  values, so portrait images display upright and sort by capture time. They work
  with or without `metadata=`.

Unmapped files are served unchanged. If a mapped file cannot be composed (its
preview JPEG is missing, or the DNG has an unexpected structure) it is served
unchanged and a warning goes to the server's stderr. The preview JPEGs are
and metadata DNGs are opened read-only, without following symlinks, exactly like
source files, and a read fails if a source, preview or metadata file changed
after the disk was built. A JPEG whose metadata DNG is missing or unreadable is
still served with its `orientation=` and `datetime=` values, with a warning.

## Capturing ratings

With `ratings` set, files that a client writes on a writable view (shadow-file
disk or NFS; see the main README) are read for these XMP properties, and only
these: `xmp:Rating`, `xmp:Label`, `photomechanic:ColorClass` and
`photomechanic:Tagged`. Everything else the application writes (dates it made up,
copied camera metadata, tool names) is ignored. They are found in a JPEG's XMP
APP1 segment, a TIFF/DNG's XMP tag (700) or an `.xmp`/`.XMP` sidecar.

Each photo gets a minimal sidecar mirroring its folder: `X.DNG` (or its `X.XMP`
sidecar) becomes `X.xmp`, `X.JPG` becomes `X.JPG.xmp` so a JPEG and a raw file
with the same name never collide, and a stand-in served as `X.JPG.dng` or
`X.RW2.dng` is recorded under its original's name. A sidecar is only rewritten
when a value changed, and every change is appended to `JOURNAL.jsonl` with the
previous values. The folder must be outside the source.

## Map file

UTF-8 text, one line per file: a path relative to the source, then TAB-separated
`key=value` fields. Paths may not contain `.`/`..` components or TABs.

```text
2024/trip/IMG_0001.DNG	preview=a1/IMG_0001.jpg
2024/trip/IMG_0002.JPG	orientation=6	datetime=2024:10:02 04:45:42
2024/trip/IMG_0003.JPG	metadata=0/0A1B/0A1B2C3D.dng
```

| Key | Applies to | Value |
| --- | --- | --- |
| `preview` | DNG, TIFF | JPEG path relative to `preview_root` |
| `metadata` | JPEG | DNG path relative to `metadata_root`, whose EXIF and XMP the JPEG is served with |
| `orientation` | JPEG | EXIF orientation, 1–8 |
| `datetime` | JPEG | `YYYY:MM:DD HH:MM:SS` |
| `preview_size` | DNG, TIFF | Optional: the preview JPEG's size in bytes |
| `preview_mtime` | DNG, TIFF | Optional: its modification time, `SECONDS.NANOSECONDS` (9 digits) |
| `preview_dims` | DNG, TIFF | Optional: its pixel size, `WIDTHxHEIGHT` |

When all three optional preview fields are given, the disk is built without
opening the preview JPEGs, which makes startup of a large tree much faster. The
recorded size and modification time are still checked against the real file on
every read, so a preview that changed after the map was written fails to read
rather than serving wrong bytes. Write them from the files themselves, for
example with `stat -f '%z %Fm'`.

## Configuration

```json
{
  "source": "/absolute/path/to/stand-in-tree",
  "plugin": "previews",
  "label": "Previews",
  "plugins": {
    "previews": {
      "map": "/absolute/path/to/previews.tsv",
      "preview_root": "/absolute/path/to/preview-jpegs",
      "metadata_root": "/absolute/path/to/Catalog Smart Previews.lrdata",
      "ratings": "/absolute/path/to/ratings"
    }
  }
}
```

```sh
diskproxy --config previews.json start
diskproxy stop
```

Building the disk reads every mapped file's header, its preview JPEG's header
unless its stamp is in the map, and each mapped JPEG's metadata DNG header and
EXIF block. A tree of 1.26 million files with 1.24 million map entries, 566,000
of them JPEGs with `metadata=`, scans in about 110 seconds and then uses about
1.9 GB of memory, with a 2.5 GB peak during startup. Without `metadata=` the same
tree scans in about a minute and uses 1.2 GB. The launcher shows scan
progress and waits up to 15 minutes while the server is alive.

To capture ratings, add `"writable": true` (disk mode) or `"mode": "nfs"` to the
config; see [Writable views](../../README.md#writable-views).

## Limits

- Classic TIFF only (not BigTIFF), files under 4 GiB.
- The preview is added, never replaced; an existing large preview stays first.
- A mapped JPEG whose own EXIF cannot be parsed is served unchanged, with a
  warning.
- A metadata DNG's XMP is served as it is, so its `dc:format` still says
  `image/dng`. An XMP packet larger than one JPEG segment (64 KB) is left out.
