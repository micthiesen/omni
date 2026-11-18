// Creates the LAN-only NPM proxy host for Fond, the recipe app Omni serves on
// recipes.syas.ca. Run inside the NPM container with `node --input-type=module`
// on stdin, like deploy/gateway/route.mjs. Uses NPM's own permission checks and
// create/update/delete path; the short-lived token stays in memory and is
// never printed.
//
//   OMNI_RECIPES_ROUTE_MODE=inspect|apply|rollback
//
// The host forwards to Omni like omni.syas.ca (host 47) and sits behind the
// same access list, looked up by name. It is not public: the DNS record is a
// private address and the access list denies every peer outside the LAN.
import { createHash } from "node:crypto";
import { readFile, writeFile, unlink } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import ProxyHost from "/app/models/proxy_host.js";
import AccessList from "/app/models/access_list.js";
import Token from "/app/models/token.js";
import Access from "/app/lib/access.js";
import proxyHosts from "/app/internal/proxy-host.js";

const mode = process.env.OMNI_RECIPES_ROUTE_MODE || "inspect";
if (!["inspect", "apply", "rollback"].includes(mode)) throw new Error("Invalid route mode");
const domain = "recipes.syas.ca";
const templateId = 47;
const accessListName = "Omni HTTPS LAN only";
const recordPath = "/data/omni-recipes-route.json";
const marker = "# omni-recipes:begin";
// The access list compares the address nginx's real-IP module settles on. NPM
// trusts X-Real-IP from CDN and private ranges by default, so without this
// override a forged header could make an outside peer look like a LAN client.
// It must stay identical to the override on the omni.syas.ca host.
const realIp = `set_real_ip_from unix:;
real_ip_header X-Real-IP;
real_ip_recursive off;`;
const advanced = `${marker}
${realIp}
# Photo uploads.
client_max_body_size 64m;
# The /api/events stream. These are host-level because a location block here
# would sit outside "location /", which is where NPM puts the access list.
proxy_buffering off;
proxy_read_timeout 3600s;
# omni-recipes:end`;
const hash = (value) => createHash("sha256").update(value).digest("hex");

const template = await ProxyHost.query().findById(templateId);
if (
  !template ||
  template.is_deleted ||
  template.domain_names.join() !== "omni.syas.ca" ||
  template.forward_scheme !== "http" ||
  template.forward_host !== "10.10.1.100" ||
  template.forward_port !== 8080 ||
  !template.certificate_id
)
  throw new Error("The omni.syas.ca template host differs; inspect before proceeding");
if ((template.advanced_config || "").trim() !== realIp)
  throw new Error("The omni.syas.ca real-IP override differs; inspect before proceeding");
const lists = await AccessList.query()
  .where("is_deleted", 0)
  .where("name", accessListName)
  .withGraphFetched("[clients]");
if (lists.length !== 1)
  throw new Error(`Expected one access list named "${accessListName}", found ${lists.length}`);
const accessList = lists[0];
if (template.access_list_id !== accessList.id)
  throw new Error("omni.syas.ca no longer uses that access list; inspect before proceeding");
const rules = (accessList.clients || []).map((c) => `${c.directive} ${c.address};`);
if (!rules.length || rules.some((r) => !r.startsWith("allow ")) || accessList.satisfy_any)
  throw new Error("The access list is not an allow list of addresses; refusing");

const live = (await ProxyHost.query().where("is_deleted", 0)).filter((h) =>
  h.domain_names.includes(domain),
);
const existing = live[0];
const wanted = {
  domain_names: [domain],
  forward_scheme: "http",
  forward_host: template.forward_host,
  forward_port: template.forward_port,
  access_list_id: accessList.id,
  certificate_id: template.certificate_id,
  ssl_forced: true,
  http2_support: true,
  block_exploits: true,
  caching_enabled: false,
  allow_websocket_upgrade: false,
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
    upstream: `${host.forward_scheme}://${host.forward_host}:${host.forward_port}`,
    certificate: host.certificate_id,
    accessList: host.access_list_id,
    ours: (host.advanced_config || "").includes(marker),
    advancedHash: hash(host.advanced_config || ""),
  };
// NPM writes the access list into the generated server block; a host whose
// file lacks the rules would be open to every peer NPM answers.
const assertGuarded = async (id) => {
  const conf = await readFile(`/data/nginx/proxy_host/${id}.conf`, "utf8");
  for (const line of [...rules, "deny all;", ...realIp.split("\n")])
    if (!conf.includes(line)) throw new Error(`Generated config lacks "${line}"; run rollback`);
};

if (mode === "inspect") {
  console.log(
    JSON.stringify({
      existing: summary(existing) ?? null,
      wantedAdvancedHash: hash(advanced),
      accessList: { id: accessList.id, name: accessList.name, rules },
    }),
  );
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
  if (existing && !(existing.advanced_config || "").includes(marker))
    throw new Error(`${domain} exists and was not created by this script; refusing`);
  if (
    existing &&
    existing.advanced_config === advanced &&
    existing.access_list_id === accessList.id
  ) {
    await assertGuarded(existing.id);
    console.log(JSON.stringify({ mode, unchanged: true, host: summary(existing) }));
    process.exit(0);
  }
  // The proposed server block must be valid nginx before NPM writes anything.
  const candidate = `/tmp/omni-recipes-${process.pid}.conf`;
  await writeFile(
    candidate,
    `user root; pid /tmp/omni-recipes-test.pid; events {} http { server { listen 127.0.0.1:47893;\n${advanced}\nlocation / { return 204; } } }`,
    { mode: 0o600 },
  );
  execFileSync("nginx", ["-t", "-c", candidate], { stdio: "pipe" });
  await unlink(candidate);
  const saved = existing
    ? await proxyHosts.update(access, { id: existing.id, ...wanted })
    : await proxyHosts.create(access, wanted);
  if (saved.meta?.nginx_online === false)
    throw new Error("NPM rejected the nginx configuration; run rollback");
  execFileSync("nginx", ["-t"], { stdio: "pipe" });
  await writeFile(recordPath, JSON.stringify({ id: saved.id, domain }), { mode: 0o600 });
  await assertGuarded(saved.id);
  const verified = await ProxyHost.query().findById(saved.id);
  console.log(JSON.stringify({ mode, [existing ? "updated" : "created"]: summary(verified) }));
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
  throw new Error("The recorded host is not the recipes host; refusing");
await proxyHosts.delete(access, { id: host.id });
execFileSync("nginx", ["-t"], { stdio: "pipe" });
console.log(JSON.stringify({ mode, deleted: host.id }));
process.exit(0);
