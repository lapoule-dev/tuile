# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""A static server for the page that also takes the bench's results: `POST /results/<name>.json` writes the body
to ``--out/<name>.json``. Local only.

    python3 serve.py --port 8892 --out bench-out
"""

import argparse
import http.server
import re
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument("--port", type=int, default=8892)
p.add_argument("--out", type=Path, default=Path("bench-out"))
args = p.parse_args()
args.out.mkdir(parents=True, exist_ok=True)


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {**http.server.SimpleHTTPRequestHandler.extensions_map, ".wasm": "application/wasm"}

    def do_POST(self):
        m = re.fullmatch(r"/results/([\w.-]+\.json)", self.path)
        if not m:
            self.send_error(404)
            return
        body = self.rfile.read(int(self.headers["Content-Length"]))
        (args.out / m.group(1)).write_bytes(body)
        self.send_response(204)
        self.end_headers()

    def log_message(self, *a):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()
