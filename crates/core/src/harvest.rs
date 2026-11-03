//! Harvest client writes from a writable mount of a served disk (for example
//! one attached with a shadow file) by passing changed files to the plugin's
//! `written` hook. Changes are found through filesystem events, so the volume is
//! never walked in full unless asked.
use crate::Plugin;
use anyhow::{Context, Result};
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::BTreeSet;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub struct Options {
    /// How often batched changes are passed to the plugin.
    pub interval: Duration,
    /// When this file appears, harvest what is pending and return.
    pub stop_file: Option<PathBuf>,
    /// Walk the volume first and harvest files modified at or after this Unix time.
    pub since: Option<i64>,
    /// Harvest once (the walk above, if any) and return without watching.
    pub once: bool,
}

/// Components of `path` relative to `volume`, or `None` if outside or hidden.
fn parts(volume: &Path, path: &Path) -> Option<Vec<String>> {
    let rel = path.strip_prefix(volume).ok()?;
    let parts: Option<Vec<String>> = rel
        .components()
        .map(|c| c.as_os_str().to_str().map(str::to_owned))
        .collect();
    let parts = parts?;
    // Skip the volume's own housekeeping (.fseventsd, .Spotlight-V100, .Trashes).
    (!parts.is_empty() && !parts[0].starts_with('.')).then_some(parts)
}

fn harvest_one(volume: &Path, plugin: &dyn Plugin, path: &Path) -> Result<bool> {
    let Some(parts) = parts(volume, path) else {
        return Ok(false);
    };
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc_nofollow())
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| path.display().to_string()),
    };
    if !file.metadata()?.is_file() {
        return Ok(false);
    }
    plugin.written(&parts, &file)?;
    Ok(true)
}

fn libc_nofollow() -> i32 {
    rustix::fs::OFlags::NOFOLLOW.bits() as i32
}

fn walk(volume: &Path, dir: &Path, since: i64, out: &mut BTreeSet<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if parts(volume, &path).is_none() {
            continue;
        }
        if kind.is_dir() {
            walk(volume, &path, since, out)?;
        } else if kind.is_file() && entry.metadata()?.mtime() >= since {
            out.insert(path);
        }
    }
    Ok(())
}

fn process(volume: &Path, plugin: &dyn Plugin, pending: &mut BTreeSet<PathBuf>) -> usize {
    let mut n = 0;
    for path in std::mem::take(pending) {
        match harvest_one(volume, plugin, &path) {
            Ok(true) => n += 1,
            Ok(false) => (),
            Err(e) => eprintln!("harvest: {}: {e:#}", path.display()),
        }
    }
    n
}

pub fn harvest(volume: &Path, plugin: &dyn Plugin, options: Options) -> Result<()> {
    let volume = volume
        .canonicalize()
        .with_context(|| format!("volume {}", volume.display()))?;
    let mut pending = BTreeSet::new();
    let (tx, rx) = mpsc::channel();
    let mut watcher = if options.once {
        None
    } else {
        // Watch before walking so nothing written during the walk is missed.
        let mut w = notify::recommended_watcher(tx)?;
        w.watch(&volume, RecursiveMode::Recursive)?;
        Some(w)
    };
    if let Some(since) = options.since {
        walk(&volume, &volume, since, &mut pending)?;
        let n = process(&volume, plugin, &mut pending);
        eprintln!("harvest: walked volume, passed {n} files changed since {since}");
    }
    if watcher.is_none() {
        return Ok(());
    }
    eprintln!("harvest: watching {}", volume.display());
    let mut last = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(event)) => {
                if matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any
                ) {
                    pending.extend(event.paths);
                }
            }
            Ok(Err(e)) => eprintln!("harvest: watch error: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let stopping = options.stop_file.as_ref().is_some_and(|f| f.exists());
        if stopping || last.elapsed() >= options.interval {
            if !pending.is_empty() {
                let n = process(&volume, plugin, &mut pending);
                eprintln!("harvest: passed {n} changed files");
            }
            last = Instant::now();
        }
        if stopping {
            break;
        }
    }
    drop(watcher.take());
    eprintln!("harvest: stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Record(Mutex<Vec<String>>);
    impl Plugin for Record {
        fn written(&self, parts: &[String], file: &File) -> Result<()> {
            let len = file.metadata()?.len();
            self.0
                .lock()
                .unwrap()
                .push(format!("{}:{len}", parts.join("/")));
            Ok(())
        }
    }

    #[test]
    fn walk_passes_recent_files_and_skips_hidden_ones() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("a/b"))?;
        std::fs::create_dir_all(dir.path().join(".fseventsd"))?;
        std::fs::write(dir.path().join("a/b/X.DNG"), b"1234")?;
        std::fs::write(dir.path().join(".fseventsd/x"), b"1")?;
        let plugin = Record::default();
        let opts = |since| Options {
            interval: Duration::from_secs(60),
            stop_file: None,
            since: Some(since),
            once: true,
        };
        harvest(dir.path(), &plugin, opts(0))?;
        assert_eq!(*plugin.0.lock().unwrap(), ["a/b/X.DNG:4"]);
        plugin.0.lock().unwrap().clear();
        harvest(dir.path(), &plugin, opts(i64::MAX))?;
        assert!(plugin.0.lock().unwrap().is_empty());
        Ok(())
    }

    #[test]
    fn watch_passes_changes_and_stops_on_the_stop_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let vol = dir.path().join("vol");
        std::fs::create_dir_all(vol.join("a"))?;
        let stop = dir.path().join("stop");
        let plugin = Record::default();
        std::thread::scope(|s| -> Result<()> {
            let h = s.spawn(|| {
                harvest(
                    &vol,
                    &plugin,
                    Options {
                        interval: Duration::from_secs(1),
                        stop_file: Some(stop.clone()),
                        since: None,
                        once: false,
                    },
                )
            });
            std::thread::sleep(Duration::from_millis(1500));
            std::fs::write(vol.join("a/X.JPG"), b"abc")?;
            // FSEvents latency varies with load: wait until the change is seen.
            for _ in 0..200 {
                if !plugin.0.lock().unwrap().is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            std::fs::write(&stop, b"")?;
            h.join().unwrap()
        })?;
        assert!(
            plugin.0.lock().unwrap().iter().any(|e| e == "a/X.JPG:3"),
            "{:?}",
            plugin.0.lock().unwrap()
        );
        Ok(())
    }
}
