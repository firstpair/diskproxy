//! A generated exFAT volume: metadata lives in RAM, photo extents stream from
//! O_RDONLY source descriptors. No disk image or copy of the photos is written.
use super::{Reply, View, byte_range, error_status, header};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Datelike, Timelike, Utc};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::Arc;
use tiny_http::Request;

const SECTOR: u64 = 512;
const CLUSTER: u64 = 256 * 1024;

struct Node {
    parts: Vec<String>,
    children: Vec<usize>,
    directory: bool,
    length: u64,
    raw_mtime: i64,
    raw_nsec: i64,
    mtime: i64,
    cluster: u32,
    clusters: u32,
}

enum Data {
    Bytes(Vec<u8>),
    Source {
        parts: Vec<String>,
        length: u64,
        mtime: i64,
        nsec: i64,
    },
}

struct Extent {
    length: u64,
    data: Data,
}

pub struct Image {
    pub length: u64,
    extents: BTreeMap<u64, Extent>,
}

fn put16(data: &mut [u8], pos: usize, value: u16) {
    data[pos..pos + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(data: &mut [u8], pos: usize, value: u32) {
    data[pos..pos + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(data: &mut [u8], pos: usize, value: u64) {
    data[pos..pos + 8].copy_from_slice(&value.to_le_bytes());
}
fn clusters(length: u64) -> Result<u32> {
    Ok(u32::try_from(length.div_ceil(CLUSTER))?)
}
fn checksum32(data: &[u8]) -> u32 {
    data.iter()
        .fold(0u32, |sum, b| sum.rotate_right(1).wrapping_add(*b as u32))
}
fn upper(c: u16) -> u16 {
    if (97..=122).contains(&c) { c - 32 } else { c }
}

fn timestamp(epoch: i64) -> Result<(u32, u8)> {
    let d = DateTime::<Utc>::from_timestamp(epoch, 0).context("invalid image timestamp")?;
    ensure!(
        (1980..=2107).contains(&d.year()),
        "exFAT timestamp is outside 1980..2107"
    );
    let date = ((d.year() - 1980) as u32) << 9 | d.month() << 5 | d.day();
    let time = (d.hour() << 11) | (d.minute() << 5) | (d.second() / 2);
    Ok((date << 16 | time, (d.second() % 2 * 100) as u8))
}

fn entry(node: &Node) -> Result<Vec<u8>> {
    let name: Vec<u16> = node
        .parts
        .last()
        .context("missing disk entry name")?
        .encode_utf16()
        .collect();
    ensure!(
        !name.is_empty() && name.len() <= 255,
        "invalid exFAT name length"
    );
    let count = 2 + name.len().div_ceil(15);
    let mut out = vec![0u8; count * 32];
    out[0] = 0x85;
    out[1] = (count - 1) as u8;
    put16(&mut out, 4, if node.directory { 0x10 } else { 0x20 });
    let (time, increment) = timestamp(node.mtime)?;
    for pos in [8, 12, 16] {
        put32(&mut out, pos, time);
    }
    out[20] = increment;
    out[21] = increment;
    // Explicit UTC timestamps: valid offset zero, independent of mount timezone.
    out[22..25].fill(0x80);
    out[32] = 0xc0;
    out[33] = if node.clusters == 0 { 1 } else { 3 };
    out[35] = name.len() as u8;
    let hash = name
        .iter()
        .flat_map(|c| upper(*c).to_le_bytes())
        .fold(0u16, |sum, b| sum.rotate_right(1).wrapping_add(b as u16));
    put16(&mut out, 36, hash);
    put64(&mut out, 40, node.length);
    put32(&mut out, 52, node.cluster);
    put64(&mut out, 56, node.length);
    for (i, chunk) in name.chunks(15).enumerate() {
        let pos = (i + 2) * 32;
        out[pos] = 0xc1;
        for (j, c) in chunk.iter().enumerate() {
            put16(&mut out, pos + 2 + j * 2, *c);
        }
    }
    let sum = out
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 2 && *i != 3)
        .fold(0u16, |sum, (_, b)| {
            sum.rotate_right(1).wrapping_add(*b as u16)
        });
    put16(&mut out, 2, sum);
    Ok(out)
}

fn scan(view: &View, nodes: &mut Vec<Node>, parts: Vec<String>) -> Result<usize> {
    ensure!(
        parts.len() <= 64 && nodes.len() < 1_000_000,
        "card tree too large or deep"
    );
    let file = view.open(&parts)?;
    let raw = file.metadata()?;
    let meta = view.metadata(&parts, &file)?;
    let id = nodes.len();
    nodes.push(Node {
        parts: parts.clone(),
        children: vec![],
        directory: meta.directory,
        length: meta.size,
        raw_mtime: raw.mtime(),
        raw_nsec: raw.mtime_nsec(),
        mtime: meta.mtime,
        cluster: 0,
        clusters: 0,
    });
    if meta.directory {
        let names = view.names(&parts, &file)?;
        for name in names {
            let mut child = parts.clone();
            child.push(name);
            match scan(view, nodes, child) {
                Ok(child) => nodes[id].children.push(child),
                Err(e) if matches!(error_status(&e), 403 | 404) => (),
                Err(e) => return Err(e),
            }
        }
    }
    Ok(id)
}

impl Image {
    pub fn new(view: &View) -> Result<Self> {
        let mut nodes = Vec::new();
        scan(view, &mut nodes, vec![])?;
        for i in 0..nodes.len() {
            if nodes[i].directory {
                let mut length = if i == 0 { 96 } else { 0 }; // label, bitmap, upcase
                for &child in &nodes[i].children {
                    let n = nodes[child].parts.last().unwrap().encode_utf16().count();
                    length += (2 + n.div_ceil(15)) * 32;
                }
                nodes[i].length = (length as u64 + 32).div_ceil(CLUSTER) * CLUSTER;
            }
            nodes[i].clusters = clusters(nodes[i].length)?;
        }
        let node_clusters: u64 = nodes.iter().map(|n| n.clusters as u64).sum();
        // Full-range compressed upcase table with ASCII case folding.
        let mut upcase = Vec::new();
        for code in [0xffffu16, 97]
            .into_iter()
            .chain(65..=90)
            .chain([0xffff, 65536u32.wrapping_sub(123) as u16])
        {
            upcase.extend_from_slice(&code.to_le_bytes());
        }
        let mut count = (node_clusters + 34).max(128);
        loop {
            let next = (node_clusters + 1 + count.div_ceil(8).div_ceil(CLUSTER) + 32).max(128);
            if next == count {
                break;
            }
            count = next;
        }
        ensure!(count < 0xfffffff5, "exFAT volume too large");
        let bitmap_length = count.div_ceil(8);
        let bitmap_clusters = clusters(bitmap_length)?;
        let mut next = 2u32;
        for node in &mut nodes {
            if node.clusters != 0 {
                node.cluster = next;
                next += node.clusters;
            }
        }
        let bitmap_cluster = next;
        next += bitmap_clusters;
        let upcase_cluster = next;
        next += 1;
        let allocated = (next - 2) as usize;
        let fat_offset = 128u32;
        let fat_length = ((count + 2) * 4).div_ceil(SECTOR) as u32;
        let heap_sector =
            (fat_offset as u64 + fat_length as u64).div_ceil(CLUSTER / SECTOR) * (CLUSTER / SECTOR);
        let heap = heap_sector * SECTOR;
        let length = heap + count * CLUSTER;
        let mut prefix = vec![0u8; heap as usize];
        let boot = &mut prefix[..12 * 512];
        boot[..3].copy_from_slice(&[0xeb, 0x76, 0x90]);
        boot[3..11].copy_from_slice(b"EXFAT   ");
        put64(boot, 72, length / SECTOR);
        put32(boot, 80, fat_offset);
        put32(boot, 84, fat_length);
        put32(boot, 88, heap_sector as u32);
        put32(boot, 92, count as u32);
        put32(boot, 96, nodes[0].cluster);
        put32(boot, 100, checksum32(view.identity()?.as_bytes()));
        put16(boot, 104, 0x100);
        boot[108] = 9;
        boot[109] = 9;
        boot[110] = 1;
        boot[111] = 0x80;
        boot[112] = (allocated as u64 * 100 / count) as u8;
        boot[510..512].copy_from_slice(&[0x55, 0xaa]);
        for sector in 1..=8 {
            put32(boot, sector * 512 + 508, 0xaa550000);
        }
        let sum = boot[..11 * 512]
            .iter()
            .enumerate()
            .filter(|(i, _)| ![106, 107, 112].contains(i))
            .fold(0u32, |s, (_, b)| s.rotate_right(1).wrapping_add(*b as u32));
        for i in 0..128 {
            put32(boot, 11 * 512 + i * 4, sum);
        }
        prefix.copy_within(0..12 * 512, 12 * 512);
        let fat = &mut prefix[fat_offset as usize * 512..];
        put32(fat, 0, 0xfffffff8);
        put32(fat, 4, 0xffffffff);
        for node in &nodes {
            for c in node.cluster..node.cluster + node.clusters {
                put32(
                    fat,
                    c as usize * 4,
                    if c + 1 == node.cluster + node.clusters {
                        0xffffffff
                    } else {
                        c + 1
                    },
                );
            }
        }
        for c in bitmap_cluster..bitmap_cluster + bitmap_clusters {
            put32(
                fat,
                c as usize * 4,
                if c + 1 == bitmap_cluster + bitmap_clusters {
                    0xffffffff
                } else {
                    c + 1
                },
            );
        }
        put32(fat, upcase_cluster as usize * 4, 0xffffffff);
        let offset = |cluster: u32| heap + (cluster as u64 - 2) * CLUSTER;
        let mut image = Self {
            length,
            extents: BTreeMap::new(),
        };
        image.bytes(0, prefix);
        for (i, node) in nodes.iter().enumerate() {
            if node.clusters == 0 {
                continue;
            }
            if node.directory {
                let mut bytes = Vec::new();
                if i == 0 {
                    let mut label = [0u8; 32];
                    label[0] = 0x83;
                    label[1] = view.label.encode_utf16().count() as u8;
                    for (j, c) in view.label.encode_utf16().enumerate() {
                        put16(&mut label, 2 + j * 2, c);
                    }
                    bytes.extend_from_slice(&label);
                    let mut bitmap = [0u8; 32];
                    bitmap[0] = 0x81;
                    put32(&mut bitmap, 20, bitmap_cluster);
                    put64(&mut bitmap, 24, bitmap_length);
                    bytes.extend_from_slice(&bitmap);
                    let mut table = [0u8; 32];
                    table[0] = 0x82;
                    put32(&mut table, 4, checksum32(&upcase));
                    put32(&mut table, 20, upcase_cluster);
                    put64(&mut table, 24, upcase.len() as u64);
                    bytes.extend_from_slice(&table);
                }
                for &child in &node.children {
                    bytes.extend_from_slice(&entry(&nodes[child])?);
                }
                bytes.resize(node.length as usize, 0);
                image.bytes(offset(node.cluster), bytes);
            } else {
                image.extents.insert(
                    offset(node.cluster),
                    Extent {
                        length: node.length,
                        data: Data::Source {
                            parts: node.parts.clone(),
                            length: node.length,
                            mtime: node.raw_mtime,
                            nsec: node.raw_nsec,
                        },
                    },
                );
            }
        }
        let mut bitmap = vec![0u8; bitmap_length as usize];
        for i in 0..allocated {
            bitmap[i / 8] |= 1 << (i % 8);
        }
        image.bytes(offset(bitmap_cluster), bitmap);
        image.bytes(offset(upcase_cluster), upcase);
        Ok(image)
    }

    fn bytes(&mut self, offset: u64, bytes: Vec<u8>) {
        self.extents.insert(
            offset,
            Extent {
                length: bytes.len() as u64,
                data: Data::Bytes(bytes),
            },
        );
    }

    pub fn reply(self: &Arc<Self>, view: &Arc<View>, request: &Request) -> Result<Reply> {
        let range = header(request, "Range");
        let (start, length) = match byte_range(range, self.length) {
            Ok(r) => r,
            Err(()) => {
                return Ok(
                    Reply::empty(416).header("Content-Range", format!("bytes */{}", self.length))
                );
            }
        };
        let mut reply = Reply {
            code: if range.is_some() { 206 } else { 200 },
            headers: vec![],
            body: Box::new(ImageReader {
                image: self.clone(),
                view: view.clone(),
                position: start,
                remaining: length,
                cached: None,
            }),
            length: usize::try_from(length)?,
        }
        .header("Content-Type", "application/octet-stream")
        .header("Accept-Ranges", "bytes")
        .header("ETag", format!("\"pmp-image-{}\"", self.length));
        if range.is_some() {
            reply = reply.header(
                "Content-Range",
                format!("bytes {start}-{}/{}", start + length - 1, self.length),
            );
        }
        Ok(reply)
    }
}

struct ImageReader {
    image: Arc<Image>,
    view: Arc<View>,
    position: u64,
    remaining: u64,
    cached: Option<(u64, File)>,
}

impl Read for ImageReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let limit = out.len().min(self.remaining as usize);
        if limit == 0 {
            return Ok(0);
        }
        let pos = self.position;
        let mut length = limit;
        if let Some((&start, extent)) = self
            .image
            .extents
            .range(..=pos)
            .next_back()
            .filter(|(s, e)| pos < **s + e.length)
        {
            length = length.min((extent.length - (pos - start)) as usize);
            match &extent.data {
                Data::Bytes(bytes) => out[..length].copy_from_slice(
                    &bytes[(pos - start) as usize..(pos - start) as usize + length],
                ),
                Data::Source {
                    parts,
                    length: expected,
                    mtime,
                    nsec,
                } => {
                    if self.cached.as_ref().map(|(s, _)| *s) != Some(start) {
                        let file = self.view.open(parts).map_err(io::Error::other)?;
                        let meta = file.metadata()?;
                        if meta.len() != *expected
                            || meta.mtime() != *mtime
                            || meta.mtime_nsec() != *nsec
                        {
                            return Err(io::Error::other(
                                "source file changed since disk snapshot; detach and restart",
                            ));
                        }
                        self.cached = Some((start, file));
                    }
                    self.cached
                        .as_ref()
                        .unwrap()
                        .1
                        .read_exact_at(&mut out[..length], pos - start)?;
                }
            }
        } else {
            if let Some((&next, _)) = self.image.extents.range(pos..).next() {
                length = length.min((next - pos) as usize);
            }
            out[..length].fill(0);
        }
        self.position += length as u64;
        self.remaining -= length as u64;
        Ok(length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_timestamp_has_valid_utc_and_correct_checksum() {
        let node = Node {
            parts: vec!["ALXY8703.DNG".into()],
            children: vec![],
            directory: false,
            length: 42484224,
            raw_mtime: 1789355532,
            raw_nsec: 0,
            mtime: 1789413132,
            cluster: 3,
            clusters: 162,
        };
        let e = entry(&node).unwrap();
        assert_eq!(&e[22..25], &[0x80, 0x80, 0x80]);
        let sum = e
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 2 && *i != 3)
            .fold(0u16, |s, (_, b)| s.rotate_right(1).wrapping_add(*b as u16));
        assert_eq!(u16::from_le_bytes([e[2], e[3]]), sum);
        assert_eq!(e[1], 2);
    }
}
