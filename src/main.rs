use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use diskproxy_core::{Mode, Passthrough, Source, View};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, ValueEnum, PartialEq)]
#[serde(rename_all = "lowercase")]
enum PluginName {
    #[default]
    Passthrough,
    Pmp,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, ValueEnum, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Presentation {
    #[default]
    Disk,
    Webdav,
    Filtered,
}
#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    source: Option<PathBuf>,
    plugin: PluginName,
    mode: Presentation,
    port: u16,
    label: Option<String>,
    plugins: BTreeMap<String, Value>,
}
#[derive(Parser)]
#[command(
    version,
    about = "Register a card by default; use serve to run a read-only disk or WebDAV proxy",
    override_usage = "diskproxy-server [serve|register] [OPTIONS] [SOURCE]",
    after_help = "Without a command, registers a new card using the default config and current timezone. Existing profiles are reused unchanged. Use serve for headless serving, --check for a read-only check, and --register to explicitly refresh samples."
)]
struct Args {
    source: Option<PathBuf>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, value_enum)]
    plugin: Option<PluginName>,
    #[arg(long, value_enum)]
    mode: Option<Presentation>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    label: Option<String>,
    /// Override a config key, e.g. --set plugins.pmp.hide_ingested=true
    #[arg(long = "set", value_name = "KEY=VALUE")]
    overrides: Vec<String>,
    #[arg(long)]
    check: bool,
    #[arg(long, conflicts_with = "check")]
    register: bool,
    // Familiar PMP aliases; these populate the selected plugin's config.
    #[arg(long, allow_hyphen_values = true)]
    source_offset: Option<String>,
    #[arg(long, allow_hyphen_values = true, conflicts_with = "target_zone")]
    target_offset: Option<String>,
    #[arg(long)]
    target_zone: Option<String>,
    #[arg(long)]
    history: Option<PathBuf>,
    #[arg(long)]
    profiles_dir: Option<PathBuf>,
}
fn override_value(root: &mut Value, expression: &str) -> Result<()> {
    let (key, value) = expression
        .split_once('=')
        .context("--set expects KEY=VALUE")?;
    let parts: Vec<_> = key.split('.').collect();
    ensure!(!parts.iter().any(|p| p.is_empty()), "empty override key");
    let mut node = root;
    for part in &parts[..parts.len() - 1] {
        if node.is_null() {
            *node = json!({});
        }
        node = node
            .as_object_mut()
            .context("override parent is not an object")?
            .entry((*part).to_owned())
            .or_insert(json!({}));
    }
    let v = serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.into()));
    node.as_object_mut()
        .context("override parent is not an object")?
        .insert(parts.last().unwrap().to_string(), v);
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(if error.is::<diskproxy_pmp::RegistrationRequired>() {
            78
        } else {
            1
        });
    }
}
fn run() -> Result<()> {
    let mut argv: Vec<_> = std::env::args_os().collect();
    let operation = argv
        .get(1)
        .and_then(|s| s.to_str())
        .filter(|s| matches!(*s, "serve" | "register"))
        .map(str::to_owned);
    if operation.is_some() {
        argv.remove(1);
    }
    let args = Args::parse_from(argv);
    ensure!(
        !(operation.as_deref() == Some("serve") && args.register),
        "serve and --register conflict"
    );
    let register = !args.check && (args.register || operation.as_deref() != Some("serve"));
    let refresh = args.register; // Bare registration never rewrites an established profile.
    let config_path = args
        .config
        .clone()
        .or_else(|| std::env::var_os("DISKPROXY_CONFIG").map(PathBuf::from))
        .or_else(|| {
            let file =
                PathBuf::from(std::env::var_os("HOME")?).join(".config/diskproxy/config.json");
            file.is_file().then_some(file)
        });
    let mut raw = if let Some(path) = &config_path {
        serde_json::from_slice(
            &std::fs::read(path).with_context(|| format!("read config {}", path.display()))?,
        )?
    } else {
        json!({})
    };
    for expr in &args.overrides {
        override_value(&mut raw, expr)?;
    }
    let mut config: Config = serde_json::from_value(raw).context("invalid diskproxy config")?;
    if let Some(v) = args.source {
        config.source = Some(v);
    }
    if let Some(v) = args.plugin {
        config.plugin = v;
    }
    if let Some(v) = args.mode {
        config.mode = v;
    }
    if let Some(v) = args.port {
        config.port = v;
    }
    if let Some(v) = args.label {
        config.label = Some(v);
    }
    // Validate the complete config before a plugin can perform setup writes.
    for (name, value) in &config.plugins {
        match name.as_str() {
            "pmp" => {
                let _: diskproxy_pmp::Config =
                    serde_json::from_value(value.clone()).context("invalid plugins.pmp config")?;
            }
            _ => bail!("unknown plugin configuration: {name}"),
        }
    }
    if let Some(label) = &config.label {
        diskproxy_core::validate_label(label)?;
    }
    let source_path = config
        .source
        .context("source directory required on command line or in config")?;
    ensure!(
        !(config.mode == Presentation::Filtered && config.plugin != PluginName::Pmp),
        "filtered mode requires the PMP plugin"
    );
    let (source, plugin, info, default_label): (_, Arc<dyn diskproxy_core::Plugin>, _, _) =
        match config.plugin {
            PluginName::Passthrough => {
                ensure!(
                    !register
                        && args.source_offset.is_none()
                        && args.target_offset.is_none()
                        && args.target_zone.is_none()
                        && args.history.is_none()
                        && args.profiles_dir.is_none(),
                    "registration/PMP options require the PMP plugin; select a PMP config with --config FILE, or use diskproxy-server serve for a generic view"
                );
                (
                    Arc::new(Source::new(&source_path)?),
                    Arc::new(Passthrough),
                    json!({"plugin":"passthrough", "source":source_path}),
                    "DiskProxy",
                )
            }
            PluginName::Pmp => {
                let mut options: diskproxy_pmp::Config =
                    serde_json::from_value(config.plugins.remove("pmp").unwrap_or(json!({})))
                        .context("invalid plugins.pmp config")?;
                options.source = source_path;
                options.check = args.check;
                options.register = refresh;
                options.register_if_new = register;
                if args.source_offset.is_some() {
                    options.source_offset = args.source_offset;
                }
                if args.target_offset.is_some() {
                    options.target_offset = args.target_offset;
                    options.target_zone = None;
                }
                if args.target_zone.is_some() {
                    options.target_zone = args.target_zone;
                    options.target_offset = None;
                }
                if args.history.is_some() {
                    options.history = args.history;
                }
                if args.profiles_dir.is_some() {
                    options.profiles_dir = args.profiles_dir;
                }
                ensure!(
                    !(options.target_offset.is_some() && options.target_zone.is_some()),
                    "choose target_offset or target_zone"
                );
                if config.mode == Presentation::Filtered {
                    options.hide_ingested = true;
                }
                let p = diskproxy_pmp::prepare(options)?;
                (p.source, p.plugin, p.info, "PMP Pacific")
            }
        };
    let label = config.label.unwrap_or_else(|| default_label.into());
    let view = View::new(source, plugin, label)?;
    if args.check || register {
        let mut info = info;
        info["mode"] = json!(if config.mode == Presentation::Disk {
            "disk"
        } else {
            "webdav"
        });
        println!("{info}");
        return Ok(());
    }
    diskproxy_core::serve(
        view,
        if config.mode == Presentation::Disk {
            Mode::Disk
        } else {
            Mode::Webdav
        },
        config.port,
        info,
    )
}
