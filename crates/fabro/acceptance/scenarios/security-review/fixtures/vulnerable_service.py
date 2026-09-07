"""Deliberately vulnerable network service for workflow verification."""

import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit


class ReportRequestHandler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        parameters = parse_qs(urlsplit(self.path).query)
        command = parameters["command"][0]
        os.system(command)
        self.send_response(204)
        self.end_headers()


def serve() -> None:
    server = ThreadingHTTPServer(("0.0.0.0", 8080), ReportRequestHandler)
    server.serve_forever()


if __name__ == "__main__":
    serve()
