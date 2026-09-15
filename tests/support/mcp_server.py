#!/usr/bin/env python3
"""A fake MCP server for agt's end-to-end tests.

    mcp_server.py stdio modern|legacy   speaks over its standard streams
    mcp_server.py http modern|legacy    prints its address, then serves it

Its tools count their calls, return an image or an error, and echo their
text with the headers they were sent. Each checks what its era requires of
clients: a modern server the metadata and the headers mirroring it, a legacy
one the handshake and its session. A legacy HTTP server forgets the session
once it has listed its tools, as a restarted server would.
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODERN = "2026-07-28"
LEGACY = "2025-06-18"
INFO = {"name": "fake", "title": "Fake Server", "version": "1.0.0"}
INSTRUCTIONS = "Tools for testing agt.\n\nCount keeps its state while the server runs."
PIXEL = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
TOOLS = [
    {"name": "count", "description": "Counts its calls.", "inputSchema": {"type": "object"}},
    {
        "name": "echo",
        "description": "Returns its text and the headers it was sent.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "region": {"type": "string", "x-mcp-header": "Region"},
            },
            "required": ["text"],
        },
    },
    {"name": "fail", "description": "Always fails.", "inputSchema": {"type": "object"}},
    {"name": "image", "description": "Returns a pixel.", "inputSchema": {"type": "object"}},
]

calls = 0


def error(code, message):
    return {"error": {"code": code, "message": message}}


def text(body, failed=False):
    return {"result": {"content": [{"type": "text", "text": body}], "isError": failed}}


def handle(era, message, headers):
    """The reply to a request: a result or an error."""
    global calls
    method, params = message["method"], message.get("params") or {}
    meta = params.get("_meta", {}).get("io.modelcontextprotocol/protocolVersion")
    if era == "modern" and meta != MODERN:
        return error(-32602, "every request must carry its protocol version")
    if method == "server/discover" and era == "modern":
        return {
            "result": {
                "supportedVersions": [MODERN],
                "capabilities": {"tools": {}},
                "instructions": INSTRUCTIONS,
                "_meta": {"io.modelcontextprotocol/serverInfo": INFO},
            }
        }
    if method == "initialize" and era == "legacy":
        return {
            "result": {
                "protocolVersion": LEGACY,
                "capabilities": {"tools": {}},
                "serverInfo": INFO,
                "instructions": INSTRUCTIONS,
            }
        }
    if method == "tools/list":
        return {"result": {"tools": TOOLS}}
    if method == "tools/call":
        name, arguments = params["name"], params.get("arguments", {})
        if name == "count":
            calls += 1
            return text(f"count {calls}")
        if name == "echo":
            lines = [arguments["text"]] + [f"{key}: {value}" for key, value in sorted(headers.items())]
            return text("\n".join(lines))
        if name == "fail":
            return text("it failed", failed=True)
        if name == "image":
            image = {"type": "image", "data": PIXEL, "mimeType": "image/png"}
            return {"result": {"content": [{"type": "text", "text": "a pixel"}, image]}}
        return error(-32602, f"unknown tool {name}")
    return error(-32601, f"method not found: {method}")


def stdio(era):
    initialized = era == "modern"
    for line in sys.stdin:
        message = json.loads(line)
        if "method" not in message:
            continue
        if "id" not in message:
            initialized = initialized or message["method"] == "notifications/initialized"
            continue
        if initialized or message["method"] in ("initialize", "server/discover"):
            reply = handle(era, message, {})
        else:
            reply = error(-32002, "the server is not initialized")
        print(json.dumps({"jsonrpc": "2.0", "id": message["id"], **reply}), flush=True)


class Handler(BaseHTTPRequestHandler):
    sessions = set()
    started = 0

    def do_POST(self):
        message = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        headers = {
            name.lower(): value
            for name, value in self.headers.items()
            if name.lower() == "authorization" or name.lower().startswith("mcp-")
        }
        if self.server.era == "modern":
            self.modern(message, headers)
        else:
            self.legacy(message, headers)

    def modern(self, message, headers):
        if "id" not in message:
            return self.send(202)
        params = message.get("params") or {}
        expected = {"mcp-protocol-version": MODERN, "mcp-method": message["method"]}
        if message["method"] == "tools/call":
            expected["mcp-name"] = params["name"]
            if "region" in params.get("arguments", {}):
                expected["mcp-param-region"] = params["arguments"]["region"]
        for name, value in expected.items():
            if headers.get(name) != value:
                reply = error(-32020, f"{name} does not match the request")
                return self.send(400, {"jsonrpc": "2.0", "id": message["id"], **reply})
        self.send(200, {"jsonrpc": "2.0", "id": message["id"], **handle("modern", message, headers)})

    def legacy(self, message, headers):
        session = headers.get("mcp-session-id")
        if message.get("method") == "initialize":
            Handler.started += 1
            session = f"session-{Handler.started}"
            Handler.sessions.add(session)
            reply = {"jsonrpc": "2.0", "id": message["id"], **handle("legacy", message, headers)}
            return self.send(200, reply, events=True, session=session)
        if session is None:
            return self.send(400, {"jsonrpc": "2.0", "id": None, **error(-32000, "no valid session")})
        if session not in Handler.sessions:
            return self.send(404)
        if "id" not in message:
            return self.send(202)
        reply = {"jsonrpc": "2.0", "id": message["id"], **handle("legacy", message, headers)}
        if message["method"] == "tools/list":
            Handler.sessions.discard(session)
        self.send(200, reply, events=True)

    def do_DELETE(self):
        Handler.sessions.discard(self.headers.get("Mcp-Session-Id"))
        self.send(200)

    def send(self, status, body=None, events=False, session=None):
        data = b"" if body is None else json.dumps(body).encode()
        if events:
            data = b"event: message\ndata: " + data + b"\n\n"
        self.send_response(status)
        self.send_header("Content-Type", "text/event-stream" if events else "application/json")
        self.send_header("Content-Length", str(len(data)))
        if session:
            self.send_header("Mcp-Session-Id", session)
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


def http(era):
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.era = era
    print(f"http://127.0.0.1:{server.server_address[1]}/mcp", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    transport, era = sys.argv[1:3]
    {"stdio": stdio, "http": http}[transport](era)
