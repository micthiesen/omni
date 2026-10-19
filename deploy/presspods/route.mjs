// Locks the public PressPods proxy host (pods.syas.ca, NPM host 42) down to
// the feed, content-addressed audio and artwork. Run inside the NPM container
// with `node --input-type=module` on stdin, like deploy/gateway/route.mjs.
// Uses NPM's own update path; the short-lived token stays in memory.
//
//   OMNI_PODS_ROUTE_MODE=inspect|apply|rollback
//
// NPM forwards the raw request URI, so the rule checks $request_uri: an
// encoded, dotted or doubled-slash path that would normalize into /pods/ is a
// 404 like everything else. Omni enforces the same allowlist for this host
// (omni_presspods::routes::is_public_path).
import { createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import ProxyHost from "/app/models/proxy_host.js";
import Token from "/app/models/token.js";
import Access from "/app/lib/access.js";
import proxyHosts from "/app/internal/proxy-host.js";

const mode = process.env.OMNI_PODS_ROUTE_MODE || "inspect";
if (!["inspect", "apply", "rollback"].includes(mode)) throw new Error("Invalid route mode");
const id = 42;
const domain = "pods.syas.ca";
const backupPath = "/data/omni-pods-route.json";
const wanted = `# Only the PressPods feed, its audio and artwork are public on this host.
# NPM forwards the raw request URI, so the check runs on $request_uri.
if ($request_uri !~ "^/pods/(rss|logo\\.jpeg|audio/[A-Za-z0-9_-]+\\.mp3)(\\?.*)?$") { return 404; }`;
const hash = (value) => createHash("sha256").update(value).digest("hex");

const host = await ProxyHost.query().findById(id);
if (!host || host.is_deleted || host.domain_names.join() !== domain)
  throw new Error(`Host ${id} is not ${domain}; inspect before proceeding`);
const current = host.advanced_config || "";
const summary = { id, domain, upstream: `${host.forward_host}:${host.forward_port}`, advancedHash: hash(current) };

if (mode === "inspect") {
  console.log(JSON.stringify({ ...summary, current, applied: current === wanted }));
  process.exit(0);
}

const signed = await Token().create({
  iss: "local-maintenance",
  attrs: { id: host.owner_user_id },
  scope: ["user"],
  expiresIn: "5m",
});
const access = new Access(signed.token);
await access.can("proxy_hosts:update", id);

let target;
if (mode === "apply") {
  if (current === wanted) {
    console.log(JSON.stringify({ mode, unchanged: true, ...summary }));
    process.exit(0);
  }
  // The rule must be valid nginx before NPM writes anything.
  const candidate = `/tmp/omni-pods-${process.pid}.conf`;
  await writeFile(
    candidate,
    `user root; pid /tmp/omni-pods-test.pid; events {} http { server { listen 127.0.0.1:47892;\n${wanted}\nlocation / { return 204; } } }`,
    { mode: 0o600 },
  );
  execFileSync("nginx", ["-t", "-c", candidate], { stdio: "pipe" });
  await writeFile(backupPath, JSON.stringify({ id, advanced_config: current }), { mode: 0o600, flag: "wx" });
  target = wanted;
} else {
  const backup = JSON.parse(await readFile(backupPath, "utf8"));
  if (backup.id !== id) throw new Error("The backup is for another host");
  target = backup.advanced_config;
}
const updated = await proxyHosts.update(access, { id, advanced_config: target });
if (updated.meta?.nginx_online === false) throw new Error("NPM rejected the configuration; run rollback");
execFileSync("nginx", ["-t"], { stdio: "pipe" });
const verified = await ProxyHost.query().findById(id);
if (verified.advanced_config !== target) throw new Error("Route read-back mismatch");
console.log(JSON.stringify({ mode, ...summary, advancedHash: hash(target) }));
process.exit(0);
