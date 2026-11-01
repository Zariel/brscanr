#!/usr/bin/env python3
"""Exercise the built service through HTTP, UDP, TCP and its persistent volume.

All network traffic stays on loopback. No physical printer or Paperless instance
is contacted. Run from the repository root inside the Nix development shell.
"""
import contextlib
import http.server
import io
import json
import os
from pathlib import Path
import signal
import socket
import socketserver
import sqlite3
import struct
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid

from PIL import Image


def eventually(predicate, seconds=40):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError("observation deadline exceeded")


class Printer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 54921), Acquisition)
        self.connections = 0
        self.release = threading.Event()
        self.release.set()
        self.commands = []


class Acquisition(socketserver.BaseRequestHandler):
    def handle(self):
        self.server.connections += 1
        self.request.settimeout(10)

        def command():
            data = bytearray()
            while True:
                part = self.request.recv(1)
                if not part:
                    raise EOFError()
                if part == b"\x80":
                    self.server.commands.append(bytes(data))
                    return bytes(data)
                data.extend(part)

        try:
            self.request.sendall(b"+OK 200\r\n")
            settings = command()
            self.request.sendall(b"\0SSP\0\0\0\0")
            command()
            self.server.release.wait(30)
            for page in range(1, 4 if b"PSRC=ADF\n" in settings else 2):
                output = io.BytesIO()
                Image.new("L", (64, 32), page * 50).save(output, format="JPEG")
                jpeg = bytearray(output.getvalue())
                sof = jpeg.index(b"\xff\xc0")
                jpeg[sof + 5:sof + 7] = (48).to_bytes(2, "big")
                record = b"\0\x02" + struct.pack("<HHII", page, 17, len(jpeg), 32) + jpeg
                self.request.sendall(record[:7])
                self.request.sendall(record[7:] + b"\0\x21" + struct.pack("<H", page))
            self.request.sendall(b"\0\x20")
        except (EOFError, OSError):
            pass


class Paperless(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address):
        super().__init__(address, PaperlessRequest)
        self.uploads = []
        self.ambiguous = False
        self.missing = False
        self.task_id = str(uuid.uuid4())


class PaperlessRequest(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        assert self.path == "/api/documents/post_document/"
        assert self.headers["Authorization"] == "Token test-token"
        assert self.headers["Accept"] == "application/json; version=9"
        body = self.rfile.read(int(self.headers["Content-Length"]))
        assert b'name="document"' in body and b"%PDF-" in body
        self.server.uploads.append(body)
        if self.server.ambiguous:
            self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
            return
        self.respond(self.server.task_id)

    def do_GET(self):
        assert self.path == f"/api/tasks/?task_id={self.server.task_id}"
        self.respond([] if self.server.missing else [{
            "task_id": self.server.task_id, "status": "SUCCESS", "related_document": 42,
        }])

    def respond(self, value):
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@contextlib.contextmanager
def serving(server):
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def unused_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    with tempfile.TemporaryDirectory(prefix="brscanr-workflow-") as temporary, serving(Printer()) as printer:
        root = Path(temporary)
        http_port, paperless_port, udp_port = unused_port(), unused_port(), unused_port()
        base = f"http://127.0.0.1:{http_port}"
        env = dict(os.environ, BRSCANR_PRINTER_IP="127.0.0.1",
                   BRSCANR_CALLBACK=f"127.0.0.1:{udp_port}", BRSCANR_DESTINATION="BrscanrTest",
                   BRSCANR_LISTEN=f"127.0.0.1:{http_port}", BRSCANR_SPOOL=str(root / "spool"),
                   BRSCANR_PAPERLESS_URL=f"http://127.0.0.1:{paperless_port}/",
                   BRSCANR_PAPERLESS_TOKEN="test-token", BRSCANR_MIN_FREE_BYTES="0")
        process = None
        log = (root / "service.log").open("wb")

        def request(path, data=None):
            req = urllib.request.Request(base + path, data=None if data is None else json.dumps(data).encode(),
                                         headers={"Content-Type": "application/json"})
            try:
                with urllib.request.urlopen(req, timeout=3) as response:
                    body = response.read()
                    return response.status, json.loads(body) if response.headers.get_content_type() == "application/json" else body
            except urllib.error.HTTPError as error:
                return error.code, error.read()

        def start():
            nonlocal process
            process = subprocess.Popen(["target/debug/brscanr"], env=env, stdout=log, stderr=log)

            def ready():
                assert process.poll() is None, (root / "service.log").read_text()
                try:
                    return request("/readyz")[0] == 200
                except OSError:
                    return False
            eventually(ready)

        def stop(crash=False):
            if process and process.poll() is None:
                process.send_signal(signal.SIGKILL if crash else signal.SIGTERM)
                code = process.wait(timeout=25)
                assert code == (-signal.SIGKILL if crash else 0), (root / "service.log").read_text()

        def job(identifier):
            return request(f"/api/jobs/{identifier}")[1]

        def wait_state(identifier, expected):
            return eventually(lambda: (j if (j := job(identifier))["state"] == expected else None))

        def scan(source="feeder", key=None):
            return request("/api/scans", {"source": source, "idempotency_key": key or str(uuid.uuid4())})

        def button(seq):
            text = f'TYPE=BR;BUTTON=SCAN;USER="BrscanrTest";FUNC=FILE;HOST=127.0.0.1:{udp_port};APPNUM=5;REGID=100;SEQ={seq};'
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
                sock.sendto(b"\x02\0\x76\x30" + text.encode(), ("127.0.0.1", udp_port))

        try:
            start()
            # Registration failure must not hide diagnostics or prevent remote scanning.
            assert request("/livez")[0] == 200
            printer.release.clear()
            key = str(uuid.uuid4())
            status, first = scan(key=key)
            assert status == 202
            eventually(lambda: printer.connections == 1)
            busy_key = str(uuid.uuid4())
            assert scan(key=busy_key)[0] == 409
            button(1)  # A busy physical request must remain rejected after restart.
            time.sleep(0.3)
            assert scan(key=key)[1]["id"] == first["id"]
            printer.release.set()
            wait_state(first["id"], "ready")
            pdf = root / "spool" / first["id"] / "document.pdf"
            info = subprocess.check_output(["pdfinfo", str(pdf)], text=True)
            assert "Pages:           3" in info, info
            rendered = subprocess.run(["pdftoppm", "-scale-to", "128", "-png", str(pdf), str(root / "page")], capture_output=True, check=True)
            assert not rendered.stderr, rendered.stderr
            previews = sorted(root.glob("page-*.png"))
            assert len(previews) == 3
            for index, preview in enumerate(previews, 1):
                with Image.open(preview) as image:
                    assert abs(image.convert("L").getpixel((20, 20)) - index * 50) < 3
            assert scan(key=busy_key)[0] == 409
            stop(crash=True)
            start()
            button(1)
            assert scan(key=key)[1]["id"] == first["id"]
            time.sleep(0.5)
            assert printer.connections == 1
            with serving(Paperless(("127.0.0.1", paperless_port))) as paperless:
                imported = wait_state(first["id"], "imported")
                assert imported["document_id"] == 42
                assert len(paperless.uploads) == 1
                assert request(f'/jobs/{first["id"]}/document.pdf')[1] == pdf.read_bytes()
                # Unknown HTTP outcomes must never be retried on their own, even after restart.
                paperless.ambiguous = True
                second = scan("platen")[1]
                wait_state(second["id"], "unknown")
                stop(crash=True)
                start()
                time.sleep(6)
                assert job(second["id"])["state"] == "unknown"
                assert len(paperless.uploads) == 2
                assert printer.connections == 2
                # An interrupted acquisition is never resumed, and blocks future paper movement.
                printer.release.clear()
                third = scan()[1]
                eventually(lambda: printer.connections == 3)
                stop(crash=True)
                printer.release.set()
                start()
                assert job(third["id"])["state"] == "interrupted"
                assert scan()[0] == 409
                assert request("/api/status")[1]["scanner_uncertain"]
                # Independently inspect durable state, rather than accepting HTTP status alone.
                with sqlite3.connect(root / "spool" / "jobs.sqlite") as database:
                    assert database.execute("SELECT state FROM jobs WHERE id=?", (second["id"],)).fetchone() == ("unknown",)
                    assert database.execute("SELECT count(*) FROM jobs").fetchone() == (3,)
                metrics = request("/metrics")[1].decode()
                assert "process_open_fds" in metrics
                assert "brscanr_undelivered_jobs" in metrics
            stop()
            print("Workflow passed: PDF rendering, busy/duplicate admission, outage recovery, API import, ambiguous upload and crash recovery.")
        finally:
            if process and process.poll() is None:
                process.kill()
                process.wait()
            log.close()


if __name__ == "__main__":
    main()
