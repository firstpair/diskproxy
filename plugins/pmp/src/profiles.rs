//! Per-card conventions and filesystem-clock anchors. EXIF is never consulted.
use super::{PmpPlugin, Target, parse_offset, ro_flags, valid_guid};
use anyhow::{Context, Result, ensure};
use chrono::FixedOffset;
use rustix::fs::{FlockOperation, Mode, OFlags, flock, open, openat};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct Sample {
    parts: Vec<String>,
    size: u64,
    sha256: String,
    wall_epoch: i64,
}

#[derive(Serialize, Deserialize)]
pub struct Profile {
    version: u32,
    pub guid: String,
    pub target_offset: Option<String>,
    pub target_zone: Option<String>,
    samples: Vec<Sample>,
}

pub struct Identity {
    pub guid: String,
    pub marker: Option<PathBuf>,
}

pub struct Store {
    pub path: PathBuf,
}

fn fingerprint(file: &mut File) -> Result<String> {
    let before = file.metadata()?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    let after = file.metadata()?;
    ensure!(
        before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec(),
        "sample changed while verifying card"
    );
    Ok(format!("{:x}", hash.finalize()))
}

fn plausible(seconds: i64) -> bool {
    (-12 * 3600..=14 * 3600).contains(&seconds) && seconds % 900 == 0
}

fn random_guid() -> Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("{e}"))?;
    b[6] = (b[6] & 15) | 0x40;
    b[8] = (b[8] & 63) | 0x80;
    let hex: String = b.iter().map(|b| format!("{b:02X}")).collect();
    Ok(format!(
        "{{{}-{}-{}-{}-{}}}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

impl Store {
    pub fn new(source: &Path, path: Option<PathBuf>) -> Self {
        Self {
            path: path.unwrap_or_else(|| source.to_owned()),
        }
    }

    pub fn lock(&self) -> Result<File> {
        fs::create_dir_all(&self.path)?;
        let file = OpenOptions::new().write(true).create(true).truncate(false).mode(0o600)
            .open(self.path.join(".pmp-ingest-proxy.lock"))
            .context("card profile needs a writable card; finish ingestion, detach proxy views, then remount the card writable")?;
        flock(&file, FlockOperation::NonBlockingLockExclusive)
            .context("another proxy is registering a card; retry")?;
        Ok(file)
    }

    pub fn identity(&self, source: &Path) -> Result<Identity> {
        let root = open(source, ro_flags() | OFlags::DIRECTORY, Mode::empty())?;
        match openat(&root, ".pmingest.dat", ro_flags(), Mode::empty()) {
            Ok(fd) => {
                let mut guid = String::new();
                File::from(fd).take(256).read_to_string(&mut guid)?;
                ensure!(valid_guid(&guid), "invalid .pmingest.dat GUID");
                return Ok(Identity { guid, marker: None });
            }
            Err(rustix::io::Errno::NOENT) => (),
            Err(e) => return Err(e.into()),
        }
        ensure!(
            !self.path.join(".pmp-ingest-proxy.json").exists(),
            "card marker missing but profile exists; restore its original .pmingest.dat rather than creating a new identity"
        );
        Ok(Identity {
            guid: random_guid()?,
            marker: Some(source.join(".pmingest.dat")),
        })
    }

    pub fn load(&self, guid: &str) -> Result<Option<Profile>> {
        ensure!(valid_guid(guid), "invalid profile GUID");
        match fs::read(self.path.join(".pmp-ingest-proxy.json")) {
            Ok(bytes) => {
                let p: Profile = serde_json::from_slice(&bytes)?;
                ensure!(
                    p.version == 1 && p.guid == guid,
                    "unsupported or mismatched card profile"
                );
                Ok(Some(p))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, identity: &Identity, profile: &Profile) -> Result<()> {
        ensure!(profile.guid == identity.guid, "profile identity mismatch");
        if let Some(marker) = &identity.marker {
            // A new marker is created only once; never replace a marker another process added.
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(marker)?;
            f.write_all(identity.guid.as_bytes())?;
            f.sync_all()?;
        }
        self.atomic(
            &self.path.join(".pmp-ingest-proxy.json"),
            &serde_json::to_vec_pretty(profile)?,
        )?;
        Ok(())
    }

    fn atomic(&self, target: &Path, bytes: &[u8]) -> Result<()> {
        fs::create_dir_all(&self.path)?;
        let temporary = self.path.join(format!(".{}.tmp", random_guid()?));
        let result = (|| -> Result<()> {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            fs::rename(&temporary, target)?;
            Ok(())
        })();
        if temporary.exists() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

impl Profile {
    pub fn target(&self) -> Result<Target> {
        match (&self.target_offset, &self.target_zone) {
            (Some(s), None) => Ok(Target::Fixed(parse_offset(s)?)),
            (None, Some(s)) => Ok(Target::Zone(s.parse()?)),
            _ => anyhow::bail!("invalid profile target"),
        }
    }

    pub fn source(&self, view: &PmpPlugin) -> Result<FixedOffset> {
        let mut offsets = HashSet::new();
        for sample in &self.samples {
            let Ok(mut file) = view.open_raw(&sample.parts) else {
                continue;
            };
            let meta = file.metadata()?;
            if meta.len() != sample.size || fingerprint(&mut file)? != sample.sha256 {
                continue;
            }
            let delta = sample.wall_epoch - meta.mtime();
            ensure!(
                plausible(delta),
                "saved sample has an invalid mount offset; use --source-offset after remounting"
            );
            offsets.insert(delta);
        }
        ensure!(
            offsets.len() == 1,
            "cannot verify one mount offset from unchanged card samples; remount under the current zone and specify --source-offset"
        );
        FixedOffset::east_opt(*offsets.iter().next().unwrap() as i32)
            .context("invalid source offset")
    }

    pub fn capture(view: &PmpPlugin) -> Result<Self> {
        let mut pending = vec![vec!["DCIM".to_owned()]];
        let mut paths = Vec::new();
        while let Some(parts) = pending.pop() {
            for name in view.names(&view.open_raw(&parts)?)? {
                let mut child = parts.clone();
                child.push(name);
                let Ok(file) = view.open_raw(&child) else {
                    continue;
                };
                if file.metadata()?.is_dir() {
                    pending.push(child);
                } else if super::image_path(&child) {
                    paths.push(child);
                }
            }
        }
        paths.sort();
        let mut samples = Vec::new();
        let selected: HashSet<_> = if paths.is_empty() {
            HashSet::new()
        } else {
            [0, paths.len() / 2, paths.len() - 1].into_iter().collect()
        };
        for index in selected {
            let parts = paths[index].clone();
            let mut file = view.open_raw(&parts)?;
            let meta = file.metadata()?;
            samples.push(Sample {
                parts,
                size: meta.len(),
                sha256: fingerprint(&mut file)?,
                wall_epoch: meta.mtime() + view.source.local_minus_utc() as i64,
            });
        }
        Ok(Self {
            version: 1,
            guid: view.guid.clone(),
            target_offset: match view.target {
                Target::Fixed(o) => Some(o.to_string()),
                _ => None,
            },
            target_zone: match view.target {
                Target::Zone(z) => Some(z.to_string()),
                _ => None,
            },
            samples,
        })
    }
}

pub fn infer_target(view: &PmpPlugin) -> Result<Option<FixedOffset>> {
    let Some(history) = &view.history else {
        return Ok(None);
    };
    if history.is_empty() {
        return Ok(None);
    }
    let mut votes = BTreeMap::<i64, usize>::new();
    for (name, size, stamp) in history {
        let parts = name.split('/').map(str::to_owned).collect::<Vec<_>>();
        let Ok(file) = view.open_raw(&parts) else {
            continue;
        };
        let s = file.metadata()?;
        if s.len() != *size {
            continue;
        }
        let delta = s.mtime() + view.source.local_minus_utc() as i64 - stamp;
        if plausible(delta) {
            *votes.entry(delta).or_default() += 1;
        }
    }
    let (&offset, &count) = votes
        .iter()
        .max_by_key(|(_, n)| *n)
        .context("no usable history matches; specify --target-offset")?;
    ensure!(
        count * 100 >= votes.values().sum::<usize>() * 95,
        "history contains mixed time conventions; specify --target-offset rather than guessing"
    );
    Ok(FixedOffset::east_opt(offset as i32))
}
