#!/usr/bin/env python3
# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

"""Deterministic local MCP JSONL fixture used only by the macOS reachability probe."""

import json
import sys


def write_message(message: dict) -> None:
    """Flush each response so the client can observe the protocol boundary immediately."""
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def main() -> int:
    """Serve only the MCP methods needed by the probe and fail closed on malformed input."""
    for line in sys.stdin:
        if not line.strip():
            continue
        request = json.loads(line)
        method = str(request.get("method", ""))
        request_id = request.get("id")
        if method == "initialize":
            write_message(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "ja-native-probe", "version": "1.0.0"},
                    },
                }
            )
        elif method == "notifications/initialized":
            continue
        elif method == "tools/list":
            write_message(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "tools": [
                            {
                                "name": "probe_echo",
                                "description": "Deterministic local probe tool",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {"text": {"type": "string"}},
                                    "required": ["text"],
                                },
                            }
                        ]
                    },
                }
            )
        elif method == "tools/call":
            text = str(request.get("params", {}).get("arguments", {}).get("text", ""))
            write_message(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "content": [{"type": "text", "text": f"probe:{text}"}],
                        "isError": False,
                    },
                }
            )
        elif method == "ping":
            write_message({"jsonrpc": "2.0", "id": request_id, "result": {}})
        else:
            write_message(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32601, "message": f"unsupported:{method}"},
                }
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
