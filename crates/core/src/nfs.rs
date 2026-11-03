//! An NFSv3 view of the source tree for clients that write.
//!
//! Reads are the same composed files the disk and WebDAV transports serve.
//! Writes never reach the source: the first write to a served file copies its
//! presented bytes into a per-session overlay folder, and new files, renames
//! and deletions are recorded in the overlay too. About two seconds after the
//! last write to a file, it is passed to the plugin's `written` hook.
use crate::view::{Meta, View};
use crate::{error_status, layout_reader};
use anyhow::{Context, Result};
use async_trait::async_trait;
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, set_mtime, set_size3,
    specdata3,
};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ROOT: fileid3 = 1;
/// A file is captured once it has had no writes for this long.
const SETTLE: Duration = Duration::from_secs(2);

#[derive(Default)]
struct State {
    paths: HashMap<fileid3, String>,
    ids: HashMap<String, fileid3>,
    next: fileid3,
    /// Client-written files: path -> overlay file.
    overlay: HashMap<String, PathBuf>,
    /// Client-created directories.
    dirs: HashSet<String>,
    /// Served paths the client removed or renamed away.
    hidden: HashSet<String>,
    /// Presented metadata of served files, computed on first use.
    meta: HashMap<String, Arc<Meta>>,
    /// Paths written recently, with the time of their last write.
    dirty: HashMap<String, Instant>,
    seq: u64,
}

pub struct Nfs {
    view: Arc<View>,
    overlay_dir: PathBuf,
    state: Arc<Mutex<State>>,
    uid: u32,
    gid: u32,
}

fn parts(path: &str) -> Vec<String> {
    if path.is_empty() {
        vec![]
    } else {
        path.split('/').map(str::to_owned).collect()
    }
}
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}
fn name(n: &filename3) -> Result<String, nfsstat3> {
    let s = std::str::from_utf8(&n.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
    if s.is_empty() || s == "." || s == ".." || s.contains(['/', '\0']) {
        return Err(nfsstat3::NFS3ERR_INVAL);
    }
    Ok(s.to_owned())
}
fn status(e: &anyhow::Error) -> nfsstat3 {
    match error_status(e) {
        404 => nfsstat3::NFS3ERR_NOENT,
        403 => nfsstat3::NFS3ERR_ACCES,
        _ => nfsstat3::NFS3ERR_IO,
    }
}
fn io(e: std::io::Error) -> nfsstat3 {
    status(&e.into())
}
fn time(secs: i64, nsec: u32) -> nfstime3 {
    nfstime3 {
        seconds: secs.clamp(0, u32::MAX as i64) as u32,
        nseconds: nsec,
    }
}

enum Kind {
    Dir,
    Overlay(PathBuf),
    Served(Arc<Meta>),
}

impl Nfs {
    pub fn new(view: Arc<View>, overlay_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&overlay_dir)
            .with_context(|| format!("create overlay {}", overlay_dir.display()))?;
        let mut state = State {
            next: ROOT + 1,
            ..State::default()
        };
        state.paths.insert(ROOT, String::new());
        state.ids.insert(String::new(), ROOT);
        Ok(Self {
            view,
            overlay_dir,
            state: Arc::new(Mutex::new(state)),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
        })
    }

    fn id_for(st: &mut State, path: &str) -> fileid3 {
        if let Some(id) = st.ids.get(path) {
            return *id;
        }
        let id = st.next;
        st.next += 1;
        st.paths.insert(id, path.to_owned());
        st.ids.insert(path.to_owned(), id);
        id
    }

    fn path_of(&self, id: fileid3) -> Result<String, nfsstat3> {
        self.state
            .lock()
            .unwrap()
            .paths
            .get(&id)
            .cloned()
            .ok_or(nfsstat3::NFS3ERR_STALE)
    }

    /// What `path` currently is, or NOENT.
    fn kind(&self, path: &str) -> Result<Kind, nfsstat3> {
        {
            let st = self.state.lock().unwrap();
            if let Some(f) = st.overlay.get(path) {
                return Ok(Kind::Overlay(f.clone()));
            }
            if path.is_empty() || st.dirs.contains(path) {
                return Ok(Kind::Dir);
            }
            if st.hidden.contains(path) {
                return Err(nfsstat3::NFS3ERR_NOENT);
            }
            if let Some(m) = st.meta.get(path) {
                return Ok(Kind::Served(m.clone()));
            }
        }
        let p = parts(path);
        let file = self.view.open(&p).map_err(|e| status(&e))?;
        let meta = self.view.metadata(&p, &file).map_err(|e| status(&e))?;
        if meta.directory {
            return Ok(Kind::Dir);
        }
        let meta = Arc::new(meta);
        self.state
            .lock()
            .unwrap()
            .meta
            .insert(path.to_owned(), meta.clone());
        Ok(Kind::Served(meta))
    }

    fn attr(&self, id: fileid3, path: &str) -> Result<fattr3, nfsstat3> {
        let (ftype, mode, size, mtime) = match self.kind(path)? {
            Kind::Dir => {
                let m = if path.is_empty() || self.state.lock().unwrap().dirs.contains(path) {
                    None
                } else {
                    let p = parts(path);
                    let f = self.view.open(&p).map_err(|e| status(&e))?;
                    Some(self.view.metadata(&p, &f).map_err(|e| status(&e))?.mtime)
                };
                (ftype3::NF3DIR, 0o755, 4096, time(m.unwrap_or(0), 0))
            }
            Kind::Overlay(f) => {
                let m = std::fs::metadata(&f).map_err(io)?;
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .unwrap_or_default();
                (
                    ftype3::NF3REG,
                    0o644,
                    m.len(),
                    time(t.as_secs() as i64, t.subsec_nanos()),
                )
            }
            Kind::Served(m) => (ftype3::NF3REG, 0o644, m.size, time(m.mtime, 0)),
        };
        Ok(fattr3 {
            ftype,
            mode,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            size,
            used: size,
            rdev: specdata3::default(),
            fsid: 1,
            fileid: id,
            atime: mtime,
            mtime,
            ctime: mtime,
        })
    }

    /// The overlay file for `path`, copying the served bytes in on first write.
    fn overlay_file(&self, path: &str) -> Result<PathBuf, nfsstat3> {
        let kind = self.kind(path)?;
        let meta = match kind {
            Kind::Overlay(f) => return Ok(f),
            Kind::Dir => return Err(nfsstat3::NFS3ERR_ISDIR),
            Kind::Served(m) => m,
        };
        let target = self.new_overlay_path();
        let mut reader = layout_reader(&self.view, &parts(path), &meta.layout, 0, meta.size)
            .map_err(|e| status(&e))?;
        let mut out = File::create(&target).map_err(io)?;
        std::io::copy(&mut reader, &mut out).map_err(io)?;
        let t = UNIX_EPOCH + Duration::from_secs(meta.mtime.max(0) as u64);
        out.set_modified(t).map_err(io)?;
        self.state
            .lock()
            .unwrap()
            .overlay
            .insert(path.to_owned(), target.clone());
        Ok(target)
    }

    fn new_overlay_path(&self) -> PathBuf {
        let mut st = self.state.lock().unwrap();
        st.seq += 1;
        self.overlay_dir.join(format!("{:08}", st.seq))
    }

    fn touch(&self, path: &str) {
        self.state
            .lock()
            .unwrap()
            .dirty
            .insert(path.to_owned(), Instant::now());
    }

    fn capturer(&self) -> (Arc<View>, Arc<Mutex<State>>) {
        (self.view.clone(), self.state.clone())
    }
}

/// Pass files that have settled since their last write (or all pending files)
/// to the plugin.
fn capture_settled(view: &View, state: &Mutex<State>, all: bool) {
    {
        let ready: Vec<(String, PathBuf)> = {
            let mut st = state.lock().unwrap();
            let done: Vec<String> = st
                .dirty
                .iter()
                .filter(|(_, t)| all || t.elapsed() >= SETTLE)
                .map(|(p, _)| p.clone())
                .collect();
            done.into_iter()
                .filter_map(|p| {
                    st.dirty.remove(&p);
                    st.overlay.get(&p).cloned().map(|f| (p, f))
                })
                .collect()
        };
        for (path, file) in ready {
            let result = File::open(&file)
                .map_err(anyhow::Error::from)
                .and_then(|f| view.written(&parts(&path), &f));
            if let Err(e) = result {
                eprintln!("nfs: capture {path}: {e:#}");
            }
        }
    }
}

impl Nfs {
    fn apply(&self, path: &str, attr: &sattr3) -> Result<(), nfsstat3> {
        if let set_size3::size(size) = attr.size {
            let f = self.overlay_file(path)?;
            OpenOptions::new()
                .write(true)
                .open(&f)
                .and_then(|f| f.set_len(size))
                .map_err(io)?;
            self.touch(path);
        }
        let when = match attr.mtime {
            set_mtime::DONT_CHANGE => None,
            set_mtime::SET_TO_SERVER_TIME => Some(SystemTime::now()),
            set_mtime::SET_TO_CLIENT_TIME(t) => {
                Some(UNIX_EPOCH + Duration::new(t.seconds as u64, t.nseconds))
            }
        };
        if let Some(when) = when
            && !matches!(self.kind(path)?, Kind::Dir)
        {
            let f = self.overlay_file(path)?;
            File::options()
                .write(true)
                .open(&f)
                .and_then(|f| f.set_modified(when))
                .map_err(io)?;
        }
        Ok(())
    }

    fn children(&self, dir: &str) -> Result<BTreeMap<String, ()>, nfsstat3> {
        let mut names = BTreeMap::new();
        let st_dirs = self.state.lock().unwrap().dirs.contains(dir);
        if !st_dirs {
            let p = parts(dir);
            let f = self.view.open(&p).map_err(|e| status(&e))?;
            for n in self.view.names(&p, &f).map_err(|e| status(&e))? {
                names.insert(n, ());
            }
        }
        let st = self.state.lock().unwrap();
        names.retain(|n, _| !st.hidden.contains(&join(dir, n)));
        for p in st.overlay.keys().chain(st.dirs.iter()) {
            if parent(p) == dir && !p.is_empty() {
                names.insert(p.rsplit('/').next().unwrap().to_owned(), ());
            }
        }
        Ok(names)
    }
}

#[async_trait]
impl NFSFileSystem for Nfs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }
    fn root_dir(&self) -> fileid3 {
        ROOT
    }
    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let dir = self.path_of(dirid)?;
        if filename.0 == b"." {
            return Ok(dirid);
        }
        if filename.0 == b".." {
            let p = parent(&dir).to_owned();
            return Ok(Self::id_for(&mut self.state.lock().unwrap(), &p));
        }
        let path = join(&dir, &name(filename)?);
        self.kind(&path)?;
        Ok(Self::id_for(&mut self.state.lock().unwrap(), &path))
    }
    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let path = self.path_of(id)?;
        self.attr(id, &path)
    }
    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let path = self.path_of(id)?;
        self.apply(&path, &setattr)?;
        self.attr(id, &path)
    }
    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let path = self.path_of(id)?;
        match self.kind(&path)? {
            Kind::Dir => Err(nfsstat3::NFS3ERR_ISDIR),
            Kind::Overlay(f) => {
                let f = File::open(f).map_err(io)?;
                let size = f.metadata().map_err(io)?.len();
                let len = (count as u64).min(size.saturating_sub(offset));
                let mut buf = vec![0u8; len as usize];
                f.read_exact_at(&mut buf, offset).map_err(io)?;
                Ok((buf, offset + len >= size))
            }
            Kind::Served(m) => {
                let len = (count as u64).min(m.size.saturating_sub(offset));
                let mut buf = Vec::with_capacity(len as usize);
                if len > 0 {
                    layout_reader(&self.view, &parts(&path), &m.layout, offset, len)
                        .map_err(|e| status(&e))?
                        .read_to_end(&mut buf)
                        .map_err(io)?;
                }
                Ok((buf, offset + len >= m.size))
            }
        }
    }
    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let path = self.path_of(id)?;
        let f = self.overlay_file(&path)?;
        OpenOptions::new()
            .write(true)
            .open(&f)
            .and_then(|f| f.write_all_at(data, offset))
            .map_err(io)?;
        self.touch(&path);
        self.attr(id, &path)
    }
    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir = self.path_of(dirid)?;
        let path = join(&dir, &name(filename)?);
        if matches!(self.kind(&path), Ok(Kind::Dir)) {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        let target = self.new_overlay_path();
        File::create(&target).map_err(io)?;
        let id = {
            let mut st = self.state.lock().unwrap();
            st.overlay.insert(path.clone(), target);
            st.hidden.remove(&path);
            Self::id_for(&mut st, &path)
        };
        self.apply(&path, &attr)?;
        self.touch(&path);
        Ok((id, self.attr(id, &path)?))
    }
    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let dir = self.path_of(dirid)?;
        let path = join(&dir, &name(filename)?);
        if self.kind(&path).is_ok() {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }
        Ok(self.create(dirid, filename, sattr3::default()).await?.0)
    }
    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir = self.path_of(dirid)?;
        let path = join(&dir, &name(dirname)?);
        if self.kind(&path).is_ok() {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }
        let id = {
            let mut st = self.state.lock().unwrap();
            st.dirs.insert(path.clone());
            st.hidden.remove(&path);
            Self::id_for(&mut st, &path)
        };
        Ok((id, self.attr(id, &path)?))
    }
    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let dir = self.path_of(dirid)?;
        let path = join(&dir, &name(filename)?);
        let kind = self.kind(&path)?;
        if matches!(kind, Kind::Dir) && !self.children(&path)?.is_empty() {
            return Err(nfsstat3::NFS3ERR_NOTEMPTY);
        }
        let mut st = self.state.lock().unwrap();
        if let Some(f) = st.overlay.remove(&path) {
            let _ = std::fs::remove_file(f);
        }
        st.dirs.remove(&path);
        st.dirty.remove(&path);
        st.hidden.insert(path.clone());
        if let Some(id) = st.ids.remove(&path) {
            st.paths.remove(&id);
        }
        Ok(())
    }
    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from = join(&self.path_of(from_dirid)?, &name(from_filename)?);
        let to = join(&self.path_of(to_dirid)?, &name(to_filename)?);
        if matches!(self.kind(&from)?, Kind::Dir) {
            return Err(nfsstat3::NFS3ERR_NOTSUPP);
        }
        if matches!(self.kind(&to), Ok(Kind::Dir)) {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        let file = self.overlay_file(&from)?;
        let mut st = self.state.lock().unwrap();
        st.overlay.remove(&from);
        if let Some(old) = st.overlay.insert(to.clone(), file) {
            let _ = std::fs::remove_file(old);
        }
        st.hidden.insert(from.clone());
        st.hidden.remove(&to);
        st.dirty.remove(&from);
        st.dirty.insert(to.clone(), Instant::now());
        // The renamed file keeps its handle; whatever was at `to` goes stale.
        if let Some(old) = st.ids.remove(&to) {
            st.paths.remove(&old);
        }
        if let Some(id) = st.ids.remove(&from) {
            st.paths.insert(id, to.clone());
            st.ids.insert(to, id);
        }
        Ok(())
    }
    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let dir = self.path_of(dirid)?;
        if !matches!(self.kind(&dir)?, Kind::Dir) {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        let names: Vec<String> = self.children(&dir)?.into_keys().collect();
        let ids: Vec<(String, fileid3)> = {
            let mut st = self.state.lock().unwrap();
            names
                .into_iter()
                .map(|n| {
                    let p = join(&dir, &n);
                    let id = Self::id_for(&mut st, &p);
                    (n, id)
                })
                .collect()
        };
        let start = if start_after == 0 {
            0
        } else {
            ids.iter()
                .position(|(_, id)| *id == start_after)
                .map(|i| i + 1)
                .ok_or(nfsstat3::NFS3ERR_BAD_COOKIE)?
        };
        let mut entries = Vec::new();
        for (n, id) in ids.iter().skip(start) {
            if entries.len() >= max_entries {
                break;
            }
            match self.attr(*id, &join(&dir, n)) {
                Ok(attr) => entries.push(DirEntry {
                    fileid: *id,
                    name: n.as_bytes().to_vec().into(),
                    attr,
                }),
                Err(nfsstat3::NFS3ERR_NOENT) | Err(nfsstat3::NFS3ERR_ACCES) => (),
                Err(e) => return Err(e),
            }
        }
        let end = start + entries.len() >= ids.len();
        Ok(ReadDirResult { entries, end })
    }
    async fn symlink(
        &self,
        _: fileid3,
        _: &filename3,
        _: &nfspath3,
        _: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
    async fn readlink(&self, _: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

/// Serve `view` over NFSv3 on localhost. Client writes go to `overlay_dir`.
/// Startup info (including `nfs_port`) is printed as one JSON line.
pub fn serve_nfs(
    view: View,
    port: u16,
    overlay_dir: PathBuf,
    mut info: serde_json::Value,
) -> Result<()> {
    let fs = Nfs::new(Arc::new(view), overlay_dir)?;
    let (view, state) = fs.capturer();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = NFSTcpListener::bind(&format!("127.0.0.1:{port}"), fs).await?;
        info["mode"] = serde_json::json!("nfs");
        info["nfs_port"] = serde_json::json!(listener.get_listen_port());
        println!("{info}");
        let (v, s) = (view.clone(), state.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let (v, s) = (v.clone(), s.clone());
                let _ = tokio::task::spawn_blocking(move || capture_settled(&v, &s, false)).await;
            }
        });
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            r = listener.handle_forever() => r?,
            _ = term.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
        // Capture whatever is still pending before exiting.
        capture_settled(&view, &state, true);
        eprintln!("nfs: stopped");
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Plugin, Source};

    #[derive(Default)]
    struct Record(Mutex<Vec<(String, Vec<u8>)>>);
    impl Plugin for Record {
        fn written(&self, parts: &[String], file: &File) -> Result<()> {
            let mut data = Vec::new();
            (&*file).read_to_end(&mut data)?;
            self.0.lock().unwrap().push((parts.join("/"), data));
            Ok(())
        }
    }
    struct Shared(Arc<Record>);
    impl Plugin for Shared {
        fn written(&self, parts: &[String], file: &File) -> Result<()> {
            self.0.written(parts, file)
        }
    }

    fn fname(s: &str) -> filename3 {
        s.as_bytes().to_vec().into()
    }

    #[tokio::test]
    async fn overlay_keeps_source_untouched_and_captures_settled_writes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("a"))?;
        std::fs::write(src.join("a/X.JPG"), b"served-jpeg")?;
        std::fs::write(src.join("a/Y.DNG"), b"served-dng")?;
        let record = Arc::new(Record::default());
        let view = View::new(
            Arc::new(Source::new(&src)?),
            Arc::new(Shared(record.clone())),
            "T".into(),
        )?;
        let fs = Nfs::new(Arc::new(view), dir.path().join("overlay"))?;
        let a = fs.lookup(ROOT, &fname("a")).await.unwrap();
        let y = fs.lookup(a, &fname("Y.DNG")).await.unwrap();
        // In-place write to a served file: copy-up, then patch.
        let attr = fs.write(y, 7, b"DNG").await.unwrap();
        assert_eq!(attr.size, 10);
        assert_eq!(fs.read(y, 0, 100).await.unwrap().0, b"served-DNG");
        // Temporary file + rename over a served file (how JPEGs get rewritten).
        let (t, _) = fs
            .create(a, &fname("X.JPG.tmp"), sattr3::default())
            .await
            .unwrap();
        fs.write(t, 0, b"new-jpeg").await.unwrap();
        fs.rename(a, &fname("X.JPG.tmp"), a, &fname("X.JPG"))
            .await
            .unwrap();
        let x = fs.lookup(a, &fname("X.JPG")).await.unwrap();
        assert_eq!(x, t, "renamed file keeps its handle");
        assert_eq!(fs.read(x, 0, 100).await.unwrap().0, b"new-jpeg");
        assert!(fs.lookup(a, &fname("X.JPG.tmp")).await.is_err());
        // New sidecar, then a listing that merges served and overlay entries.
        let (s, _) = fs
            .create(a, &fname("Y.XMP"), sattr3::default())
            .await
            .unwrap();
        fs.write(s, 0, b"<x/>").await.unwrap();
        let names: Vec<String> = fs
            .readdir(a, 0, 100)
            .await
            .unwrap()
            .entries
            .iter()
            .map(|e| String::from_utf8(e.name.0.clone()).unwrap())
            .collect();
        assert_eq!(names, ["X.JPG", "Y.DNG", "Y.XMP"]);
        // Removing a served file hides it.
        fs.remove(a, &fname("Y.DNG")).await.unwrap();
        assert!(fs.lookup(a, &fname("Y.DNG")).await.is_err());
        // The source is untouched.
        assert_eq!(std::fs::read(src.join("a/X.JPG"))?, b"served-jpeg");
        assert_eq!(std::fs::read(src.join("a/Y.DNG"))?, b"served-dng");
        assert_eq!(std::fs::read_dir(src.join("a"))?.count(), 2);
        // Nothing is captured before writes settle; everything pending is on stop.
        let (v, st) = fs.capturer();
        capture_settled(&v, &st, false);
        assert!(record.0.lock().unwrap().is_empty());
        capture_settled(&v, &st, true);
        let mut got: Vec<_> = record.0.lock().unwrap().clone();
        got.sort();
        assert_eq!(
            got,
            [
                ("a/X.JPG".to_owned(), b"new-jpeg".to_vec()),
                ("a/Y.XMP".to_owned(), b"<x/>".to_vec())
            ]
        );
        Ok(())
    }
}
