#!/usr/bin/env python3
"""Minimal YEP capability server used by src/extensions spawn tests.

Speaks the yolop extension protocol (specs/extensions.md): newline-delimited
JSON-RPC over stdio. Serves one tool (`echo`) and a static prompt; emits a
`tool/update` notification before each tool result to exercise streaming.
stdout carries only protocol JSON; this file has no dependencies.
"""

import json
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = msg.get("method")
        msg_id = msg.get("id")
        if method == "initialize":
            send({
                "id": msg_id,
                "result": {
                    "protocol_version": "1.0",
                    "name": "echo",
                    "capabilities": ["tools", "prompt", "streaming"],
                    "capability_params": {
                        "prompt": {"static": "echo fixture prompt"},
                        "tools": [{"name": "echo"}],
                    },
                },
            })
        elif method == "initialized":
            continue
        elif method == "tool/call":
            params = msg.get("params") or {}
            if params.get("name") != "echo":
                send({"id": msg_id, "error": {"message": "no such tool"}})
                continue
            send({
                "method": "tool/update",
                "params": {"request_id": msg_id, "output": "echoing"},
            })
            args = params.get("args") or {}
            send({"id": msg_id, "result": {"echoed": args.get("text", "")}})
        elif method == "shutdown":
            send({"id": msg_id, "result": {}})
            return
        elif msg_id is not None:
            send({"id": msg_id, "error": {"code": -32601,
                                          "message": f"method not found: {method}"}})


if __name__ == "__main__":
    main()
