# diskproxy

Present a directory as a **read-only virtual exFAT disk or WebDAV share**, with
application-specific plugins controlling which files are visible and how their
timestamps are presented. Source bytes stream on demand; disk metadata is built
in memory. No full disk-image copy, FUSE driver, or Python runtime is needed.

The default **passthrough** policy exposes ordinary directory trees and arbitrary
regular file types. The included [Photo Mechanic Plus plugin](plugins/pmp/README.md)
provides stable camera-card ingest across timezone changes and optional hiding
of already-ingested files. Its setup and workflow are documented separately.

## Quick start on macOS

Install Rust/Cargo, then:

```sh
git clone https://github.com/firstpair/diskproxy.git
cd diskproxy
scripts/install.sh --config configs/passthrough.json

diskproxy start /absolute/path/to/folder
# Use the mounted /Volumes/DiskProxy volume in your application.
diskproxy stop
```

The installer places the launcher and `diskproxy-server` binary in `~/bin`;
ensure that directory is on PATH. It seeds `~/.config/diskproxy/config.json`
from the selected example only if the default config does not already exist.
Existing settings are preserved. For the camera-card use case, follow the
[PMP plugin guide](plugins/pmp/README.md) instead.

The launcher uses zsh, `hdiutil`, `diskutil`, `plutil`, `ps`, and `nohup`, included
with macOS. The Rust server builds on Unix; automatic disk mounting requires macOS.

## Everyday commands

Once a config supplies your source directory and plugin:

```sh
diskproxy                    # same as diskproxy start
diskproxy start
diskproxy status
diskproxy stop
```

Start launches the server in the background, attaches its read-only disk, and
prints the mount location. Stop detaches first, then stops the server. A busy
disk stays served until it can be detached. Closing Terminal does not stop it.
No manual URL copying or second Terminal is required.

A positional path overrides the configured source. Config files can be selected
before or after `start`:

```sh
diskproxy start /another/folder
diskproxy --config ~/my-view.json start
diskproxy start --config ~/my-view.json
diskproxy start --set label=MyDisk
```

## Configuration

The default config is `~/.config/diskproxy/config.json`. A general-purpose example:

```json
{
  "source": "/absolute/path/to/folder",
  "plugin": "passthrough",
  "mode": "disk",
  "label": "DiskProxy",
  "port": 0,
  "plugins": {}
}
```

| Key | Meaning |
| --- | --- |
| `source` | Directory to expose; an explicit positional path overrides it. |
| `plugin` | Registered policy: `passthrough` or `pmp`. |
| `mode` | Low-level transport: `disk` or `webdav`; the lifecycle launcher always mounts disk mode. |
| `label` | Virtual volume name, up to 11 UTF-16 code units. |
| `port` | Localhost HTTP port; `0` selects an available port. |
| `plugins.<name>` | Settings specific to that plugin; see its own guide. |

JSON paths are literal: use absolute paths, not `~` within a config file.
Precedence is **config file → `--set KEY=VALUE` → explicit flags**. Overrides
parse values as JSON when possible, otherwise as strings. Unknown config keys
are errors. Plugin settings accept the same dotted-key overrides; for example:

```sh
diskproxy start --config configs/pmp.json \
  --set plugins.pmp.hide_ingested=true
```

For headless service, WebDAV, or a read-only configuration check, use the server
directly. Its startup output is JSON containing a random-capability URL:

```sh
diskproxy-server /path/to/folder --plugin passthrough --mode webdav
diskproxy-server --config ~/my-view.json --check
```

## Plugins and architecture

- **[Core engine](crates/core):** read-only source descriptors, policy hooks,
  HTTP byte ranges, WebDAV, and virtual exFAT streaming. No PMP-specific policy.
- **[PMP plugin](plugins/pmp/README.md):** Photo Mechanic Plus camera layout,
  portable card profiles, timestamp normalization, and ingest-history filtering.
  Existing `pmp-card` commands and profiles remain compatible.
- **Application:** configuration, command-line overrides, and plugin selection.
- **Launcher:** background process lifecycle and macOS disk attachment. The
  volume UUID is checked before detaching a managed disk.

Plugins are separate Rust crates linked at build time. Implement the public
`Plugin` trait and register the policy in the application to add another use
case. Plugins are trusted code; there is no dynamic plugin ABI or runtime
sandbox. See [architecture and extension points](docs/architecture.md).

## Read-only behavior and limits

HTTP binds only to localhost with a fresh 192-bit capability token. Mutation
methods are rejected. Source descriptors use `O_RDONLY` and `O_NOFOLLOW`; symlinks
are not traversed, and only regular files/directories are exposed. The core
never writes to a source. Plugin setup may have its own explicitly documented
writes, such as registering a camera-card profile.

Keep a source unchanged while its disk view is mounted. A source extent's
size/mtime changes cause disk reads to fail; detach and restart after intended
edits. Generated exFAT timestamps have valid UTC offsets. Disk mode requires
UTF-8, exFAT-compatible names and timestamp years 1980–2107. The current upcase
table folds ASCII letters; non-ASCII case folding is not implemented. Unsupported
names and timestamps fail rather than being rewritten. This is a virtual view,
not a writable storage driver or disk repair tool.

## Development and verification

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
uv run --python 3.13 python -m unittest discover -s tests -p 'test_*.py'
```

Tests use synthetic sources for profiles, timestamp policies, history filtering,
HTTP ranges, write rejection, config precedence, and mocked launcher lifecycle.
A real macOS attach/read/write-rejection/detach smoke test was also performed on
a synthetic non-camera directory. Python is needed only for the launcher tests.
