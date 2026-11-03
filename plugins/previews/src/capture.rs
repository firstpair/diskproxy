//! Capture ratings, color labels and flags that an application writes into a
//! served file, as small XMP sidecars in a separate folder.
//!
//! Only these properties are kept; everything else the application writes
//! (dates, copied camera metadata, tool names) is ignored:
//!
//! | Property | Namespace |
//! | --- | --- |
//! | `xmp:Rating`, `xmp:Label` | `http://ns.adobe.com/xap/1.0/` |
//! | `photomechanic:ColorClass`, `photomechanic:Tagged` | `http://ns.camerabits.com/photomechanic/1.0/` |
//!
//! A written `X.DNG` (or a `X.XMP` sidecar beside it) becomes `X.xmp`; a written
//! `X.JPG` becomes `X.JPG.xmp`, so a DNG and a JPEG with the same name never
//! collide. A stand-in served as `X.JPG.dng` or `X.RW2.dng` is recorded under its
//! original's name. Each change is appended to `JOURNAL.jsonl` in the folder.
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const XMP_NS: &str = "http://ns.adobe.com/xap/1.0/";
const PM_NS: &str = "http://ns.camerabits.com/photomechanic/1.0/";
const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
/// (namespace, local name, prefix used in the sidecar)
const FIELDS: [(&str, &str, &str); 4] = [
    (XMP_NS, "Rating", "xmp"),
    (XMP_NS, "Label", "xmp"),
    (PM_NS, "ColorClass", "photomechanic"),
    (PM_NS, "Tagged", "photomechanic"),
];
const MAX_PACKET: usize = 4 << 20;
const IMAGE_EXTENSIONS: [&str; 22] = [
    "jpg", "jpeg", "dng", "tif", "tiff", "heic", "png", "psd", "cr2", "cr3", "nef", "arw", "raf",
    "rw2", "orf", "3fr", "pef", "srw", "iiq", "rwl", "gpr", "avif",
];

pub type Fields = BTreeMap<String, String>;

pub struct Capture {
    dir: PathBuf,
    lock: Mutex<()>,
}

fn extension(name: &str) -> Option<&str> {
    name.rsplit_once('.')
        .map(|(_, e)| e)
        .filter(|e| !e.is_empty())
}
fn is_image(ext: &str) -> bool {
    IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

/// The sidecar path (relative to the ratings folder) for a written file, or
/// `None` for files that carry no image metadata (temporaries, `._` files).
pub fn sidecar_path(parts: &[String]) -> Option<Vec<String>> {
    let (name, dirs) = parts.split_last()?;
    if name.starts_with('.') {
        return None;
    }
    let ext = extension(name)?;
    // The name of the original the written file stands for.
    let original: &str = if ext.eq_ignore_ascii_case("xmp") {
        &name[..name.len() - 4]
    } else if !is_image(ext) {
        return None;
    } else if ext == "dng" && extension(&name[..name.len() - 4]).is_some_and(is_image) {
        &name[..name.len() - 4] // stand-in X.JPG.dng / X.RW2.dng
    } else {
        name
    };
    let file = match extension(original) {
        Some(e) if e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg") => {
            format!("{original}.xmp")
        }
        Some(e) if is_image(e) => format!("{}.xmp", &original[..original.len() - e.len() - 1]),
        _ => format!("{original}.xmp"),
    };
    let mut out = dirs.to_vec();
    out.push(file);
    Some(out)
}

/// The XMP packet of an `.xmp` sidecar, a JPEG (APP1) or a TIFF/DNG (tag 700).
pub fn packet(name: &str, file: &File) -> Result<Option<Vec<u8>>> {
    let size = file.metadata()?.len();
    if extension(name).is_some_and(|e| e.eq_ignore_ascii_case("xmp")) {
        ensure!(size as usize <= MAX_PACKET, "sidecar too large");
        let mut data = Vec::new();
        (&*file).read_to_end(&mut data)?;
        return Ok(Some(data));
    }
    let mut head = [0u8; 8];
    if size < 8 {
        return Ok(None);
    }
    file.read_exact_at(&mut head, 0)?;
    if head[..2] == [0xff, 0xd8] {
        let mut pos = 2u64;
        while pos + 4 <= size {
            let mut m = [0u8; 4];
            file.read_exact_at(&mut m, pos)?;
            if m[0] != 0xff || m[1] == 0xda {
                break;
            }
            let len = u16::from_be_bytes([m[2], m[3]]) as u64;
            const SIG: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
            if m[1] == 0xe1 && len as usize >= 2 + SIG.len() {
                let mut seg = vec![0u8; len as usize - 2];
                file.read_exact_at(&mut seg, pos + 4)?;
                if seg.starts_with(SIG) {
                    return Ok(Some(seg[SIG.len()..].to_vec()));
                }
            }
            pos += 2 + len;
        }
        return Ok(None);
    }
    let little = match &head[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Ok(None),
    };
    let u16_ = |b: &[u8]| {
        if little {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    };
    let u32_ = |b: &[u8]| {
        let a = [b[0], b[1], b[2], b[3]];
        if little {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    };
    let ifd0 = u32_(&head[4..]) as u64;
    let mut n = [0u8; 2];
    file.read_exact_at(&mut n, ifd0)?;
    let n = u16_(&n) as usize;
    let mut entries = vec![0u8; 12 * n];
    file.read_exact_at(&mut entries, ifd0 + 2)?;
    for e in entries.chunks(12) {
        if u16_(e) == 700 {
            let count = u32_(&e[4..]) as usize;
            ensure!(count <= MAX_PACKET, "XMP packet too large");
            let mut data = vec![0u8; count];
            if count <= 4 {
                data.copy_from_slice(&e[8..8 + count]);
            } else {
                file.read_exact_at(&mut data, u32_(&e[8..]) as u64)?;
            }
            return Ok(Some(data));
        }
    }
    Ok(None)
}

/// The captured properties present in an XMP packet, keyed `prefix:Name`.
pub fn fields(packet: &[u8]) -> Result<Fields> {
    let end = packet
        .iter()
        .rposition(|b| *b != 0 && !b.is_ascii_whitespace())
        .map_or(0, |i| i + 1);
    let text = std::str::from_utf8(&packet[..end]).context("XMP is not UTF-8")?;
    let text = text.trim_start_matches('\u{feff}');
    let doc = roxmltree::Document::parse(text).context("parse XMP")?;
    let mut out = Fields::new();
    for d in doc
        .descendants()
        .filter(|n| n.has_tag_name((RDF_NS, "Description")))
    {
        for (ns, name, prefix) in FIELDS {
            let value = d.attribute((ns, name)).map(str::to_owned).or_else(|| {
                d.children()
                    .find(|c| c.has_tag_name((ns, name)))
                    .and_then(|c| c.text())
                    .map(|t| t.trim().to_owned())
            });
            if let Some(v) = value {
                out.insert(format!("{prefix}:{name}"), v);
            }
        }
    }
    Ok(out)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A minimal sidecar holding only the captured properties.
pub fn sidecar(fields: &Fields) -> String {
    let attrs: String = fields
        .iter()
        .map(|(k, v)| format!("\n    {k}=\"{}\"", escape(v)))
        .collect();
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n \
         <rdf:RDF xmlns:rdf=\"{RDF_NS}\">\n  \
         <rdf:Description rdf:about=\"\"\n    \
         xmlns:xmp=\"{XMP_NS}\"\n    \
         xmlns:photomechanic=\"{PM_NS}\"{attrs}/>\n \
         </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>\n"
    )
}

impl Capture {
    pub fn new(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir)
            .with_context(|| format!("create ratings folder {}", dir.display()))?;
        Ok(Self {
            dir,
            lock: Mutex::new(()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Record the properties of a written file. Returns the sidecar's relative
    /// path when it changed, `None` when there was nothing new to record.
    pub fn record(&self, parts: &[String], file: &File) -> Result<Option<String>> {
        let Some(rel) = sidecar_path(parts) else {
            return Ok(None);
        };
        let Some(packet) = packet(parts.last().unwrap(), file)? else {
            return Ok(None);
        };
        let new = fields(&packet)?;
        let _guard = self.lock.lock().unwrap();
        let path = rel.iter().fold(self.dir.clone(), |p, c| p.join(c));
        let old = match fs::read(&path) {
            Ok(data) => fields(&data)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Fields::new(),
            Err(e) => return Err(e.into()),
        };
        if new == old || (new.is_empty() && !path.exists()) {
            return Ok(None);
        }
        let parent = path.parent().context("sidecar has no folder")?;
        fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".{}.tmp-{}",
            rel.last().unwrap(),
            std::process::id()
        ));
        fs::write(&tmp, sidecar(&new))?;
        fs::rename(&tmp, &path)?;
        let line = serde_json::json!({
            "time_unix": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64(),
            "sidecar": rel.join("/"),
            "written": parts.join("/"),
            "fields": new,
            "previous": old,
        });
        let mut journal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("JOURNAL.jsonl"))?;
        writeln!(journal, "{line}")?;
        Ok(Some(rel.join("/")))
    }
}

/// Reject a ratings folder inside the served tree: captures must never write
/// into the source.
pub fn check_outside(ratings: &Path, source: &Path) -> Result<()> {
    let (r, s) = (
        ratings
            .canonicalize()
            .unwrap_or_else(|_| ratings.to_path_buf()),
        source
            .canonicalize()
            .unwrap_or_else(|_| source.to_path_buf()),
    );
    if r.starts_with(&s) {
        bail!(
            "ratings folder {} is inside the source {}",
            r.display(),
            s.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(path: &str) -> Vec<String> {
        path.split('/').map(str::to_owned).collect()
    }

    #[test]
    fn sidecar_names_follow_the_original() {
        let cases = [
            ("a/X.DNG", Some("a/X.xmp")),
            ("a/X.dng", Some("a/X.xmp")),
            ("a/X.XMP", Some("a/X.xmp")),
            ("a/X.JPG", Some("a/X.JPG.xmp")),
            ("a/X.JPG.dng", Some("a/X.JPG.xmp")),
            ("a/X.JPG.XMP", Some("a/X.JPG.xmp")),
            ("a/X.RW2.dng", Some("a/X.xmp")),
            (
                "a/EDGAR CASTAN\u{303}ON (1).jpg.dng",
                Some("a/EDGAR CASTAN\u{303}ON (1).jpg.xmp"),
            ),
            ("a/._X.DNG", None),
            ("a/.X.JPG.tmp", None),
            ("a/notes.txt", None),
        ];
        for (input, want) in cases {
            assert_eq!(
                sidecar_path(&p(input)).map(|v| v.join("/")),
                want.map(str::to_owned),
                "{input}"
            );
        }
    }

    const PMP_ATTRS: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
      <rdf:Description rdf:about="Leica Camera AG" xmlns:xmp="http://ns.adobe.com/xap/1.0/"
        xmlns:photomechanic="http://ns.camerabits.com/photomechanic/1.0/" xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"
        xmp:Label="Green" xmp:Rating="4" photoshop:DateCreated="2026-11-03T00:50:37-08:00"
        photomechanic:ColorClass="4" photomechanic:Tagged="False"/></rdf:RDF></x:xmpmeta>"#;

    #[test]
    fn only_captured_properties_are_kept_in_both_xmp_forms() -> Result<()> {
        let f = fields(PMP_ATTRS.as_bytes())?;
        assert_eq!(
            f.into_iter().collect::<Vec<_>>(),
            [
                ("photomechanic:ColorClass", "4"),
                ("photomechanic:Tagged", "False"),
                ("xmp:Label", "Green"),
                ("xmp:Rating", "4")
            ]
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
        );
        let elements = r#"<?xpacket begin="﻿"?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
          <rdf:Description xmlns:a="http://ns.adobe.com/xap/1.0/"><a:Rating> 2 </a:Rating><a:Label>Red &amp; more</a:Label></rdf:Description>
          </rdf:RDF></x:xmpmeta><?xpacket end="w"?>          "#;
        let f = fields(elements.as_bytes())?;
        assert_eq!(f["xmp:Rating"], "2");
        assert_eq!(f["xmp:Label"], "Red & more");
        // A sidecar we write reads back as the same fields.
        assert_eq!(fields(sidecar(&f).as_bytes())?, f);
        Ok(())
    }

    #[test]
    fn record_writes_sidecar_and_journal_only_on_change() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let capture = Capture::new(dir.path().join("ratings"))?;
        let written = dir.path().join("X.XMP");
        fs::write(&written, PMP_ATTRS)?;
        let parts = p("2024/October/1657_L1/X.XMP");
        let rel = capture.record(&parts, &File::open(&written)?)?;
        assert_eq!(rel.as_deref(), Some("2024/October/1657_L1/X.xmp"));
        let side = fs::read(dir.path().join("ratings/2024/October/1657_L1/X.xmp"))?;
        assert_eq!(fields(&side)?["xmp:Rating"], "4");
        assert!(!String::from_utf8(side)?.contains("DateCreated"));
        // Same values again: nothing written.
        assert!(capture.record(&parts, &File::open(&written)?)?.is_none());
        fs::write(
            &written,
            PMP_ATTRS.replace("xmp:Rating=\"4\"", "xmp:Rating=\"5\""),
        )?;
        assert!(capture.record(&parts, &File::open(&written)?)?.is_some());
        let journal = fs::read_to_string(dir.path().join("ratings/JOURNAL.jsonl"))?;
        let lines: Vec<serde_json::Value> = journal
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["previous"]["xmp:Rating"], "4");
        assert_eq!(lines[1]["fields"]["xmp:Rating"], "5");
        Ok(())
    }

    #[test]
    fn packets_are_found_in_jpeg_and_tiff() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let xmp = PMP_ATTRS.as_bytes();
        // JPEG: SOI, JFIF APP0, XMP APP1, SOS.
        let mut j = vec![0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0];
        let sig = b"http://ns.adobe.com/xap/1.0/\0";
        j.extend([0xff, 0xe1]);
        j.extend(((2 + sig.len() + xmp.len()) as u16).to_be_bytes());
        j.extend(sig);
        j.extend(xmp);
        j.extend([0xff, 0xda, 0, 2]);
        fs::write(dir.path().join("a.jpg"), &j)?;
        let got = packet("a.jpg", &File::open(dir.path().join("a.jpg"))?)?.unwrap();
        assert_eq!(got, xmp);
        // Little-endian TIFF with tag 700 pointing at the packet.
        let mut t = b"II\x2a\0\x08\0\0\0".to_vec();
        t.extend(1u16.to_le_bytes());
        t.extend(700u16.to_le_bytes());
        t.extend(1u16.to_le_bytes());
        t.extend((xmp.len() as u32).to_le_bytes());
        t.extend(26u32.to_le_bytes());
        t.extend([0; 4]);
        t.extend(xmp);
        fs::write(dir.path().join("a.dng"), &t)?;
        let got = packet("a.dng", &File::open(dir.path().join("a.dng"))?)?.unwrap();
        assert_eq!(got, xmp);
        Ok(())
    }
}
