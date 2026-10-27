//! Portable-card profile tests. All writes are to generated synthetic cards.
use std::fs::{self, File, FileTimes};
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};

fn command(root: &Path, args: &[&str]) -> Output {
    let defaults = tempfile::NamedTempFile::new().unwrap();
    fs::write(defaults.path(), b"{}").unwrap();
    Command::new(env!("CARGO_BIN_EXE_diskproxy-server"))
        .env("DISKPROXY_CONFIG", defaults.path())
        .arg(root)
        .arg("--plugin=pmp")
        .args(args)
        .output()
        .unwrap()
}

fn success(root: &Path, args: &[&str]) -> serde_json::Value {
    let out = command(root, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn photo(root: &Path, mtime: u64) {
    fs::create_dir_all(root.join("DCIM/100LEICA")).unwrap();
    fs::write(
        root.join("DCIM/100LEICA/A.DNG"),
        b"EXIF capture time stays 01:26:03; synthetic image bytes",
    )
    .unwrap();
    timestamp(root, mtime);
}

fn timestamp(root: &Path, seconds: u64) {
    File::open(root.join("DCIM/100LEICA/A.DNG"))
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
        .unwrap();
}

#[test]
fn appledouble_files_are_not_images_or_profile_samples() {
    let root = tempfile::tempdir().unwrap();
    photo(root.path(), 1789355532);
    fs::write(root.path().join("DCIM/100LEICA/._A.DNG"), b"AppleDouble").unwrap();
    fs::write(root.path().join("DCIM/100LEICA/._A.JPG"), b"AppleDouble").unwrap();
    let checked = success(root.path(), &["--source-offset=+09:00", "--check"]);
    assert_eq!(checked["visible_images"], 1);
    success(root.path(), &["--source-offset=+09:00", "--register"]);
    let profile: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join(".pmp-ingest-proxy.json")).unwrap())
            .unwrap();
    assert_eq!(profile["samples"].as_array().unwrap().len(), 1);
    assert_eq!(profile["samples"][0]["parts"][2], "A.DNG");
}

#[test]
fn new_card_uses_utc_and_profile_travels_with_card() {
    let a = tempfile::tempdir().unwrap();
    photo(a.path(), 1789355532); // Tokyo mount
    let preview = success(a.path(), &["--source-offset=+09:00", "--check"]);
    assert_eq!(preview["target"], "+00:00");
    assert!(!a.path().join(".pmingest.dat").exists());
    assert!(!a.path().join(".pmp-ingest-proxy.json").exists());
    let registered = success(a.path(), &["--source-offset=+09:00", "--register"]);
    assert_eq!(registered["target"], "+00:00");
    let marker = fs::read(a.path().join(".pmingest.dat")).unwrap();
    let profile = fs::read(a.path().join(".pmp-ingest-proxy.json")).unwrap();
    assert_eq!(marker.len(), 38);
    // A different host/path and filesystem interpretation, without a local profile.
    let b = tempfile::tempdir().unwrap();
    photo(b.path(), 1789359132); // Shanghai mount
    fs::write(b.path().join(".pmingest.dat"), &marker).unwrap();
    fs::write(b.path().join(".pmp-ingest-proxy.json"), &profile).unwrap();
    let detected = success(b.path(), &["--check"]);
    assert_eq!(detected["source_offset"], "+08:00");
    assert_eq!(detected["source_basis"], "verified samples");
    assert_eq!(detected["target"], "+00:00");
    assert_eq!(
        fs::read(b.path().join(".pmp-ingest-proxy.json")).unwrap(),
        profile
    );
    assert_eq!(fs::read(b.path().join(".pmingest.dat")).unwrap(), marker);
}

#[test]
fn legacy_history_infers_pacific_convention_and_new_mount_is_automatic() {
    let root = tempfile::tempdir().unwrap();
    photo(root.path(), 1789355532);
    fs::write(
        root.path().join(".pmingest.dat"),
        "{FA82B2D0-03D3-e144-B987-7AB6F743C201}",
    )
    .unwrap();
    let size = fs::metadata(root.path().join("DCIM/100LEICA/A.DNG"))
        .unwrap()
        .len();
    let history = root.path().join("history");
    fs::write(
        &history,
        format!("/DCIM/100LEICA/A.DNG\t{size}\t1789413132\n"),
    )
    .unwrap();
    let before = fs::read(&history).unwrap();
    let registered = success(
        root.path(),
        &[
            "--source-offset=+09:00",
            "--history",
            history.to_str().unwrap(),
            "--register",
        ],
    );
    assert_eq!(registered["target"], "-07:00");
    assert_eq!(registered["hidden_images"], 1);
    timestamp(root.path(), 1789413132); // mounted later under Pacific
    let detected = success(root.path(), &["--check"]);
    assert_eq!(detected["source_offset"], "-07:00");
    assert_eq!(detected["target"], "-07:00");
    assert_eq!(fs::read(history).unwrap(), before);
    let wrong = command(root.path(), &["--source-offset=+09:00", "--check"]);
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("disagrees"));
}

#[test]
fn edited_samples_cannot_masquerade_as_a_timezone_change() {
    let root = tempfile::tempdir().unwrap();
    photo(root.path(), 1789355532);
    success(root.path(), &["--source-offset=+09:00", "--register"]);
    let p = root.path().join("DCIM/100LEICA/A.DNG");
    let mut bytes = fs::read(&p).unwrap();
    bytes[0] = b'x';
    fs::write(&p, bytes).unwrap();
    timestamp(root.path(), 1789355532); // even preserving mtime cannot hide a content edit
    let failed = command(root.path(), &["--check"]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("unchanged card samples"));
    success(root.path(), &["--source-offset=+09:00", "--register"]);
    assert_eq!(success(root.path(), &["--check"])["target"], "+00:00");
}

#[test]
fn unregistered_card_requires_known_mount_offset_and_never_guesses_from_exif() {
    let root = tempfile::tempdir().unwrap();
    photo(root.path(), 1789355532);
    let result = command(root.path(), &["--check"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("first registration"));
    assert!(!root.path().join(".pmingest.dat").exists());
    assert!(!root.path().join(".pmp-ingest-proxy.json").exists());
}

#[test]
fn bare_server_registers_with_default_config_and_current_zone_once() {
    let temp = tempfile::tempdir().unwrap();
    let card = temp.path().join("card");
    photo(&card, 1789355532);
    let config = temp.path().join("default.json");
    fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({"plugin":"pmp", "source":card})).unwrap(),
    )
    .unwrap();
    let invoke = || {
        Command::new(env!("CARGO_BIN_EXE_diskproxy-server"))
            .env("DISKPROXY_CONFIG", &config)
            .env("TZ", "Asia/Tokyo")
            .output()
            .unwrap()
    };
    let first = invoke();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(info["source_offset"], "+09:00");
    assert_eq!(info["source_basis"], "current timezone");
    assert_eq!(info["target"], "+00:00");
    assert_eq!(info["profile_saved"], true);
    assert!(info.get("url").is_none());
    let profile = fs::read(card.join(".pmp-ingest-proxy.json")).unwrap();
    let marker = fs::read(card.join(".pmingest.dat")).unwrap();
    let second = invoke();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(info["profile_saved"], false);
    assert_eq!(info["source_basis"], "verified samples");
    assert_eq!(
        fs::read(card.join(".pmp-ingest-proxy.json")).unwrap(),
        profile
    );
    assert_eq!(fs::read(card.join(".pmingest.dat")).unwrap(), marker);
}

#[test]
fn serving_a_new_card_requests_registration_without_writing() {
    let root = tempfile::tempdir().unwrap();
    photo(root.path(), 1789355532);
    let out = Command::new(env!("CARGO_BIN_EXE_diskproxy-server"))
        .args(["serve", "--plugin=pmp", "--source-offset=+09:00"])
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
    assert!(!root.path().join(".pmingest.dat").exists());
    assert!(!root.path().join(".pmp-ingest-proxy.json").exists());
}

#[test]
fn invalid_mount_offset_is_diagnosed_before_registration_advice() {
    let root = tempfile::tempdir().unwrap();
    photo(root.path(), 1789355532);
    let out = Command::new(env!("CARGO_BIN_EXE_diskproxy-server"))
        .args(["serve", "--plugin=pmp", "--source-offset=wrong"])
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("offset must be signed"));
    assert!(!root.path().join(".pmingest.dat").exists());
}
