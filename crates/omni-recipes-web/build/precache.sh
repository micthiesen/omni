#!/bin/sh
# Trunk post-build hook: writes sw.js into the staging directory with the list
# of app-shell URLs and a digest of their contents in front of js/sw.js, so
# every build that changes a shell file installs a new shell cache.
# POSIX sh, find and cksum only: it runs in the Docker web stage, in CI and on
# macOS.
set -eu

STAGING=${TRUNK_STAGING_DIR:?run by trunk}
SOURCE=${TRUNK_SOURCE_DIR:?run by trunk}

cd "$STAGING"
FILES=$(find . -type f ! -name sw.js ! -name '*.br' ! -name '*.gz' | LC_ALL=C sort)

URLS=
for file in $FILES; do
  path=${file#.}
  [ "$path" = /index.html ] && path=/
  URLS="$URLS\"$path\","
done

# shellcheck disable=SC2086
BUILD=$( (cat $FILES; cat "$SOURCE/js/sw.js") | cksum | tr ' ' '-')

{
  printf 'const BUILD = "%s";\n' "$BUILD"
  printf 'const SHELL = [%s];\n' "${URLS%,}"
  cat "$SOURCE/js/sw.js"
} > sw.js
