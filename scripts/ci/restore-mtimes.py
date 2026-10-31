#!/usr/bin/env python3
"""Make a restored Cargo cache safe and useful for this checkout.

Cargo decides whether a workspace crate is fresh by comparing its source files'
mtimes with the cached build output. A fresh checkout stamps every file with
the checkout time, so a restored cache of workspace crates would always look
stale. This sets each tracked file's mtime to its last commit time, so files
unchanged since the cache was built are older than its output and are reused.

Commit times alone are not enough: a commit dated before the cache was built
(pushed while CI was running, or an older pull request commit) would look
older than the cached output and be skipped, shipping stale code. So the cache
records the commit it was built from (`target/.cache-sha`, written before each
save), and every file changed between that commit and HEAD is stamped with the
current time. A cache whose commit is unknown is deleted. Needs full history
(`fetch-depth: 0`). See docs/ci.md.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import time

CACHE_SHA = os.path.join("target", ".cache-sha")


def git(*args: str) -> str:
    return subprocess.run(["git", "-c", "core.quotepath=off", *args], check=True,
                          capture_output=True, text=True).stdout


def commit_times(log: str) -> dict[str, int]:
    """Newest commit time per path from `git log --format=%x00%ct --name-only`."""
    times: dict[str, int] = {}
    stamp = None
    for line in log.splitlines():
        if line.startswith("\0"):
            stamp = int(line[1:])
        elif line and stamp is not None:
            times.setdefault(line, stamp)
    return times


def changed_since_cache() -> list[str] | None:
    """Paths changed since the cached build's commit; None when unusable."""
    try:
        with open(CACHE_SHA, encoding="utf-8") as f:
            cached = f.read().strip()
    except FileNotFoundError:
        return None
    try:
        git("cat-file", "-e", f"{cached}^{{commit}}")
        return [p for p in git("diff", "--name-only", "--no-renames", cached, "HEAD").splitlines() if p]
    except subprocess.CalledProcessError:
        return None


def main() -> int:
    tracked = [p for p in git("ls-files", "-z").split("\0") if p]
    times = commit_times(git("log", "--format=%x00%ct", "--name-only", "--no-renames", "HEAD"))
    missing = [path for path in tracked if path not in times]
    if missing:
        # A shallow clone cannot date these files; fail rather than leave a
        # cache that silently never reuses anything.
        print(f"no commit time for {len(missing)} tracked files (shallow clone?): {missing[:5]}",
              file=sys.stderr)
        return 1
    for path in tracked:
        if os.path.lexists(path) and not os.path.islink(path):
            os.utime(path, (times[path], times[path]))
    print(f"restored commit mtimes for {len(tracked)} files")
    if not os.path.isdir("target"):
        return 0
    changed = changed_since_cache()
    if changed is None:
        # Without the cache's commit, a file older than the cache cannot be
        # told from one committed earlier but never built: start cold.
        shutil.rmtree("target")
        print("cached build has no known commit; removed it")
        return 0
    now = time.time()
    touched = 0
    for path in changed:
        if os.path.lexists(path) and not os.path.islink(path):
            os.utime(path, (now, now))
            touched += 1
    print(f"marked {touched} files changed since the cached build as new")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
