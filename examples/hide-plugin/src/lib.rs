//! A small, compiled reference plugin. Register it in the application to select
//! it through --plugin hide; see docs/plugins.md and register.patch.
use anyhow::{Result, ensure};
use diskproxy_core::Plugin;
use serde::Deserialize;
use std::fs::Metadata;

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub exclude_names: Vec<String>,
    pub exclude_suffixes: Vec<String>,
}

pub struct Hide {
    config: Config,
}
impl Hide {
    pub fn new(config: Config) -> Result<Self> {
        for name in &config.exclude_names {
            ensure!(
                !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0']),
                "exclude_names must contain nonempty single path components"
            );
        }
        for suffix in &config.exclude_suffixes {
            ensure!(
                !suffix.is_empty() && !suffix.contains(['/', '\0']),
                "exclude_suffixes must contain nonempty suffixes without slashes or NUL"
            );
        }
        Ok(Self { config })
    }
}
impl Plugin for Hide {
    fn visible(&self, parts: &[String], _metadata: &Metadata) -> Result<bool> {
        // Check every component so direct requests for children of a hidden
        // directory cannot bypass the policy merely by skipping its listing.
        Ok(!parts.iter().any(|part| {
            self.config.exclude_names.contains(part)
                || self
                    .config
                    .exclude_suffixes
                    .iter()
                    .any(|suffix| part.ends_with(suffix))
        }))
    }
    // mtime and identity keep the core's default behavior.
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hides_named_entries_and_their_descendants_but_keeps_the_root() -> Result<()> {
        let source = tempfile::tempdir()?;
        let meta = source.path().metadata()?;
        let policy = Hide::new(Config {
            exclude_names: vec!["cache".into()],
            exclude_suffixes: vec![".tmp".into()],
        })?;
        assert!(policy.visible(&[], &meta)?);
        assert!(!policy.visible(&["cache".into()], &meta)?);
        assert!(!policy.visible(&["cache".into(), "payload.txt".into()], &meta)?);
        assert!(!policy.visible(&["documents".into(), "draft.tmp".into()], &meta)?);
        assert!(policy.visible(&["documents".into(), "keep.txt".into()], &meta)?);
        Ok(())
    }
    #[test]
    fn rejects_empty_or_path_shaped_configuration() {
        for names in [vec!["".into()], vec!["..".into()], vec!["a/b".into()]] {
            assert!(
                Hide::new(Config {
                    exclude_names: names,
                    ..Default::default()
                })
                .is_err()
            );
        }
        assert!(
            Hide::new(Config {
                exclude_suffixes: vec!["".into()],
                ..Default::default()
            })
            .is_err()
        );
    }
}
