//! Previews plugin: present stand-in files with better previews, without copying.
//!
//! A map file lists source-relative paths, one per line, followed by TAB-separated
//! `key=value` fields:
//!
//! * `preview=<path under preview_root>` on a DNG/TIFF: serve the file with that
//!   JPEG appended as an additional reduced-resolution preview SubIFD. The TIFF
//!   header is repointed at an appended copy of IFD0; every original byte is kept.
//! * `metadata=<path under metadata_root>` on a JPEG without EXIF: serve it with
//!   the camera, lens, exposure and GPS EXIF of that DNG (typically the photo's
//!   Lightroom Smart Preview) and, as an XMP APP1 segment, the DNG's XMP packet,
//!   read in place from the DNG on every read.
//! * `orientation=<1..8>` and `datetime=YYYY:MM:DD HH:MM:SS` on a JPEG without
//!   EXIF: set those tags in the inserted EXIF, overriding the DNG's values.
//!
//! Unmapped files are served unchanged. A mapped file that cannot be composed
//! (missing preview, unexpected structure) is served unchanged with a warning.
pub mod capture;

use anyhow::{Context, Result, bail, ensure};
use diskproxy_core::{Piece, Plugin, Source, Stamp};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// TSV map file described in the crate documentation.
    pub map: Option<PathBuf>,
    /// Directory holding the preview JPEGs named by `preview=` fields.
    pub preview_root: Option<PathBuf>,
    /// Folder for captured ratings sidecars (see [`capture`]); captures are
    /// disabled when absent. Must be outside the served source.
    pub ratings: Option<PathBuf>,
    /// Directory holding the DNGs named by `metadata=` fields.
    pub metadata_root: Option<PathBuf>,
}

#[derive(Default, Debug, PartialEq)]
struct Entry {
    /// Shared with every layout built for this file.
    preview: Option<Arc<str>>,
    /// Size, mtime and dimensions of the preview, so startup need not open it.
    preview_size: Option<u64>,
    preview_mtime: Option<(i64, i64)>,
    preview_dims: Option<(u32, u32)>,
    /// DNG whose EXIF and XMP a JPEG is served with.
    metadata: Option<Arc<str>>,
    orientation: Option<u16>,
    datetime: Option<[u8; 19]>,
}

pub struct Previews {
    entries: HashMap<Box<str>, Entry>,
    previews: Option<Arc<Source>>,
    metadata: Option<Arc<Source>>,
    capture: Option<capture::Capture>,
}

fn components(path: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = path.split('/').map(str::to_owned).collect();
    ensure!(
        parts
            .iter()
            .all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains('\0')),
        "invalid path in previews map: {path}"
    );
    Ok(parts)
}

fn parse_map(text: &str) -> Result<HashMap<Box<str>, Entry>> {
    let mut entries = HashMap::new();
    for (n, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let path = fields.next().unwrap();
        components(path).with_context(|| format!("previews map line {}", n + 1))?;
        let mut entry = Entry::default();
        for field in fields {
            let (key, value) = field
                .split_once('=')
                .with_context(|| format!("previews map line {}: expected key=value", n + 1))?;
            match key {
                "preview" => {
                    components(value)?;
                    entry.preview = Some(Arc::from(value));
                }
                "metadata" => {
                    components(value)?;
                    entry.metadata = Some(Arc::from(value));
                }
                "preview_size" => entry.preview_size = Some(value.parse()?),
                "preview_mtime" => {
                    let (sec, nsec) = value.split_once('.').context("preview_mtime is SEC.NSEC")?;
                    ensure!(nsec.len() == 9, "preview_mtime needs 9 nanosecond digits");
                    entry.preview_mtime = Some((sec.parse()?, nsec.parse()?));
                }
                "preview_dims" => {
                    let (w, h) = value.split_once('x').context("preview_dims is WxH")?;
                    let (w, h): (u32, u32) = (w.parse()?, h.parse()?);
                    ensure!(w > 0 && h > 0, "preview_dims must be positive");
                    entry.preview_dims = Some((w, h));
                }
                "orientation" => {
                    let v: u16 = value.parse()?;
                    ensure!((1..=8).contains(&v), "orientation must be 1..8");
                    entry.orientation = Some(v);
                }
                "datetime" => {
                    let b = value.as_bytes();
                    ensure!(
                        b.len() == 19
                            && b.iter().enumerate().all(|(i, c)| match i {
                                4 | 7 => *c == b':',
                                10 => *c == b' ',
                                13 | 16 => *c == b':',
                                _ => c.is_ascii_digit(),
                            }),
                        "datetime must be YYYY:MM:DD HH:MM:SS"
                    );
                    entry.datetime = Some(b.try_into().unwrap());
                }
                _ => bail!("previews map line {}: unknown key {key}", n + 1),
            }
        }
        ensure!(
            entries.insert(path.into(), entry).is_none(),
            "duplicate path in previews map: {path}"
        );
    }
    Ok(entries)
}

impl Previews {
    pub fn new(config: Config) -> Result<Self> {
        let map = config.map.context("plugins.previews.map is required")?;
        let entries = parse_map(
            &std::fs::read_to_string(&map)
                .with_context(|| format!("read previews map {}", map.display()))?,
        )?;
        let needs_root = entries.values().any(|e| e.preview.is_some());
        let previews = match config.preview_root {
            Some(root) => {
                Some(Arc::new(Source::new(&root).with_context(|| {
                    format!("open preview_root {}", root.display())
                })?))
            }
            None if needs_root => bail!("plugins.previews.preview_root is required"),
            None => None,
        };
        let needs_metadata = entries.values().any(|e| e.metadata.is_some());
        let metadata = match config.metadata_root {
            Some(root) => {
                Some(Arc::new(Source::new(&root).with_context(|| {
                    format!("open metadata_root {}", root.display())
                })?))
            }
            None if needs_metadata => bail!("plugins.previews.metadata_root is required"),
            None => None,
        };
        let capture = config.ratings.map(capture::Capture::new).transpose()?;
        Ok(Self {
            entries,
            previews,
            metadata,
            capture,
        })
    }
    /// Only the ratings capture, without loading the map: for harvesting a
    /// writable mount, which needs no layouts.
    pub fn capture_only(config: Config) -> Result<Self> {
        let ratings = config
            .ratings
            .context("plugins.previews.ratings is required to harvest ratings")?;
        Ok(Self {
            entries: HashMap::new(),
            previews: None,
            metadata: None,
            capture: Some(capture::Capture::new(ratings)?),
        })
    }
    pub fn ratings(&self) -> Option<&Path> {
        self.capture.as_ref().map(capture::Capture::dir)
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Plugin for Previews {
    fn compose(&self, parts: &[String], file: &File) -> Result<Option<Vec<Piece>>> {
        let Some(entry) = self.entries.get(parts.join("/").as_str()) else {
            return Ok(None);
        };
        let size = file.metadata()?.len();
        let result = if let Some(preview) = &entry.preview {
            let source = self.previews.clone().context("no preview_root")?;
            let known = match (entry.preview_size, entry.preview_mtime, entry.preview_dims) {
                (Some(size), Some((mtime, nsec)), Some(dims)) => {
                    Some((Stamp { size, mtime, nsec }, dims))
                }
                _ => None,
            };
            dng_with_preview(file, size, source, preview.clone(), known)
        } else if entry.metadata.is_some()
            || entry.orientation.is_some()
            || entry.datetime.is_some()
        {
            let datetime = entry
                .datetime
                .as_ref()
                .map(|d| std::str::from_utf8(d).unwrap());
            let dng = entry.metadata.as_ref().and_then(|path| {
                let source = self.metadata.clone()?;
                match source_metadata(&source, path) {
                    Ok(m) => Some((m, source, path.clone())),
                    Err(e) => {
                        eprintln!(
                            "previews: {} without metadata from {path}: {e:#}",
                            parts.join("/")
                        );
                        None
                    }
                }
            });
            jpeg_with_exif(file, size, entry.orientation, datetime, dng)
        } else {
            return Ok(None);
        };
        match result {
            Ok(pieces) => Ok(pieces),
            Err(e) => {
                eprintln!("previews: serving {} unchanged: {e:#}", parts.join("/"));
                Ok(None)
            }
        }
    }
    fn written(&self, parts: &[String], file: &File) -> Result<()> {
        if let Some(capture) = &self.capture {
            match capture.record(parts, file) {
                Ok(Some(rel)) => eprintln!("ratings: {rel}"),
                Ok(None) => (),
                // A half-written or unparsable file is retried on its next write.
                Err(e) => eprintln!("ratings: skipped {}: {e:#}", parts.join("/")),
            }
        }
        Ok(())
    }
}

fn read(file: &File, offset: u64, length: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; length];
    file.read_exact_at(&mut buf, offset)
        .with_context(|| format!("read {length} bytes at {offset}"))?;
    Ok(buf)
}

/// Reads served from one up-front read of the file's start when possible.
struct Head<'a> {
    file: &'a File,
    buf: Vec<u8>,
}
impl<'a> Head<'a> {
    fn new(file: &'a File, size: u64) -> Result<Self> {
        let buf = read(file, 0, size.min(16384) as usize)?;
        Ok(Self { file, buf })
    }
    fn get(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let end = offset as usize + length;
        if end <= self.buf.len() {
            Ok(self.buf[offset as usize..end].to_vec())
        } else {
            read(self.file, offset, length)
        }
    }
}

/// Width and height from a JPEG's first SOF marker.
fn jpeg_dimensions(file: &File, size: u64) -> Result<(u32, u32)> {
    let head = Head::new(file, size)?;
    ensure!(head.get(0, 2)? == [0xff, 0xd8], "preview is not a JPEG");
    let mut pos = 2u64;
    while pos + 4 <= size {
        let m = head.get(pos, 4)?;
        ensure!(m[0] == 0xff, "bad JPEG marker");
        let len = u16::from_be_bytes([m[2], m[3]]) as u64;
        if matches!(m[1], 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let f = head.get(pos + 5, 4)?;
            return Ok((
                u16::from_be_bytes([f[2], f[3]]) as u32,
                u16::from_be_bytes([f[0], f[1]]) as u32,
            ));
        }
        ensure!(m[1] != 0xda, "no SOF before SOS");
        pos += 2 + len;
    }
    bail!("no SOF marker")
}

#[derive(Clone, Copy)]
struct Order(bool); // true = little-endian
impl Order {
    fn u16(&self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        if self.0 {
            u16::from_le_bytes(a)
        } else {
            u16::from_be_bytes(a)
        }
    }
    fn u32(&self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if self.0 {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    }
    fn p16(&self, v: u16) -> [u8; 2] {
        if self.0 {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    }
    fn p32(&self, v: u32) -> [u8; 4] {
        if self.0 {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    }
    fn entry(&self, tag: u16, typ: u16, count: u32, value: u32) -> Vec<u8> {
        let mut e = Vec::with_capacity(12);
        e.extend(self.p16(tag));
        e.extend(self.p16(typ));
        e.extend(self.p32(count));
        if typ == 3 && count == 1 {
            e.extend(self.p16(value as u16));
            e.extend([0, 0]);
        } else {
            e.extend(self.p32(value));
        }
        e
    }
}

fn dng_with_preview(
    file: &File,
    size: u64,
    previews: Arc<Source>,
    preview: Arc<str>,
    known: Option<(Stamp, (u32, u32))>,
) -> Result<Option<Vec<Piece>>> {
    let head = Head::new(file, size)?;
    let header = head.get(0, 8)?;
    let o = Order(match &header[..2] {
        b"II" => true,
        b"MM" => false,
        _ => bail!("not a TIFF/DNG file"),
    });
    ensure!(o.u16(&header[2..4]) == 42, "not classic TIFF");
    let ifd0 = o.u32(&header[4..8]) as u64;
    let n = o.u16(&head.get(ifd0, 2)?) as usize;
    let body = head.get(ifd0 + 2, 12 * n + 4)?;
    let tags: Vec<u16> = body[..12 * n].chunks(12).map(|e| o.u16(e)).collect();
    ensure!(
        tags.windows(2).all(|w| w[0] < w[1]),
        "IFD0 tags are not sorted"
    );
    // The existing SubIFDs entry (replaced) or the position to insert one.
    let existing = tags.iter().position(|&t| t == 330);
    let split = existing.unwrap_or_else(|| tags.iter().position(|&t| t > 330).unwrap_or(n));
    let mut subifds = Vec::new();
    if let Some(i) = existing {
        let e = &body[12 * i..12 * i + 12];
        let (typ, count) = (o.u16(&e[2..4]), o.u32(&e[4..8]) as usize);
        ensure!(matches!(typ, 4 | 13) && count > 0, "unexpected SubIFDs tag");
        if count == 1 {
            subifds.push(o.u32(&e[8..12]));
        } else {
            let at = head.get(o.u32(&e[8..12]) as u64, 4 * count)?;
            subifds.extend(at.chunks(4).map(|c| o.u32(c)));
        }
    }
    let (stamp, (width, height)) = match known {
        Some(k) => k,
        None => {
            let jpeg = previews.open_raw(&components(&preview)?)?;
            let stamp = Stamp::of(&jpeg)?;
            (stamp, jpeg_dimensions(&jpeg, stamp.size)?)
        }
    };
    let jpeg_size = stamp.size;
    let fits = |v: u64| u32::try_from(v).context("file too large for classic TIFF");

    // Appended layout: [pad] SubIFD array | preview IFD | BitsPerSample | new IFD0 | [pad] JPEG
    let base = size + (size & 1);
    let sub_array = base;
    let prev_ifd = sub_array + 4 * (subifds.len() as u64 + 1);
    const PREV_ENTRIES: u64 = 11;
    let bps = prev_ifd + 2 + 12 * PREV_ENTRIES + 4;
    let new_ifd0 = bps + 6;
    let count = n + existing.is_none() as usize;
    let mut jpeg_at = new_ifd0 + 2 + 12 * count as u64 + 4;
    jpeg_at += jpeg_at & 1;

    let prev = [
        o.entry(254, 4, 1, 1), // NewSubFileType: reduced-resolution preview
        o.entry(256, 4, 1, width),
        o.entry(257, 4, 1, height),
        o.entry(258, 3, 3, fits(bps)?), // BitsPerSample 8,8,8
        o.entry(259, 3, 1, 7),          // Compression: JPEG
        o.entry(262, 3, 1, 6),          // Photometric: YCbCr
        o.entry(273, 4, 1, fits(jpeg_at)?),
        o.entry(277, 3, 1, 3),
        o.entry(278, 4, 1, height),
        o.entry(279, 4, 1, fits(jpeg_size)?),
        o.entry(50970, 4, 1, 2), // PreviewColorSpace: sRGB
    ];
    let mut new_subifds = subifds;
    new_subifds.push(fits(prev_ifd)?);
    let sub_entry = o.entry(330, 4, new_subifds.len() as u32, fits(sub_array)?);

    // Generated: [pad] SubIFD array, preview IFD, BitsPerSample, new IFD0 count.
    let mut block = vec![0u8; (base - size) as usize];
    for s in &new_subifds {
        block.extend(o.p32(*s));
    }
    block.extend(o.p16(PREV_ENTRIES as u16));
    for e in &prev {
        block.extend(e);
    }
    block.extend([0; 4]);
    for _ in 0..3 {
        block.extend(o.p16(8));
    }
    block.extend(o.p16(count as u16));
    // The new IFD0 reuses the original entries straight from the file: those
    // before the SubIFDs tag, the new SubIFDs entry, then the rest plus the
    // next-IFD pointer. Only the SubIFDs entry is generated.
    let entries_at = ifd0 + 2;
    let after = split + existing.is_some() as usize;
    let mut pad = vec![0u8; (jpeg_at - (new_ifd0 + 2 + 12 * count as u64 + 4)) as usize];
    let mut new_header = header[..4].to_vec();
    new_header.extend(o.p32(fits(new_ifd0)?));
    let mut pieces = vec![
        Piece::Bytes(new_header),
        Piece::Own {
            offset: 8,
            length: size - 8,
        },
        Piece::Bytes(block),
        Piece::Own {
            offset: entries_at,
            length: 12 * split as u64,
        },
        Piece::Bytes(sub_entry),
        Piece::Own {
            offset: entries_at + 12 * after as u64,
            length: 12 * (n - after) as u64 + 4,
        },
    ];
    if !pad.is_empty() {
        pieces.push(Piece::Bytes(std::mem::take(&mut pad)));
    }
    pieces.push(Piece::Other {
        source: previews,
        path: preview,
        offset: 0,
        length: jpeg_size,
        stamp: Some(stamp),
    });
    Ok(Some(pieces))
}

/// One TIFF directory entry, with its value bytes in the file's byte order.
#[derive(Clone, Debug, PartialEq)]
struct Field {
    tag: u16,
    typ: u16,
    count: u32,
    data: Vec<u8>,
}

impl Field {
    fn short(o: Order, tag: u16, v: u16) -> Self {
        Self {
            tag,
            typ: 3,
            count: 1,
            data: o.p16(v).to_vec(),
        }
    }
    fn long(o: Order, tag: u16, v: u32) -> Self {
        Self {
            tag,
            typ: 4,
            count: 1,
            data: o.p32(v).to_vec(),
        }
    }
    fn ascii(tag: u16, s: &str) -> Self {
        let mut data = s.as_bytes().to_vec();
        data.push(0);
        Self {
            tag,
            typ: 2,
            count: data.len() as u32,
            data,
        }
    }
}

fn type_size(typ: u16) -> Option<u64> {
    match typ {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 | 13 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

/// A directory entry as stored: tag, type, count and the 4-byte value field.
type RawEntry = (u16, u16, u32, [u8; 4]);

fn raw_entries(head: &Head, o: Order, at: u64) -> Result<Vec<RawEntry>> {
    let n = o.u16(&head.get(at, 2)?) as usize;
    ensure!(n <= 1000, "implausible TIFF directory");
    let body = head.get(at + 2, 12 * n)?;
    Ok(body
        .chunks(12)
        .map(|e| {
            (
                o.u16(e),
                o.u16(&e[2..]),
                o.u32(&e[4..]),
                [e[8], e[9], e[10], e[11]],
            )
        })
        .collect())
}

/// The kept fields of the directory at `at`, values read in one range.
fn read_fields(
    head: &Head,
    o: Order,
    at: u64,
    keep: impl Fn(u16, u16) -> bool,
) -> Result<Vec<Field>> {
    let mut fields = Vec::new();
    let (mut lo, mut hi) = (u64::MAX, 0);
    for (tag, typ, count, value) in raw_entries(head, o, at)? {
        let Some(size) = type_size(typ) else { continue };
        let len = size * count as u64;
        if !keep(tag, typ) || len == 0 || len > 65000 {
            continue;
        }
        if len > 4 {
            let off = o.u32(&value) as u64;
            (lo, hi) = (lo.min(off), hi.max(off + len));
        }
        fields.push((tag, typ, count, len, value));
    }
    let block = if hi > lo {
        ensure!(hi - lo <= 1 << 20, "TIFF values too spread out");
        head.get(lo, (hi - lo) as usize)?
    } else {
        Vec::new()
    };
    Ok(fields
        .into_iter()
        .map(|(tag, typ, count, len, value)| {
            let data = if len <= 4 {
                value[..len as usize].to_vec()
            } else {
                let at = (o.u32(&value) as u64 - lo) as usize;
                block[at..at + len as usize].to_vec()
            };
            Field {
                tag,
                typ,
                count,
                data,
            }
        })
        .collect())
}

/// IFD0 tags worth carrying into a JPEG; image-structure tags are dropped.
const IFD0_KEEP: [u16; 8] = [270, 271, 272, 274, 305, 306, 315, 33432];
const CAMERA_SERIAL: u16 = 50735; // DNG IFD0 -> EXIF BodySerialNumber
const LENS_INFO: u16 = 50736; // DNG IFD0 -> EXIF LensSpecification

/// The EXIF and XMP of a DNG, for serving with a JPEG.
struct DngMetadata {
    order: Order,
    ifd0: Vec<Field>,
    exif: Vec<Field>,
    gps: Vec<Field>,
    /// Offset and length of the XMP packet in the DNG.
    xmp: Option<(u64, u64)>,
    stamp: Stamp,
}

fn source_metadata(source: &Source, path: &str) -> Result<DngMetadata> {
    let file = source.open_raw(&components(path)?)?;
    ensure!(file.metadata()?.is_file(), "not a file");
    dng_metadata(&file)
}

fn dng_metadata(file: &File) -> Result<DngMetadata> {
    let stamp = Stamp::of(file)?;
    let head = Head::new(file, stamp.size)?;
    let header = head.get(0, 8)?;
    let o = Order(match &header[..2] {
        b"II" => true,
        b"MM" => false,
        _ => bail!("not a TIFF/DNG file"),
    });
    ensure!(o.u16(&header[2..4]) == 42, "not classic TIFF");
    let ifd0_at = o.u32(&header[4..8]) as u64;
    let raw = raw_entries(&head, o, ifd0_at)?;
    let pointer = |tag| {
        raw.iter()
            .find(|e| e.0 == tag && matches!(e.1, 4 | 13) && e.2 == 1)
            .map(|e| o.u32(&e.3) as u64)
    };
    let xmp = raw
        .iter()
        .find(|e| e.0 == 700 && matches!(e.1, 1 | 7) && e.2 > 0)
        .map(|e| (o.u32(&e.3) as u64, e.2 as u64))
        .filter(|&(off, len)| off.checked_add(len).is_some_and(|end| end <= stamp.size));
    let ifd0 = read_fields(&head, o, ifd0_at, |tag, _| {
        IFD0_KEEP.contains(&tag) || tag == CAMERA_SERIAL || tag == LENS_INFO
    })?;
    // Pointers and the maker note (whose offsets are its own) are not copied.
    let exif = match pointer(34665) {
        Some(at) => read_fields(&head, o, at, |tag, typ| {
            typ != 13 && !matches!(tag, 34665 | 34853 | 37500 | 40965)
        })?,
        None => Vec::new(),
    };
    let gps = match pointer(34853) {
        Some(at) => read_fields(&head, o, at, |_, typ| typ != 13)?,
        None => Vec::new(),
    };
    Ok(DngMetadata {
        order: o,
        ifd0,
        exif,
        gps,
        xmp,
        stamp,
    })
}

fn set(fields: &mut Vec<Field>, field: Field) {
    fields.retain(|f| f.tag != field.tag);
    fields.push(field);
}

/// Bytes a directory of `fields` takes, including its out-of-line values.
fn dir_size(fields: &[Field]) -> u32 {
    let values: usize = fields
        .iter()
        .filter(|f| f.data.len() > 4)
        .map(|f| f.data.len() + (f.data.len() & 1))
        .sum();
    (2 + 12 * fields.len() + 4 + values) as u32
}

fn write_dir(o: Order, out: &mut Vec<u8>, fields: &[Field]) {
    let mut values_at = out.len() + 2 + 12 * fields.len() + 4;
    let mut values = Vec::new();
    out.extend(o.p16(fields.len() as u16));
    for f in fields {
        out.extend(o.p16(f.tag));
        out.extend(o.p16(f.typ));
        out.extend(o.p32(f.count));
        if f.data.len() <= 4 {
            let mut v = f.data.clone();
            v.resize(4, 0);
            out.extend(v);
        } else {
            out.extend(o.p32(values_at as u32));
            values.extend(&f.data);
            if f.data.len() & 1 == 1 {
                values.push(0);
            }
            values_at += f.data.len() + (f.data.len() & 1);
        }
    }
    out.extend([0; 4]);
    out.extend(values);
}

/// An EXIF APP1 segment holding IFD0, an EXIF IFD and a GPS IFD.
fn exif_segment(
    o: Order,
    mut ifd0: Vec<Field>,
    mut exif: Vec<Field>,
    mut gps: Vec<Field>,
) -> Result<Vec<u8>> {
    for dir in [&mut ifd0, &mut exif, &mut gps] {
        dir.sort_by_key(|f| f.tag);
        dir.dedup_by_key(|f| f.tag);
    }
    if !exif.is_empty() {
        set(&mut ifd0, Field::long(o, 34665, 0));
    }
    if !gps.is_empty() {
        set(&mut ifd0, Field::long(o, 34853, 0));
    }
    ifd0.sort_by_key(|f| f.tag);
    let exif_at = 8 + dir_size(&ifd0);
    let gps_at = exif_at + if exif.is_empty() { 0 } else { dir_size(&exif) };
    for f in &mut ifd0 {
        match f.tag {
            34665 => f.data = o.p32(exif_at).to_vec(),
            34853 => f.data = o.p32(gps_at).to_vec(),
            _ => (),
        }
    }
    let mut tiff = if o.0 {
        b"II\x2a\0".to_vec()
    } else {
        b"MM\0\x2a".to_vec()
    };
    tiff.extend(o.p32(8));
    write_dir(o, &mut tiff, &ifd0);
    if !exif.is_empty() {
        write_dir(o, &mut tiff, &exif);
    }
    if !gps.is_empty() {
        write_dir(o, &mut tiff, &gps);
    }
    ensure!(
        tiff.len() + 8 <= 65535,
        "EXIF too large for one APP1 segment"
    );
    let mut seg = vec![0xff, 0xe1];
    seg.extend(((tiff.len() + 8) as u16).to_be_bytes());
    seg.extend(b"Exif\0\0");
    seg.extend(tiff);
    Ok(seg)
}

const XMP_SIGNATURE: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

fn jpeg_with_exif(
    file: &File,
    size: u64,
    orientation: Option<u16>,
    datetime: Option<&str>,
    dng: Option<(DngMetadata, Arc<Source>, Arc<str>)>,
) -> Result<Option<Vec<Piece>>> {
    let head = Head::new(file, size)?;
    ensure!(head.get(0, 2)? == [0xff, 0xd8], "not a JPEG");
    let mut pos = 2u64;
    let mut has_xmp = false;
    let mut dims = None;
    while pos + 4 <= size {
        let m = head.get(pos, 4)?;
        ensure!(m[0] == 0xff, "bad JPEG marker");
        if m[1] == 0xda {
            break;
        }
        let len = u16::from_be_bytes([m[2], m[3]]) as u64;
        if m[1] == 0xe1 && len >= 8 && head.get(pos + 4, 6)? == b"Exif\0\0" {
            return Ok(None); // already has EXIF; leave it alone
        }
        if m[1] == 0xe1
            && len >= 2 + XMP_SIGNATURE.len() as u64
            && head.get(pos + 4, XMP_SIGNATURE.len())? == XMP_SIGNATURE
        {
            has_xmp = true;
        }
        if matches!(m[1], 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let f = head.get(pos + 5, 4)?;
            dims = Some((
                u16::from_be_bytes([f[2], f[3]]) as u32,
                u16::from_be_bytes([f[0], f[1]]) as u32,
            ));
        }
        pos += 2 + len;
    }
    let (o, mut ifd0, mut exif, gps) = match &dng {
        Some((m, ..)) => (m.order, m.ifd0.clone(), m.exif.clone(), m.gps.clone()),
        None => (Order(false), Vec::new(), Vec::new(), Vec::new()),
    };
    // DNG-only tags become their EXIF equivalents.
    for (from, to) in [(CAMERA_SERIAL, 42033), (LENS_INFO, 42034)] {
        if let Some(i) = ifd0.iter().position(|f| f.tag == from) {
            let mut f = ifd0.remove(i);
            if !exif.iter().any(|e| e.tag == to) {
                f.tag = to;
                exif.push(f);
            }
        }
    }
    if let Some(v) = orientation {
        set(&mut ifd0, Field::short(o, 274, v));
    }
    if let Some(dt) = datetime {
        set(&mut exif, Field::ascii(36867, dt));
    }
    if let Some((w, h)) = dims.filter(|_| !exif.is_empty()) {
        set(&mut exif, Field::long(o, 40962, w)); // ExifImageWidth of this JPEG
        set(&mut exif, Field::long(o, 40963, h));
    }
    let mut bytes = vec![0xff, 0xd8];
    bytes.extend(exif_segment(o, ifd0, exif, gps)?);
    let mut pieces = Vec::new();
    if let Some((m, source, path)) = dng
        && let Some((offset, length)) = m
            .xmp
            .filter(|&(_, l)| !has_xmp && l <= 65533 - XMP_SIGNATURE.len() as u64)
    {
        bytes.extend([0xff, 0xe1]);
        bytes.extend(((2 + XMP_SIGNATURE.len() as u64 + length) as u16).to_be_bytes());
        bytes.extend(XMP_SIGNATURE);
        pieces.push(Piece::Bytes(std::mem::take(&mut bytes)));
        pieces.push(Piece::Other {
            source,
            path,
            offset,
            length,
            stamp: Some(m.stamp),
        });
    }
    if !bytes.is_empty() {
        pieces.push(Piece::Bytes(bytes));
    }
    pieces.push(Piece::Own {
        offset: 2,
        length: size - 2,
    });
    Ok(Some(pieces))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut j = vec![0xff, 0xd8, 0xff, 0xe0, 0, 16];
        j.extend(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        j.extend([0xff, 0xc0, 0, 11, 8]);
        j.extend(height.to_be_bytes());
        j.extend(width.to_be_bytes());
        j.extend([1, 1, 0x11, 0]);
        j.extend([0xff, 0xda, 0, 2, 0xaa, 0xbb, 0xff, 0xd9]);
        j
    }

    /// Minimal little-endian DNG-like TIFF: IFD0 with a thumbnail tag and one SubIFD.
    fn tiff() -> Vec<u8> {
        let o = Order(true);
        let mut t = b"II\x2a\0".to_vec();
        t.extend(o.p32(8));
        t.extend(o.p16(3));
        t.extend(o.entry(254, 4, 1, 1));
        t.extend(o.entry(274, 3, 1, 8));
        t.extend(o.entry(330, 4, 1, 50));
        t.extend([0; 4]);
        t.resize(50, 0);
        t.extend(o.p16(1));
        t.extend(o.entry(254, 4, 1, 0));
        t.extend([0; 4]);
        t.extend(b"RAWDATA");
        t
    }

    fn assemble(pieces: Vec<Piece>, own: &[u8], other: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in pieces {
            match p {
                Piece::Bytes(b) => out.extend(b),
                Piece::Own { offset, length } => {
                    out.extend(&own[offset as usize..(offset + length) as usize])
                }
                Piece::Other { offset, length, .. } => {
                    out.extend(&other[offset as usize..(offset + length) as usize])
                }
            }
        }
        out
    }

    fn ifd(t: &[u8], at: usize) -> Vec<(u16, u16, u32, u32)> {
        let o = Order(&t[..2] == b"II");
        let n = o.u16(&t[at..]) as usize;
        (0..n)
            .map(|i| {
                let e = &t[at + 2 + 12 * i..];
                let typ = o.u16(&e[2..]);
                let v = if typ == 3 && o.u32(&e[4..]) == 1 {
                    o.u16(&e[8..]) as u32
                } else {
                    o.u32(&e[8..])
                };
                (o.u16(e), typ, o.u32(&e[4..]), v)
            })
            .collect()
    }

    fn setup(map: &str, files: &[(&str, &[u8])]) -> Result<(tempfile::TempDir, Previews)> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("src"))?;
        std::fs::create_dir_all(dir.path().join("prev/ab"))?;
        std::fs::create_dir_all(dir.path().join("meta"))?;
        for (name, data) in files {
            File::create(dir.path().join(name))?.write_all(data)?;
        }
        std::fs::write(dir.path().join("map.tsv"), map)?;
        let p = Previews::new(Config {
            map: Some(dir.path().join("map.tsv")),
            preview_root: Some(dir.path().join("prev")),
            ratings: None,
            metadata_root: Some(dir.path().join("meta")),
        })?;
        Ok((dir, p))
    }

    #[test]
    fn dng_gains_preview_subifd_and_keeps_every_byte() -> Result<()> {
        let (t, j) = (tiff(), jpeg(1600, 1064));
        let (dir, p) = setup(
            "a.dng\tpreview=ab/x.jpg\n",
            &[("src/a.dng", &t), ("prev/ab/x.jpg", &j)],
        )?;
        let f = File::open(dir.path().join("src/a.dng"))?;
        let out = assemble(p.compose(&["a.dng".into()], &f)?.unwrap(), &t, &j);
        assert_eq!(&out[8..t.len()], &t[8..], "original bytes kept in place");
        let o = Order(true);
        let ifd0 = o.u32(&out[4..]) as usize;
        let tags = ifd(&out, ifd0);
        assert!(tags.windows(2).all(|w| w[0].0 < w[1].0), "IFD0 sorted");
        assert!(tags.contains(&(274, 3, 1, 8)), "orientation kept");
        let sub = tags.iter().find(|e| e.0 == 330).unwrap();
        assert_eq!(sub.2, 2);
        let arr = sub.3 as usize;
        assert_eq!(o.u32(&out[arr..]), 50, "original SubIFD kept first");
        let prev = ifd(&out, o.u32(&out[arr + 4..]) as usize);
        let get = |tag| prev.iter().find(|e| e.0 == tag).unwrap().3;
        assert_eq!((get(256), get(257), get(259)), (1600, 1064, 7));
        let (start, len) = (get(273) as usize, get(279) as usize);
        assert_eq!(start % 2, 0);
        assert_eq!(&out[start..start + len], &j[..], "JPEG is the preview");
        assert_eq!(out.len(), start + len);
        Ok(())
    }

    #[test]
    fn jpeg_gains_exif_orientation_and_datetime() -> Result<()> {
        let j = jpeg(1600, 1064);
        let (dir, p) = setup(
            "b.jpg\torientation=6\tdatetime=2024:10:02 04:45:42\n",
            &[("src/b.jpg", &j)],
        )?;
        let f = File::open(dir.path().join("src/b.jpg"))?;
        let out = assemble(p.compose(&["b.jpg".into()], &f)?.unwrap(), &j, &[]);
        assert_eq!(&out[..4], &[0xff, 0xd8, 0xff, 0xe1]);
        let seglen = u16::from_be_bytes([out[4], out[5]]) as usize;
        assert_eq!(&out[6..12], b"Exif\0\0");
        let t = &out[12..4 + seglen];
        let tags = ifd(t, 8);
        assert_eq!(tags[0], (274, 3, 1, 6));
        let exif = ifd(t, tags[1].3 as usize);
        assert_eq!((exif[0].0, exif[0].2), (36867, 20));
        let at = exif[0].3 as usize;
        assert_eq!(&t[at..at + 20], b"2024:10:02 04:45:42\0");
        assert_eq!(&out[4 + seglen..], &j[2..], "image data follows unchanged");
        // A JPEG that already has EXIF is left alone.
        let f2 = dir.path().join("src/c.jpg");
        let mut with = vec![0xff, 0xd8];
        let o = Order(false);
        with.extend(exif_segment(
            o,
            vec![Field::short(o, 274, 1)],
            vec![],
            vec![],
        )?);
        with.extend(&j[2..]);
        std::fs::write(&f2, &with)?;
        assert!(
            jpeg_with_exif(&File::open(&f2)?, with.len() as u64, Some(6), None, None)?.is_none()
        );
        Ok(())
    }

    /// Little-endian DNG-like TIFF with camera tags, an EXIF IFD, a GPS IFD and XMP.
    fn dng_with_metadata(xmp: &[u8]) -> Vec<u8> {
        let o = Order(true);
        let ifd0 = vec![
            Field::short(o, 254, 1),
            Field::short(o, 259, 7),
            Field::ascii(271, "Maker"),
            Field::ascii(272, "Model X"),
            Field::short(o, 274, 8),
            Field {
                tag: 700,
                typ: 1,
                count: xmp.len() as u32,
                data: xmp.to_vec(),
            },
            Field::ascii(CAMERA_SERIAL, "SN123"),
        ];
        let rational = |n: u32, d: u32| [o.p32(n), o.p32(d)].concat();
        let exif = vec![
            Field {
                tag: 33434,
                typ: 5,
                count: 1,
                data: rational(1, 200),
            },
            Field::short(o, 34855, 400),
            Field::ascii(36867, "2020:01:02 03:04:05"),
            Field {
                tag: 37500,
                typ: 7,
                count: 6,
                data: b"secret".to_vec(),
            },
            Field::ascii(42036, "Lens 35mm"),
        ];
        let gps = vec![Field::ascii(1, "N")];
        // Built with the plugin's own writer, then used as a TIFF file.
        let seg = exif_segment(o, ifd0, exif, gps).unwrap();
        let mut t = seg[10..].to_vec();
        t.extend(b"RAWDATA");
        t
    }

    /// The value bytes of `tag` in the directory at `at`.
    fn value(t: &[u8], at: usize, tag: u16) -> Vec<u8> {
        let o = Order(&t[..2] == b"II");
        let n = o.u16(&t[at..]) as usize;
        let e = (0..n)
            .map(|i| &t[at + 2 + 12 * i..at + 14 + 12 * i])
            .find(|e| o.u16(e) == tag)
            .unwrap_or_else(|| panic!("tag {tag} missing"));
        let len = (type_size(o.u16(&e[2..])).unwrap() * o.u32(&e[4..]) as u64) as usize;
        if len <= 4 {
            e[8..8 + len].to_vec()
        } else {
            let v = o.u32(&e[8..]) as usize;
            t[v..v + len].to_vec()
        }
    }

    #[test]
    fn jpeg_gains_exif_and_xmp_from_its_dng() -> Result<()> {
        let j = jpeg(1600, 1064);
        let xmp = b"<x:xmpmeta>rating</x:xmpmeta>";
        let d = dng_with_metadata(xmp);
        let (dir, p) = setup(
            "b.jpg\tmetadata=g/b.dng\torientation=6\tdatetime=2024:10:02 04:45:42\n",
            &[("src/b.jpg", &j)],
        )?;
        std::fs::create_dir_all(dir.path().join("meta/g"))?;
        std::fs::write(dir.path().join("meta/g/b.dng"), &d)?;
        let f = File::open(dir.path().join("src/b.jpg"))?;
        let pieces = p.compose(&["b.jpg".into()], &f)?.unwrap();
        assert!(
            matches!(&pieces[1], Piece::Other { length, .. } if *length == xmp.len() as u64),
            "XMP is read from the DNG, not copied"
        );
        let out = assemble(pieces, &j, &d);
        let seglen = u16::from_be_bytes([out[4], out[5]]) as usize;
        assert_eq!(&out[6..12], b"Exif\0\0");
        let t = &out[12..4 + seglen];
        assert_eq!(&t[..2], b"II", "the DNG's byte order is kept");
        let o = Order(true);
        let tags: Vec<u16> = ifd(t, 8).iter().map(|e| e.0).collect();
        assert_eq!(
            tags,
            [271, 272, 274, 34665, 34853],
            "image-structure tags dropped"
        );
        assert_eq!(value(t, 8, 271), b"Maker\0");
        assert_eq!(value(t, 8, 274), [6, 0], "map orientation wins");
        let exif_at = o.u32(&value(t, 8, 34665)) as usize;
        let exif: Vec<u16> = ifd(t, exif_at).iter().map(|e| e.0).collect();
        assert_eq!(
            exif,
            [33434, 34855, 36867, 40962, 40963, 42033, 42036],
            "maker note dropped, serial and this JPEG's size added"
        );
        assert_eq!(value(t, exif_at, 40962), 1600u32.to_le_bytes());
        assert_eq!(value(t, exif_at, 40963), 1064u32.to_le_bytes());
        assert_eq!(value(t, exif_at, 33434), [1, 0, 0, 0, 200, 0, 0, 0]);
        assert_eq!(value(t, exif_at, 34855), [144, 1]);
        assert_eq!(
            value(t, exif_at, 36867),
            b"2024:10:02 04:45:42\0",
            "map date wins"
        );
        assert_eq!(value(t, exif_at, 42033), b"SN123\0");
        assert_eq!(value(t, exif_at, 42036), b"Lens 35mm\0");
        let gps_at = o.u32(&value(t, 8, 34853)) as usize;
        assert_eq!(value(t, gps_at, 1), b"N\0");
        let x = 4 + seglen;
        assert_eq!(&out[x..x + 2], &[0xff, 0xe1]);
        let xlen = u16::from_be_bytes([out[x + 2], out[x + 3]]) as usize;
        assert_eq!(&out[x + 4..x + 4 + XMP_SIGNATURE.len()], XMP_SIGNATURE);
        assert_eq!(&out[x + 4 + XMP_SIGNATURE.len()..x + 2 + xlen], xmp);
        assert_eq!(
            &out[x + 2 + xlen..],
            &j[2..],
            "image data follows unchanged"
        );

        // Without its DNG, a JPEG still gets the map's orientation and date.
        std::fs::remove_file(dir.path().join("meta/g/b.dng"))?;
        let out = assemble(p.compose(&["b.jpg".into()], &f)?.unwrap(), &j, &[]);
        let seglen = u16::from_be_bytes([out[4], out[5]]) as usize;
        assert_eq!(ifd(&out[12..4 + seglen], 8)[0], (274, 3, 1, 6));
        assert_eq!(&out[4 + seglen..], &j[2..]);
        Ok(())
    }

    #[test]
    fn known_preview_stamp_skips_opening_and_matches_the_opened_layout() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let (t, j) = (tiff(), jpeg(1600, 1064));
        let (dir, _) = setup("", &[("src/a.dng", &t), ("prev/ab/x.jpg", &j)])?;
        let m = std::fs::metadata(dir.path().join("prev/ab/x.jpg"))?;
        let line = format!(
            "a.dng\tpreview=ab/x.jpg\tpreview_size={}\tpreview_mtime={}.{:09}\tpreview_dims=1600x1064\n",
            m.len(),
            m.mtime(),
            m.mtime_nsec()
        );
        let p = |map: &str| -> Result<Previews> {
            std::fs::write(dir.path().join("map.tsv"), map)?;
            Previews::new(Config {
                map: Some(dir.path().join("map.tsv")),
                preview_root: Some(dir.path().join("prev")),
                ratings: None,
                metadata_root: Some(dir.path().join("meta")),
            })
        };
        let f = File::open(dir.path().join("src/a.dng"))?;
        let opened = p("a.dng\tpreview=ab/x.jpg\n")?
            .compose(&["a.dng".into()], &f)?
            .unwrap();
        // With the stamp known, the preview may even be absent at build time.
        std::fs::rename(
            dir.path().join("prev/ab/x.jpg"),
            dir.path().join("prev/ab/y.jpg"),
        )?;
        let known = p(&line)?.compose(&["a.dng".into()], &f)?.unwrap();
        assert_eq!(assemble(opened, &t, &j), assemble(known, &t, &j));
        Ok(())
    }

    #[test]
    fn unmapped_or_broken_files_are_served_unchanged_and_bad_maps_rejected() -> Result<()> {
        let t = tiff();
        let (dir, p) = setup("a.dng\tpreview=ab/missing.jpg\n", &[("src/a.dng", &t)])?;
        let f = File::open(dir.path().join("src/a.dng"))?;
        assert!(p.compose(&["other.dng".into()], &f)?.is_none());
        assert!(
            p.compose(&["a.dng".into()], &f)?.is_none(),
            "missing preview"
        );
        for bad in [
            "a\tpreview=../x\n",
            "a\torientation=9\n",
            "a\tdatetime=2024-10-02 04:45:42\n",
            "a\tcolour=red\n",
            "a\n a\n".replace(' ', "").as_str(),
        ] {
            assert!(parse_map(bad).is_err(), "{bad:?}");
        }
        Ok(())
    }
}
