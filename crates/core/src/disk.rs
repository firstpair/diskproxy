//! A generated exFAT volume: metadata lives in RAM, photo extents stream from
//! O_RDONLY source descriptors. No disk image or copy of the photos is written.
use super::view::{Meta, Resolved, Source, Stamp, split_path};
use super::{Reply, View, byte_range, error_status, header};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Datelike, Timelike, Utc};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tiny_http::Request;

const SECTOR: u64 = 512;
const CLUSTER: u64 = 256 * 1024;

struct Node {
    /// Last path component; empty for the root.
    name: Box<str>,
    parent: u32,
    children: Vec<u32>,
    directory: bool,
    length: u64,
    layout: Vec<Resolved>,
    mtime: i64,
    cluster: u32,
    clusters: u32,
}

enum Data {
    Bytes(Box<[u8]>),
    /// A range of `files[file]`; `base` is the file offset at the extent start.
    File {
        file: u32,
        base: u64,
    },
}

/// A file read by the image, with the identity captured at build time.
enum FileRef {
    /// The presented source file at tree node `node`.
    Own { node: u32, stamp: Stamp },
    Other {
        source: Arc<Source>,
        path: Arc<str>,
        stamp: Stamp,
    },
}

struct Extent {
    start: u64,
    length: u64,
    data: Data,
}

pub struct Image {
    pub length: u64,
    /// Sorted by start, non-overlapping; gaps read as zeros.
    extents: Vec<Extent>,
    files: Vec<FileRef>,
    /// (name, parent) for every scanned node, to rebuild source paths on read.
    tree: Vec<(Box<str>, u32)>,
}

fn parts_of(tree: &[(Box<str>, u32)], mut id: u32) -> Vec<String> {
    let mut parts = Vec::new();
    while id != 0 {
        let (name, parent) = &tree[id as usize];
        parts.push(name.to_string());
        id = *parent;
    }
    parts.reverse();
    parts
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

/// First and last instants an exFAT timestamp can hold (UTC; even seconds).
const EXFAT_FIRST: i64 = 315_532_800; // 1980-01-01 00:00:00
const EXFAT_LAST: i64 = 4_354_819_198; // 2107-12-31 23:59:58

/// The time to write for `epoch`: itself, or the nearest representable one.
fn exfat_time(epoch: i64) -> i64 {
    epoch.clamp(EXFAT_FIRST, EXFAT_LAST)
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

/// The name as written to exFAT: composed (NFC), the form macOS's exFAT driver
/// uses for lookups. Source files keep their own spelling for opens.
fn disk_name(name: &str) -> Vec<u16> {
    use unicode_normalization::UnicodeNormalization;
    name.nfc().collect::<String>().encode_utf16().collect()
}

fn entry(node: &Node) -> Result<Vec<u8>> {
    let name = disk_name(&node.name);
    ensure!(
        !name.is_empty() && name.len() <= 255,
        "invalid exFAT name length"
    );
    let count = 2 + name.len().div_ceil(15);
    let mut out = vec![0u8; count * 32];
    out[0] = 0x85;
    out[1] = (count - 1) as u8;
    put16(&mut out, 4, if node.directory { 0x10 } else { 0x20 });
    let (time, increment) = timestamp(exfat_time(node.mtime))?;
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

const SCAN_THREADS: usize = 8;

/// Scan counters, reported to stderr every two seconds on long scans.
#[derive(Default)]
struct Progress {
    folders: AtomicUsize,
    found: AtomicUsize,
    /// Set once the folder walk is complete.
    total: AtomicUsize,
    done: AtomicUsize,
    finished: AtomicBool,
}
impl Progress {
    fn line(&self) -> String {
        let (folders, found) = (
            self.folders.load(Ordering::Relaxed),
            self.found.load(Ordering::Relaxed),
        );
        match self.total.load(Ordering::Relaxed) {
            0 => format!("progress: listing folders: {folders} folders, {found} files"),
            total => {
                let done = self.done.load(Ordering::Relaxed);
                format!(
                    "progress: reading files: {done} of {total} ({}%)",
                    done * 100 / total
                )
            }
        }
    }
    fn report(&self) {
        let start = std::time::Instant::now();
        while !self.finished.load(Ordering::Relaxed) {
            std::thread::park_timeout(std::time::Duration::from_secs(2));
            if !self.finished.load(Ordering::Relaxed) {
                eprintln!("{}", self.line());
            }
        }
        if start.elapsed().as_secs() >= 2 {
            eprintln!(
                "progress: scanned {} files in {} folders in {} s",
                self.done.load(Ordering::Relaxed),
                self.folders.load(Ordering::Relaxed),
                start.elapsed().as_secs()
            );
        }
    }
}

fn not_found() -> anyhow::Error {
    io::Error::new(io::ErrorKind::NotFound, "entry changed type during scan").into()
}

/// Phase 1: walk directories serially, adding placeholder nodes for files.
fn walk(
    view: &View,
    progress: &Progress,
    nodes: &mut Vec<Node>,
    jobs: &mut Vec<(u32, Vec<u32>)>,
    parts: Vec<String>,
    parent: u32,
    file: File,
) -> Result<u32> {
    ensure!(
        parts.len() <= 64 && nodes.len() < 8_000_000,
        "card tree too large or deep"
    );
    let meta = view.metadata(&parts, &file)?;
    if !meta.directory {
        return Err(not_found());
    }
    progress.folders.fetch_add(1, Ordering::Relaxed);
    let id = nodes.len() as u32;
    nodes.push(Node {
        name: parts.last().map_or("", String::as_str).into(),
        parent,
        children: vec![],
        directory: true,
        length: meta.size,
        layout: vec![],
        mtime: meta.mtime,
        cluster: 0,
        clusters: 0,
    });
    let mut files = Vec::new();
    for (name, is_dir) in view.entries(&file)? {
        let mut child = parts.clone();
        child.push(name);
        if is_dir == Some(true) {
            let result = view
                .open_child(&child, &file)
                .and_then(|f| walk(view, progress, nodes, jobs, child, id, f));
            match result {
                Ok(c) => nodes[id as usize].children.push(c),
                Err(e) if matches!(error_status(&e), 403 | 404) => (),
                Err(e) => return Err(e),
            }
        } else {
            ensure!(nodes.len() < 8_000_000, "card tree too large");
            let c = nodes.len() as u32;
            nodes.push(Node {
                name: child.last().unwrap().as_str().into(),
                parent: id,
                children: vec![],
                directory: false,
                length: 0,
                layout: vec![],
                mtime: 0,
                cluster: 0,
                clusters: 0,
            });
            nodes[id as usize].children.push(c);
            files.push(c);
            progress.found.fetch_add(1, Ordering::Relaxed);
        }
    }
    if !files.is_empty() {
        jobs.push((id, files));
    }
    Ok(id)
}

/// Phase 2: open and lay out files in parallel, one directory per task, opening
/// each file relative to its already-open directory.
fn scan(view: &View) -> Result<Vec<Node>> {
    let progress = Progress::default();
    std::thread::scope(|scope| {
        let reporter = scope.spawn(|| progress.report());
        let result = scan_files(view, &progress);
        progress.finished.store(true, Ordering::Relaxed);
        reporter.thread().unpark();
        result
    })
}

fn parts_of_nodes(nodes: &[Node], mut id: u32) -> Vec<String> {
    let mut parts = Vec::new();
    while id != 0 {
        let node = &nodes[id as usize];
        parts.push(node.name.to_string());
        id = node.parent;
    }
    parts.reverse();
    parts
}

fn scan_files(view: &View, progress: &Progress) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    let mut jobs = Vec::new();
    walk(
        view,
        progress,
        &mut nodes,
        &mut jobs,
        vec![],
        0,
        view.open(&[])?,
    )?;
    progress.total.store(
        progress.found.load(Ordering::Relaxed).max(1),
        Ordering::Relaxed,
    );
    let next = AtomicUsize::new(0);
    let results = std::thread::scope(|scope| -> Result<Vec<_>> {
        let workers: Vec<_> = (0..SCAN_THREADS)
            .map(|_| {
                scope.spawn(|| -> Result<Vec<(u32, Option<Meta>)>> {
                    let mut out = Vec::new();
                    loop {
                        let j = next.fetch_add(1, Ordering::Relaxed);
                        let Some((dir_id, files)) = jobs.get(j) else {
                            break;
                        };
                        let dir_parts = parts_of_nodes(&nodes, *dir_id);
                        let dir = view.open(&dir_parts)?;
                        for &fid in files {
                            let mut parts = dir_parts.clone();
                            parts.push(nodes[fid as usize].name.to_string());
                            let parts = &parts;
                            let meta = view.open_child(parts, &dir).and_then(|f| {
                                let m = view.metadata(parts, &f)?;
                                if m.directory { Err(not_found()) } else { Ok(m) }
                            });
                            progress.done.fetch_add(1, Ordering::Relaxed);
                            match meta {
                                Ok(m) => out.push((fid, Some(m))),
                                Err(e) if matches!(error_status(&e), 403 | 404) => {
                                    out.push((fid, None))
                                }
                                Err(e) => return Err(e.context(parts.join("/"))),
                            }
                        }
                    }
                    Ok(out)
                })
            })
            .collect();
        let mut all = Vec::new();
        for w in workers {
            all.extend(
                w.join()
                    .map_err(|_| anyhow::anyhow!("scan thread panicked"))??,
            );
        }
        Ok(all)
    })?;
    let mut removed = std::collections::HashSet::new();
    for (fid, meta) in results {
        match meta {
            Some(m) => {
                let node = &mut nodes[fid as usize];
                node.length = m.size;
                node.layout = m.layout;
                node.mtime = m.mtime;
            }
            None => {
                removed.insert(fid);
            }
        }
    }
    if !removed.is_empty() {
        for node in nodes.iter_mut().filter(|n| n.directory) {
            node.children.retain(|c| !removed.contains(c));
        }
    }
    Ok(nodes)
}

impl Image {
    pub fn new(view: &View) -> Result<Self> {
        let mut nodes = scan(view)?;
        for i in 0..nodes.len() {
            if nodes[i].directory {
                let mut length = if i == 0 { 96 } else { 0 }; // label, bitmap, upcase
                for &child in &nodes[i].children {
                    let n = disk_name(&nodes[child as usize].name).len();
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
            extents: Vec::new(),
            files: Vec::new(),
            tree: Vec::new(),
        };
        image.bytes(0, prefix)?;
        // Move each file's layout into the extent map instead of copying it.
        let mut layouts: Vec<Vec<Resolved>> = nodes
            .iter_mut()
            .map(|n| std::mem::take(&mut n.layout))
            .collect();
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
                    let c = &nodes[child as usize];
                    if exfat_time(c.mtime) != c.mtime {
                        eprintln!(
                            "disk: {} has a modification time ({}) outside exFAT's 1980..2107; shown as {}",
                            parts_of_nodes(&nodes, child).join("/"),
                            c.mtime,
                            exfat_time(c.mtime)
                        );
                    }
                    bytes.extend_from_slice(&entry(c)?);
                }
                // Unused directory space reads as zeros; no need to store it.
                image.bytes(offset(node.cluster), bytes)?;
            } else {
                let mut at = offset(node.cluster);
                let mut own = None; // one FileRef shared by all of this file's own ranges
                for piece in std::mem::take(&mut layouts[i]) {
                    let length = piece.length();
                    let data = match piece {
                        Resolved::Bytes(b) => Data::Bytes(b),
                        Resolved::Own { stamp, offset, .. } => Data::File {
                            file: *own.get_or_insert_with(|| {
                                image.files.push(FileRef::Own {
                                    node: i as u32,
                                    stamp,
                                });
                                image.files.len() as u32 - 1
                            }),
                            base: offset,
                        },
                        Resolved::Other {
                            source,
                            path,
                            stamp,
                            offset,
                            ..
                        } => {
                            image.files.push(FileRef::Other {
                                source,
                                path,
                                stamp,
                            });
                            Data::File {
                                file: image.files.len() as u32 - 1,
                                base: offset,
                            }
                        }
                    };
                    image.push(at, length, data)?;
                    at += length;
                }
            }
        }
        let mut bitmap = vec![0u8; bitmap_length as usize];
        for i in 0..allocated {
            bitmap[i / 8] |= 1 << (i % 8);
        }
        image.bytes(offset(bitmap_cluster), bitmap)?;
        image.bytes(offset(upcase_cluster), upcase)?;
        // A fresh, right-sized list (collecting in place would keep the node array).
        let mut tree = Vec::with_capacity(nodes.len());
        tree.extend(nodes.into_iter().map(|n| (n.name, n.parent)));
        image.tree = tree;
        image.extents.shrink_to_fit();
        image.files.shrink_to_fit();
        Ok(image)
    }

    fn push(&mut self, start: u64, length: u64, data: Data) -> Result<()> {
        if length == 0 {
            return Ok(());
        }
        ensure!(
            self.extents
                .last()
                .is_none_or(|e| e.start + e.length <= start),
            "disk extents out of order"
        );
        self.extents.push(Extent {
            start,
            length,
            data,
        });
        Ok(())
    }

    fn bytes(&mut self, offset: u64, bytes: Vec<u8>) -> Result<()> {
        let length = bytes.len() as u64;
        self.push(offset, length, Data::Bytes(bytes.into_boxed_slice()))
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
    /// The last opened file, by index into `Image::files`.
    cached: Option<(u32, File)>,
}

impl Read for ImageReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let limit = out.len().min(self.remaining as usize);
        if limit == 0 {
            return Ok(0);
        }
        let pos = self.position;
        let mut length = limit;
        let extents = &self.image.extents;
        let i = extents.partition_point(|e| e.start <= pos);
        if let Some(extent) = i
            .checked_sub(1)
            .map(|j| &extents[j])
            .filter(|e| pos < e.start + e.length)
        {
            let offset = pos - extent.start;
            length = length.min((extent.length - offset) as usize);
            match &extent.data {
                Data::Bytes(bytes) => {
                    out[..length].copy_from_slice(&bytes[offset as usize..offset as usize + length])
                }
                Data::File { file, base } => {
                    if self.cached.as_ref().map(|(f, _)| *f) != Some(*file) {
                        let opened = match &self.image.files[*file as usize] {
                            FileRef::Own { node, stamp } => self.view.open_piece(
                                None,
                                &parts_of(&self.image.tree, *node),
                                *stamp,
                            )?,
                            FileRef::Other {
                                source,
                                path,
                                stamp,
                            } => self
                                .view
                                .open_piece(Some(source), &split_path(path), *stamp)?,
                        };
                        self.cached = Some((*file, opened));
                    }
                    self.cached
                        .as_ref()
                        .unwrap()
                        .1
                        .read_exact_at(&mut out[..length], base + offset)?;
                }
            }
        } else {
            if let Some(next) = extents.get(i) {
                length = length.min((next.start - pos) as usize);
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
    fn decomposed_names_are_written_composed() {
        // "CASTAN" + COMBINING TILDE + "ON" (NFD, as APFS may store it).
        assert_eq!(
            disk_name("CASTAN\u{303}ON"),
            "CASTA\u{d1}ON".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(
            disk_name("plain.DNG"),
            "plain.DNG".encode_utf16().collect::<Vec<_>>()
        );
    }

    #[test]
    fn times_outside_exfat_are_clamped_not_fatal() {
        assert_eq!(
            DateTime::<Utc>::from_timestamp(EXFAT_FIRST, 0)
                .unwrap()
                .to_rfc3339(),
            "1980-01-01T00:00:00+00:00"
        );
        assert_eq!(
            DateTime::<Utc>::from_timestamp(EXFAT_LAST, 0)
                .unwrap()
                .to_rfc3339(),
            "2107-12-31T23:59:58+00:00"
        );
        assert!(timestamp(EXFAT_FIRST).is_ok() && timestamp(EXFAT_LAST).is_ok());
        // 1969-12-31, the placeholder some cameras' unset clocks leave behind.
        let mut node = Node {
            name: "a.jpg".into(),
            parent: 0,
            children: vec![],
            directory: false,
            length: 1,
            layout: vec![],
            mtime: -1,
            cluster: 3,
            clusters: 1,
        };
        let first = entry(&node).unwrap();
        node.mtime = EXFAT_FIRST;
        assert_eq!(first, entry(&node).unwrap(), "shown as 1980-01-01");
        node.mtime = i64::MAX / 2;
        assert!(entry(&node).is_ok());
    }

    #[test]
    fn directory_timestamp_has_valid_utc_and_correct_checksum() {
        let node = Node {
            name: "ALXY8703.DNG".into(),
            parent: 0,
            children: vec![],
            directory: false,
            length: 42484224,
            layout: vec![],
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
