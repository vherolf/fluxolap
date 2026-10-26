"""A fluxolap server with the example databases for the tests, on a free port.

Uses target/release/fluxolap (or target/debug) and target/examples:
    cargo build --release -p fluxolap-server && cargo run --release -p xtask -- examples
"""

import json
import os
import socket
import subprocess
import time
import unittest
import urllib.request

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))


class Fluxolap:
    def __init__(self):
        binary = next((p for p in (os.path.join(ROOT, "target", b, "fluxolap") for b in ("release", "debug")) if os.path.exists(p)), None)
        examples = os.path.join(ROOT, "target", "examples")
        if binary is None or not os.path.isdir(examples):
            raise unittest.SkipTest("needs target/release/fluxolap and target/examples (see tests/server.py)")
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            self.port = s.getsockname()[1]
        env = dict(os.environ, FLUXOLAP_PORT=str(self.port), FLUXOLAP_EXAMPLES=examples, FLUXOLAP_UI_DIR="")
        env.pop("FLUXOLAP_DATA_DIR", None)
        self.process = subprocess.Popen([binary], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.address = f"localhost:{self.port}"
        for _ in range(200):
            try:
                self.get("")
                return
            except OSError:
                time.sleep(0.05)
        self.stop()
        raise RuntimeError("fluxolap didn't start")

    def get(self, path):
        with urllib.request.urlopen(f"http://{self.address}/api/v1{path}", timeout=10) as r:
            return json.load(r)

    def requests(self):
        return self.get("/stats")["requests"]["total"]

    def stop(self):
        self.process.terminate()
        self.process.wait(10)
