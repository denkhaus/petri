"""A tiny HTTP service with no shell interpolation."""

import shlex
import subprocess
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

ALLOWED = {"uptime", "whoami"}


class Handler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        query = parse_qs(urlsplit(self.path).query)
        command = (query.get("command") or ["uptime"])[0]
        if command not in ALLOWED:
            self.send_response(400)
            self.end_headers()
            return
        output = subprocess.run(shlex.split(command), capture_output=True, check=False)
        self.send_response(200)
        self.end_headers()
        self.wfile.write(output.stdout)


def serve() -> None:
    ThreadingHTTPServer(("127.0.0.1", 8080), Handler).serve_forever()
