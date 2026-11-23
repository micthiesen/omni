#!/usr/bin/env python3
"""Delete a Cargo cache flavor's older entries on main before saving a new one.

Usage: drop-superseded-cache.py <key prefix> <new key>

The job already restored the entry it builds from, so older entries of the same
flavor (any commit or toolchain) are dead weight. Saving next to them briefly
doubles the Cargo caches and pushes the repository past GitHub's 10 GB quota,
where GitHub evicts the least recently used entries: the Docker layer cache,
which the publish job then rebuilds and re-exports from nothing. Runs in the
server job's Rust container too, so it needs only the standard library.
Failures only print: the prune job deletes whatever is left.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.parse
import urllib.request

API = os.environ.get("GITHUB_API_URL", "https://api.github.com")
REF = "refs/heads/main"


def request(method: str, path: str) -> dict:
    req = urllib.request.Request(
        f"{API}/repos/{os.environ['GITHUB_REPOSITORY']}/{path}",
        method=method,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
            "User-Agent": "OpenAI File Downloader, XaiImageApiFetch/1.0",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(req, timeout=30) as response:
        body = response.read()
    return json.loads(body) if body else {}


def main() -> int:
    prefix, keep = sys.argv[1], sys.argv[2]
    query = urllib.parse.urlencode({"key": prefix, "ref": REF, "per_page": 100})
    try:
        caches = request("GET", f"actions/caches?{query}").get("actions_caches", [])
        for cache in caches:
            if cache["key"].startswith(prefix) and cache["key"] != keep:
                request("DELETE", f"actions/caches/{cache['id']}")
                print(f"deleted {cache['key']} ({cache['size_in_bytes'] / 1e9:.2f} GB)")
    except Exception as error:  # noqa: BLE001 - never fail the job over cleanup
        print(f"could not delete superseded caches: {error}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
