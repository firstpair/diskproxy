# diskproxy

A read-only view of a directory, served as a virtual exFAT disk or WebDAV.
Source bytes are streamed on demand; disk metadata is generated in memory.
No full disk image, photo staging copy, FUSE driver, or Python runtime is needed.

Application policies are plugins. The included **PMP plugin** makes Photo
Mechanic Plus incremental ingest stable across timezone changes and can hide
files already present in its ingest history. The default **passthrough** view
works with ordinary directory trees and arbitrary regular file types.

## Start, status, stop

On macOS, after installing and selecting a config:

```sh
diskproxy                    # same as diskproxy start
diskproxy start
diskproxy status
diskproxy stop
```

Start launches the server in the background, attaches its read-only virtual disk
with `hdiutil`, and prints the mounted volume. Stop detaches first, then stops
the server. If the disk is busy, the server stays running. No URL copying or
second Terminal is needed. The server does not quit when Terminal closes.

```sh
diskproxy start '/Volumes/OTHER CARD'
diskproxy --config ~/my-view.json start
diskproxy start --config ~/my-view.json
diskproxy start --set plugins.pmp.hide_ingested=true
```

`pmp-card`, `pmp-card start`, `pmp-card status`, and `pmp-card stop` remain
compatibility entry points selecting the PMP config. Saved card profiles,
markers, GUIDs, and ingest histories retain their original format and names.

## Build and install

Rust/Cargo is needed to build. The macOS launcher uses zsh, `hdiutil`,
`diskutil`, `plutil`, `ps`, and `nohup`, all included with macOS.

```sh
cargo test --workspace --locked
scripts/install.sh --config configs/pmp.json
```

The installer places `diskproxy`, `diskproxy-server`, and `pmp-card` in `~/bin`,
plus a `pmp-ingest-proxy` compatibility shim for old low-level commands.
It seeds `~/.config/diskproxy/config.json` from the chosen config only if absent,
and preserves existing config files. Ensure `~/bin` is on PATH. To use a generic
folder instead, install with `--config configs/passthrough.json`, then:

```sh
diskproxy start /path/to/folder
```

See [the PMP plugin guide](plugins/pmp/README.md) before first card registration.

## Configuration

Example PMP use case (`~/.config/diskproxy/config.json`):

```json
{
  "source": "/Volumes/LEICA M",
  "plugin": "pmp",
  "mode": "disk",
  "label": "PMP Pacific",
  "plugins": {
    "pmp": {
      "hide_ingested": false
    }
  }
}
```

The shared config keys are `source`, `plugin`, `mode`, `label`, and `port`.
`plugins.<name>` holds that plugin's definitions. Config files are JSON; paths
are literal (use absolute paths, not `~` within JSON). The default label is
`DiskProxy` for passthrough and `PMP Pacific` for the PMP compatibility use case.
A label has at most 11 UTF-16 code units.

Precedence: **config file → `--set KEY=VALUE` → explicit flags**. Override values
are parsed as JSON when possible, otherwise strings. Unknown config fields are
errors. Examples:

```sh
diskproxy start --config configs/pmp.json \
  --set plugins.pmp.hide_ingested=true

diskproxy start --set label=MyCard

diskproxy-server --config configs/pmp.json --check
```

Familiar PMP flags are aliases into plugin settings: `--source-offset`,
`--target-offset`, `--target-zone`, `--history`, and `--profiles-dir`.
They require the PMP plugin. Neither a config file nor a CLI override may
silently change a registered card's saved timestamp convention.

The lifecycle launcher mounts disk mode. For headless service, diagnostics, or
WebDAV, use the server directly:

```sh
diskproxy-server /path/to/folder --plugin passthrough --mode webdav
diskproxy-server --config configs/pmp-filtered.json --check
```

`--mode filtered` remains a compatibility alias for PMP-filtered WebDAV;
`plugins.pmp.hide_ingested=true` enables filtering with either transport.
Startup output is JSON and includes the random-capability URL. This is the
low-level interface, not an extra step for normal launcher use.

## Architecture and guarantees

- [`diskproxy-core`](crates/core): pinned read-only source descriptors; no
  symlink traversal; plugin visibility/timestamp/identity hooks; HTTP byte
  ranges, WebDAV, and virtual exFAT streaming.
- [`diskproxy-pmp`](plugins/pmp): camera layout, GUID markers, portable profiles,
  timezone normalization, ingest-history parsing, and hiding known images.
- Application: JSON configuration, CLI overrides, and plugin selection.
- macOS launcher: background server lifecycle and disk attachment, independent
  of the selected policy. Active disks are checked by volume UUID before detach.

Plugins are separate Rust crates linked at build time. There is no dynamic
library ABI, executable-plugin protocol, or runtime sandbox. To add a policy,
implement the public `Plugin` trait and register it in the application; see
[the architecture notes](docs/architecture.md).

HTTP binds only to localhost and uses a fresh 192-bit capability token. HTTP
mutation methods are rejected. Source descriptors are opened `O_RDONLY` and
`O_NOFOLLOW`; only regular files/directories are served. The PMP setup code
can write its marker/profile, but the core never writes to the source. Do not
edit a source while its disk view is mounted: size/mtime changes cause disk
reads to fail; detach and restart after an intended source edit.

The core builds on Unix; the one-command mounting workflow requires macOS.
Generated exFAT timestamps have explicit UTC offsets. Disk mode requires UTF-8
names compatible with exFAT and timestamp years 1980–2107. The current exFAT
upcase table folds ASCII letters; non-ASCII case folding is not implemented.
Non-UTF-8 names and unsupported timestamps fail rather than being rewritten.
This is a read-only virtual view, not a writable storage driver or disk repair tool.

## Verification

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
uv run --python 3.13 python -m unittest discover -s tests -p 'test_*.py'
```

Rust tests use synthetic directories for source safety, profiles and timezone
changes, history filtering, HTTP ranges, write rejection, and config precedence.
Launcher tests mock disk utilities; a real macOS attach/read/write-rejection/
detach smoke test was also performed on a synthetic non-camera directory.
