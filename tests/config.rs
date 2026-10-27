use std::fs::{self, File, FileTimes};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

fn run(args: &[&str]) -> std::process::Output {
    let defaults = tempfile::NamedTempFile::new().unwrap();
    fs::write(defaults.path(), b"{}").unwrap();
    Command::new(env!("CARGO_BIN_EXE_diskproxy-server"))
        .env("DISKPROXY_CONFIG", defaults.path())
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn passthrough_needs_no_card_layout_and_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("notes.txt"), b"arbitrary file").unwrap();
    let out = run(&[root.path().to_str().unwrap(), "--check"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(info["plugin"], "passthrough");
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}
#[test]
fn json_plugin_config_and_command_line_overrides_filter_history() {
    let root = tempfile::tempdir().unwrap();
    let card = root.path().join("card");
    fs::create_dir_all(card.join("DCIM")).unwrap();
    let image = card.join("DCIM/test.DNG");
    fs::write(&image, b"image").unwrap();
    File::open(image)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1789355532)))
        .unwrap();
    let history = root.path().join("history");
    fs::write(&history, "/DCIM/test.DNG\t5\t1789387932\n").unwrap();
    let config = root.path().join("config.json");
    fs::write(&config, serde_json::to_vec(&serde_json::json!({"plugin":"pmp", "source":card,
        "plugins":{"pmp":{"source_offset":"+09:00", "target_offset":"+00:00", "history":history,"hide_ingested":false}}})).unwrap()).unwrap();
    let out = run(&[
        "--config",
        config.to_str().unwrap(),
        "--set",
        "plugins.pmp.hide_ingested=true",
        "--check",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(info["hidden_images"], 1);
    assert_eq!(info["visible_images"], 0);
    assert_eq!(info["target"], "+00:00");
    assert!(!card.join(".pmingest.dat").exists());
    assert!(!card.join(".pmp-ingest-proxy.json").exists());
}
#[test]
fn explicit_flags_override_config_and_set_values() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.json");
    fs::write(
        &config,
        r#"{"source":"/does/not/exist","plugin":"pmp","mode":"webdav"}"#,
    )
    .unwrap();
    let out = run(&[
        "--config",
        config.to_str().unwrap(),
        "--set",
        "plugin=pmp",
        "--plugin",
        "passthrough",
        root.path().to_str().unwrap(),
        "--check",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(info["plugin"], "passthrough");
    assert_eq!(info["mode"], "webdav");
}
#[test]
fn unknown_settings_and_unsafe_override_shapes_fail() {
    let root = tempfile::tempdir().unwrap();
    for expr in [
        "plugins.pmp.typo=true",
        "source.child=x",
        "plugins..pmp=true",
    ] {
        let out = run(&[
            root.path().to_str().unwrap(),
            "--plugin=pmp",
            "--set",
            expr,
            "--check",
        ]);
        assert!(!out.status.success(), "{expr}");
    }
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn invalid_config_cannot_trigger_pmp_setup_writes() {
    for extra in [
        vec!["--set", "plugins.unknown={}"],
        vec!["--label", "label much too long"],
    ] {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("DCIM")).unwrap();
        let mut args = vec![
            root.path().to_str().unwrap(),
            "--plugin=pmp",
            "--source-offset=+09:00",
        ];
        args.extend(extra);
        let out = run(&args);
        assert!(!out.status.success());
        assert!(!root.path().join(".pmingest.dat").exists());
        assert!(!root.path().join(".pmp-ingest-proxy.json").exists());
    }
}
