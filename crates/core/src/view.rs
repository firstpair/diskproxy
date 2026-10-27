use anyhow::{Context, Result, ensure};
use rustix::fs::{Dir, Mode, OFlags, open, openat};
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

/// Application policy. The transport owns every source descriptor and enforces
/// read-only, no-symlink access independently of plugin decisions.
pub trait Plugin: Send + Sync {
    fn identity(&self) -> Option<String> {
        None
    }
    fn visible(&self, _parts: &[String], _metadata: &Metadata) -> Result<bool> {
        Ok(true)
    }
    fn mtime(&self, _parts: &[String], metadata: &Metadata) -> Result<i64> {
        Ok(metadata.mtime())
    }
}
pub struct Passthrough;
impl Plugin for Passthrough {}

pub struct Source {
    root: File,
}
fn ro_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC
}
impl Source {
    pub fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            root: File::from(open(root, ro_flags() | OFlags::DIRECTORY, Mode::empty())?),
        })
    }
    pub fn open_raw(&self, parts: &[String]) -> Result<File> {
        let mut file = File::from(openat(
            &self.root,
            ".",
            ro_flags() | OFlags::DIRECTORY,
            Mode::empty(),
        )?);
        for (i, part) in parts.iter().enumerate() {
            ensure!(
                !part.is_empty() && part != "." && part != ".." && !part.contains(['/', '\0']),
                "invalid path component"
            );
            let flags = ro_flags()
                | if i + 1 < parts.len() {
                    OFlags::DIRECTORY
                } else {
                    OFlags::empty()
                };
            file = File::from(openat(&file, part.as_str(), flags, Mode::empty())?);
        }
        let meta = file.metadata()?;
        if !(meta.is_file() || meta.is_dir()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only regular files and directories are exposed",
            )
            .into());
        }
        Ok(file)
    }
    pub fn names(&self, directory: &File) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in Dir::read_from(directory)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .context("non-UTF-8 source filename")?;
            if name != "." && name != ".." {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }
}

pub struct View {
    source: Arc<Source>,
    plugin: Arc<dyn Plugin>,
    pub(crate) label: String,
}
pub(crate) struct Meta {
    pub directory: bool,
    pub size: u64,
    pub mtime: i64,
    pub etag: String,
}
pub fn validate_label(label: &str) -> Result<()> {
    ensure!(
        !label.is_empty()
            && label.encode_utf16().count() <= 11
            && !label.contains(['\0', '/', '\\']),
        "volume label must be 1–11 UTF-16 code units without slashes or NUL"
    );
    Ok(())
}
impl View {
    pub fn new(source: Arc<Source>, plugin: Arc<dyn Plugin>, label: String) -> Result<Self> {
        validate_label(&label)?;
        Ok(Self {
            source,
            plugin,
            label,
        })
    }
    pub(crate) fn identity(&self) -> Result<String> {
        if let Some(id) = self.plugin.identity() {
            return Ok(id);
        }
        let meta = self.source.root.metadata()?;
        Ok(format!("{}:{}", meta.dev(), meta.ino()))
    }
    pub(crate) fn open_raw(&self, parts: &[String]) -> Result<File> {
        self.source.open_raw(parts)
    }
    pub(crate) fn open(&self, parts: &[String]) -> Result<File> {
        let file = self.open_raw(parts)?;
        if !self.plugin.visible(parts, &file.metadata()?)? {
            return Err(io::Error::new(io::ErrorKind::NotFound, "hidden by plugin").into());
        }
        Ok(file)
    }
    pub(crate) fn metadata(&self, parts: &[String], file: &File) -> Result<Meta> {
        let s = file.metadata()?;
        Ok(Meta {
            directory: s.is_dir(),
            size: s.len(),
            mtime: self.plugin.mtime(parts, &s)?,
            etag: format!(
                "\"{:x}-{:x}-{:x}-{:x}\"",
                s.ino(),
                s.len(),
                s.mtime(),
                s.mtime_nsec()
            ),
        })
    }
    pub(crate) fn names(&self, _parts: &[String], file: &File) -> Result<Vec<String>> {
        self.source.names(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_tree_is_read_only_and_symlinks_are_rejected() -> Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::write(temp.path().join("notes.txt"), b"general purpose")?;
        std::os::unix::fs::symlink("/etc/hosts", temp.path().join("escape"))?;
        let source = Arc::new(Source::new(temp.path())?);
        let view = View::new(source, Arc::new(Passthrough), "DiskProxy".into())?;
        assert!(view.open(&["notes.txt".into()]).is_ok());
        assert!(view.open(&["escape".into()]).is_err());
        assert!(view.open(&["..".into(), "etc".into()]).is_err());
        assert!(
            View::new(
                Arc::new(Source::new(temp.path())?),
                Arc::new(Passthrough),
                "label too long".into()
            )
            .is_err()
        );
        Ok(())
    }
}
