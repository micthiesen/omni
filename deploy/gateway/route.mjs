// Creates the public NPM proxy host for Omni's MCP gateway (docs/gateway.md).
// Run inside the NPM container with `node --input-type=module` on stdin, like
// deploy/executor-events/route.mjs. Uses NPM's own permission checks and
// create/delete path; the short-lived token stays in memory and is never printed.
//
//   OMNI_GATEWAY_ROUTE_MODE=inspect|apply|rollback
//
// The host copies the TLS settings of the existing pods.syas.ca host (42) and
// forwards to Omni like it, but only /gateway/* and /.well-known/oauth-* reach
// Omni; every other path is a 404 at the proxy, so the UI and /api stay LAN-only.
import { createHash } from "node:crypto";
import { readFile, writeFile, unlink } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import ProxyHost from "/app/models/proxy_host.js";
import Token from "/app/models/token.js";
import Access from "/app/lib/access.js";
import proxyHosts from "/app/internal/proxy-host.js";

const mode = process.env.OMNI_GATEWAY_ROUTE_MODE || "inspect";
if (!["inspect", "apply", "rollback"].includes(mode)) throw new Error("Invalid route mode");
const domain = "gateway.syas.ca";
const templateId = 42;
const recordPath = "/data/omni-gateway-route.json";
const marker = "# omni-gateway:begin";
const advanced = `${marker}
# Only the MCP gateway and its OAuth documents are public on this host.
location ~ ^/(?!gateway/|\\.well-known/oauth-) { return 404; }
# Notification streams and long tool calls.
proxy_buffering off;
proxy_read_timeout 3600s;
proxy_send_timeout 3600s;
# omni-gateway:end`;
const hash = (value) => createHash("sha256").update(value).digest("hex");

const template = await ProxyHost.query().findById(templateId);
if (
  !template ||
  template.domain_names.join() !== "pods.syas.ca" ||
  template.forward_host !== "10.10.1.100" ||
  template.forward_port !== 8080 ||
  !template.certificate_id
)
  throw new Error("The pods.syas.ca template host differs; inspect before proceeding");
const live = (await ProxyHost.query().where("is_deleted", 0)).filter((h) =>
  h.domain_names.includes(domain),
);
const existing = live[0];
const wanted = {
  domain_names: [domain],
  forward_scheme: "http",
  forward_host: template.forward_host,
  forward_port: template.forward_port,
  access_list_id: 0,
  certificate_id: template.certificate_id,
  ssl_forced: true,
  http2_support: true,
  block_exploits: true,
  caching_enabled: false,
  allow_websocket_upgrade: false,
  // Bearer tokens travel to this host: browsers never downgrade it to HTTP.
  hsts_enabled: true,
  hsts_subdomains: false,
  advanced_config: advanced,
  locations: [],
  meta: {},
};
const summary = (host) =>
  host && {
    id: host.id,
    domain: host.domain_names.join(),
    upstream: `${host.forward_host}:${host.forward_port}`,
    certificate: host.certificate_id,
    accessList: host.access_list_id,
    ours: (host.advanced_config || "").includes(marker),
    advancedHash: hash(host.advanced_config || ""),
  };

if (mode === "inspect") {
  console.log(JSON.stringify({ existing: summary(existing) ?? null, wantedAdvancedHash: hash(advanced) }));
  process.exit(0);
}

const signed = await Token().create({
  iss: "local-maintenance",
  attrs: { id: template.owner_user_id },
  scope: ["user"],
  expiresIn: "5m",
});
const access = new Access(signed.token);

if (mode === "apply") {
  if (existing) {
    if (!(existing.advanced_config || "").includes(marker))
      throw new Error(`${domain} exists and was not created by this script; refusing`);
    console.log(JSON.stringify({ mode, unchanged: true, host: summary(existing) }));
    process.exit(0);
  }
  // The proposed server block must be valid nginx before NPM writes anything.
  const candidate = `/tmp/omni-gateway-${process.pid}.conf`;
  await writeFile(
    candidate,
    `user root; pid /tmp/omni-gateway-test.pid; events {} http { server { listen 127.0.0.1:47891;\n${advanced}\nlocation / { return 204; } } }`,
    { mode: 0o600 },
  );
  execFileSync("nginx", ["-t", "-c", candidate], { stdio: "pipe" });
  await unlink(candidate);
  const created = await proxyHosts.create(access, wanted);
  if (created.meta?.nginx_online === false)
    throw new Error("NPM rejected the nginx configuration; run rollback");
  execFileSync("nginx", ["-t"], { stdio: "pipe" });
  await writeFile(recordPath, JSON.stringify({ id: created.id, domain }), { mode: 0o600 });
  const verified = await ProxyHost.query().findById(created.id);
  console.log(JSON.stringify({ mode, created: summary(verified) }));
  process.exit(0);
}

// rollback: delete only the host this script created.
const record = JSON.parse(await readFile(recordPath, "utf8"));
const host = await ProxyHost.query().findById(record.id);
if (!host || host.is_deleted) {
  console.log(JSON.stringify({ mode, unchanged: true }));
  process.exit(0);
}
if (host.domain_names.join() !== domain || !(host.advanced_config || "").includes(marker))
  throw new Error("The recorded host is not the gateway host; refusing");
await proxyHosts.delete(access, { id: host.id });
execFileSync("nginx", ["-t"], { stdio: "pipe" });
console.log(JSON.stringify({ mode, deleted: host.id }));
process.exit(0);
