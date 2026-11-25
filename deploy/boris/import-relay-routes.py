#!/usr/bin/env python3
"""Moves the omni-relay's routes into Omni's event journal (one-time, 2026-11-24).

Run on Boris from ~/compose:  python3 import-relay-routes.py [--apply]

For each route in rakazo/.env OMNI_RELAY_ROUTES it creates (or keeps) a stored
event target for the bot's webhook, reached like Omni's other Rakazo targets
(https://rakazo.syas.ca/<webhook path> through npm.compose_default), and a
stored route for the event and arguments. It trusts itself with a one-time
`omni owner-link`, removes that trusted browser when done, and prints ids and
outcomes only: never a URL path, key or token. Without --apply it only reports
what it would do.
"""
import json
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

APPLY = "--apply" in sys.argv
BASE = "http://127.0.0.1:8080"
HOST = "omni.boris"
UA = "OpenAI File Downloader, XaiImageApiFetch/1.0"
PUBLIC_RAKAZO = "https://rakazo.syas.ca"
CONNECT = "npm.compose_default"
# Relay URL variable -> (stored target id, label). Business already exists
# (imported from CLAUDE_NOTIFY_TARGETS) with the same webhook and key.
TARGETS = {
    "RAKAZO_POST_WEBHOOK_URL": ("inbox", "Inbox bot (Rakazo)"),
    "RAKAZO_CARE_WEBHOOK_URL": ("health", "Health bot (Rakazo)"),
    "RAKAZO_BUSINESS_WEBHOOK_URL": ("business", "Business bot (Rakazo)"),
}


def env(path):
    values = {}
    for line in open(path):
        line = line.rstrip("\n")
        if "=" in line and not line.lstrip().startswith("#"):
            key, value = line.split("=", 1)
            values[key.strip()] = value.strip().strip('"').strip("'")
    return values


def call(method, path, body=None, cookie=None, form=False):
    data = None
    headers = {"Host": HOST, "Origin": f"http://{HOST}", "User-Agent": UA}
    if body is not None:
        data = body.encode() if form else json.dumps(body).encode()
        headers["Content-Type"] = "application/x-www-form-urlencoded" if form else "application/json"
    if cookie:
        headers["Cookie"] = cookie
    req = urllib.request.Request(BASE + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return resp.status, resp.headers, resp.read().decode()
    except urllib.error.HTTPError as error:
        return error.code, error.headers, error.read().decode()


def main():
    rakazo = env("rakazo/.env")
    routes = json.loads(rakazo["OMNI_RELAY_ROUTES"])
    link = subprocess.run(
        ["docker", "exec", "omni", "omni", "owner-link"], check=True, capture_output=True, text=True
    ).stdout.strip().splitlines()[-1]
    status, headers, _ = call("POST", urllib.parse.urlparse(link).path, "action=approve", form=True)
    cookie = (headers.get("Set-Cookie") or "").split(";", 1)[0]
    if status != 200 or not cookie.startswith("omni_owner="):
        sys.exit(f"could not trust this script ({status})")
    try:
        status, _, text = call("GET", "/api/gateway/outbound", cookie=cookie)
        overview = json.loads(text)
        stored = {t["id"]: t for t in overview["targets"]}
        existing_routes = {(r["event"], json.dumps(r["arguments"], sort_keys=True), r["target"]) for r in overview["routes"]}
        for url_env in sorted({r["url_env"] for r in routes}):
            target_id, label = TARGETS[url_env]
            path = urllib.parse.urlparse(rakazo[url_env]).path
            url = PUBLIC_RAKAZO + path
            key = rakazo[url_env.replace("_URL", "_KEY")]
            if target_id in stored:
                same = stored[target_id]["url"] == url
                print(f"target {target_id}: already stored ({'same webhook' if same else 'DIFFERENT webhook; left as is'})")
                continue
            if not APPLY:
                print(f"target {target_id}: would create")
                continue
            status, _, text = call(
                "PUT", f"/api/gateway/outbound/targets/{target_id}",
                {"label": label, "url": url, "key": key, "connect": CONNECT}, cookie,
            )
            print(f"target {target_id}: {status}")
        for route in routes:
            target_id = TARGETS[route["url_env"]][0]
            args = route.get("arguments", {})
            ident = (route["event"], json.dumps(args, sort_keys=True), target_id)
            if ident in existing_routes:
                print(f"route {route['event']} -> {target_id}: already stored")
                continue
            if not APPLY:
                print(f"route {route['event']} {json.dumps(args)} -> {target_id}: would create")
                continue
            status, _, text = call(
                "POST", "/api/gateway/outbound/routes",
                {"event": route["event"], "arguments": args, "target": target_id}, cookie,
            )
            detail = "" if status == 201 else f" {text[:200]}"
            print(f"route {route['event']} -> {target_id}: {status}{detail}")
    finally:
        status, _, text = call("GET", "/api/owner", cookie=cookie)
        for browser in json.loads(text).get("browsers", []):
            if browser.get("current"):
                call("DELETE", f"/api/owner/browsers/{browser['id']}", cookie=cookie)
                print("removed this script's trusted browser")


if __name__ == "__main__":
    main()
