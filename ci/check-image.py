#!/usr/bin/env python3
"""Check the runtime image and exercise the exact static executable it ships."""
import io
import json
import os
from pathlib import Path
import socket
import subprocess
import tarfile
import tempfile
import time
import urllib.request
import uuid


def docker(*args, **kwargs):
    return subprocess.check_output(["docker", *args], text=True, **kwargs).strip()


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def eventually(predicate):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            result = predicate()
            if result:
                return result
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.1)
    raise AssertionError("container observation deadline exceeded")


def main():
    image = os.environ.get("BRSCANR_TEST_IMAGE", "brscanr:ci")
    config = json.loads(docker("image", "inspect", image))[0]["Config"]
    assert config["User"] == "65532:65532"
    assert config["Entrypoint"] == ["/brscanr"]
    assert set(config["ExposedPorts"]) == {"8080/tcp", "54925/udp"}
    identity = f"brscanr-check-{uuid.uuid4().hex[:12]}"
    volume = identity + "-spool"
    http, callback = port(), port()
    base = f"http://127.0.0.1:{http}"

    def request(path, data=None):
        req = urllib.request.Request(base + path, data=None if data is None else json.dumps(data).encode(),
                                     headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=3) as response:
            body = response.read()
            return json.loads(body) if response.headers.get_content_type() == "application/json" else body

    docker("volume", "create", volume)
    try:
        docker("create", "--name", identity, "--read-only", "--network", "host",
               "--mount", f"type=volume,source={volume},target=/var/lib/brscanr",
               "-e", "BRSCANR_PRINTER_IP=127.0.0.1", "-e", f"BRSCANR_CALLBACK=127.0.0.1:{callback}",
               "-e", f"BRSCANR_LISTEN=0.0.0.0:{http}", "-e", "BRSCANR_PAPERLESS_URL=http://127.0.0.1:9/",
               "-e", "BRSCANR_PAPERLESS_TOKEN=test-token", "-e", "BRSCANR_MIN_FREE_BYTES=0", image)
        with tempfile.TemporaryDirectory(prefix="brscanr-image-") as temporary:
            directory = Path(temporary)
            binary = directory / "brscanr"
            docker("cp", f"{identity}:/brscanr", str(binary))
            # Inspect the image's root filesystem independently of its entrypoint.
            exported = subprocess.check_output(["docker", "export", identity])
            with tarfile.open(fileobj=io.BytesIO(exported)) as archive:
                files = {m.name.lstrip("./") for m in archive if m.isfile()}
                assert not files.intersection({"bin/sh", "bin/bash", "usr/bin/apt", "sbin/apk", "out/program-headers"})
                assert not any("cargo" in name or name.endswith(".so") or ".so." in name for name in files)
                programs = [m.name.lstrip("./") for m in archive if m.isfile()
                            and archive.extractfile(m).read(4) == b"\x7fELF"]
                assert programs == ["brscanr"]
                print(f"Runtime filesystem: {len(exported)} bytes exported; no shell, package manager, build tools or shared libraries.")
            docker("start", identity)
            eventually(lambda: request("/readyz") is not None)
            job = request("/api/scans", {"source": "platen", "idempotency_key": str(uuid.uuid4())})
            eventually(lambda: request(f'/api/jobs/{job["id"]}')["state"] == "interrupted")
            docker("stop", "--time", "25", identity)
            docker("start", identity)
            eventually(lambda: request("/readyz") is not None)
            assert request(f'/api/jobs/{job["id"]}')["state"] == "interrupted"
            assert request("/api/status")["scanner_uncertain"]
            docker("stop", "--time", "25", identity)
            subprocess.run(["python", "tests/workflow.py"], check=True,
                           env=dict(os.environ, BRSCANR_TEST_BINARY=str(binary)))
    finally:
        subprocess.run(["docker", "rm", "--force", identity], capture_output=True)
        subprocess.run(["docker", "volume", "rm", volume], capture_output=True, check=True)
    print("Image checks passed: nonroot read-only startup, persistent restart, static executable, and full recovery workflow.")


if __name__ == "__main__":
    main()
