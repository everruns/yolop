#!/usr/bin/env python3
"""Minimal real MCP server over stdio, for yolop's black-box MCP tests.

Speaks newline-delimited JSON-RPC 2.0 on stdin/stdout (the MCP stdio
transport). Advertises two tools:

  - ``echo`` — non-readonly (``readOnlyHint: false``, ``destructiveHint: true``)
  - ``peek`` — readonly (``readOnlyHint: true``)

On every ``tools/call`` it writes ``<marker_dir>/<tool>.called`` containing the
received arguments, so a test can prove (via the filesystem) whether the tool
actually executed. ``argv[1]`` is the marker directory.

The everruns stdio transport spawns a fresh process per request, performs the
``initialize`` handshake, issues one request, then kills the process — so a
simple read/respond loop is sufficient.
"""

import json
import os
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


TOOLS = [
    {
        "name": "echo",
        "description": "Echo back the message.",
        "inputSchema": {
            "type": "object",
            "properties": {"message": {"type": "string"}},
            "required": ["message"],
        },
        "annotations": {"readOnlyHint": False, "destructiveHint": True},
    },
    {
        "name": "peek",
        "description": "Read-only peek at the message.",
        "inputSchema": {
            "type": "object",
            "properties": {"message": {"type": "string"}},
        },
        "annotations": {"readOnlyHint": True},
    },
]


def main():
    marker_dir = sys.argv[1] if len(sys.argv) > 1 else None
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        method = msg.get("method")
        mid = msg.get("id")
        if method == "initialize":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "yolop-echo", "version": "0"},
                },
            })
        elif method == "notifications/initialized":
            continue
        elif method == "tools/list":
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
        elif method == "tools/call":
            params = msg.get("params") or {}
            name = params.get("name", "")
            args = params.get("arguments") or {}
            if marker_dir:
                try:
                    with open(os.path.join(marker_dir, name + ".called"), "w") as fh:
                        json.dump(args, fh)
                except OSError:
                    # Marker persistence is best-effort test instrumentation; a
                    # filesystem hiccup must not break the MCP tools/call reply.
                    pass
            text = args.get("message", "")
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "content": [{"type": "text", "text": "echoed: " + str(text)}],
                    "isError": False,
                },
            })
        elif mid is not None:
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "error": {"code": -32601, "message": "method not found"},
            })


if __name__ == "__main__":
    main()
