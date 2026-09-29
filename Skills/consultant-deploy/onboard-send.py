#!/usr/bin/env python3
"""onboard-send.py, deliver the consultant onboarding welcome to the peer through the Aqua
System bridge, once, with a hard deadline. Used by spawn-consultant.sh --onboard.

The bridge MCP server (aqua-system-bridge-mcp) speaks newline-delimited JSON-RPC 2.0 on
stdio and handles one request at a time, in order, so the whole exchange is written up
front: initialize -> notifications/initialized -> tools/call send_message. The daemon
behind it messages ONLY people on ~/.aqua-system-bridge/allowlist.toml; that Tim-approved
list is what authorizes a direct send, and a refusal is an expected outcome, not an error.

Usage:  onboard-send.py <peer-mxid> < welcome.md
Env:    AQUA_SYSTEM_BRIDGE_MCP  bridge binary (default ~/.local/bin/aqua-system-bridge-mcp)
        ONBOARD_SEND_TIMEOUT    seconds before giving up (default 30)

Prints exactly ONE line and exits with:
  0  DELIVERED <event-id>
  3  REFUSED <reason>   the bridge refused the recipient (e.g. not on the allow-list)
  4  ERROR <reason>     bridge missing, crashed, timed out or answered something unexpected
"""
import json
import os
import re
import subprocess
import sys

MAX_REASON = 200


def short(text):
    """One line, bounded, no trailing full stop (the caller ends the sentence)."""
    text = " ".join(str(text).split())
    if len(text) > MAX_REASON:
        text = text[: MAX_REASON - 3].rstrip() + "..."
    return text.rstrip(".")


def done(code, word, detail):
    print(f"{word} {detail}")
    sys.exit(code)


def main():
    if len(sys.argv) != 2 or not sys.argv[1].startswith("@"):
        done(4, "ERROR", "usage: onboard-send.py <peer-mxid> < markdown")
    to = sys.argv[1]
    markdown = sys.stdin.read()
    if not markdown.strip():
        done(4, "ERROR", "empty onboarding text")
    binary = os.environ.get("AQUA_SYSTEM_BRIDGE_MCP") or os.path.expanduser(
        "~/.local/bin/aqua-system-bridge-mcp"
    )
    try:
        timeout = float(os.environ.get("ONBOARD_SEND_TIMEOUT") or 30)
    except ValueError:
        timeout = 30.0
    if not (os.path.isfile(binary) and os.access(binary, os.X_OK)):
        done(4, "ERROR", f"bridge client {binary} is missing or not executable")

    requests = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "spawn-consultant-onboard", "version": "1"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "send_message",
                "arguments": {
                    "to": to,
                    "markdown": markdown,
                    "from_label": "consultant onboarding",
                },
            },
        },
    ]
    stdin = "".join(json.dumps(r) + "\n" for r in requests)
    try:
        proc = subprocess.run(
            [binary], input=stdin, capture_output=True, text=True, timeout=timeout
        )
    except subprocess.TimeoutExpired:
        # The daemon may already hold the request and finish it: say so, so Tim checks
        # with the peer before forwarding a second copy.
        done(4, "ERROR", f"no answer within {timeout:g}s, it may still arrive, check before forwarding")
    except OSError as e:
        done(4, "ERROR", f"cannot run the bridge client: {short(e)}")

    answer = None
    for line in proc.stdout.splitlines():
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if isinstance(msg, dict) and msg.get("id") == 2:
            answer = msg
    if answer is None:
        err = short(proc.stderr.strip().splitlines()[-1]) if proc.stderr.strip() else ""
        done(4, "ERROR", f"no answer to send_message (exit {proc.returncode}{', ' + err if err else ''})")
    if "error" in answer:
        e = answer["error"]
        done(4, "ERROR", short(e.get("message", e) if isinstance(e, dict) else e))

    result = answer.get("result") or {}
    text = " ".join(
        c.get("text", "") for c in result.get("content", []) if isinstance(c, dict)
    ).strip()
    if result.get("isError"):
        if text.startswith("REFUSED"):
            done(3, "REFUSED", short(text[len("REFUSED"):].lstrip(": ")))
        done(4, "ERROR", short(text or "unspecified bridge error"))
    if text.startswith("Delivered to"):
        m = re.search(r"Matrix event (\$\S+?)\.?(?:\s|$)", text)
        done(0, "DELIVERED", m.group(1) if m else "(event id not reported)")
    done(4, "ERROR", f"unexpected answer: {short(text or result)}")


if __name__ == "__main__":
    main()
