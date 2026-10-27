#!/usr/bin/env python3
"""Apply the documented integration patch in isolation and exercise its HTTP policy."""
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import urllib.error
import urllib.request

PROJECT = Path(__file__).resolve().parents[1]


def main():
    with tempfile.TemporaryDirectory(prefix="diskproxy-plugin-example-") as directory:
        root = Path(directory)
        checkout = root / "checkout"
        shutil.copytree(PROJECT, checkout,
                        ignore=shutil.ignore_patterns(".git", "target", "__pycache__"))
        subprocess.run(["git", "apply", "--check", "examples/hide-plugin/register.patch"],
                       cwd=checkout, check=True)
        subprocess.run(["git", "apply", "examples/hide-plugin/register.patch"],
                       cwd=checkout, check=True)
        target = PROJECT / "target/plugin-example"
        subprocess.run(["cargo", "build", "--workspace", "--locked", "--offline",
                        "--target-dir", str(target)], cwd=checkout, check=True)
        source = root / "source"
        (source / "cache").mkdir(parents=True)
        (source / "notes.txt").write_bytes(b"keep")
        (source / "draft.tmp").write_bytes(b"hidden")
        (source / "cache/secret.txt").write_bytes(b"hidden")
        config = root / "hide.json"
        config.write_text(json.dumps({"source": str(source), "plugin": "hide",
            "plugins": {"hide": {"exclude_names": ["cache"], "exclude_suffixes": [".tmp"]}}}))
        server = subprocess.Popen([str(target / "debug/diskproxy-server"), "serve",
            "--config", str(config), "--mode", "webdav"], stdout=subprocess.PIPE, text=True)
        try:
            startup = json.loads(server.stdout.readline())
            url = startup["url"]
            with urllib.request.urlopen(url + "notes.txt", timeout=10) as response:
                assert response.read() == b"keep"
            for path in ("draft.tmp", "cache/secret.txt"):
                try:
                    urllib.request.urlopen(url + path, timeout=10)
                except urllib.error.HTTPError as error:
                    assert error.code == 404
                else:
                    raise AssertionError("hidden path was exposed: " + path)
            request = urllib.request.Request(url, method="PROPFIND", headers={"Depth": "1"})
            with urllib.request.urlopen(request, timeout=10) as response:
                listing = response.read().decode()
            assert "draft.tmp" not in listing and "/cache/" not in listing
            assert (source / "draft.tmp").read_bytes() == b"hidden"
        finally:
            server.terminate()
            server.wait(timeout=10)
            server.stdout.close()
    print("Plugin integration patch compiles; hidden listings/direct paths and source preservation verified.")


if __name__ == "__main__":
    main()
