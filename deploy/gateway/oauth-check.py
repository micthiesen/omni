"""Walks the gateway's downstream OAuth flow as ChatGPT would, with ChatGPT's
real client metadata document and redirect URI. The grant ends revoked: the
check replays the authorization code, which revokes it.

usage: oauth-check.py <public origin> <LAN origin> [--via <base URL>]

--via sends every request for the public origin to <base URL> instead (a
local server or proxy whose OMNI_GATEWAY_PUBLIC_URL is the public origin),
with the public Host header, and skips the public path checks.
Prints statuses only; no token, code or verifier is printed or stored.
"""
import base64
import hashlib
import json
import re
import secrets
import sys
import urllib.parse
import urllib.request

args = sys.argv[1:]
VIA = None
if "--via" in args:
    i = args.index("--via")
    VIA = args[i + 1].rstrip("/")
    del args[i : i + 2]
PUBLIC, LAN = args[0].rstrip("/"), args[1].rstrip("/")
UA = "OpenAI File Downloader, XaiImageApiFetch/1.0"
CLIENT = "https://chatgpt.com/oauth/client.json"
REDIRECT = "https://chatgpt.com/connector_platform_oauth_redirect"
RESOURCE = PUBLIC + "/gateway/mcp"


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


opener = urllib.request.build_opener(NoRedirect)
browser_cookie = {}


def route(url):
    if VIA and url.startswith(PUBLIC):
        return VIA + url[len(PUBLIC) :]
    return url


def fetch(method, url, data=None, headers=None, browser=False):
    req = urllib.request.Request(route(url), data=data, method=method)
    req.add_header("User-Agent", UA)
    if VIA and url.startswith(PUBLIC):
        # As the proxy would: the public host, so Omni's public-host guard applies.
        req.add_header("Host", urllib.parse.urlparse(PUBLIC).netloc)
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    if browser and browser_cookie:
        req.add_header("Cookie", "; ".join(f"{k}={v}" for k, v in browser_cookie.items()))
    try:
        with opener.open(req, timeout=30) as resp:
            status, hdrs, body = resp.status, resp.headers, resp.read().decode()
    except urllib.error.HTTPError as e:
        status, hdrs, body = e.code, e.headers, e.read().decode()
    if browser:
        for value in hdrs.get_all("Set-Cookie") or []:
            name, _, rest = value.partition("=")
            browser_cookie[name.strip()] = rest.split(";", 1)[0]
    return status, {k.lower(): v for k, v in hdrs.items()}, body


def step(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + (f" ({detail})" if detail else ""))
    if not ok:
        sys.exit(1)


lan_write = {"Origin": LAN, "Content-Type": "application/json"}
status, _, body = fetch("GET", PUBLIC + "/.well-known/oauth-protected-resource/gateway/mcp")
prm = json.loads(body)
step("protected resource metadata", status == 200 and prm["resource"] == RESOURCE, prm.get("resource", ""))
status, _, body = fetch("GET", PUBLIC + "/.well-known/oauth-authorization-server")
asm = json.loads(body)
step("authorization server metadata", status == 200 and asm["issuer"] == prm["authorization_servers"][0]
     and "S256" in asm["code_challenge_methods_supported"] and asm.get("client_id_metadata_document_supported") is True
     and asm.get("authorization_response_iss_parameter_supported") is True, asm["issuer"])
status, headers, _ = fetch("POST", RESOURCE, b"{}", {"Content-Type": "application/json"})
step("unauthenticated MCP is 401 with resource_metadata", status == 401 and "resource_metadata" in headers.get("www-authenticate", ""))
if not VIA:
    # Everything but the exact public paths is a 404, including variants
    # that normalize to an allowed path and the SPA fallback.
    for path in ["/api/gateway", "/gateway", "/gateway/", "/", "/index.html", "/api/health", "/mcp",
                 "/gateway//", "/gateway/mcp/", "//gateway/mcp", "/gateway//mcp", "/gateway/./mcp",
                 "/x/../gateway/mcp", "/gateway/%6Dcp", "/GATEWAY/mcp", "/gateway/../api/gateway",
                 "/gateway/..%2Fapi%2Fgateway", "/api/gateway/integrations/..%2F..%2F..%2Fgateway%2Fmcp",
                 "/.well-known/oauth-foo", "/.well-known/oauth-authorization-server/"]:
        status, _, body = fetch("GET", PUBLIC + path)
        step(f"public {path} is not served", status == 404 and "<!doctype html>" not in body.lower(), str(status))

verifier = base64.urlsafe_b64encode(secrets.token_bytes(32)).rstrip(b"=").decode()
challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
state = secrets.token_urlsafe(16)
query = urllib.parse.urlencode({
    "response_type": "code", "client_id": CLIENT, "redirect_uri": REDIRECT, "state": state,
    "code_challenge": challenge, "code_challenge_method": "S256", "resource": RESOURCE, "scope": "omni",
})
status, _, page = fetch("GET", asm["authorization_endpoint"] + "?" + query, browser=True)
match = re.search(r"request=([A-Za-z0-9_-]+)", page)
code_match = re.search(r'<p class="code"[^>]*>([^<]+)</p>', page)
step("authorization request waits for the owner", status == 200 and match is not None and code_match is not None, str(status))
request_id, user_code = match.group(1), code_match.group(1)
status, _, body = fetch("GET", LAN + "/api/gateway")
pending = [p for p in json.loads(body)["pending"] if p["id"] == request_id]
step("request listed on the LAN page", status == 200 and len(pending) == 1, pending[0]["clientName"] if pending else "")
step("the LAN page never carries the code", user_code not in body and "userCode" not in body)
step("ChatGPT's metadata document counts as verified", pending[0].get("verified") is True)
wrong = "".join("A" if c != "A" else "B" for c in user_code)
status, _, _ = fetch("POST", LAN + f"/api/gateway/authorizations/{request_id}/approve",
                     json.dumps({"userCode": wrong}).encode(), lan_write)
step("a wrong code is refused", status == 400, str(status))
status, _, _ = fetch("POST", LAN + f"/api/gateway/authorizations/{request_id}/approve",
                     json.dumps({"userCode": user_code}).encode(), {"Content-Type": "application/json"})
step("an approval without Origin is refused", status == 403, str(status))
status, _, _ = fetch("POST", LAN + f"/api/gateway/authorizations/{request_id}/approve",
                     json.dumps({"userCode": user_code}).encode(), lan_write)
step("owner approves with the code", status == 204, str(status))
status, headers, _ = fetch("GET", PUBLIC + f"/gateway/oauth/authorize/complete?request={request_id}", browser=True)
location = headers.get("location", "")
params = dict(urllib.parse.parse_qsl(urllib.parse.urlparse(location).query))
step("browser is redirected to ChatGPT with code, state and iss", status in (302, 303) and location.startswith(REDIRECT)
     and params.get("state") == state and params.get("iss") == asm["issuer"] and "code" in params, str(status))
form = urllib.parse.urlencode({
    "grant_type": "authorization_code", "code": params["code"], "redirect_uri": REDIRECT, "client_id": CLIENT,
    "code_verifier": verifier, "resource": RESOURCE,
}).encode()
status, _, body = fetch("POST", asm["token_endpoint"], form, {"Content-Type": "application/x-www-form-urlencoded"})
tokens = json.loads(body)
step("code exchanged for tokens", status == 200 and tokens.get("token_type") == "Bearer" and "refresh_token" in tokens, str(status))
mcp_headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2025-11-25"}
listing = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).encode()
bearer = {**mcp_headers, "Authorization": "Bearer " + tokens["access_token"]}
status, _, body = fetch("POST", RESOURCE, listing, bearer)
step("the access token lists tools", status == 200 and '"tools"' in body, str(status))
status, _, body = fetch("GET", LAN + "/api/gateway")
grants = [g for g in json.loads(body)["grants"] if g["id"] == request_id]
step("the grant is listed", len(grants) == 1)

# A reused code means it leaked: the grant made from it is revoked (RFC 6749 4.1.2).
replay = fetch("POST", asm["token_endpoint"], form, {"Content-Type": "application/x-www-form-urlencoded"})[0]
step("a replayed code is refused", replay == 400, str(replay))
status, _, _ = fetch("POST", RESOURCE, listing, bearer)
step("the replay revoked the access token", status == 401, str(status))
status, _, body = fetch("GET", LAN + "/api/gateway")
step("the grant is gone", not [g for g in json.loads(body)["grants"] if g["id"] == request_id])
refresh = urllib.parse.urlencode({"grant_type": "refresh_token", "refresh_token": tokens["refresh_token"], "client_id": CLIENT}).encode()
status = fetch("POST", asm["token_endpoint"], refresh, {"Content-Type": "application/x-www-form-urlencoded"})[0]
step("the revoked refresh token is refused", status == 400, str(status))
