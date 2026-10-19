//! Pure decisions for the gateway page: validation, request bodies, labels
//! and setup snippets. The server stays authoritative; these rules only let
//! mistakes show before a round trip.

use std::collections::BTreeSet;

use omni_api::gateway::{
    AuthKind, BundledServerView, CredentialsUpdate, FeedState, GatewayErrorKind, GatewayOverview,
    GatewayToolView, IntegrationConfigView, IntegrationCreate, IntegrationDetail, IntegrationPatch,
    IntegrationStatus, IntegrationSummary, SecretPair, ToolVisibilityUpdate, TransportInput,
    TransportKind,
};
use omni_web_kit::components::StatusKind;
use omni_web_kit::utils::format::format_relative_at;

/// Integration ids are at most this many characters.
pub const MAX_ID_CHARS: usize = 30;
pub const MAX_NAME_CHARS: usize = 100;
pub const MAX_LABEL_CHARS: usize = 80;
pub const MIN_TIMEOUT: u32 = 5;
pub const MAX_TIMEOUT: u32 = 600;
pub const DEFAULT_TIMEOUT: u32 = 120;
/// The variable Codex reads the client token from.
pub const CODEX_TOKEN_ENV: &str = "OMNI_GATEWAY_TOKEN";
pub const TOKEN_PLACEHOLDER: &str = "<token>";
const AUTHORIZATION: &str = "Authorization";

/// How long ago a past event was. The page clock ticks every few seconds,
/// so an event newer than the last tick reads "just now", never "in 7s".
pub fn ago(at: f64, now: f64) -> String {
    // A millisecond into the past: `format_relative_at` reads 0 as future.
    format_relative_at(at.min(now - 1.0), now)
}

/// `"1 tool"`, `"3 tools"`.
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// ----- ids, names, URLs, timeouts -----

/// Why `id` cannot name an integration (`^[a-z][a-z0-9_]{0,29}$`, no `__`,
/// no trailing `_`, unused by `existing`).
pub fn id_problem(id: &str, existing: &[String]) -> Option<String> {
    let first = id.chars().next()?;
    if !first.is_ascii_lowercase() {
        return Some("Start with a lowercase letter".to_owned());
    }
    if id.chars().count() > MAX_ID_CHARS {
        return Some(format!("At most {MAX_ID_CHARS} characters"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Some("Use lowercase letters, digits and _ only".to_owned());
    }
    if id.contains("__") {
        return Some("No double underscore: tools are named <id>__<tool>".to_owned());
    }
    if id.ends_with('_') {
        return Some("Do not end with _".to_owned());
    }
    if existing.iter().any(|e| e == id) {
        return Some("Another integration uses this id".to_owned());
    }
    None
}

/// An id derived from a display name ("Linear (work)" -> `linear_work`).
pub fn suggest_id(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().map(|c| c.to_ascii_lowercase()) {
        if c.is_ascii_lowercase() || (c.is_ascii_digit() && !out.is_empty()) {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    let mut out: String = out.chars().take(MAX_ID_CHARS).collect();
    while out.ends_with('_') {
        out.pop();
    }
    out
}

pub fn name_problem(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        Some("Enter a name".to_owned())
    } else if name.chars().count() > MAX_NAME_CHARS {
        Some(format!("At most {MAX_NAME_CHARS} characters"))
    } else {
        None
    }
}

fn strip_scheme<'a>(url: &'a str, scheme: &str) -> Option<&'a str> {
    let head = url.get(..scheme.len())?;
    head.eq_ignore_ascii_case(scheme)
        .then(|| &url[scheme.len()..])
}

/// Why `url` is not a usable remote MCP endpoint (https only, no
/// credentials in the URL).
pub fn url_problem(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return Some("Enter the server's MCP URL".to_owned());
    }
    let Some(rest) = strip_scheme(url, "https://") else {
        return Some("Use an https:// URL".to_owned());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Some("Leave credentials out of the URL; add them as headers".to_owned());
    }
    if authority.is_empty() || authority.starts_with(':') || url.chars().any(char::is_whitespace) {
        return Some("Enter a complete URL, such as https://example.com/mcp".to_owned());
    }
    None
}

/// `scheme://host[:port]`, lowercased, default ports dropped.
pub fn url_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => ":443",
        "http" => ":80",
        _ => "",
    };
    let host = match authority.strip_suffix(default_port) {
        Some(host) if !default_port.is_empty() => host.to_owned(),
        _ => authority,
    };
    (!host.is_empty()).then(|| format!("{scheme}://{host}"))
}

pub fn parse_timeout(input: &str) -> Result<u32, String> {
    input
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|n| (MIN_TIMEOUT..=MAX_TIMEOUT).contains(n))
        .ok_or_else(|| format!("Enter {MIN_TIMEOUT} to {MAX_TIMEOUT} seconds"))
}

pub fn label_problem(label: &str) -> Option<String> {
    let label = label.trim();
    if label.is_empty() {
        Some("Name the client, such as \u{201c}Laptop Claude Code\u{201d}".to_owned())
    } else if label.chars().count() > MAX_LABEL_CHARS {
        Some(format!("At most {MAX_LABEL_CHARS} characters"))
    } else {
        None
    }
}

// ----- auth kinds -----

/// The auth kinds a transport can use: headers and OAuth go over HTTP,
/// environment variables only reach a bundled stdio server.
pub fn auth_kinds(transport: TransportKind) -> &'static [AuthKind] {
    match transport {
        TransportKind::Http => &[AuthKind::None, AuthKind::Headers, AuthKind::OAuth],
        TransportKind::Stdio => &[AuthKind::None, AuthKind::Env],
    }
}

pub fn auth_label(kind: AuthKind) -> &'static str {
    match kind {
        AuthKind::None => "None",
        AuthKind::Headers => "Headers",
        AuthKind::OAuth => "OAuth",
        AuthKind::Env => "Environment",
    }
}

/// The short mono tag in rows.
pub fn auth_tag(kind: AuthKind) -> &'static str {
    match kind {
        AuthKind::None => "no auth",
        AuthKind::Headers => "headers",
        AuthKind::OAuth => "oauth",
        AuthKind::Env => "env",
    }
}

pub fn auth_help(kind: AuthKind) -> &'static str {
    match kind {
        AuthKind::None => "Requests go out without credentials.",
        AuthKind::Headers => {
            "Static request headers, such as an API key sent as a bearer token. Values are write-only."
        }
        AuthKind::OAuth => {
            "Sign in through the server's OAuth page; Omni stores and refreshes the tokens."
        }
        AuthKind::Env => {
            "Environment variables for the bundled server process. Values are write-only."
        }
    }
}

pub fn transport_label(kind: TransportKind) -> &'static str {
    match kind {
        TransportKind::Http => "Remote (HTTP)",
        TransportKind::Stdio => "Bundled server",
    }
}

// ----- status words -----

pub fn integration_status(status: IntegrationStatus) -> (StatusKind, &'static str) {
    match status {
        IntegrationStatus::Ready => (StatusKind::Ok, "Ready"),
        IntegrationStatus::Pending => (StatusKind::Info, "Pending"),
        IntegrationStatus::Stale => (StatusKind::Stale, "Stale"),
        IntegrationStatus::NeedsAuth => (StatusKind::Warn, "Needs auth"),
        IntegrationStatus::Error => (StatusKind::Fault, "Error"),
        IntegrationStatus::Disabled => (StatusKind::Idle, "Disabled"),
    }
}

/// Whether the integration is enabled and not serving as it should.
pub fn needs_attention(summary: &IntegrationSummary) -> bool {
    summary.enabled
        && matches!(
            summary.status,
            IntegrationStatus::Stale | IntegrationStatus::NeedsAuth | IntegrationStatus::Error
        )
}

pub fn feed_status(state: FeedState) -> (StatusKind, &'static str) {
    match state {
        FeedState::Active => (StatusKind::Ok, "Active"),
        FeedState::Idle => (StatusKind::Idle, "Idle"),
        FeedState::Paused => (StatusKind::Warn, "Paused"),
        FeedState::Error => (StatusKind::Fault, "Error"),
    }
}

/// What went wrong, as a headline (the message is the detail).
pub fn error_kind_label(kind: GatewayErrorKind) -> &'static str {
    match kind {
        GatewayErrorKind::Network => "Could not reach the server",
        GatewayErrorKind::Timeout => "The server timed out",
        GatewayErrorKind::AuthRequired => "Sign-in required",
        GatewayErrorKind::AuthRejected => "Credentials rejected",
        GatewayErrorKind::Protocol => "Not a valid MCP response",
        GatewayErrorKind::Upstream => "The server returned an error",
        GatewayErrorKind::Process => "The server process failed",
        GatewayErrorKind::Config => "Configuration problem",
        GatewayErrorKind::Unsupported => "Not supported",
    }
}

/// The one-click fix offered beside an integration's problem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fix {
    /// Start (or restart) the OAuth sign-in.
    Connect,
    /// Enter or replace stored headers or variables.
    Credentials,
    /// The server wants credentials but the auth kind is none.
    ChooseAuth,
}

pub fn fix_for(summary: &IntegrationSummary) -> Option<Fix> {
    let auth_problem = summary.status == IntegrationStatus::NeedsAuth
        || summary.error.as_ref().is_some_and(|e| {
            matches!(
                e.kind,
                GatewayErrorKind::AuthRequired | GatewayErrorKind::AuthRejected
            )
        });
    if !summary.enabled || !auth_problem {
        return None;
    }
    Some(match summary.auth {
        AuthKind::OAuth => Fix::Connect,
        AuthKind::Headers | AuthKind::Env => Fix::Credentials,
        AuthKind::None => Fix::ChooseAuth,
    })
}

/// The host of a URL-shaped OAuth client id (a metadata document); other
/// ids are opaque registrations, named as such.
pub fn client_host(client_id: &str) -> String {
    ["https://", "http://"]
        .iter()
        .find_map(|scheme| strip_scheme(client_id, scheme))
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .map(|authority| authority.rsplit('@').next().unwrap_or(authority))
        .filter(|host| !host.is_empty())
        .unwrap_or("self-registered app")
        .to_owned()
}

// ----- endpoint, snippets, overview -----

pub fn lan_url(origin: &str, path: &str) -> String {
    format!("{}{path}", origin.trim_end_matches('/'))
}

pub fn claude_code_command(url: &str, token: &str) -> String {
    format!("claude mcp add --transport http omni {url} --header \"Authorization: Bearer {token}\"")
}

pub fn codex_config(url: &str) -> String {
    format!("[mcp_servers.omni]\nurl = \"{url}\"\nbearer_token_env_var = \"{CODEX_TOKEN_ENV}\"")
}

/// The page lede: what the endpoint serves now.
pub fn lede(overview: &GatewayOverview) -> String {
    let from_integrations = overview.exposed_tools.saturating_sub(overview.native_tools) as usize;
    let integrations = overview.integrations.iter().filter(|i| i.enabled).count();
    format!(
        "One MCP endpoint for agents: {} now, {} of Omni's own and {} from {}.",
        plural(overview.exposed_tools as usize, "tool", "tools"),
        overview.native_tools,
        from_integrations,
        plural(integrations, "enabled integration", "enabled integrations"),
    )
}

/// The figures the readout band shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub native: u32,
    pub exposed: u32,
    pub integrations: usize,
    pub ready: usize,
    pub attention: usize,
    pub tokens: usize,
    pub grants: usize,
    pub pending: usize,
    pub feeds: usize,
    pub active_feeds: usize,
    pub gaps: u64,
}

impl Counts {
    pub fn of(overview: &GatewayOverview) -> Self {
        Self {
            native: overview.native_tools,
            exposed: overview.exposed_tools,
            integrations: overview.integrations.len(),
            ready: overview
                .integrations
                .iter()
                .filter(|i| i.status == IntegrationStatus::Ready)
                .count(),
            attention: overview
                .integrations
                .iter()
                .filter(|i| needs_attention(i))
                .count(),
            tokens: overview.clients.len(),
            grants: overview.grants.len(),
            pending: overview.pending.len(),
            feeds: overview.event_feeds.len(),
            active_feeds: overview
                .event_feeds
                .iter()
                .filter(|f| f.state == FeedState::Active)
                .count(),
            gaps: overview.event_feeds.iter().map(|f| u64::from(f.gaps)).sum(),
        }
    }
}

/// The toast after a manual refresh: the report when there is one, else
/// the summary's error.
pub fn refresh_outcome(detail: &IntegrationDetail) -> (String, bool) {
    let summary = &detail.summary;
    let ok = detail
        .last_refresh
        .as_ref()
        .map_or(summary.error.is_none(), |r| r.ok);
    match (ok, &detail.last_refresh) {
        (true, report) => {
            let mut parts = vec![plural(summary.tool_count as usize, "tool", "tools")];
            if let Some(report) = report {
                for (n, word) in [
                    (report.added.len(), "added"),
                    (report.removed.len(), "removed"),
                    (report.changed.len(), "changed"),
                ] {
                    if n > 0 {
                        parts.push(format!("{n} {word}"));
                    }
                }
            }
            (
                format!("Refreshed {}: {}", summary.name, parts.join(", ")),
                true,
            )
        }
        (false, _) => {
            let why = summary
                .error
                .as_ref()
                .map_or("see the integration for details", |e| e.message.as_str());
            (format!("{} did not refresh: {why}", summary.name), false)
        }
    }
}

// ----- tools -----

pub fn hidden_tools(tools: &[GatewayToolView]) -> BTreeSet<String> {
    tools
        .iter()
        .filter(|t| !t.visible)
        .map(|t| t.name.clone())
        .collect()
}

/// Every word of `query` appears in the tool's names, title or description.
pub fn tool_matches(tool: &GatewayToolView, query: &str) -> bool {
    let haystack = format!(
        "{} {} {} {}",
        tool.name,
        tool.exposed_name,
        tool.title.as_deref().unwrap_or_default(),
        tool.description.as_deref().unwrap_or_default()
    )
    .to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

/// `hidden` with every tool in `names` shown (`visible`) or hidden.
pub fn set_visibility<'a>(
    hidden: &BTreeSet<String>,
    names: impl IntoIterator<Item = &'a String>,
    visible: bool,
) -> BTreeSet<String> {
    let mut next = hidden.clone();
    for name in names {
        if visible {
            next.remove(name);
        } else {
            next.insert(name.clone());
        }
    }
    next
}

/// The PUT body: the full hidden list, limited to tools that still exist.
pub fn visibility_update(
    tools: &[GatewayToolView],
    hidden: &BTreeSet<String>,
) -> ToolVisibilityUpdate {
    ToolVisibilityUpdate {
        hidden: tools
            .iter()
            .filter(|t| hidden.contains(&t.name))
            .map(|t| t.name.clone())
            .collect(),
    }
}

// ----- credentials -----

/// One credential input row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretKind {
    /// `Authorization: Bearer <value>`.
    Bearer,
    /// A named request header.
    Header,
    /// An environment variable; `required` ones come from the bundled server.
    Env { required: bool },
}

#[derive(Clone, PartialEq, Eq)]
pub struct SecretRow {
    /// Stable key for keyed rendering.
    pub key: u32,
    pub kind: SecretKind,
    pub name: String,
    pub value: String,
}

impl std::fmt::Debug for SecretRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.kind {
            SecretKind::Bearer => "bearer",
            SecretKind::Header => "header",
            SecretKind::Env { required: true } => "env (required)",
            SecretKind::Env { required: false } => "env",
        };
        f.debug_struct("SecretRow")
            .field("key", &self.key)
            .field("kind", &kind)
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// Write-only credential inputs. Saving replaces every stored value, so
/// rows start from the stored names with empty values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CredentialDraft {
    pub rows: Vec<SecretRow>,
    next_key: u32,
}

fn header_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn env_name_ok(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl CredentialDraft {
    fn push(&mut self, kind: SecretKind, name: &str) {
        self.rows.push(SecretRow {
            key: self.next_key,
            kind,
            name: name.to_owned(),
            value: String::new(),
        });
        self.next_key += 1;
    }

    /// Stored header names, `Authorization` as the bearer shortcut; one
    /// bearer row when nothing is stored.
    pub fn for_headers(stored: &[String]) -> Self {
        let mut draft = Self::default();
        for name in stored {
            if name.eq_ignore_ascii_case(AUTHORIZATION) && !draft.has_bearer() {
                draft.push(SecretKind::Bearer, AUTHORIZATION);
            } else {
                draft.push(SecretKind::Header, name);
            }
        }
        if draft.rows.is_empty() {
            draft.push(SecretKind::Bearer, AUTHORIZATION);
        }
        draft
    }

    /// The bundled server's variables first, then other stored names.
    pub fn for_env(required: &[String], stored: &[String]) -> Self {
        let mut draft = Self::default();
        for name in required {
            draft.push(SecretKind::Env { required: true }, name);
        }
        for name in stored.iter().filter(|n| !required.contains(n)) {
            draft.push(SecretKind::Env { required: false }, name);
        }
        if draft.rows.is_empty() {
            draft.push(SecretKind::Env { required: false }, "");
        }
        draft
    }

    pub fn for_auth(auth: AuthKind, required_env: &[String], stored: &[String]) -> Self {
        match auth {
            AuthKind::Headers => Self::for_headers(stored),
            AuthKind::Env => Self::for_env(required_env, stored),
            AuthKind::None | AuthKind::OAuth => Self::default(),
        }
    }

    pub fn has_bearer(&self) -> bool {
        self.rows.iter().any(|r| {
            r.kind == SecretKind::Bearer
                || (r.kind == SecretKind::Header
                    && r.name.trim().eq_ignore_ascii_case(AUTHORIZATION))
        })
    }

    pub fn add_header(&mut self) {
        self.push(SecretKind::Header, "");
    }

    pub fn add_bearer(&mut self) {
        if !self.has_bearer() {
            self.push(SecretKind::Bearer, AUTHORIZATION);
        }
    }

    pub fn add_env(&mut self) {
        self.push(SecretKind::Env { required: false }, "");
    }

    pub fn remove(&mut self, key: u32) {
        self.rows.retain(|r| r.key != key);
    }

    pub fn set_name(&mut self, key: u32, name: String) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.key == key) {
            row.name = name;
        }
    }

    pub fn set_value(&mut self, key: u32, value: String) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.key == key) {
            row.value = value;
        }
    }

    /// Whether anything was typed into a value.
    pub fn has_input(&self) -> bool {
        self.rows.iter().any(|r| !r.value.trim().is_empty())
    }

    /// Forgets every typed value (after saving or on cancel).
    pub fn clear_values(&mut self) {
        for row in &mut self.rows {
            row.value.clear();
        }
    }

    /// The PUT body for `auth`. Blank optional rows are skipped; every other
    /// row needs a valid name and a value.
    pub fn build(&self, auth: AuthKind) -> Result<CredentialsUpdate, String> {
        let mut pairs: Vec<SecretPair> = Vec::new();
        for row in &self.rows {
            let name = row.name.trim();
            let value = row.value.trim();
            let required = matches!(row.kind, SecretKind::Env { required: true });
            if name.is_empty() && value.is_empty() && !required {
                continue;
            }
            if value.contains(['\r', '\n']) {
                return Err(format!("{name}: the value must be one line"));
            }
            let pair = match &row.kind {
                SecretKind::Bearer => {
                    let token = strip_scheme(value, "Bearer ").unwrap_or(value).trim();
                    if token.is_empty() {
                        return Err("Enter the bearer token".to_owned());
                    }
                    if token.chars().any(char::is_whitespace) {
                        return Err("A bearer token has no spaces".to_owned());
                    }
                    SecretPair {
                        name: AUTHORIZATION.to_owned(),
                        value: format!("Bearer {token}"),
                    }
                }
                SecretKind::Header => {
                    if !header_name_ok(name) {
                        return Err(if name.is_empty() {
                            "Name every header".to_owned()
                        } else {
                            format!("\u{201c}{name}\u{201d} is not a valid header name")
                        });
                    }
                    if value.is_empty() {
                        return Err(format!("Enter a value for {name}"));
                    }
                    SecretPair {
                        name: name.to_owned(),
                        value: value.to_owned(),
                    }
                }
                SecretKind::Env { .. } => {
                    if !env_name_ok(name) {
                        return Err(if name.is_empty() {
                            "Name every variable".to_owned()
                        } else {
                            format!(
                                "\u{201c}{name}\u{201d} is not a valid variable name (letters, digits and _)"
                            )
                        });
                    }
                    if value.is_empty() {
                        return Err(format!("Enter a value for {name}"));
                    }
                    SecretPair {
                        name: name.to_owned(),
                        value: value.to_owned(),
                    }
                }
            };
            let duplicate = pairs.iter().any(|p| match row.kind {
                SecretKind::Env { .. } => p.name == pair.name,
                _ => p.name.eq_ignore_ascii_case(&pair.name),
            });
            if duplicate {
                return Err(format!("{} appears twice", pair.name));
            }
            pairs.push(pair);
        }
        if pairs.is_empty() {
            return Err(match auth {
                AuthKind::Env => "Enter at least one variable".to_owned(),
                _ => "Enter at least one header".to_owned(),
            });
        }
        Ok(match auth {
            AuthKind::Env => CredentialsUpdate {
                headers: Vec::new(),
                env: pairs,
            },
            _ => CredentialsUpdate {
                headers: pairs,
                env: Vec::new(),
            },
        })
    }
}

/// Stored names for an auth kind.
pub fn stored_names(auth: AuthKind, config: &IntegrationConfigView) -> Vec<String> {
    match auth {
        AuthKind::Headers => config.header_names.clone(),
        AuthKind::Env => config.env_names.clone(),
        AuthKind::None | AuthKind::OAuth => Vec::new(),
    }
}

fn has_stored_credentials(config: &IntegrationConfigView) -> bool {
    !config.header_names.is_empty()
        || !config.env_names.is_empty()
        || config
            .oauth
            .as_ref()
            .is_some_and(|o| o.connected || o.refreshable)
}

// ----- create -----

/// The add form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateDraft {
    pub id: String,
    /// The id was typed rather than derived from the name.
    pub id_edited: bool,
    pub name: String,
    pub transport: TransportKind,
    pub url: String,
    /// Bundled server id.
    pub server: String,
    pub auth: AuthKind,
    pub timeout: String,
}

impl Default for CreateDraft {
    fn default() -> Self {
        Self {
            id: String::new(),
            id_edited: false,
            name: String::new(),
            transport: TransportKind::Http,
            url: String::new(),
            server: String::new(),
            auth: AuthKind::None,
            timeout: DEFAULT_TIMEOUT.to_string(),
        }
    }
}

impl CreateDraft {
    pub fn set_name(&mut self, name: String) {
        if !self.id_edited {
            self.id = suggest_id(&name);
        }
        self.name = name;
    }

    pub fn set_id(&mut self, id: String) {
        self.id_edited = !id.is_empty();
        self.id = id;
    }

    /// Keeps the auth kind valid for the transport: a bundled server that
    /// needs variables defaults to environment auth.
    pub fn set_transport(&mut self, transport: TransportKind, bundled: &[BundledServerView]) {
        self.transport = transport;
        if transport == TransportKind::Stdio && self.server.is_empty() {
            self.server = bundled
                .iter()
                .find(|b| b.available)
                .map(|b| b.id.clone())
                .unwrap_or_default();
        }
        self.fit_auth(bundled);
    }

    pub fn set_server(&mut self, server: String, bundled: &[BundledServerView]) {
        self.server = server;
        self.fit_auth(bundled);
    }

    fn fit_auth(&mut self, bundled: &[BundledServerView]) {
        if self.transport == TransportKind::Stdio {
            let needs_env = bundled
                .iter()
                .find(|b| b.id == self.server)
                .is_some_and(|b| !b.env.is_empty());
            if needs_env || self.auth == AuthKind::Env {
                self.auth = if needs_env {
                    AuthKind::Env
                } else {
                    AuthKind::None
                };
            }
        }
        if !auth_kinds(self.transport).contains(&self.auth) {
            self.auth = AuthKind::None;
        }
    }

    /// The selected bundled server's required variables.
    pub fn required_env<'a>(&self, bundled: &'a [BundledServerView]) -> &'a [String] {
        if self.transport != TransportKind::Stdio {
            return &[];
        }
        bundled
            .iter()
            .find(|b| b.id == self.server)
            .map_or(&[], |b| b.env.as_slice())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateProblems {
    pub id: Option<String>,
    pub name: Option<String>,
    pub url: Option<String>,
    pub server: Option<String>,
    pub timeout: Option<String>,
}

impl CreateProblems {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

pub fn create_problems(
    draft: &CreateDraft,
    existing_ids: &[String],
    bundled: &[BundledServerView],
) -> CreateProblems {
    let id = if draft.id.is_empty() {
        Some("Enter an id".to_owned())
    } else {
        id_problem(&draft.id, existing_ids)
    };
    let (url, server) = match draft.transport {
        TransportKind::Http => (url_problem(&draft.url), None),
        TransportKind::Stdio => {
            let server = match bundled.iter().find(|b| b.id == draft.server) {
                None => Some("Choose a bundled server".to_owned()),
                Some(b) if !b.available => {
                    Some(format!("{} is not installed in this image", b.name))
                }
                Some(_) => None,
            };
            (None, server)
        }
    };
    CreateProblems {
        id,
        name: name_problem(&draft.name),
        url,
        server,
        timeout: parse_timeout(&draft.timeout).err(),
    }
}

/// The create body; call only when [`create_problems`] is empty.
pub fn build_create(draft: &CreateDraft) -> IntegrationCreate {
    let timeout = parse_timeout(&draft.timeout).unwrap_or(DEFAULT_TIMEOUT);
    IntegrationCreate {
        id: draft.id.clone(),
        name: draft.name.trim().to_owned(),
        transport: match draft.transport {
            TransportKind::Http => TransportInput::Http {
                url: draft.url.trim().to_owned(),
            },
            TransportKind::Stdio => TransportInput::Stdio {
                server: draft.server.clone(),
            },
        },
        auth: draft.auth,
        timeout_seconds: (timeout != DEFAULT_TIMEOUT).then_some(timeout),
        enabled: None,
    }
}

// ----- edit -----

/// The settings form of an existing integration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsDraft {
    pub name: String,
    pub url: String,
    pub auth: AuthKind,
    pub timeout: String,
}

impl SettingsDraft {
    pub fn of(summary: &IntegrationSummary, config: &IntegrationConfigView) -> Self {
        Self {
            name: summary.name.clone(),
            url: config.url.clone().unwrap_or_default(),
            auth: summary.auth,
            timeout: config.timeout_seconds.to_string(),
        }
    }
}

/// Only the fields that differ; `Err` names the first invalid field.
pub fn build_patch(
    summary: &IntegrationSummary,
    config: &IntegrationConfigView,
    draft: &SettingsDraft,
) -> Result<IntegrationPatch, String> {
    if let Some(problem) = name_problem(&draft.name) {
        return Err(problem);
    }
    let http = summary.transport == TransportKind::Http;
    let url = draft.url.trim();
    if http && let Some(problem) = url_problem(url) {
        return Err(problem);
    }
    let timeout = parse_timeout(&draft.timeout)?;
    let name = draft.name.trim();
    Ok(IntegrationPatch {
        name: (name != summary.name).then(|| name.to_owned()),
        url: (http && Some(url) != config.url.as_deref()).then(|| url.to_owned()),
        auth: (draft.auth != summary.auth).then_some(draft.auth),
        timeout_seconds: (timeout != config.timeout_seconds).then_some(timeout),
        enabled: None,
    })
}

/// Why saving `draft` would clear the stored credentials, if it would.
pub fn clears_credentials(
    summary: &IntegrationSummary,
    config: &IntegrationConfigView,
    draft: &SettingsDraft,
) -> Option<&'static str> {
    if !has_stored_credentials(config) {
        return None;
    }
    if draft.auth != summary.auth {
        return Some("the auth kind changes");
    }
    let before = config.url.as_deref().and_then(url_origin);
    let after = url_origin(&draft.url);
    (summary.transport == TransportKind::Http && after.is_some() && before != after)
        .then_some("the URL points at another origin")
}

#[cfg(test)]
mod tests {
    use omni_api::gateway::{
        GatewayErrorView, OAuthConnectionView, RefreshReport, UpstreamServerView,
    };

    use super::*;

    fn summary(id: &str) -> IntegrationSummary {
        IntegrationSummary {
            id: id.into(),
            name: "Linear".into(),
            enabled: true,
            transport: TransportKind::Http,
            endpoint: "https://mcp.linear.app/mcp".into(),
            auth: AuthKind::Headers,
            status: IntegrationStatus::Ready,
            tool_count: 3,
            visible_tool_count: 3,
            resource_count: 0,
            event_count: 0,
            last_refresh_at: None,
            last_success_at: None,
            error: None,
        }
    }

    fn config() -> IntegrationConfigView {
        IntegrationConfigView {
            url: Some("https://mcp.linear.app/mcp".into()),
            bundled: None,
            header_names: vec!["Authorization".into()],
            env_names: Vec::new(),
            credentials_set: true,
            oauth: None,
            timeout_seconds: 120,
        }
    }

    fn tool(name: &str, visible: bool) -> GatewayToolView {
        GatewayToolView {
            name: name.into(),
            exposed_name: format!("linear__{name}"),
            title: None,
            description: Some("Create an issue in a team".into()),
            visible,
            read_only: None,
            destructive: None,
            open_world: None,
            change: None,
        }
    }

    fn bundled(id: &str, env: &[&str], available: bool) -> BundledServerView {
        BundledServerView {
            id: id.into(),
            name: id.to_uppercase(),
            package: format!("@example/{id}"),
            env: env.iter().map(|e| (*e).to_owned()).collect(),
            available,
        }
    }

    #[test]
    fn ids_follow_the_tool_prefix_rules() {
        assert_eq!(id_problem("linear", &[]), None);
        assert_eq!(id_problem("gh_2", &[]), None);
        assert!(id_problem("2gh", &[]).is_some());
        assert!(id_problem("Linear", &[]).is_some());
        assert!(id_problem("lin-ear", &[]).is_some());
        assert!(id_problem("lin__ear", &[]).is_some());
        assert!(id_problem("linear_", &[]).is_some());
        assert!(id_problem(&"a".repeat(31), &[]).is_some());
        assert_eq!(id_problem(&"a".repeat(30), &[]), None);
        assert!(id_problem("linear", &["linear".to_owned()]).is_some());
    }

    #[test]
    fn suggested_ids_are_valid() {
        assert_eq!(suggest_id("Linear (work)"), "linear_work");
        assert_eq!(suggest_id("  GitHub  Copilot! "), "github_copilot");
        assert_eq!(suggest_id("1Password"), "password");
        assert_eq!(suggest_id("Notion 2"), "notion_2");
        assert_eq!(suggest_id("Café"), "caf");
        assert_eq!(suggest_id("!!!"), "");
        let long = suggest_id(&"word ".repeat(20));
        assert!(long.len() <= MAX_ID_CHARS);
        assert_eq!(id_problem(&long, &[]), None);
    }

    #[test]
    fn remote_urls_must_be_https_without_credentials() {
        assert_eq!(url_problem(" https://mcp.linear.app/mcp "), None);
        assert_eq!(url_problem("HTTPS://example.com"), None);
        assert!(url_problem("http://example.com/mcp").is_some());
        assert!(url_problem("https://").is_some());
        assert!(url_problem("https://:443/mcp").is_some());
        assert!(url_problem("https://user:key@example.com/mcp").is_some());
        assert!(url_problem("https://exa mple.com").is_some());
        assert!(url_problem("").is_some());
    }

    #[test]
    fn origins_ignore_paths_case_and_default_ports() {
        assert_eq!(
            url_origin("https://MCP.example.com:443/a?b").as_deref(),
            Some("https://mcp.example.com")
        );
        assert_eq!(
            url_origin("https://example.com:8443/mcp").as_deref(),
            Some("https://example.com:8443")
        );
        assert_eq!(url_origin("not a url"), None);
    }

    #[test]
    fn timeouts_are_bounded() {
        assert_eq!(parse_timeout(" 120 "), Ok(120));
        assert_eq!(parse_timeout("5"), Ok(5));
        assert_eq!(parse_timeout("600"), Ok(600));
        assert!(parse_timeout("4").is_err());
        assert!(parse_timeout("601").is_err());
        assert!(parse_timeout("1.5").is_err());
    }

    #[test]
    fn env_auth_is_only_for_bundled_servers() {
        assert!(!auth_kinds(TransportKind::Http).contains(&AuthKind::Env));
        assert!(auth_kinds(TransportKind::Stdio).contains(&AuthKind::Env));
        let servers = [bundled("github", &["GITHUB_TOKEN"], true)];
        let mut draft = CreateDraft::default();
        draft.set_transport(TransportKind::Stdio, &servers);
        assert_eq!(draft.server, "github");
        assert_eq!(draft.auth, AuthKind::Env);
        assert_eq!(draft.required_env(&servers), ["GITHUB_TOKEN".to_owned()]);
        draft.set_transport(TransportKind::Http, &servers);
        assert_eq!(draft.auth, AuthKind::None);
        assert!(draft.required_env(&servers).is_empty());
        draft.auth = AuthKind::OAuth;
        draft.set_transport(TransportKind::Stdio, &servers);
        assert_eq!(draft.auth, AuthKind::Env);
    }

    #[test]
    fn names_fill_the_id_until_it_is_typed() {
        let mut draft = CreateDraft::default();
        draft.set_name("Linear".into());
        assert_eq!(draft.id, "linear");
        draft.set_id("lin".into());
        draft.set_name("Linear work".into());
        assert_eq!(draft.id, "lin");
        draft.set_id(String::new());
        draft.set_name("Linear work".into());
        assert_eq!(draft.id, "linear_work");
    }

    #[test]
    fn create_checks_fields_and_omits_the_default_timeout() {
        let servers = [
            bundled("github", &["GITHUB_TOKEN"], true),
            bundled("slack", &[], false),
        ];
        let mut draft = CreateDraft::default();
        draft.set_name(" Linear ".into());
        draft.url = "https://mcp.linear.app/mcp".into();
        assert!(create_problems(&draft, &[], &servers).is_empty());
        let body = build_create(&draft);
        assert_eq!(body.name, "Linear");
        assert_eq!(body.timeout_seconds, None);
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({
                "id": "linear",
                "name": "Linear",
                "transport": {"kind": "http", "url": "https://mcp.linear.app/mcp"},
                "auth": "none"
            })
        );
        draft.timeout = "30".into();
        assert_eq!(build_create(&draft).timeout_seconds, Some(30));
        assert!(
            create_problems(&draft, &["linear".to_owned()], &servers)
                .id
                .is_some()
        );
        draft.set_transport(TransportKind::Stdio, &servers);
        draft.set_server("slack".into(), &servers);
        assert!(create_problems(&draft, &[], &servers).server.is_some());
        assert_eq!(create_problems(&draft, &[], &servers).url, None);
    }

    #[test]
    fn bearer_rows_send_an_authorization_header() {
        let mut draft = CredentialDraft::for_headers(&[]);
        assert_eq!(draft.rows.len(), 1);
        let key = draft.rows[0].key;
        draft.set_value(key, " Bearer sk_live_1 ".into());
        assert_eq!(
            draft.build(AuthKind::Headers),
            Ok(CredentialsUpdate {
                headers: vec![SecretPair {
                    name: "Authorization".into(),
                    value: "Bearer sk_live_1".into()
                }],
                env: Vec::new(),
            })
        );
        draft.set_value(key, "two words".into());
        assert!(draft.build(AuthKind::Headers).is_err());
        // Only one bearer row.
        draft.add_bearer();
        assert_eq!(draft.rows.len(), 1);
    }

    #[test]
    fn header_rows_need_valid_unique_names_and_values() {
        let mut draft = CredentialDraft::for_headers(&["Authorization".into(), "X-Team".into()]);
        assert_eq!(draft.rows[0].kind, SecretKind::Bearer);
        assert_eq!(draft.rows[1].name, "X-Team");
        let (bearer, team) = (draft.rows[0].key, draft.rows[1].key);
        draft.set_value(bearer, "tok".into());
        assert!(
            draft.build(AuthKind::Headers).is_err(),
            "X-Team needs a value"
        );
        draft.set_value(team, "eng".into());
        assert_eq!(
            draft.build(AuthKind::Headers).map(|u| u.headers.len()),
            Ok(2)
        );
        draft.add_header();
        let extra = draft.rows[2].key;
        // A blank extra row is ignored.
        assert!(draft.build(AuthKind::Headers).is_ok());
        draft.set_name(extra, "x-team".into());
        draft.set_value(extra, "ops".into());
        assert!(draft.build(AuthKind::Headers).is_err(), "duplicate");
        draft.set_name(extra, "Bad Name".into());
        assert!(draft.build(AuthKind::Headers).is_err());
        draft.remove(extra);
        draft.set_value(team, "a\nb".into());
        assert!(draft.build(AuthKind::Headers).is_err());
    }

    #[test]
    fn env_rows_start_from_the_bundled_variables() {
        let mut draft = CredentialDraft::for_env(
            &["GITHUB_TOKEN".into()],
            &["GITHUB_TOKEN".into(), "GH_HOST".into()],
        );
        assert_eq!(draft.rows.len(), 2);
        assert_eq!(draft.rows[0].kind, SecretKind::Env { required: true });
        let (token, host) = (draft.rows[0].key, draft.rows[1].key);
        draft.set_value(host, "github.example.com".into());
        assert!(
            draft.build(AuthKind::Env).is_err(),
            "required value missing"
        );
        draft.set_value(token, "ghp_1".into());
        let update = draft.build(AuthKind::Env).unwrap();
        assert!(update.headers.is_empty());
        assert_eq!(update.env.len(), 2);
        assert!(format!("{draft:?}").contains("<redacted>"));
        assert!(!format!("{draft:?}").contains("ghp_1"));
        draft.clear_values();
        assert!(!draft.has_input());
    }

    #[test]
    fn patches_carry_only_changed_fields() {
        let s = summary("linear");
        let c = config();
        let mut draft = SettingsDraft::of(&s, &c);
        assert_eq!(build_patch(&s, &c, &draft), Ok(IntegrationPatch::default()));
        draft.name = " Linear (work) ".into();
        draft.timeout = "60".into();
        assert_eq!(
            build_patch(&s, &c, &draft),
            Ok(IntegrationPatch {
                name: Some("Linear (work)".into()),
                timeout_seconds: Some(60),
                ..IntegrationPatch::default()
            })
        );
        draft.url = "http://insecure.example".into();
        assert!(build_patch(&s, &c, &draft).is_err());
        let mut stdio = summary("gh");
        stdio.transport = TransportKind::Stdio;
        let mut draft = SettingsDraft::of(&stdio, &c);
        draft.url = String::new();
        assert_eq!(
            build_patch(&stdio, &c, &draft),
            Ok(IntegrationPatch::default())
        );
    }

    #[test]
    fn origin_or_auth_changes_warn_about_cleared_credentials() {
        let s = summary("linear");
        let c = config();
        let mut draft = SettingsDraft::of(&s, &c);
        draft.url = "https://MCP.linear.app/v2/mcp".into();
        assert_eq!(clears_credentials(&s, &c, &draft), None);
        draft.url = "https://other.example/mcp".into();
        assert!(clears_credentials(&s, &c, &draft).is_some());
        draft.url = c.url.clone().unwrap();
        draft.auth = AuthKind::OAuth;
        assert!(clears_credentials(&s, &c, &draft).is_some());
        let bare = IntegrationConfigView {
            header_names: Vec::new(),
            ..config()
        };
        assert_eq!(clears_credentials(&s, &bare, &draft), None);
        let oauth = IntegrationConfigView {
            header_names: Vec::new(),
            oauth: Some(OAuthConnectionView {
                connected: true,
                issuer: None,
                scope: None,
                access_expires_at: None,
                refreshable: true,
                connected_at: None,
            }),
            ..config()
        };
        assert!(clears_credentials(&s, &oauth, &draft).is_some());
    }

    #[test]
    fn visibility_updates_list_every_hidden_tool_that_exists() {
        let tools = [
            tool("create", true),
            tool("delete", false),
            tool("list", true),
        ];
        let hidden = hidden_tools(&tools);
        assert_eq!(hidden.iter().collect::<Vec<_>>(), ["delete"]);
        let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        let none = set_visibility(&hidden, &names, true);
        assert!(none.is_empty());
        let all = set_visibility(&hidden, &names, false);
        assert_eq!(visibility_update(&tools, &all).hidden, names);
        let mut stale = hidden.clone();
        stale.insert("gone".into());
        assert_eq!(visibility_update(&tools, &stale).hidden, ["delete"]);
        assert!(tool_matches(&tools[0], "LINEAR__create"));
        assert!(tool_matches(&tools[0], "issue team"));
        assert!(!tool_matches(&tools[0], "issue project"));
    }

    #[test]
    fn auth_problems_offer_the_matching_fix() {
        let mut s = summary("linear");
        assert_eq!(fix_for(&s), None);
        s.status = IntegrationStatus::NeedsAuth;
        assert_eq!(fix_for(&s), Some(Fix::Credentials));
        s.auth = AuthKind::OAuth;
        assert_eq!(fix_for(&s), Some(Fix::Connect));
        s.auth = AuthKind::None;
        assert_eq!(fix_for(&s), Some(Fix::ChooseAuth));
        s.status = IntegrationStatus::Stale;
        s.auth = AuthKind::Headers;
        s.error = Some(GatewayErrorView {
            kind: GatewayErrorKind::AuthRejected,
            message: "401".into(),
            hint: None,
            at: 1,
        });
        assert_eq!(fix_for(&s), Some(Fix::Credentials));
        s.enabled = false;
        assert_eq!(fix_for(&s), None);
    }

    #[test]
    fn statuses_have_shapes_and_words() {
        assert_eq!(
            integration_status(IntegrationStatus::NeedsAuth),
            (StatusKind::Warn, "Needs auth")
        );
        assert_eq!(
            integration_status(IntegrationStatus::Disabled).0,
            StatusKind::Idle
        );
        assert_eq!(feed_status(FeedState::Paused).0, StatusKind::Warn);
        assert_eq!(feed_status(FeedState::Error).0, StatusKind::Fault);
    }

    #[test]
    fn client_ids_show_their_host() {
        assert_eq!(
            client_host("https://chatgpt.com/oauth/client.json"),
            "chatgpt.com"
        );
        assert_eq!(client_host("abc123"), "self-registered app");
        assert_eq!(client_host("https://"), "self-registered app");
    }

    #[test]
    fn snippets_name_the_endpoint() {
        let url = lan_url("http://omni.boris/", "/gateway/mcp");
        assert_eq!(url, "http://omni.boris/gateway/mcp");
        assert_eq!(
            claude_code_command(&url, TOKEN_PLACEHOLDER),
            "claude mcp add --transport http omni http://omni.boris/gateway/mcp --header \"Authorization: Bearer <token>\""
        );
        assert_eq!(
            codex_config(&url),
            "[mcp_servers.omni]\nurl = \"http://omni.boris/gateway/mcp\"\nbearer_token_env_var = \"OMNI_GATEWAY_TOKEN\""
        );
    }

    #[test]
    fn refresh_toasts_summarize_the_report() {
        let mut detail = IntegrationDetail {
            summary: summary("linear"),
            config: config(),
            server: Some(UpstreamServerView {
                name: None,
                version: None,
                protocol_version: None,
            }),
            tools: Vec::new(),
            resources: Vec::new(),
            events: Vec::new(),
            last_refresh: Some(RefreshReport {
                at: 1,
                ok: true,
                duration_ms: 10,
                added: vec!["a".into(), "b".into()],
                removed: Vec::new(),
                changed: vec!["c".into()],
                warnings: Vec::new(),
            }),
        };
        assert_eq!(
            refresh_outcome(&detail),
            (
                "Refreshed Linear: 3 tools, 2 added, 1 changed".to_owned(),
                true
            )
        );
        detail.last_refresh = None;
        assert_eq!(
            refresh_outcome(&detail),
            ("Refreshed Linear: 3 tools".to_owned(), true),
            "no report and no error reads as success"
        );
        detail.summary.error = Some(GatewayErrorView {
            kind: GatewayErrorKind::Timeout,
            message: "No answer in 120 s".into(),
            hint: None,
            at: 1,
        });
        assert_eq!(
            refresh_outcome(&detail),
            (
                "Linear did not refresh: No answer in 120 s".to_owned(),
                false
            )
        );
    }

    #[test]
    fn past_events_never_read_as_future() {
        assert_eq!(ago(10_000.0, 3_000.0), "just now");
        assert_eq!(ago(0.0, 60_000.0), "1m ago");
    }

    #[test]
    fn ledes_count_enabled_integrations() {
        let mut disabled = summary("off");
        disabled.enabled = false;
        let overview = GatewayOverview {
            public_url: None,
            path: "/gateway/mcp".into(),
            oauth_available: false,
            native_tools: 30,
            exposed_tools: 42,
            integrations: vec![summary("linear"), disabled],
            clients: Vec::new(),
            grants: Vec::new(),
            pending: Vec::new(),
            event_feeds: Vec::new(),
            bundled_servers: Vec::new(),
        };
        assert_eq!(
            lede(&overview),
            "One MCP endpoint for agents: 42 tools now, 30 of Omni's own and 12 from 1 enabled integration."
        );
        let counts = Counts::of(&overview);
        assert_eq!((counts.integrations, counts.ready), (2, 2));
    }
}
