#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/common.sh"

team=""
token=""
server="http://omni.boris"
bundle_id="com.micthiesen.OmniLive"

while (($#)); do
  case "$1" in
    --team) team="${2:-}"; shift 2 ;;
    --token) token="${2:-}"; shift 2 ;;
    --server) server="${2:-}"; shift 2 ;;
    --bundle-id) bundle_id="${2:-}"; shift 2 ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
done

if [[ ! "$team" =~ ^[A-Z0-9]{10}$ ]]; then
  echo "--team must be the 10-character Apple Developer Team ID" >&2
  exit 2
fi
if [[ -z "$token" ]]; then
  token="$(openssl rand -hex 32)"
  generated=true
else
  generated=false
fi
if [[ ! "$token" =~ ^[A-Za-z0-9._-]{24,}$ ]]; then
  echo "--token must be at least 24 URL-safe characters" >&2
  exit 2
fi
if [[ "$server" != http://* && "$server" != https://* ]]; then
  echo "--server must start with http:// or https://" >&2
  exit 2
fi
if [[ "$server" =~ [[:space:]] ]]; then
  echo "--server must not contain whitespace" >&2
  exit 2
fi
server="${server%/}"
if [[ "$server" == http://* && "$server" != "http://omni.boris" ]]; then
  echo "--server must use HTTPS unless it is http://omni.boris, the checked-in ATS exception" >&2
  exit 2
fi
if [[ ! "$bundle_id" =~ ^[A-Za-z0-9.-]+$ ]]; then
  echo "--bundle-id contains invalid characters" >&2
  exit 2
fi

if [[ "$server" == http://* ]]; then
  xcconfig_server="http:/\$()/${server#http://}"
else
  xcconfig_server="https:/\$()/${server#https://}"
fi
group_id="group.${bundle_id}"
destination="$IOS_PROJECT_DIR/Config/Local.xcconfig"
{
  echo "OMNI_DEVELOPMENT_TEAM = $team"
  echo "OMNI_DEFAULT_SERVER_URL = $xcconfig_server"
  echo "OMNI_DEFAULT_AUTH_TOKEN = $token"
  echo "OMNI_BUNDLE_ID = $bundle_id"
  echo "OMNI_APP_GROUP = $group_id"
} > "$destination"
chmod 600 "$destination"

echo "Wrote $destination"
echo "Set the same server secret in Omni:"
echo "IOS_CONTROL_AUTH_TOKEN=$token"
if [[ "$generated" == true ]]; then
  echo "A new random token was generated. Save the line above in the server environment."
fi
