# Architecture

`diskproxy-core` has no application-specific dependencies. Its `Source` opens a
root directory once, uses `openat` with `O_NOFOLLOW` for every component, and
returns only read-only regular-file/directory descriptors. Each traversal rejects
empty components, `.`/`..`, slashes inside a component, and NUL. Listing reopens
directory iterators to avoid shared offsets.

A `View` combines `Arc<Source>`, `Arc<dyn Plugin>`, and a volume label. Plugins
receive path components plus raw `Metadata` and may:

1. exclude an entry with `visible` (enforced on direct opens as well as listings);
2. return a presented modification time with `mtime`;
3. supply a stable volume-identity seed with `identity`.

Defaults expose every supported source entry with unchanged timestamps. Root
entries, file types, camera folders, profile formats, and history semantics are
not part of the core. A plugin cannot make the core's source descriptor writable.
A compiled plugin is trusted Rust code, however, and can perform its own setup
I/O; the trait is a policy boundary, not a security sandbox.

The HTTP/WebDAV layer and exFAT image builder both use the same View. The exFAT
builder allocates boot/FAT/directory metadata in memory and maps source-file
extents to byte ranges. A source extent is revalidated against its captured
size/mtime on read. Offset bytes are valid UTC, avoiding a later mount's local
interpretation of unknown-offset filesystem timestamps.

`diskproxy-pmp` is a separate crate depending on this public API. It prepares a
policy from card identity, portable fingerprint anchors, the mount offset, and
an optional immutable in-memory snapshot of ingest history. Its default registration
operation (bare `diskproxy-server`) can write the card marker/profile; its
source-image opens remain read-only. It never changes EXIF or an original catalog.

The application loads the default config unless overridden and merges JSON/CLI
settings. Bare invocation registers a new card; the explicit `serve` operation
starts the server.
Existing profiles are reused unchanged unless `--register` requests a refresh.
Plugin-specific configuration lives below `plugins.<name>`. Adding a plugin
requires a crate implementing the trait, config preparation code, and an
application registry entry; it does not require changing HTTP or exFAT internals.

`bin/diskproxy` invokes the server with `serve` and wraps it with start/status/stop
and `hdiutil`. A session records the PID and process start time, attachment response, and mounted volume
UUID. Busy detach or an identity mismatch leaves the server alive. Existing
sessions from the original pmp-card launcher can still be stopped through its
compatibility identity check. A single managed session is supported at a time.

The old pmp-ingest-proxy implementation originated in the mechanic workspace.
This repository extracts the reusable source/transport engine and preserves its
PMP-specific behavior as a plugin. User catalog audits and private working data
remain in mechanic, outside this repository.
