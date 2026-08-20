#!/usr/bin/env python3
"""Minimal YEP capability server used by src/extensions spawn tests.

Speaks the yolop extension protocol (knowledge/specs/extensions.md): newline-delimited
JSON-RPC over stdio. Serves one tool (`echo`) and a static prompt; emits a
`tool/update` notification before each tool result to exercise streaming.
stdout carries only protocol JSON; this file has no dependencies.
"""

import json
import sys

# Set from `initialize.config.trace_log`: a file the server appends each
# received `trace/event` type to. Lets a spawn test assert the host forwards
# the session event stream to a `trace`-declaring extension (a fire-and-forget
# notification, so there is nothing to reply on the wire). Config-driven rather
# than env-driven so parallel tests don't collide.
TRACE_LOG = None

# Set from `initialize.config.trace_delay_ms`: seconds to sleep before recording
# each `trace/event`. A teardown test needs the tail of the stream to still be
# in flight when the host stops the server, which a delay makes deterministic
# instead of a race the test would win most of the time.
TRACE_DELAY_S = 0.0


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def record_trace(event_type):
    if not TRACE_LOG:
        return
    if TRACE_DELAY_S:
        import time
        time.sleep(TRACE_DELAY_S)
    with open(TRACE_LOG, "a", encoding="utf-8") as handle:
        handle.write(event_type + "\n")


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
            config = (msg.get("params") or {}).get("config") or {}
            if isinstance(config, dict) and config.get("trace_log"):
                global TRACE_LOG
                TRACE_LOG = config["trace_log"]
            if isinstance(config, dict) and config.get("trace_delay_ms"):
                global TRACE_DELAY_S
                TRACE_DELAY_S = float(config["trace_delay_ms"]) / 1000.0
            # Echo an injected env var (for the secret-injection spawn test):
            # the host injects `secret`/`env`-mapped config fields into our
            # environment. Record it so the test can assert it arrived.
            import os
            probe = os.environ.get("YEP_ECHO_ENV")
            if probe:
                record_trace("env:" + probe)
            send({
                "id": msg_id,
                "result": {
                    "protocol_version": "1.0",
                    "name": "echo",
                    "capabilities": ["tools", "prompt", "streaming", "trace"],
                    "capability_params": {
                        "prompt": {"static": "echo fixture prompt"},
                        "tools": [{"name": "echo"}],
                    },
                },
            })
        elif method == "initialized":
            continue
        elif method == "trace/event":
            # Fire-and-forget notification: record it, send nothing back.
            params = msg.get("params") or {}
            record_trace(params.get("event_type", ""))
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
        elif method == "hook/fire":
            params = msg.get("params") or {}
            # Block any tool whose args carry {"forbidden": true}; else allow.
            args = params.get("args") or {}
            if args.get("forbidden") is True:
                send({"id": msg_id, "result": {"block": True,
                                               "reason": "forbidden by echo fixture"}})
            else:
                send({"id": msg_id, "result": {}})
        elif method == "prompt/contribution":
            send({"id": msg_id, "result": {"text": "dynamic echo prompt"}})
        elif method == "shutdown":
            send({"id": msg_id, "result": {}})
            return
        elif msg_id is not None:
            send({"id": msg_id, "error": {"code": -32601,
                                          "message": f"method not found: {method}"}})


if __name__ == "__main__":
    main()
