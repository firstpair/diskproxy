//! Previews plugin: present stand-in files with better previews, without copying.
//!
//! A map file lists source-relative paths, one per line, followed by TAB-separated
//! `key=value` fields:
//!
//! * `preview=<path under preview_root>` on a DNG/TIFF: serve the file with that
//!   JPEG appended as an additional reduced-resolution preview SubIFD. The TIFF
//!   header is repointed at an appended copy of IFD0; every original byte is kept.
//! * `orientation=<1..8>` and `datetime=YYYY:MM:DD HH:MM:SS` on a JPEG without
//!   EXIF: serve it with a small EXIF APP1 segment inserted after SOI.
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
}

#[derive(Default, Debug, PartialEq)]
struct Entry {
    /// Shared with every layout built for this file.
    preview: Option<Arc<str>>,
    /// Size, mtime and dimensions of the preview, so startup need not open it.
    preview_size: Option<u64>,
    preview_mtime: Option<(i64, i64)>,
    preview_dims: Option<(u32, u32)>,
    orientation: Option<u16>,
    datetime: Option<[u8; 19]>,
}

pub struct Previews {
    entries: HashMap<Box<str>, Entry>,
    previews: Option<Arc<Source>>,
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
        let capture = config.ratings.map(capture::Capture::new).transpose()?;
        Ok(Self {
            entries,
            previews,
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
        } else if entry.orientation.is_some() || entry.datetime.is_some() {
            let datetime = entry
                .datetime
                .as_ref()
                .map(|d| std::str::from_utf8(d).unwrap());
            jpeg_with_exif(file, size, entry.orientation, datetime)
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

/// A big-endian EXIF APP1 segment with Orientation and DateTimeOriginal.
fn exif_segment(orientation: Option<u16>, datetime: Option<&str>) -> Vec<u8> {
    let o = Order(false);
    let mut ifd0 = Vec::new();
    if let Some(v) = orientation {
        ifd0.push(o.entry(274, 3, 1, v as u32));
    }
    let exif_at = 8 + 2 + 12 * (ifd0.len() as u32 + datetime.is_some() as u32) + 4;
    if datetime.is_some() {
        ifd0.push(o.entry(34665, 4, 1, exif_at));
    }
    let mut tiff = b"MM\0\x2a".to_vec();
    tiff.extend(o.p32(8));
    tiff.extend(o.p16(ifd0.len() as u16));
    ifd0.iter().for_each(|e| tiff.extend(e));
    tiff.extend([0; 4]);
    if let Some(dt) = datetime {
        let text_at = exif_at + 2 + 12 + 4;
        tiff.extend(o.p16(1));
        tiff.extend(o.entry(36867, 2, 20, text_at));
        tiff.extend([0; 4]);
        tiff.extend(dt.as_bytes());
        tiff.push(0);
    }
    let mut seg = vec![0xff, 0xe1];
    seg.extend(((tiff.len() + 8) as u16).to_be_bytes());
    seg.extend(b"Exif\0\0");
    seg.extend(tiff);
    seg
}

fn jpeg_with_exif(
    file: &File,
    size: u64,
    orientation: Option<u16>,
    datetime: Option<&str>,
) -> Result<Option<Vec<Piece>>> {
    let head = Head::new(file, size)?;
    ensure!(head.get(0, 2)? == [0xff, 0xd8], "not a JPEG");
    let mut pos = 2u64;
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
        pos += 2 + len;
    }
    let mut head = vec![0xff, 0xd8];
    head.extend(exif_segment(orientation, datetime));
    Ok(Some(vec![
        Piece::Bytes(head),
        Piece::Own {
            offset: 2,
            length: size - 2,
        },
    ]))
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
        for (name, data) in files {
            File::create(dir.path().join(name))?.write_all(data)?;
        }
        std::fs::write(dir.path().join("map.tsv"), map)?;
        let p = Previews::new(Config {
            map: Some(dir.path().join("map.tsv")),
            preview_root: Some(dir.path().join("prev")),
            ratings: None,
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
        with.extend(exif_segment(Some(1), None));
        with.extend(&j[2..]);
        std::fs::write(&f2, &with)?;
        assert!(jpeg_with_exif(&File::open(&f2)?, with.len() as u64, Some(6), None)?.is_none());
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
