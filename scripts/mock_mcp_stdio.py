#!/usr/bin/env python3
"""Minimal stdio MCP server for smoke-testing the webOS mcp connector kind.

Speaks newline-delimited JSON-RPC 2.0 on stdin/stdout per the MCP spec:
  - initialize            → echoes the client's protocolVersion, advertises tools
  - notifications/initialized (no response)
  - tools/list            → two tools (one read-like, one write-like, one flagged
                            destructive to prove the class-derivation hint path)
  - tools/call            → echoes args + a marker reading an injected env secret
                            so the test can prove env-injection works WITHOUT the
                            secret ever appearing in argv.

Intentionally dependency-free (stdlib only) so the smoke is hermetic/offline.
"""
import json
import os
import sys


def reply(obj):
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
        mid = msg.get("id")

        if method == "initialize":
            pv = msg.get("params", {}).get("protocolVersion", "2025-06-18")
            reply({
                "jsonrpc": "2.0", "id": mid,
                "result": {
                    "protocolVersion": pv,
                    "capabilities": {"tools": {"listChanged": False}},
                    "serverInfo": {"name": "mock-mcp", "version": "0.0.1"},
                },
            })
        elif method == "notifications/initialized":
            pass  # notification: no response
        elif method == "tools/list":
            reply({
                "jsonrpc": "2.0", "id": mid,
                "result": {
                    "tools": [
                        {
                            "name": "get_greeting",
                            "description": "Return a friendly greeting for a name.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"name": {"type": "string"}},
                            },
                        },
                        {
                            # read-like NAME but server flags it destructive → must
                            # be classified WRITE by the authoritative deriver.
                            "name": "get_but_destructive",
                            "description": "A read-named tool the server marks destructive.",
                            "inputSchema": {"type": "object", "properties": {}},
                            "annotations": {"destructiveHint": True},
                        },
                        {
                            "name": "create_thing",
                            "description": "Create a thing (a write operation).",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"label": {"type": "string"}},
                            },
                        },
                    ]
                },
            })
        elif method == "tools/call":
            params = msg.get("params", {})
            name = params.get("name")
            args = params.get("arguments", {}) or {}
            # Prove the secret arrived via the CHILD ENV (never argv): include a
            # boolean of whether the expected env var is present, plus a length
            # (never the value itself).
            secret = os.environ.get("MOCK_MCP_TOKEN", "")
            text = json.dumps({
                "tool": name,
                "echo_args": args,
                "env_secret_present": bool(secret),
                "env_secret_len": len(secret),
            })
            reply({
                "jsonrpc": "2.0", "id": mid,
                "result": {"content": [{"type": "text", "text": text}], "isError": False},
            })
        elif mid is not None:
            reply({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "method not found"}})


if __name__ == "__main__":
    main()
