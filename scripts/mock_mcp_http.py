#!/usr/bin/env python3
"""Minimal streamable-HTTP MCP server for smoke-testing the webOS mcp connector
kind over the http transport (TASK 3).

Speaks the MCP "Streamable HTTP" transport the way rmcp's reqwest client expects
(see rmcp-1.7.0 src/transport/common/reqwest/streamable_http_client.rs):

  POST <endpoint>
    - initialize                 -> 200 application/json JSON-RPC InitializeResult,
                                     plus an `Mcp-Session-Id` response header.
    - notifications/initialized  -> 202 Accepted (a notification: no body).
    - tools/list                 -> 200 application/json JSON-RPC result (3 tools).
    - tools/call                 -> 200 application/json JSON-RPC result. The
                                     result proves the Authorization: Bearer token
                                     arrived (presence + length only — never the
                                     value), mirroring the stdio env-injection proof.
  GET  <endpoint>  -> 405 Method Not Allowed  (rmcp reads this as
                       "server does not support SSE" and proceeds JSON-only, so
                       this mock stays a pure request/response JSON server).
  DELETE <endpoint> -> 200 OK  (session cleanup; rmcp tolerates 405 too).

Dependency-free (stdlib http.server) so the smoke is hermetic/offline. Binds to
127.0.0.1 on a port given as argv[1]. Plaintext http is acceptable ONLY because
the daemon side requires WEBOS_ALLOW_LOCAL_MCP=1 to reach a local endpoint.
"""
import json
import os
import sys
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SESSION_ID = "mock-session-" + uuid.uuid4().hex[:12]
JSON_CT = "application/json"


def _tools():
    return [
        {
            "name": "get_greeting",
            "description": "Return a friendly greeting for a name.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}},
        },
        {
            # read-like NAME but flagged destructive -> must classify WRITE.
            "name": "get_but_destructive",
            "description": "A read-named tool the server marks destructive.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"destructiveHint": True},
        },
        {
            "name": "create_thing",
            "description": "Create a thing (a write operation).",
            "inputSchema": {"type": "object", "properties": {"label": {"type": "string"}}},
        },
    ]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # silence default stderr noise
        pass

    def _send_json(self, obj, status=200, with_session=False):
        body = json.dumps(obj).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", JSON_CT)
        self.send_header("Content-Length", str(len(body)))
        if with_session:
            self.send_header("Mcp-Session-Id", SESSION_ID)
        self.end_headers()
        self.wfile.write(body)

    def _send_status(self, status):
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        # No standalone SSE stream -> tell rmcp the server doesn't support SSE.
        self._send_status(405)

    def do_DELETE(self):
        # Session cleanup. 200 = deleted (rmcp also tolerates 405).
        self._send_status(200)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0) or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            msg = json.loads(raw.decode("utf-8")) if raw else {}
        except json.JSONDecodeError:
            self._send_status(400)
            return

        method = msg.get("method")
        mid = msg.get("id")

        # Prove the Bearer token arrived in the Authorization header WITHOUT
        # echoing the value. reqwest sends `Authorization: Bearer <token>`.
        auth = self.headers.get("Authorization", "")
        token = auth[len("Bearer "):] if auth.startswith("Bearer ") else ""

        if method == "initialize":
            pv = msg.get("params", {}).get("protocolVersion", "2025-06-18")
            self._send_json(
                {
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "protocolVersion": pv,
                        "capabilities": {"tools": {"listChanged": False}},
                        "serverInfo": {"name": "mock-mcp-http", "version": "0.0.1"},
                    },
                },
                with_session=True,
            )
        elif method == "notifications/initialized":
            self._send_status(202)  # notification: accepted, no body
        elif method == "tools/list":
            self._send_json({"jsonrpc": "2.0", "id": mid, "result": {"tools": _tools()}})
        elif method == "tools/call":
            params = msg.get("params", {})
            name = params.get("name")
            args = params.get("arguments", {}) or {}
            text = json.dumps(
                {
                    "tool": name,
                    "echo_args": args,
                    "auth_present": bool(token),
                    "auth_len": len(token),
                    "transport": "http",
                }
            )
            self._send_json(
                {
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {"content": [{"type": "text", "text": text}], "isError": False},
                }
            )
        elif mid is not None:
            self._send_json(
                {"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "method not found"}}
            )
        else:
            self._send_status(202)  # unknown notification


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    # Announce the bound port on stdout so a parent can discover it (port 0).
    print(srv.server_address[1], flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
