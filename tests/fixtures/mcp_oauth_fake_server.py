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
from urllib.parse import parse_qs, urlencode, urlparse, urlunparse


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


def _header_safe(value: str) -> bool:
    """Reject values that could split HTTP response headers (CR/LF)."""
    return "\r" not in value and "\n" not in value


def _loopback_redirect(redirect: str) -> bool:
    """Only auto-approve redirects to loopback http(s) URLs."""
    try:
        parsed = urlparse(redirect)
    except ValueError:
        return False
    if parsed.scheme not in ("http", "https"):
        return False
    if parsed.hostname not in ("127.0.0.1", "localhost", "::1"):
        return False
    return _header_safe(redirect)


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
            self._send_json(200, {"authorization_servers": [base]})
            return
        if path == "/.well-known/oauth-authorization-server":
            self._send_json(
                200,
                {
                    "authorization_endpoint": f"{base}/authorize",
                    "token_endpoint": f"{base}/token",
                    "registration_endpoint": f"{base}/register",
                    "scopes_supported": ["read"],
                },
            )
            return
        if path == "/authorize":
            qs = parse_qs(parsed.query)
            redirect = qs.get("redirect_uri", [""])[0]
            state = qs.get("state", [""])[0]
            if not _loopback_redirect(redirect) or not _header_safe(state):
                self._send(400, b"invalid redirect_uri or state", "text/plain")
                return
            code = secrets.token_urlsafe(16)
            CODES[code] = {
                "client_id": qs.get("client_id", [""])[0],
                "redirect_uri": redirect,
            }
            # Auto-approve: redirect immediately (no HTML UI).
            # Build Location via urlparse so query values are encoded and cannot
            # inject header separators even if validation above is bypassed.
            redirect_parts = urlparse(redirect)
            query = urlencode({"code": code, "state": state})
            loc = urlunparse(
                (
                    redirect_parts.scheme,
                    redirect_parts.netloc,
                    redirect_parts.path,
                    redirect_parts.params,
                    query,
                    "",
                )
            )
            self.send_response(302)
            self.send_header("Location", loc)
            self.end_headers()
            return
        if path in ("/mcp", "/"):
            self._send_json(401, {"error": "unauthorized"})
            return
        self._send(404, b"not found", "text/plain")
        return

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
            self._send_json(200, {"client_id": client_id})
            return

        if path == "/token":
            self._send_json(
                200,
                {
                    "access_token": ACCESS_TOKEN,
                    "refresh_token": "fake-refresh",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "scope": "read",
                },
            )
            return

        if path in ("/mcp", "/"):
            auth = self.headers.get("Authorization", "")
            if auth != f"Bearer {ACCESS_TOKEN}":
                self._send_json(401, {"error": "unauthorized"})
                return
            method = body.get("method")
            mid = body.get("id")
            if method == "initialize":
                self._send_json(
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
                return
            if method == "tools/list":
                self._send_json(
                    200,
                    {"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}},
                )
                return
            if method == "tools/call":
                self._send_json(
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
                return
            if mid is not None:
                self._send_json(
                    200,
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "error": {"code": -32601, "message": "method not found"},
                    },
                )
                return
            self._send(204, b"")
            return

        self._send(404, b"not found", "text/plain")
        return


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
