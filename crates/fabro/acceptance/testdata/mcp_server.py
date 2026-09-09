#!/usr/bin/env python3
"""A scripted MCP server for Petri's tests: deterministic, dependency-free.

Speaks JSON-RPC 2.0 over stdin and stdout (one message per line) as the MCP
specification describes, or streamable HTTP with ``--http PORT``. The tools
it exposes let a test observe an actual workspace effect, a result the
server marks as an error, a slow call, and a server that exits mid-session:

- ``write_file(path, content)``: writes ``content`` to ``path`` (relative to
  the server's working directory) and answers ``wrote N bytes to PATH``.
- ``read_file(path)``: answers the file's text.
- ``echo(message)``: answers the message; ``__cwd__`` answers the working
  directory, ``__env:NAME__`` the variable's value, ``__pid__`` the pid.
- ``fail(message)``: answers the message with ``isError`` set.
- ``sleep(ms, marker=None)``: waits, then answers ``slept MS ms``; with
  ``marker``, touches that file first, so a test can act once the call is
  in flight rather than guess when it is.
- ``crash()``: exits the process at once without answering.

``--tag TEXT`` is ignored; a test passes a per-case path so a leaked server
is attributable by its command line. ``--fail-init`` exits with status 3
before reading a request. ``--slow-init MS`` delays the ``initialize``
answer. ``MCP_TEST_LOG`` names a file every lifecycle step is appended to:
``started``, ``initialize``, ``call NAME``, ``shutdown`` (on EOF).
"""

import argparse
import json
import os
import socketserver
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

SERVER_INFO = {"name": "petri-test-mcp", "version": "1.0.0"}
PROTOCOL_VERSION = "2025-03-26"

TOOLS = [
    {
        "name": "write_file",
        "description": "Write content to a file in the working directory",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
            },
            "required": ["path", "content"],
        },
    },
    {
        "name": "read_file",
        "description": "Read a file in the working directory",
        "inputSchema": {
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        },
    },
    {
        "name": "echo",
        "description": "Echo back the message",
        "inputSchema": {
            "type": "object",
            "properties": {"message": {"type": "string"}},
            "required": ["message"],
        },
    },
    {
        "name": "fail",
        "description": "Answer with an error result",
        "inputSchema": {
            "type": "object",
            "properties": {"message": {"type": "string"}},
            "required": ["message"],
        },
    },
    {
        "name": "sleep",
        "description": "Wait for a number of milliseconds",
        "inputSchema": {
            "type": "object",
            "properties": {"ms": {"type": "integer"}, "marker": {"type": "string"}},
            "required": ["ms"],
        },
    },
    {
        "name": "crash",
        "description": "Exit the server process without answering",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


def log(line):
    path = os.environ.get("MCP_TEST_LOG")
    if not path:
        return
    with open(path, "a", encoding="utf-8") as handle:
        handle.write(line + "\n")


def trace(line):
    """A start-up trace for the test harness, written before ``started``:
    which interpreter runs, and each step up to the bound socket. Only when
    ``MCP_TEST_TRACE`` names a file."""
    path = os.environ.get("MCP_TEST_TRACE")
    if not path:
        return
    with open(path, "a", encoding="utf-8") as handle:
        handle.write(f"{time.time():.3f} pid={os.getpid()} {line}\n")


def text_result(text, is_error=False):
    result = {"content": [{"type": "text", "text": text}]}
    if is_error:
        result["isError"] = True
    return result


def call_tool(name, arguments):
    log(f"call {name}")
    if name == "write_file":
        path = arguments["path"]
        content = arguments["content"]
        directory = os.path.dirname(path)
        if directory:
            os.makedirs(directory, exist_ok=True)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(content)
        return text_result(f"wrote {len(content.encode('utf-8'))} bytes to {path}")
    if name == "read_file":
        with open(arguments["path"], "r", encoding="utf-8") as handle:
            return text_result(handle.read())
    if name == "echo":
        message = arguments.get("message", "")
        if message == "__cwd__":
            message = os.getcwd()
        elif message == "__pid__":
            message = str(os.getpid())
        elif message.startswith("__env:") and message.endswith("__"):
            message = os.environ.get(message[len("__env:") : -len("__")], "")
        return text_result(message)
    if name == "fail":
        return text_result(arguments.get("message", "failed"), is_error=True)
    if name == "sleep":
        ms = int(arguments.get("ms", 0))
        marker = arguments.get("marker")
        if marker:
            with open(marker, "w", encoding="utf-8"):
                pass
        time.sleep(ms / 1000)
        return text_result(f"slept {ms} ms")
    if name == "crash":
        log("crash")
        sys.stdout.flush()
        os._exit(1)
    return text_result(f"unknown tool: {name}", is_error=True)


class Server:
    def __init__(self, options):
        self.options = options

    def handle(self, request):
        """Answer one request, or ``None`` for a notification."""
        method = request.get("method")
        request_id = request.get("id")
        params = request.get("params") or {}
        if method == "initialize":
            log("initialize")
            if self.options.slow_init:
                time.sleep(self.options.slow_init / 1000)
            requested = params.get("protocolVersion", PROTOCOL_VERSION)
            return {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "protocolVersion": requested,
                    "capabilities": {"tools": {}},
                    "serverInfo": SERVER_INFO,
                },
            }
        if method == "ping":
            return {"jsonrpc": "2.0", "id": request_id, "result": {}}
        if method == "tools/list":
            return {"jsonrpc": "2.0", "id": request_id, "result": {"tools": TOOLS}}
        if method == "tools/call":
            try:
                result = call_tool(params.get("name"), params.get("arguments") or {})
            except Exception as error:  # noqa: BLE001 - the model reads the error
                result = text_result(f"{type(error).__name__}: {error}", is_error=True)
            return {"jsonrpc": "2.0", "id": request_id, "result": result}
        if request_id is None:
            if method == "notifications/cancelled":
                log("cancelled")
            return None
        return {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32601, "message": f"Method not found: {method}"},
        }


def serve_stdio(server):
    log("started")
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except json.JSONDecodeError:
            continue
        response = server.handle(request)
        if response is not None:
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()
    log("shutdown")


def serve_http(server, port):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def do_POST(self):  # noqa: N802 - http.server's naming
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length)
            try:
                request = json.loads(body)
            except json.JSONDecodeError:
                self.send_response(400)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            requests = request if isinstance(request, list) else [request]
            responses = [
                response
                for response in (server.handle(item) for item in requests)
                if response is not None
            ]
            if not responses:
                self.send_response(202)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            payload = responses[0] if not isinstance(request, list) else responses
            data = json.dumps(payload).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):  # noqa: N802
            self.send_response(405)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def do_DELETE(self):  # noqa: N802
            log("session-deleted")
            self.send_response(200)
            self.send_header("Content-Length", "0")
            self.end_headers()

    class Server(HTTPServer):
        def server_bind(self):
            # The stock server_bind resolves the bound address to a fully
            # qualified name with a reverse DNS lookup. On a GitHub macOS
            # runner that lookup for 127.0.0.1 can outlast the test's
            # startup window, so the server never answers. The name is only
            # used in error pages; a loopback test server does not need it.
            socketserver.TCPServer.server_bind(self)
            host, port = self.server_address[:2]
            self.server_name = host
            self.server_port = port

    trace(f"binding 127.0.0.1:{port} under {sys.executable} {sys.version.split()[0]}")
    httpd = Server(("127.0.0.1", port), Handler)
    trace(f"bound {httpd.server_address}")
    log("started")
    try:
        httpd.serve_forever()
    finally:
        log("shutdown")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--tag", default="")
    parser.add_argument("--http", type=int, default=0)
    parser.add_argument("--fail-init", action="store_true")
    parser.add_argument("--slow-init", type=int, default=0)
    options = parser.parse_args()
    trace(f"main argv={sys.argv[1:]} cwd={os.getcwd()}")
    if options.fail_init:
        sys.stderr.write("refusing to start: --fail-init\n")
        sys.stderr.flush()
        sys.exit(3)
    server = Server(options)
    if options.http:
        serve_http(server, options.http)
    else:
        serve_stdio(server)


if __name__ == "__main__":
    main()
