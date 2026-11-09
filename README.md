# diskproxy

Present a directory as a **virtual exFAT disk, NFS volume or WebDAV share**
that never writes to the source. Application-specific plugins control which
files are visible, how their timestamps are presented, how a file's bytes are
put together, and what happens to metadata an application writes back. Source
bytes stream on demand; disk metadata is built in memory. No full disk-image
copy, FUSE driver, kernel extension or Python runtime is needed.

The default **passthrough** policy exposes ordinary directory trees and arbitrary
regular file types. The included [Photo Mechanic Plus plugin](plugins/pmp/README.md)
provides stable camera-card ingest across timezone changes and optional hiding
of already-ingested files. Its setup and workflow are documented separately.
The [previews plugin](plugins/previews/README.md) serves DNG stand-ins with a
large preview JPEG taken from another folder, and JPEGs with the camera, lens
and XMP metadata of a matching DNG, without copying image data. On a writable view,
it saves the ratings, color labels and flags an application writes as small XMP
sidecars in a separate folder.

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
prints the mount location. Large trees take a while to scan. The launcher waits
up to 15 minutes while the server is alive and shows its progress on one line,
for example `Scanning: reading files: 523000 of 1256465 (41%)`. One tree of
1.26 million files in 2,413 folders scanned in about a minute; the server then
used about 1.2 GB of memory, peaking at 1.8 GB during startup. With camera
metadata spliced into its 566,000 JPEGs, the same tree scans in about 110 seconds
and uses 1.9 GB. Trees of up to
8 million entries are supported. Stop detaches first, then stops the server. A busy
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
| `plugin` | Registered policy: `passthrough`, `pmp` or `previews`. |
| `mode` | Transport: `disk` (default), `nfs`, or `webdav`. The launcher mounts `disk` and `nfs`; WebDAV is served with `diskproxy-server serve`. |
| `label` | Virtual volume name, up to 11 UTF-16 code units. |
| `writable` | Disk mode only: mount through a shadow file so clients can write; writes never reach the source and are harvested (see below). |
| `mountpoint` | NFS mode only: where the launcher mounts the view; default `~/Volumes/LABEL`. |
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
with the explicit `serve` command. Its startup output is JSON containing a
random-capability URL:

```sh
diskproxy-server serve /path/to/folder --plugin passthrough --mode webdav
diskproxy-server --config ~/my-view.json --check
```

Bare `diskproxy-server` is the card-registration command. It loads the default
config (or the file selected by `DISKPROXY_CONFIG`), assumes the current local
UTC offset for a newly mounted card, and uses UTC for a new card with no history.
Already registered cards retain their profile. An unregistered card encountered
by `diskproxy start` prompts `Run diskproxy-server.`; non-default arguments are
included only when needed. Registration exits without mounting or serving.
See the [PMP guide](plugins/pmp/README.md) for existing-history conventions and
explicit offset overrides. The launcher itself always invokes `serve`.

## Writable views

Some applications write metadata into the files they show, such as ratings and
labels. diskproxy never writes to the source, but it can accept those writes and
hand them to the plugin's `written` hook (the [previews plugin](plugins/previews/README.md)
records ratings as XMP sidecars in a separate folder). Two ways:

- **Shadow file** (`"writable": true`, disk mode). The disk is attached with an
  `hdiutil` shadow file in the session folder, so the volume is writable while
  the served image stays read-only. A harvester follows filesystem events on the
  mounted volume and passes changed files to the plugin every minute and once
  more on `diskproxy stop`. If the harvester died, stop first walks the volume
  for files changed since mounting. The shadow file is deleted with the session.
- **NFS** (`"mode": "nfs"`). diskproxy serves the tree over NFSv3 on localhost
  and the launcher mounts it at `~/Volumes/LABEL` with `mount_nfs` (no
  administrator rights needed). The first write to a file copies its presented
  bytes into a per-session overlay; creates, renames and deletes stay in the
  overlay. Two seconds after the last write, a file is passed to the plugin;
  anything pending is passed when the server stops.

| | Shadow file | NFS |
| --- | --- | --- |
| Status | Ready | Experimental |
| Transport | The same exFAT disk image, attached with `-shadow` | An NFSv3 server on localhost |
| When the plugin sees a write | Within about a minute, and on stop | About two seconds after the last write, and on stop |
| Where written bytes go | `disk.shadow` in the session folder | `overlay/` in the session folder |
| Mounted at | `/Volumes/LABEL` | `~/Volumes/LABEL`, or `mountpoint` |

`diskproxy stop` deletes the session folder, including the written bytes, after
the final capture. Only what the plugin's `written` hook saved elsewhere is kept.

**NFS mode is experimental.** Reads, writes and captures work. During early
development on macOS 26, unmounting hung inside the kernel's NFS client three
times, for between one minute and about half an hour. Each time, the volume
disappeared at once while the unmounting process sat in an uninterruptible wait
with no NFS traffic, and later NFS mounts queued behind it.

Since then, the current server has passed 142 consecutive mount/write/unmount
cycles without a hang:
- 82 direct cycles, covering both unmount methods, delayed and immediate
  unmounts, and exiftool rewrites that write a temporary file and rename it;
- 60 cycles through `diskproxy start` and `stop`.

Unmounts took about 0.1 s, and the slowest launcher stop took 4.6 s. The earlier
hangs may have come from the first server version and a kernel NFS client that
it left degraded, but the cause is not proven. Prefer the shadow-file mode for
real work.

The launcher unmounts with the `unmount(2)` system call
(`diskproxy-server unmount [--force] PATH`). `umount(8)` and `diskutil` instead
wait minutes afterwards for an rpcbind registration that this server doesn't
make. If an unmount does hang, leave the stuck process alone and don't start
another NFS session until it clears.

Manual harvesting of a writable mount (for example after a crash):

```sh
diskproxy-server harvest --config view.json --volume /Volumes/VIEW --since EPOCH --once
```

## Plugins and architecture

- **[Core engine](crates/core):** read-only source descriptors, policy hooks,
  HTTP byte ranges, WebDAV, and virtual exFAT streaming. No PMP-specific policy.
- **[PMP plugin](plugins/pmp/README.md):** Photo Mechanic Plus camera layout,
  portable card profiles, timestamp normalization, and ingest-history filtering.
  Existing `pmp-card` commands and profiles remain compatible.
- **[Previews plugin](plugins/previews/README.md):** composes files from the
  source, generated bytes and a second read-only folder of preview JPEGs.
- **Application:** configuration, command-line overrides, and plugin selection.
- **Launcher:** background process lifecycle and macOS disk attachment. The
  volume UUID is checked before detaching a managed disk.

Plugins are separate Rust crates linked at build time. Implement the public
`Plugin` trait and register the policy in the application to add another use
case. Plugins are trusted code; there is no dynamic plugin ABI or runtime
sandbox. See [architecture and extension points](docs/architecture.md), the
[step-by-step plugin guide](docs/plugins.md), and a
[compiled visibility-plugin example](examples/hide-plugin/README.md).

## Read-only behavior and limits

HTTP binds only to localhost with a fresh 192-bit capability token. Mutation
methods are rejected. Source descriptors use `O_RDONLY` and `O_NOFOLLOW`; symlinks
are not traversed, and only regular files/directories are exposed. The core
never writes to a source. Plugin setup may have its own explicitly documented
writes, such as registering a camera-card profile.

Disk-image names are written in composed Unicode (NFC), which macOS's exFAT
driver uses for lookups; sources with decomposed names (as APFS may store them)
are still opened by their own spelling.

Keep a source unchanged while its disk view is mounted. A source extent's
size/mtime changes cause disk reads to fail; detach and restart after intended
edits. A plugin can present a file as a sequence of pieces (generated bytes,
ranges of the file, ranges of a file under a second read-only root); every
file piece is checked the same way. Generated exFAT timestamps have valid UTC offsets. Disk mode requires
UTF-8, exFAT-compatible names. A modification time outside exFAT's 1980–2107 is
shown as the nearest one it can hold, with a warning naming the file. The current
upcase table folds ASCII letters; non-ASCII case folding is not implemented.
Unsupported names fail rather than being rewritten. This is a virtual view,
not a writable storage driver or disk repair tool.

The [diskproxy manual](docs/diskproxy-manual.md) covers all of this in one
place, with worked workflows and troubleshooting. For the implementation story,
read [diskproxy: virtual disks for stable camera ingest and offline photo libraries](docs/blog/diskproxy/post.md).
The [transport guide](docs/webdav.md) explains how WebDAV differs from the HTTP
virtual disk used by the default launcher.

## Development and verification

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
uv run --python 3.13 python -m unittest discover -s tests -p 'test_*.py'
uv run --python 3.13 python scripts/check-plugin-example.py
```

Tests use synthetic sources for profiles, timestamp policies, history filtering,
HTTP ranges, write rejection, config precedence, and mocked launcher lifecycle.
They also cover:
- reads of composed files across every piece boundary, and change detection
  on their pieces;
- the DNG preview patch, JPEG EXIF injection and metadata spliced from a DNG;
- composed (NFC) names on the disk;
- capturing ratings from JPEG, DNG and sidecar XMP;
- harvesting, both by walking the volume and from filesystem events.

A real macOS attach/read/write-rejection/detach smoke test was also run on a
synthetic non-camera directory. The writable modes were tested end to end with
Photo Mechanic and exiftool writes. Python is needed only for development checks, not the installed tool.
