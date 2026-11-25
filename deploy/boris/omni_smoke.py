#!/usr/bin/env python3
"""Post-deploy smoke test for omni, run by deploy_omni.sh on Boris.

usage: omni_smoke.py [<expected revision>]

Checks, in order, and stops at the first failure (exit 1):
1. GET /api/health is {"status": "ok"} and names the expected revision.
2. GET /api/health/deep reports ok (every critical check passed).
3. With OMNI_SMOKE_TOKEN (a gateway client token, in ~/compose/.env): an MCP
   tools/list through /gateway/mcp returns at least MIN_TOOLS tools, and the
   read-only system_status and tasks_list calls succeed.

Prints one PASS/FAIL/SKIP line per check and never prints the token. Only
checks that depend on Omni itself; iCloud, Plex or an integration being down
must not roll a deploy back.
"""
import json
import os
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("OMNI_SMOKE_BASE", "http://127.0.0.1:8080").rstrip("/")
TOKEN = os.environ.get("OMNI_SMOKE_TOKEN", "")
REVISION = sys.argv[1] if len(sys.argv) > 1 else ""
MIN_TOOLS = 150
UA = "OpenAI File Downloader, XaiImageApiFetch/1.0"
# The container reports healthy as soon as /api/health answers; boot steps
# after that (catalog loads) get this long to finish.
SETTLE_SECONDS = 60


def fail(name, detail):
    print(f"FAIL {name}: {detail}")
    sys.exit(1)


def request(method, path, body=None, headers=None, timeout=30):
    req = urllib.request.Request(BASE + path, data=body, method=method)
    req.add_header("User-Agent", UA)
    for key, value in (headers or {}).items():
        req.add_header(key, value)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read().decode()
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode(errors="replace")
    except (urllib.error.URLError, OSError) as error:
        return 0, str(error)


def health():
    status, text = request("GET", "/api/health")
    if status != 200:
        fail("health", f"HTTP {status}: {text[:200]}")
    body = json.loads(text)
    if body.get("status") != "ok":
        fail("health", text[:200])
    server = (body.get("build") or {}).get("server", "")
    if REVISION and REVISION != "unknown" and not (
        server.startswith(REVISION) or REVISION.startswith(server)
    ):
        fail("health", f"running build {server!r}, expected revision {REVISION!r}")
    print(f"PASS health (build {server})")


def deep_health():
    deadline = time.monotonic() + SETTLE_SECONDS
    while True:
        status, text = request("GET", "/api/health/deep")
        if status == 404:
            print("SKIP deep health (this build has no /api/health/deep)")
            return
        try:
            body = json.loads(text)
        except ValueError:
            body = {}
        if status == 200 and body.get("ok") is True:
            notes = [c["name"] for c in body.get("checks", []) if not c.get("ok")]
            suffix = f"; not critical and failing: {', '.join(notes)}" if notes else ""
            print(f"PASS deep health{suffix}")
            return
        if time.monotonic() >= deadline:
            failing = [
                f"{c.get('name')}: {c.get('detail', '')}"
                for c in body.get("checks", [])
                if c.get("critical") and not c.get("ok")
            ]
            fail("deep health", "; ".join(failing) or f"HTTP {status}: {text[:200]}")
        time.sleep(5)


def mcp(method, params, call_id):
    body = json.dumps({"jsonrpc": "2.0", "id": call_id, "method": method, "params": params})
    status, text = request(
        "POST",
        "/gateway/mcp",
        body.encode(),
        {
            "Authorization": f"Bearer {TOKEN}",
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": "2025-11-25",
        },
        timeout=60,
    )
    if status != 200:
        fail(method, f"HTTP {status}: {text[:200]}")
    for line in text.splitlines():  # SSE framing or plain JSON
        if line.startswith("data:"):
            text = line[5:]
    reply = json.loads(text)
    if "error" in reply:
        fail(method, json.dumps(reply["error"])[:300])
    return reply.get("result") or {}


def mcp_checks():
    if not TOKEN:
        print("SKIP mcp (OMNI_SMOKE_TOKEN is not set)")
        return
    tools = mcp("tools/list", {}, 1).get("tools", [])
    if len(tools) < MIN_TOOLS:
        fail("tools/list", f"{len(tools)} tools, expected at least {MIN_TOOLS}")
    print(f"PASS tools/list ({len(tools)} tools)")
    for call_id, name in enumerate(["system_status", "tasks_list"], start=2):
        result = mcp("tools/call", {"name": name, "arguments": {}}, call_id)
        if result.get("isError"):
            text = " ".join(c.get("text", "") for c in result.get("content", []))
            fail(name, text[:300])
        print(f"PASS {name}")


if __name__ == "__main__":
    health()
    deep_health()
    mcp_checks()
