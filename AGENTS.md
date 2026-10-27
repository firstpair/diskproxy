# Source protection

All source files are read-only through diskproxy-core. Never edit, rename,
move, or delete real photos, Photo Mechanic or Lightroom Classic databases,
SQLite sidecars, application state, original previews, or ingest histories.
Tests must use synthetic sources. Any writable artifacts belong outside those
original directories. The PMP plugin may write its documented card marker and
profile only during explicit user-authorized card setup; never rewrite EXIF.

# Architecture

`crates/core` owns source descriptors, the plugin trait, HTTP/WebDAV, and exFAT.
It must not depend on the PMP plugin or know PMP filenames, history formats,
card profiles, camera folders, or timestamp interpretation policies.
`plugins/pmp` implements those application policies using the public core API.
Plugins are Rust crates linked at build time, not dynamically loaded native code.
`src/main.rs` selects plugins, validates config, and merges CLI overrides.
`bin/diskproxy` keeps the established one-command start/status/stop workflow.
Keep `pmp-card` compatibility and the existing portable-card profile format.

Run `cargo test --workspace --locked`, `cargo clippy --workspace --all-targets
--locked -- -D warnings`, and the synthetic launcher tests for relevant changes.
Python helper tests can run with `uv run --python 3.13 python ...`.
Never commit local config, card profiles, logs, photo data, or personal catalog
inventory. Repository config examples must be generic.
