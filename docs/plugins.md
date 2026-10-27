# Writing a diskproxy plugin

A plugin chooses visibility, modification times, and an optional stable volume
identity for a source tree. The core supplies read-only source access, WebDAV,
HTTP byte ranges, and exFAT. Adding a plugin does not require implementing those
transports. The [PMP plugin](../plugins/pmp/README.md) is a complete application
policy; the [small hide example](../examples/hide-plugin/README.md) shows the
minimal approach.

Plugins are Rust crates linked at build time. Installing a config file alone
cannot load a new policy: the application must depend on and register its crate.
There is no dynamic `.so`/`.dylib` discovery or executable-plugin protocol.

## The public API

The API is exported by `diskproxy-core`; its source is
[`crates/core/src/view.rs`](../crates/core/src/view.rs):

```rust
use anyhow::Result;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;

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
```

`parts` contains source-relative path components. The root is an empty slice,
not `/`. Metadata is from the real source descriptor. The methods mean:

| Hook | Return value | Core behavior |
| --- | --- | --- |
| `visible` | `true` / `false` | Include the entry or treat it as missing; checked on direct opens as well as enumeration. |
| `mtime` | Unix epoch seconds | Present that time in WebDAV and generated exFAT metadata; source timestamps are unchanged. |
| `identity` | Stable string or `None` | Seed the virtual exFAT serial; `None` uses source device/inode identity. |

An error aborts the corresponding operation; it is not a request to expose the
unmodified entry. Keep methods deterministic for a session: the disk builder
snapshots a directory tree, and source bytes remain live. Load mutable external
history into a stable preparation snapshot before starting the server, as PMP
does. Plugins share a thread-safe object across HTTP workers.

The current API does not provide file-content transformation, path renaming,
arbitrary synthesized files, or plugin composition. It applies one selected
policy to the pinned source tree. Extend the core deliberately if another use
case needs those capabilities.

## Compile the reference plugin

The complete reference crate is already in `examples/hide-plugin`, with its own
manifest and tests. Compile it without changing the application's registry:

```sh
cargo test -p diskproxy-hide-example --locked
```

Its configuration has two lists:

```json
{
  "exclude_names": ["cache", ".DS_Store"],
  "exclude_suffixes": [".tmp"]
}
```

Names and suffixes are literal and case-sensitive, not glob expressions. An
excluded name matches any component, including a directory. Empty strings,
slashes, NUL, and traversal-shaped component names are rejected during setup.
The visibility implementation is small:

```rust
impl Plugin for Hide {
    fn visible(&self, parts: &[String], _metadata: &Metadata) -> Result<bool> {
        Ok(!parts.iter().any(|part| {
            self.config.exclude_names.contains(part)
                || self.config.exclude_suffixes.iter()
                    .any(|suffix| part.ends_with(suffix))
        }))
    }
}
```

The crate imports `diskproxy_core::Plugin` and `anyhow::Result`, and derives
`serde::Deserialize` on `Config` with `default` and `deny_unknown_fields`. The
constructor validates configuration before serving. See the
[complete source](../examples/hide-plugin/src/lib.rs) for those definitions.

Checking every path component matters: hiding `cache` from a directory listing
alone would not prevent a direct request for `cache/secret.txt`. The reference
policy rejects both paths. A policy that hides a directory should normally also
hide its descendants. Keep the empty root visible so it can be enumerated.

## Register the example with the CLI

Start from a clean experimental checkout. The included patch performs all
application integration steps below without changing the core:

```sh
git apply --check examples/hide-plugin/register.patch
git apply examples/hide-plugin/register.patch
cargo test --workspace --locked
```

The patch changes `Cargo.toml`, `Cargo.lock`, and `src/main.rs`. Read it before
applying if your checkout has other edits. It is an executable example tied to
this version, so a later refactor may require applying the steps manually.

### Add the dependency

The example is already a workspace member. Add it to the application's
`[dependencies]`:

```toml
diskproxy-hide-example = { path = "examples/hide-plugin" }
```

For an entirely new plugin named `my-policy`, create a crate such as
`plugins/my-policy`, add that path to workspace `members`, and add its path
dependency to the application. Match the relative core dependency to the crate
location: from either `plugins/my-policy` or `examples/hide-plugin`, it is
`../../crates/core`.

### Register its name and validate settings

Add `Hide` to `PluginName` in `src/main.rs`. Its Serde and Clap derives expose
`"plugin": "hide"` in JSON and `--plugin hide` on the command line.

Add a `"hide"` arm to the complete-config validation loop. Deserialize
`diskproxy_hide_example::Config` from that value and call `Hide::new` to validate
it. Validation belongs before plugin setup can write anything. Do not accept a
misspelled plugin definition silently.

### Construct the prepared view

Add `PluginName::Hide` to the application selection match. Its arm returns the
same tuple as passthrough:

```rust
let options: diskproxy_hide_example::Config = serde_json::from_value(
    config.plugins.remove("hide").unwrap_or(json!({})),
).context("invalid plugins.hide config")?;

(
    Arc::new(Source::new(&source_path)?),
    Arc::new(diskproxy_hide_example::Hide::new(options)?),
    json!({"plugin": "hide", "source": source_path}),
    "DiskProxy",
)
```

Keep the passthrough branch's guard against card registration and PMP-only
options: `register`, source/target offsets, timezone, history, and profile
directory are not features of this visibility plugin. The provided patch
contains the full arm and this guard, including the imports already supplied
by the application. The tuple's plugin is coerced to `Arc<dyn Plugin>`.

If your plugin needs state preparation, make a typed `prepare` function in its
crate instead. It can return an `Arc<Source>`, `Arc<dyn Plugin>`, and startup
JSON as PMP does. Keep app-specific filenames and parsing out of `crates/core`.

### Record dependencies and test the integrated application

When integrating manually, run `cargo check --workspace` once to update
`Cargo.lock`, inspect the changes, and commit the lockfile. The provided patch
already contains its lockfile update. Thereafter use the locked checks:

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

The integration patch was applied in an isolated source copy, compiled, and
exercised with a real HTTP server over a synthetic directory. The checks covered
kept bytes, filtered listings, direct hidden-file requests, hidden-directory
children, and unchanged physical files. Re-run that rehearsal with:

```sh
uv run --python 3.13 python scripts/check-plugin-example.py
```

## Configure and run your policy

After registering the example, create `my-hide.json` with an absolute source:

```json
{
  "source": "/absolute/path/to/folder",
  "plugin": "hide",
  "label": "HideDemo",
  "plugins": {
    "hide": {
      "exclude_names": ["cache", ".DS_Store"],
      "exclude_suffixes": [".tmp"]
    }
  }
}
```

Build and install the integrated binary, then use the normal launcher:

```sh
scripts/install.sh
# Existing default config remains unchanged.
diskproxy --config ./my-hide.json start
diskproxy stop
```

Command-line overrides use the same plugin namespace:

```sh
diskproxy start --config ./my-hide.json \
  --set 'plugins.hide.exclude_suffixes=[".tmp",".bak"]'
```

For read-only inspection or manual WebDAV serving:

```sh
diskproxy-server --config ./my-hide.json --check
diskproxy-server serve --config ./my-hide.json --mode webdav
```

The stock binary will reject `hide` until its registry/dependency changes are
built. General serving uses the explicit `serve` operation; bare
`diskproxy-server` is reserved for PMP registration in the configured use case.

## Testing and source protection

Use synthetic directories. Check the root, visible files, hidden entries,
direct requests for hidden descendants, Unicode names within supported limits,
invalid config, and both transports. Confirm timestamps and bytes remain
unchanged on disk, and that mutation requests are rejected.

Pure hooks never need a writable source descriptor. The core enforces read-only
opens, but a compiled Rust plugin is trusted process code and can call other
I/O APIs. A plugin that writes setup state must document that state and the
operation authorizing it. Do not use original images or catalog databases as
test fixtures. Visibility is a workflow view, not a runtime sandbox for untrusted
plugin code. Keep core source-validation and traversal checks intact.
