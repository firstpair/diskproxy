"""Synthetic launcher lifecycle tests; no real card or disk utility is used."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

PROJECT = Path(__file__).resolve().parents[1]


class LauncherTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="diskproxy-launcher-")
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.mount = self.root / "mounted volume"
        self.mount.mkdir()
        self.device = self.root / "device"
        self.device.touch()
        self.config = self.root / "my config.json"
        self.config.write_text('{}')
        self.write_executable("server", '''#!/bin/sh
printf '%s\\n' "$@" > "$DISKPROXY_TEST_ARGS"
printf '%s\\n' '{"url":"http://127.0.0.1:12345/token/card.img"}'
exec /bin/sleep 120
''')
        attachment = json.dumps({"system-entities": [{"dev-entry": str(self.device), "mount-point": str(self.mount)}]})
        self.write_executable("hdiutil", f'''#!/bin/sh
if [ "$1" = attach ]; then
cat <<'JSON'
{attachment}
JSON
else
[ ! -f "{self.root / 'busy'}" ] || exit 1
rm "{self.device}"
fi
''')
        self.write_executable("diskutil", '''#!/bin/sh
printf '%s\\n' '{"VolumeUUID":"SYNTHETIC-VOLUME-ID"}'
''')
        self.env = dict(os.environ, PATH=str(self.bin) + ":" + os.environ["PATH"],
                        DISKPROXY_SERVER=str(self.bin / "server"),
                        DISKPROXY_STATE_DIR=str(self.root / "state"),
                        DISKPROXY_CONFIG=str(self.root / "no-default-config.json"),
                        DISKPROXY_TEST_ARGS=str(self.root / "arguments"))

    def write_executable(self, name, contents):
        path = self.bin / name
        path.write_text(contents)
        path.chmod(0o755)

    def run_launcher(self, *args):
        return subprocess.run([str(PROJECT / "bin/diskproxy"), *args], env=self.env,
                              capture_output=True, text=True, timeout=30)

    def session(self, name="my config"):
        return self.root / "state/sessions" / name

    def tearDown(self):
        for pid_file in self.root.glob("state/**/pid"):
            try:
                os.kill(int(pid_file.read_text()), 15)
            except (ProcessLookupError, ValueError):
                pass
        self.temp.cleanup()

    def test_explicit_start_config_before_command_and_clean_stop(self):
        started = self.run_launcher("--config", str(self.config), "start", "/source with spaces",
                                    "--set", "plugins.pmp.hide_ingested=true")
        self.assertEqual(started.returncode, 0, started.stderr)
        self.assertIn(str(self.mount), started.stdout)
        args = (self.root / "arguments").read_text().splitlines()
        self.assertIn("/source with spaces", args)
        self.assertIn("plugins.pmp.hide_ingested=true", args)
        self.assertEqual(args[:3], ["serve", "--config", str(self.config)])
        self.assertIn("my config: proxy running", self.run_launcher("status").stdout)
        again = self.run_launcher("--config", str(self.config), "start")
        self.assertNotEqual(again.returncode, 0)
        self.assertIn("already running", again.stderr)
        stopped = self.run_launcher("stop")
        self.assertEqual(stopped.returncode, 0, stopped.stderr)
        self.assertFalse(self.session().exists())

    def test_plain_launch_config_after_command_and_busy_detach(self):
        started = self.run_launcher("--config", str(self.config))
        self.assertEqual(started.returncode, 0, started.stderr)
        (self.root / "busy").touch()
        failed = self.run_launcher("stop")
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("left running", failed.stderr)
        self.assertIn("proxy running", self.run_launcher("status").stdout)
        (self.root / "busy").unlink()
        self.assertEqual(self.run_launcher("stop").returncode, 0)

    def test_config_after_start_and_identity_mismatch_keep_server(self):
        started = self.run_launcher("start", "--config", str(self.config))
        self.assertEqual(started.returncode, 0, started.stderr)
        (self.session() / "volume-id").write_text("DIFFERENT-ID\n")
        self.assertNotEqual(self.run_launcher("stop").returncode, 0)
        self.assertIn("proxy running", self.run_launcher("status").stdout)
        self.assertTrue(self.device.exists())

    def test_new_card_prints_only_the_default_registration_command(self):
        self.write_executable("server", "#!/bin/sh\nexit 78\n")
        self.env["DISKPROXY_CONFIG"] = str(self.config)
        started = self.run_launcher("start")
        self.assertEqual(started.returncode, 78)
        self.assertEqual(started.stderr, "Run diskproxy-server.\n")
        self.assertEqual(started.stdout, "")
        self.assertEqual(list(self.root.glob("state/sessions/*")), [])

    def test_equivalent_pmp_alias_config_needs_no_extra_options(self):
        self.write_executable("server", "#!/bin/sh\nexit 78\n")
        self.env["DISKPROXY_CONFIG"] = str(self.config)
        equivalent = self.root / "pmp.json"
        equivalent.write_bytes(self.config.read_bytes())
        started = self.run_launcher("--config", str(equivalent), "start")
        self.assertEqual(started.returncode, 78)
        self.assertEqual(started.stderr, "Run diskproxy-server.\n")

    def test_new_card_preserves_a_custom_source_in_registration_advice(self):
        self.write_executable("server", "#!/bin/sh\nexit 78\n")
        self.env["DISKPROXY_CONFIG"] = str(self.config)
        started = self.run_launcher("start", "/another card")
        self.assertEqual(started.returncode, 78)
        self.assertIn("Run diskproxy-server", started.stderr)
        self.assertIn("another", started.stderr)
        self.assertNotIn("--source-offset", started.stderr)
        self.assertEqual(list(self.root.glob("state/sessions/*")), [])

    def test_unexpected_startup_error_prints_diagnostics_and_options(self):
        self.write_executable("server", "#!/bin/sh\necho 'invalid config' >&2\nexit 1\n")
        started = self.run_launcher("start", "--config", str(self.config))
        self.assertNotEqual(started.returncode, 0)
        self.assertIn("invalid config", started.stderr)
        self.assertIn("Options:", started.stderr)

    def test_incompatible_lifecycle_flags_fail_before_server(self):
        for arg in ("--register", "--check", "--mode=webdav"):
            self.assertNotEqual(self.run_launcher("start", arg).returncode, 0)
        self.assertEqual(list(self.root.glob("state/sessions/*")), [])


    def test_two_configs_run_at_once_and_stop_needs_a_choice(self):
        other = self.root / "photocat.json"
        other.write_text("{}")
        for config in (self.config, other):
            started = self.run_launcher("--config", str(config), "start")
            self.assertEqual(started.returncode, 0, started.stderr)
        self.assertIn(f"diskproxy --config {other} stop", started.stdout.replace("\\", ""))
        status = self.run_launcher("status").stdout
        self.assertIn("my config: proxy running", status)
        self.assertIn("photocat: proxy running", status)
        refused = self.run_launcher("stop")
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("Choose one", refused.stderr)
        self.assertTrue(self.session().exists() and self.session("photocat").exists())
        self.assertEqual(self.run_launcher("stop", "--config", str(other)).returncode, 0)
        self.assertFalse(self.session("photocat").exists())
        self.assertIn("my config: proxy running", self.run_launcher("status").stdout)
        self.assertEqual(self.run_launcher("--config", str(self.config), "stop").returncode, 0)
        self.assertEqual(self.run_launcher("status").stdout, "No managed proxy is running.\n")

    def test_a_session_from_the_older_launcher_is_still_stopped(self):
        self.env["DISKPROXY_CONFIG"] = str(self.config)
        started = self.run_launcher("start")
        self.assertEqual(started.returncode, 0, started.stderr)
        # Where the older launcher kept its single session.
        self.session().rename(self.root / "state/session")
        self.assertIn("default: proxy running", self.run_launcher("status").stdout)
        again = self.run_launcher("start")
        self.assertNotEqual(again.returncode, 0, "the default config's view is the legacy one")
        stopped = self.run_launcher("stop")
        self.assertEqual(stopped.returncode, 0, stopped.stderr)
        self.assertFalse((self.root / "state/session").exists())
        self.assertFalse(self.device.exists(), "its disk was detached")


if __name__ == "__main__":
    unittest.main()
