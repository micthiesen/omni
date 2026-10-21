#!/usr/bin/env python3
"""Refresh ChatGPT's saved copy of the Omni gateway's tools and events.

ChatGPT stores a custom MCP connection's tool list and MCP event catalog when
the connection is created and changes it only on "Refresh tools" (Plugins ->
the connection -> Manage). It does not act on `tools/list_changed`. This
script presses that button through ChatGPT's own backend, authenticated with
the ChatGPT login Codex keeps in `~/.codex/auth.json` (or `$CODEX_HOME`), and
reports which tools and events were added, removed or changed.

The endpoints are ChatGPT's internal web API, not a published one, and may
change without notice; a failure here never affects Omni itself.

  python3 -I scripts/chatgpt/refresh-connector.py
  python3 -I scripts/chatgpt/refresh-connector.py --expect shell,shell_usage --wait 15

`--expect` names tools or events that must be present after the refresh;
`--wait` retries for that many minutes (for a deploy that is still rolling
out). Exit codes: 0 refreshed (and every expected name present), 1 an
expected name is missing, 2 a usage, login or API failure.
"""

import argparse
import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.request

API = "https://chatgpt.com/backend-api"
GATEWAY_URL = "https://gateway.syas.ca/gateway/mcp"
USER_AGENT = "OpenAI File Downloader, XaiImageApiFetch/1.0"


class Failure(Exception):
    pass


def login():
    home = os.environ.get("CODEX_HOME") or os.path.expanduser("~/.codex")
    path = os.path.join(home, "auth.json")
    try:
        with open(path) as f:
            tokens = json.load(f)["tokens"]
        token, account = tokens["access_token"], tokens["account_id"]
    except (OSError, KeyError, TypeError, ValueError) as e:
        raise Failure(f"no ChatGPT login in {path} ({e}); run `codex login`") from None
    if not token or not account:
        raise Failure(f"no ChatGPT login in {path}; run `codex login`")
    return token, account


class ChatGPT:
    def __init__(self, token, account):
        self.headers = {
            "Authorization": f"Bearer {token}",
            "ChatGPT-Account-Id": account,
            "OAI-Product-Sku": "CONNECTOR_SETTING",
            "Content-Type": "application/json",
            "Accept": "application/json",
            "User-Agent": USER_AGENT,
        }

    def call(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(
            API + path, data=data, method=method, headers=self.headers
        )
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                return json.loads(response.read() or b"null")
        except urllib.error.HTTPError as e:
            detail = e.read()[:300].decode(errors="replace")
            if e.code == 401:
                raise Failure(
                    "ChatGPT rejected the Codex login (401); run any `codex` command "
                    "to refresh it, or `codex login`"
                ) from None
            raise Failure(f"{method} {path}: HTTP {e.code} {detail}") from None
        except urllib.error.URLError as e:
            raise Failure(f"{method} {path}: {e.reason}") from None

    def connections(self, url):
        """Every (link, connector) whose MCP endpoint is `url`."""
        links = self.call(
            "POST",
            "/aip/connectors/links/list_accessible",
            {"principals": [], "link_refresh_strategy": "NONE"},
        ).get("links", [])
        found = []
        for link in links:
            if link.get("connector_type") != "MCP":
                continue
            connector_id = link.get("connector_id", "")
            if connector_id.startswith(("implicit_", "connector_openai")):
                continue
            connector = self.call("GET", f"/aip/connectors/{connector_id}")
            if (connector.get("base_url") or "").rstrip("/") == url.rstrip("/"):
                found.append((link, connector))
        return found

    def catalog(self, connector_id):
        d = self.call("GET", f"/aip/connectors/{connector_id}/actions")
        return {
            "tools": {a["name"]: digest(a) for a in d.get("actions") or []},
            "events": {e["name"]: digest(e) for e in d.get("mcp_events") or []},
        }


def digest(entry):
    return hashlib.sha256(json.dumps(entry, sort_keys=True).encode()).hexdigest()


def diff(before, after):
    lines = []
    for kind in ("tools", "events"):
        old, new = before[kind], after[kind]
        added = sorted(new.keys() - old.keys())
        removed = sorted(old.keys() - new.keys())
        changed = sorted(n for n in new.keys() & old.keys() if new[n] != old[n])
        lines.append(
            f"{kind}: {len(new)} (added {len(added)}, removed {len(removed)}, changed {len(changed)})"
        )
        for label, names in (("+", added), ("-", removed), ("~", changed)):
            if names:
                lines.append(f"  {label} " + ", ".join(names))
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--url", default=GATEWAY_URL, help="the connection's MCP endpoint")
    parser.add_argument("--expect", default="", help="comma-separated tool or event names")
    parser.add_argument("--wait", type=float, default=0, help="minutes to keep retrying")
    args = parser.parse_args()
    expected = {name.strip() for name in args.expect.split(",") if name.strip()}

    chatgpt = ChatGPT(*login())
    connections = chatgpt.connections(args.url)
    if not connections:
        raise Failure(f"no ChatGPT connection for {args.url}")
    deadline = time.monotonic() + args.wait * 60
    missing = set()
    for link, connector in connections:
        print(f"{connector.get('name')} ({connector['id']}, link {link['id']})")
        before = chatgpt.catalog(connector["id"])
        while True:
            chatgpt.call("POST", "/aip/connectors/mcp/refresh_actions", {"link_id": link["id"]})
            after = chatgpt.catalog(connector["id"])
            missing = expected - after["tools"].keys() - after["events"].keys()
            if not missing or time.monotonic() >= deadline:
                break
            print(f"  still missing {', '.join(sorted(missing))}; retrying in 60 s")
            time.sleep(60)
        print(diff(before, after))
        if missing:
            print(f"missing after refresh: {', '.join(sorted(missing))}")
    print("Start a new ChatGPT conversation to use the refreshed tools.")
    return 1 if missing else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Failure as failure:
        print(f"refresh-connector: {failure}", file=sys.stderr)
        sys.exit(2)
