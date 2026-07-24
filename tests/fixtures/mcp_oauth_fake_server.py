#!/usr/bin/env python3
"""Minimal OAuth-protected fake MCP HTTP server for local login experiments.

Serves:
  - RFC 9728 / 8414 discovery + DCR + authorize/token (auto-approves)
  - Streamable-HTTP MCP initialize / tools/list / tools/call

tools/list and tools/call require Authorization: Bearer <access_token>.

Note: yolop's MCP HTTP egress blocks loopback (SSRF). This fixture is for
exercising `/mcp login` (OAuth discovery uses plain reqwest) and for manual
curl checks — not for in-process tools/list through the runtime on localhost.
"""

from __future__ import annotations

import json
import secrets
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse


ACCESS_TOKEN = "fake-access-token"
CLIENTS: dict[str, str] = {}
CODES: dict[str, dict] = {}


TOOLS = [
    {
        "name": "ping",
        "description": "Return pong.",
        "inputSchema": {"type": "object", "properties": {}},
    }
]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt: str, *args) -> None:  # quieter
        print(f"[fake-mcp] {self.command} {self.path} -> " + (fmt % args))

    def _read_json(self):
        length = int(self.headers.get("Content-Length", "0") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            return json.loads(raw.decode() or "{}")
        except json.JSONDecodeError:
            return {}

    def _send(self, status: int, body: bytes, content_type: str = "application/json"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _send_json(self, status: int, obj):
        self._send(status, json.dumps(obj).encode(), "application/json")

    def do_GET(self):
        parsed = urlparse(self.path)
        path = parsed.path
        base = f"http://127.0.0.1:{self.server.server_address[1]}"

        if path == "/.well-known/oauth-protected-resource":
            return self._send_json(200, {"authorization_servers": [base]})
        if path == "/.well-known/oauth-authorization-server":
            return self._send_json(
                200,
                {
                    "authorization_endpoint": f"{base}/authorize",
                    "token_endpoint": f"{base}/token",
                    "registration_endpoint": f"{base}/register",
                    "scopes_supported": ["read"],
                },
            )
        if path == "/authorize":
            qs = parse_qs(parsed.query)
            redirect = qs.get("redirect_uri", [""])[0]
            state = qs.get("state", [""])[0]
            code = secrets.token_urlsafe(16)
            CODES[code] = {
                "client_id": qs.get("client_id", [""])[0],
                "redirect_uri": redirect,
            }
            # Auto-approve: redirect immediately (no HTML UI).
            loc = f"{redirect}?code={code}&state={state}"
            self.send_response(302)
            self.send_header("Location", loc)
            self.end_headers()
            return
        if path in ("/mcp", "/"):
            return self._send_json(401, {"error": "unauthorized"})
        return self._send(404, b"not found", "text/plain")

    def do_POST(self):
        parsed = urlparse(self.path)
        path = parsed.path
        length = int(self.headers.get("Content-Length", "0") or 0)
        raw = self.rfile.read(length) if length else b""
        ctype = self.headers.get("Content-Type", "")
        if "application/json" in ctype:
            try:
                body = json.loads(raw.decode() or "{}")
            except json.JSONDecodeError:
                body = {}
        else:
            body = {}

        if path == "/register":
            client_id = "fake-client-" + secrets.token_hex(4)
            CLIENTS[client_id] = "public"
            return self._send_json(200, {"client_id": client_id})

        if path == "/token":
            return self._send_json(
                200,
                {
                    "access_token": ACCESS_TOKEN,
                    "refresh_token": "fake-refresh",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "scope": "read",
                },
            )

        if path in ("/mcp", "/"):
            auth = self.headers.get("Authorization", "")
            if auth != f"Bearer {ACCESS_TOKEN}":
                return self._send_json(401, {"error": "unauthorized"})
            method = body.get("method")
            mid = body.get("id")
            if method == "initialize":
                return self._send_json(
                    200,
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "fake-oauth-mcp", "version": "0"},
                        },
                    },
                )
            if method == "tools/list":
                return self._send_json(
                    200,
                    {"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}},
                )
            if method == "tools/call":
                return self._send_json(
                    200,
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "result": {
                            "content": [{"type": "text", "text": "pong"}],
                            "isError": False,
                        },
                    },
                )
            if mid is not None:
                return self._send_json(
                    200,
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "error": {"code": -32601, "message": "method not found"},
                    },
                )
            return self._send(204, b"")

        return self._send(404, b"not found", "text/plain")


def main():
    # Ephemeral port.
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    print(f"FAKE_MCP_URL=http://127.0.0.1:{port}/mcp", flush=True)
    print(f"ACCESS_TOKEN={ACCESS_TOKEN}", flush=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    thread.join()


if __name__ == "__main__":
    main()
