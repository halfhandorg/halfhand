#!/usr/bin/env python3
"""A minimal newline-delimited JSON-RPC echo server for `hh mcp-proxy` tests.

Reads one JSON object per line from stdin. For a request (has `id` + `method`)
it echoes a response with the same `id` and the request as `result`. For a
notification (has `method`, no `id`) it emits nothing back (notifications are
one-way). This is enough to exercise request/response correlation + `latency_ms`
and notification recording without a real MCP server.

Intentionally tiny and dependency-free (stdlib only) so the test harness can
spawn it via `python3 <this file>`.
"""
import json
import sys


def main() -> int:
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            # Forward-only on the wire; the proxy tolerates unparseable lines.
            continue
        if not isinstance(msg, dict):
            continue
        if "id" in msg and "method" in msg:
            resp = {"jsonrpc": "2.0", "id": msg["id"], "result": msg}
            sys.stdout.write(json.dumps(resp) + "\n")
            sys.stdout.flush()
        # Notifications (method, no id) and batches are one-way: no response.
    return 0


if __name__ == "__main__":
    sys.exit(main())