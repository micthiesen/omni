// Retires NPM proxy host rakazo-hooks.syas.ca, the public entry of the old
// omni-relay (Omni's stored event routes replaced it on 2026-11-24; see
// "Event journal" in docs/mcp-events.md). Run inside the NPM container:
//
//   docker exec -i -w /app -e RELAY_HOST_MODE=inspect|apply npm node --input-type=module < retire-relay-host.mjs
//
// apply saves the host's row to /data/rakazo-hooks-retired.json, then deletes
// it through NPM's own path (a soft delete; the record can recreate it). The
// short-lived token stays in memory and is never printed.
import { writeFile } from "node:fs/promises";
import ProxyHost from "/app/models/proxy_host.js";
import Token from "/app/models/token.js";
import Access from "/app/lib/access.js";
import proxyHosts from "/app/internal/proxy-host.js";

const mode = process.env.RELAY_HOST_MODE || "inspect";
if (!["inspect", "apply"].includes(mode)) throw new Error("Invalid mode");
const domain = "rakazo-hooks.syas.ca";
const recordPath = "/data/rakazo-hooks-retired.json";

const live = (await ProxyHost.query().where("is_deleted", 0)).filter((h) =>
  h.domain_names.includes(domain),
);
const summary = (h) =>
  h && {
    id: h.id,
    domains: h.domain_names,
    forward: `${h.forward_scheme}://${h.forward_host}:${h.forward_port}`,
    enabled: h.enabled,
  };
if (mode === "inspect" || live.length === 0) {
  console.log(JSON.stringify({ mode, live: live.map(summary) }));
  process.exit(0);
}
if (live.length !== 1) throw new Error(`${live.length} live hosts name ${domain}; inspect first`);
const host = live[0];
await writeFile(recordPath, JSON.stringify(host, null, 2), { mode: 0o600 });
const signed = await Token().create({
  iss: "local-maintenance",
  attrs: { id: host.owner_user_id },
  scope: ["user"],
  expiresIn: "5m",
});
await proxyHosts.delete(new Access(signed.token), { id: host.id });
console.log(JSON.stringify({ mode, deleted: summary(host), record: recordPath }));
