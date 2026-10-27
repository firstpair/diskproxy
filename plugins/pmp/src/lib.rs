//! Photo Mechanic Plus incremental-ingest policy for diskproxy.
use anyhow::{Context, Result, ensure};
use chrono::{FixedOffset, LocalResult, TimeZone};
use chrono_tz::Tz;
use diskproxy_core::{Plugin, Source, error_status};
use rustix::fs::OFlags;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
mod profiles;
#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub source_offset: Option<String>,
    pub target_offset: Option<String>,
    pub target_zone: Option<String>,
    pub history: Option<PathBuf>,
    pub profiles_dir: Option<PathBuf>,
    pub hide_ingested: bool,
    #[serde(skip)]
    pub source: PathBuf,
    #[serde(skip)]
    pub check: bool,
    #[serde(skip)]
    pub register: bool,
}
pub struct Prepared {
    pub source: Arc<Source>,
    pub plugin: Arc<dyn Plugin>,
    pub info: serde_json::Value,
}
fn parse_offset(value: &str) -> Result<FixedOffset> {
    let b = value.as_bytes();
    ensure!(
        b.len() == 6
            && matches!(b[0], b'+' | b'-')
            && b[3] == b':'
            && [b[1], b[2], b[4], b[5]].iter().all(u8::is_ascii_digit),
        "offset must be signed HH:MM, for example -07:00"
    );
    let hours: i32 = value[1..3].parse()?;
    let minutes: i32 = value[4..6].parse()?;
    ensure!(hours <= 23 && minutes <= 59, "invalid UTC offset");
    let seconds = (hours * 3600 + minutes * 60) * if b[0] == b'-' { -1 } else { 1 };
    FixedOffset::east_opt(seconds).context("invalid UTC offset")
}

#[derive(Clone, Copy)]
enum Target {
    Fixed(FixedOffset),
    Zone(Tz),
}

fn corrected_time(epoch: i64, source: FixedOffset, target: Target) -> Result<i64> {
    let wall = source
        .timestamp_opt(epoch, 0)
        .single()
        .context("invalid source timestamp")?
        .naive_local();
    match target {
        Target::Fixed(offset) => offset
            .from_local_datetime(&wall)
            .single()
            .map(|d| d.timestamp()),
        Target::Zone(zone) => match zone.from_local_datetime(&wall) {
            LocalResult::Single(d) => Some(d.timestamp()),
            _ => None,
        },
    }
    .context("ambiguous or nonexistent target wall time; use a fixed target offset")
}

struct PmpPlugin {
    root: Arc<Source>,
    guid: String,
    source: FixedOffset,
    target: Target,
    history: Option<HashSet<(String, u64, i64)>>,
}

struct Meta {
    size: u64,
    mtime: i64,
}

fn ro_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC
}

impl PmpPlugin {
    fn identified(root: Arc<Source>, guid: String, _marker: Option<PathBuf>) -> Result<Self> {
        ensure!(valid_guid(&guid), "invalid card GUID");
        let view = Self {
            root,
            guid,
            source: FixedOffset::east_opt(0).unwrap(),
            target: Target::Fixed(FixedOffset::east_opt(0).unwrap()),
            history: None,
        };
        ensure!(
            view.open_raw(&["DCIM".into()])?.metadata()?.is_dir(),
            "source needs a DCIM directory"
        );
        Ok(view)
    }

    fn open_raw(&self, parts: &[String]) -> Result<File> {
        self.root.open_raw(parts)
    }
    fn load_history(&mut self, path: &Path) -> Result<usize> {
        let mut file = File::open(path).context("open ingest history snapshot read-only")?;
        let before = file.metadata()?;
        ensure!(
            before.len() <= 128 * 1024 * 1024,
            "history exceeds 128 MiB limit"
        );
        let mut text = String::new();
        (&mut file)
            .take(128 * 1024 * 1024 + 1)
            .read_to_string(&mut text)?;
        let after = file.metadata()?;
        ensure!(
            before.len() == after.len()
                && before.modified()? == after.modified()?
                && text.len() as u64 == before.len(),
            "history changed while taking snapshot; stop ingest and retry"
        );
        let mut records = HashSet::new();
        for (line, row) in text.lines().enumerate() {
            let fields: Vec<_> = row.split('\t').collect();
            ensure!(fields.len() == 3, "malformed history line {}", line + 1);
            let name = fields[0].trim_start_matches('/');
            ensure!(
                !name.is_empty()
                    && !name
                        .split('/')
                        .any(|p| p.is_empty() || p == "." || p == ".." || p.contains('\0')),
                "invalid path on history line {}",
                line + 1
            );
            records.insert((name.to_owned(), fields[1].parse()?, fields[2].parse()?));
        }
        let count = records.len();
        self.history = Some(records);
        Ok(count)
    }

    fn hidden(&self, parts: &[String], file: &File) -> Result<bool> {
        let Some(history) = &self.history else {
            return Ok(false);
        };
        if !image_path(parts) || !file.metadata()?.is_file() {
            return Ok(false);
        }
        let meta = self.metadata(parts, file)?;
        Ok(history.contains(&(parts.join("/"), meta.size, meta.mtime)))
    }

    fn image_counts(&self) -> Result<(usize, usize)> {
        let mut pending = vec![vec!["DCIM".to_owned()]];
        let (mut visible, mut hidden) = (0, 0);
        while let Some(parts) = pending.pop() {
            let dir = self.open_raw(&parts)?;
            for name in self.names(&dir)? {
                let mut child = parts.clone();
                child.push(name);
                let file = match self.open_raw(&child) {
                    Ok(file) => file,
                    Err(e) if matches!(error_status(&e), 403 | 404) => continue,
                    Err(e) => return Err(e),
                };
                if file.metadata()?.is_dir() {
                    pending.push(child);
                } else if image_path(&child) {
                    if self.hidden(&child, &file)? {
                        hidden += 1;
                    } else {
                        visible += 1;
                    }
                }
            }
        }
        Ok((visible, hidden))
    }

    fn metadata(&self, parts: &[String], file: &File) -> Result<Meta> {
        let s = file.metadata()?;
        let mut mtime = s.mtime();
        if image_path(parts) {
            mtime = corrected_time(mtime, self.source, self.target)?;
        }
        Ok(Meta {
            size: s.len(),
            mtime,
        })
    }

    fn names(&self, file: &File) -> Result<Vec<String>> {
        Ok(self
            .root
            .names(file)?
            .into_iter()
            .filter(|n| !n.starts_with("._") && n != ".PM_lock")
            .collect())
    }
}

fn image_path(parts: &[String]) -> bool {
    parts.len() >= 2
        && parts[0] == "DCIM"
        && !parts.iter().any(|part| part.starts_with("._"))
        && Path::new(parts.last().unwrap())
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("dng") || e.eq_ignore_ascii_case("jpg"))
}

fn valid_guid(s: &str) -> bool {
    s.len() == 38
        && s.bytes().enumerate().all(|(i, b)| match i {
            0 => b == b'{',
            37 => b == b'}',
            9 | 14 | 19 | 24 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

impl Plugin for PmpPlugin {
    fn identity(&self) -> Option<String> {
        Some(self.guid.clone())
    }
    fn visible(&self, parts: &[String], file: &std::fs::Metadata) -> Result<bool> {
        if parts.is_empty() {
            return Ok(true);
        }
        if parts[0] != "DCIM" && parts != [".pmingest.dat"] {
            return Ok(false);
        }
        if parts.iter().any(|p| p.starts_with("._") || p == ".PM_lock") {
            return Ok(false);
        }
        if image_path(parts)
            && let Some(history) = &self.history
        {
            let stamp = corrected_time(file.mtime(), self.source, self.target)?;
            return Ok(!history.contains(&(parts.join("/"), file.len(), stamp)));
        }
        Ok(true)
    }
    fn mtime(&self, parts: &[String], file: &std::fs::Metadata) -> Result<i64> {
        if image_path(parts) {
            corrected_time(file.mtime(), self.source, self.target)
        } else {
            Ok(file.mtime())
        }
    }
}

pub fn prepare(args: Config) -> Result<Prepared> {
    let source = Arc::new(Source::new(&args.source)?);
    let store = profiles::Store::new(&args.source, args.profiles_dir.clone());
    let identity = store.identity(&args.source)?;
    let saved = store.load(&identity.guid)?;
    let mut view = PmpPlugin::identified(
        Arc::clone(&source),
        identity.guid.clone(),
        identity.marker.clone(),
    )?;
    let source = if let Some(source) = &args.source_offset {
        let explicit = parse_offset(source)?;
        if let Some(profile) = &saved {
            match profile.source(&view) {
                Ok(detected) => ensure!(
                    detected == explicit,
                    "--source-offset disagrees with unchanged card samples (detected {detected})"
                ),
                Err(e) => ensure!(
                    args.register,
                    "{e}; use --register with a known --source-offset to refresh samples"
                ),
            }
        }
        explicit
    } else {
        saved.as_ref().context("first registration needs --source-offset for the current card mount; it will be automatic afterward")?.source(&view)?
    };
    view.source = source;
    let default_history = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is unset; specify --history")?
        .join("Library/Caches/Photo Mechanic/ingest/incremental")
        .join(&view.guid);
    let candidate_history = args.history.clone().unwrap_or(default_history);
    let requested_target = if let Some(zone) = &args.target_zone {
        Some(Target::Zone(zone.parse()?))
    } else {
        args.target_offset
            .as_ref()
            .map(|s| parse_offset(s).map(Target::Fixed))
            .transpose()?
    };
    let target = if let Some(profile) = &saved {
        let existing = profile.target()?;
        if let Some(requested) = requested_target {
            ensure!(
                target_name(requested) == target_name(existing),
                "requested target differs from card profile {}; keep the existing convention",
                target_name(existing)
            );
        }
        existing
    } else if let Some(requested) = requested_target {
        requested
    } else if candidate_history.exists() {
        view.load_history(&candidate_history)?;
        Target::Fixed(profiles::infer_target(&view)?.unwrap_or(FixedOffset::east_opt(0).unwrap()))
    } else {
        Target::Fixed(FixedOffset::east_opt(0).unwrap())
    };
    view.target = target;
    let target_label = target_name(target);
    let filtering = args.hide_ingested || args.history.is_some();
    let history_path = if filtering {
        Some(candidate_history)
    } else {
        None
    };
    let history_records = if let Some(path) = &history_path {
        if path.exists() {
            Some(view.load_history(path)?)
        } else if saved.is_none() || identity.marker.is_some() {
            view.history = Some(HashSet::new());
            Some(0)
        } else {
            anyhow::bail!("ingest history is missing; cannot filter an established card")
        }
    } else {
        view.history = None;
        None
    };
    let (visible_images, hidden_images) = view.image_counts()?;
    let profile_path = store.path.join(".pmp-ingest-proxy.json");
    if args.register {
        let _lock = store.lock()?;
        store.save(&identity, &profiles::Profile::capture(&view)?)?;
    }
    if saved.is_none() && !args.check && !args.register {
        let _lock = store.lock()?;
        ensure!(
            store.load(&identity.guid)?.is_none(),
            "profile was registered concurrently; restart to load it"
        );
        store.save(&identity, &profiles::Profile::capture(&view)?)?;
    }
    let info = serde_json::json!({"plugin": "pmp", "guid": view.guid, "source": args.source,
        "visible_images": visible_images, "hidden_images": hidden_images,
        "history_records": history_records, "history_snapshot": history_path,
        "source_offset": source.to_string(), "source_basis": if args.source_offset.is_some() { "explicit" } else { "verified samples" },
        "target": target_label, "profile": profile_path, "profile_saved": args.register});
    Ok(Prepared {
        source: Arc::clone(&view.root),
        plugin: Arc::new(view),
        info,
    })
}
fn target_name(target: Target) -> String {
    match target {
        Target::Fixed(o) => o.to_string(),
        Target::Zone(z) => z.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timezone_reinterpretation_matches_legacy_history() {
        let target = Target::Fixed(parse_offset("-07:00").unwrap());
        for (source, epoch) in [
            ("+08:00", 1789359132),
            ("+09:00", 1789355532),
            ("-07:00", 1789413132),
        ] {
            assert_eq!(
                corrected_time(epoch, parse_offset(source).unwrap(), target).unwrap(),
                1789413132
            );
        }
        for invalid in ["8:00", "+24:00", "+08:60", "-ab:00"] {
            assert!(parse_offset(invalid).is_err());
        }
    }
    #[test]
    fn ambiguous_daylight_time_is_rejected() {
        let source = parse_offset("+08:00").unwrap();
        let target = Target::Zone(chrono_tz::America::Los_Angeles);
        for (month, day, hour) in [(3, 8, 2), (11, 1, 1)] {
            let epoch = source
                .with_ymd_and_hms(2026, month, day, hour, 30, 0)
                .unwrap()
                .timestamp();
            assert!(corrected_time(epoch, source, target).is_err());
        }
    }
}
