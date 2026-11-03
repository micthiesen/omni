//! A cookie jar with tough-cookie 6 semantics and its `serializeSync()` JSON format.
//!
//! The Apple session persists `JSON.stringify(jar.serializeSync())`; this module reads
//! and writes that document unchanged, so the stored session in
//! `/data/reminders-private` stays valid. Matching follows
//! tough-cookie's `getCookies` (host-only and domain matching, `pathMatch`, secure
//! contexts, lazy expiry with `lastAccessed`-relative Max-Age) and ordering follows
//! tough-cookie's `cookieCompare`.
//!
//! Deliberate simplification: tough-cookie rejects cookie domains that are public
//! suffixes using the full Public Suffix List. Requests here only reach the Apple
//! allowlist (`*.apple.com`, `*.icloud.com`), and a cookie's domain must contain the
//! request host, so the only reachable public suffixes are single labels (`com`),
//! which are rejected.

use std::net::IpAddr;

use serde_json::{Map, Value};
use url::Url;

const MAX_TIME_MS: f64 = 2_147_483_647_000.0;
const TOUGH_COOKIE_VERSION: &str = "tough-cookie@6.0.2";

/// A Set-Cookie header or stored jar the Apple client cannot accept.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CookieJarError {
    #[error("Cookie failed to parse")]
    Parse,
    #[error("Cookie has domain set to a public suffix")]
    PublicSuffix,
    #[error("Cookie not in this host's domain")]
    Domain,
    #[error("serialized jar has no cookies array")]
    NoCookies,
}

/// `expires`, `creation` and `lastAccessed`: a date, `"Infinity"`, or `null`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum When {
    At(f64),
    Infinity,
    Null,
}

/// `maxAge`: seconds, or the strings `"Infinity"` / `"-Infinity"`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MaxAge {
    Seconds(f64),
    PositiveInfinity,
    NegativeInfinity,
}

#[derive(Clone, Debug, PartialEq)]
struct Cookie {
    key: String,
    value: String,
    expires: When,
    max_age: Option<MaxAge>,
    domain: Option<String>,
    path: Option<String>,
    secure: bool,
    http_only: bool,
    extensions: Option<Vec<String>>,
    host_only: Option<bool>,
    path_is_default: Option<bool>,
    creation: When,
    last_accessed: When,
    same_site: Option<String>,
}

impl Default for Cookie {
    fn default() -> Self {
        Self {
            key: String::new(),
            value: String::new(),
            expires: When::Infinity,
            max_age: None,
            domain: None,
            path: None,
            secure: false,
            http_only: false,
            extensions: None,
            host_only: None,
            path_is_default: None,
            creation: When::Null,
            last_accessed: When::Null,
            same_site: None,
        }
    }
}

fn parse_js_date(value: &Value) -> When {
    match value {
        Value::String(s) if s == "Infinity" => When::Infinity,
        Value::String(s) => s
            .parse::<jiff::Timestamp>()
            .map(|t| When::At(t.as_millisecond() as f64))
            .unwrap_or(When::Null),
        Value::Number(n) => n.as_f64().map(When::At).unwrap_or(When::Null),
        _ => When::Null,
    }
}

fn when_json(when: When) -> Value {
    match when {
        When::Infinity => Value::String("Infinity".into()),
        When::Null => Value::Null,
        #[allow(clippy::cast_possible_truncation)]
        When::At(ms) => Value::String(omni_core::js::to_iso_string(ms as i64)),
    }
}

impl Cookie {
    /// `Cookie.fromJSON`.
    fn from_json(value: &Value) -> Self {
        let mut cookie = Self::default();
        let Some(obj) = value.as_object() else {
            return cookie;
        };
        if let Some(Value::String(s)) = obj.get("key") {
            cookie.key = s.clone();
        }
        if let Some(Value::String(s)) = obj.get("value") {
            cookie.value = s.clone();
        }
        if let Some(Value::String(s)) = obj.get("sameSite") {
            cookie.same_site = Some(s.clone());
        }
        for (name, slot) in [
            ("expires", &mut cookie.expires),
            ("creation", &mut cookie.creation),
            ("lastAccessed", &mut cookie.last_accessed),
        ] {
            match obj.get(name) {
                Some(v @ (Value::String(_) | Value::Number(_))) => *slot = parse_js_date(v),
                Some(Value::Null) => *slot = When::Null,
                _ => {}
            }
        }
        cookie.max_age = match obj.get("maxAge") {
            Some(Value::Number(n)) => n.as_f64().map(MaxAge::Seconds),
            Some(Value::String(s)) if s == "Infinity" => Some(MaxAge::PositiveInfinity),
            Some(Value::String(s)) if s == "-Infinity" => Some(MaxAge::NegativeInfinity),
            _ => None,
        };
        if let Some(Value::String(s)) = obj.get("domain") {
            cookie.domain = Some(s.clone());
        }
        if let Some(Value::String(s)) = obj.get("path") {
            cookie.path = Some(s.clone());
        }
        if let Some(Value::Bool(b)) = obj.get("secure") {
            cookie.secure = *b;
        }
        if let Some(Value::Bool(b)) = obj.get("httpOnly") {
            cookie.http_only = *b;
        }
        if let Some(Value::Array(items)) = obj.get("extensions")
            && items.iter().all(Value::is_string)
        {
            cookie.extensions = Some(
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect(),
            );
        }
        if let Some(Value::Bool(b)) = obj.get("hostOnly") {
            cookie.host_only = Some(*b);
        }
        if let Some(Value::Bool(b)) = obj.get("pathIsDefault") {
            cookie.path_is_default = Some(*b);
        }
        cookie
    }

    /// `Cookie#toJSON`, omitting values equal to tough-cookie's defaults.
    fn to_json(&self) -> Value {
        let mut obj = Map::new();
        if !self.key.is_empty() {
            obj.insert("key".into(), Value::String(self.key.clone()));
        }
        if !self.value.is_empty() {
            obj.insert("value".into(), Value::String(self.value.clone()));
        }
        if self.expires != When::Infinity {
            obj.insert("expires".into(), when_json(self.expires));
        }
        if let Some(max_age) = self.max_age {
            obj.insert(
                "maxAge".into(),
                match max_age {
                    MaxAge::Seconds(n) => serde_json::Number::from_f64(n)
                        .map(|n| {
                            if n.as_f64()
                                .is_some_and(|f| f.fract() == 0.0 && f.abs() < 9.0e15)
                            {
                                #[allow(clippy::cast_possible_truncation)]
                                Value::from(n.as_f64().unwrap_or_default() as i64)
                            } else {
                                Value::Number(n)
                            }
                        })
                        .unwrap_or(Value::Null),
                    MaxAge::PositiveInfinity => Value::String("Infinity".into()),
                    MaxAge::NegativeInfinity => Value::String("-Infinity".into()),
                },
            );
        }
        if let Some(domain) = &self.domain {
            obj.insert("domain".into(), Value::String(domain.clone()));
        }
        if let Some(path) = &self.path {
            obj.insert("path".into(), Value::String(path.clone()));
        }
        if self.secure {
            obj.insert("secure".into(), Value::Bool(true));
        }
        if self.http_only {
            obj.insert("httpOnly".into(), Value::Bool(true));
        }
        if let Some(extensions) = &self.extensions {
            obj.insert(
                "extensions".into(),
                Value::Array(extensions.iter().cloned().map(Value::String).collect()),
            );
        }
        if let Some(host_only) = self.host_only {
            obj.insert("hostOnly".into(), Value::Bool(host_only));
        }
        if let Some(default) = self.path_is_default {
            obj.insert("pathIsDefault".into(), Value::Bool(default));
        }
        if self.creation != When::Null {
            obj.insert("creation".into(), when_json(self.creation));
        }
        if self.last_accessed != When::Null {
            obj.insert("lastAccessed".into(), when_json(self.last_accessed));
        }
        if let Some(same_site) = &self.same_site {
            obj.insert("sameSite".into(), Value::String(same_site.clone()));
        }
        Value::Object(obj)
    }

    /// `Cookie#expiryTime()`: Max-Age is relative to `lastAccessed` (or now).
    fn expiry_time(&self, now_ms: f64) -> Option<f64> {
        if let Some(max_age) = self.max_age {
            let relative_to = match self.last_accessed {
                When::At(ms) => ms,
                When::Infinity => return Some(f64::INFINITY),
                When::Null => now_ms,
            };
            let seconds = match max_age {
                MaxAge::Seconds(n) => n,
                MaxAge::PositiveInfinity | MaxAge::NegativeInfinity => f64::NEG_INFINITY,
            };
            let age = if seconds <= 0.0 {
                f64::NEG_INFINITY
            } else {
                seconds * 1000.0
            };
            return Some(relative_to + age);
        }
        match self.expires {
            When::Infinity => Some(f64::INFINITY),
            When::At(ms) => Some(ms),
            When::Null => None,
        }
    }

    fn cookie_string(&self) -> String {
        if self.key.is_empty() {
            self.value.clone()
        } else {
            format!("{}={}", self.key, self.value)
        }
    }
}

/// tough-cookie `canonicalDomain` for ASCII hosts (non-ASCII goes through IDNA).
fn canonical_domain(domain: &str) -> String {
    let trimmed = domain.trim();
    let stripped = trimmed.strip_prefix('.').unwrap_or(trimmed);
    if stripped.is_ascii() {
        stripped.to_ascii_lowercase()
    } else {
        Url::parse(&format!("http://{stripped}"))
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_else(|| stripped.to_lowercase())
    }
}

fn is_ip(host: &str) -> bool {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok()
}

/// `domainMatch(str, domStr, false)`.
fn domain_match(host: &str, cookie_domain: &str) -> bool {
    if host == cookie_domain {
        return true;
    }
    let Some(prefix) = host.strip_suffix(cookie_domain) else {
        return false;
    };
    !prefix.is_empty() && prefix.ends_with('.') && !is_ip(host)
}

/// `pathMatch(reqPath, cookiePath)`.
fn path_match(request: &str, cookie: &str) -> bool {
    if request == cookie {
        return true;
    }
    if request.starts_with(cookie) {
        if cookie.ends_with('/') {
            return true;
        }
        if request.as_bytes().get(cookie.len()) == Some(&b'/') {
            return true;
        }
    }
    false
}

/// `defaultPath(path)` (RFC 6265 5.1.4).
fn default_path(path: &str) -> String {
    if !path.starts_with('/') || path == "/" {
        return "/".into();
    }
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(index) => path[..index].into(),
    }
}

fn host_of(url: &Url) -> String {
    canonical_domain(url.host_str().unwrap_or(""))
}

fn potentially_trustworthy(url: &Url) -> bool {
    if matches!(url.scheme(), "https" | "wss") {
        return true;
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.octets()[0] == 127,
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost")
        }
        None => false,
    }
}

fn is_delimiter(c: char) -> bool {
    matches!(c, '\x09' | '\x20'..='\x2F' | '\x3B'..='\x40' | '\x5B'..='\x60' | '\x7B'..='\x7E')
}

fn leading_digits(token: &str, min: usize, max: usize) -> Option<(u32, &str)> {
    let digits = token.bytes().take_while(u8::is_ascii_digit).count();
    if digits < min || digits > max {
        return None;
    }
    let rest = &token[digits..];
    // The remainder must start with a non-digit (it already does) and be Latin-1.
    if rest.chars().any(|c| u32::from(c) > 0xFF) {
        return None;
    }
    token[..digits].parse().ok().map(|n| (n, rest))
}

/// tough-cookie `parseDate` (RFC 6265 5.1.1), returning epoch milliseconds.
fn parse_cookie_date(input: &str) -> Option<f64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let mut time: Option<(u32, u32, u32)> = None;
    let mut day: Option<u32> = None;
    let mut month: Option<u32> = None;
    let mut year: Option<u32> = None;
    for token in input.split(is_delimiter).filter(|t| !t.is_empty()) {
        if time.is_none() {
            let parsed = (|| {
                let (h, rest) = leading_digits(token, 1, 2)?;
                let rest = rest.strip_prefix(':')?;
                let (m, rest) = leading_digits(rest, 1, 2)?;
                let rest = rest.strip_prefix(':')?;
                let (s, _) = leading_digits(rest, 1, 2)?;
                Some((h, m, s))
            })();
            if let Some(parsed) = parsed {
                time = Some(parsed);
                continue;
            }
        }
        if day.is_none()
            && let Some((d, _)) = leading_digits(token, 1, 2)
        {
            day = Some(d);
            continue;
        }
        if month.is_none() && token.len() >= 3 && token.is_char_boundary(3) {
            let prefix = token[..3].to_ascii_lowercase();
            if let Some(index) = MONTHS.iter().position(|m| *m == prefix)
                && !token.chars().any(|c| u32::from(c) > 0xFF)
            {
                month = u32::try_from(index).ok();
                continue;
            }
        }
        if year.is_none()
            && let Some((y, _)) = leading_digits(token, 2, 4)
        {
            year = Some(y);
            continue;
        }
    }
    let mut year = year?;
    if (70..=99).contains(&year) {
        year += 1900;
    } else if year <= 69 {
        year += 2000;
    }
    let (day, month, (h, m, s)) = (day?, month?, time?);
    if !(1..=31).contains(&day) || year < 1601 || h > 23 || m > 59 || s > 59 {
        return None;
    }
    let date = jiff::civil::Date::new(
        i16::try_from(year).ok()?,
        i8::try_from(month + 1).ok()?,
        i8::try_from(day).ok()?,
    )
    .ok()?;
    let datetime = date.at(
        i8::try_from(h).ok()?,
        i8::try_from(m).ok()?,
        i8::try_from(s).ok()?,
        0,
    );
    let ts = datetime.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp();
    Some(ts.as_millisecond() as f64)
}

/// `Cookie.parse(str, {loose})`.
fn parse_set_cookie(header: &str, loose: bool) -> Option<Cookie> {
    let header = header.trim();
    if header.is_empty() {
        return None;
    }
    let (pair, attributes) = match header.find(';') {
        Some(i) => (&header[..i], Some(&header[i + 1..])),
        None => (header, None),
    };
    let pair = pair.split(['\n', '\r', '\0']).next().unwrap_or_default();
    let mut pair = pair.to_owned();
    let mut first_eq = pair.find('=');
    if loose {
        if first_eq == Some(0) {
            pair.remove(0);
            first_eq = pair.find('=');
        }
    } else if first_eq.is_none_or(|i| i == 0) {
        return None;
    }
    let (key, value) = match first_eq {
        Some(i) if i > 0 => (pair[..i].trim().to_owned(), pair[i + 1..].trim().to_owned()),
        _ => (String::new(), pair.trim().to_owned()),
    };
    let control = |s: &str| s.chars().any(|c| c <= '\x1F');
    if control(&key) || control(&value) {
        return None;
    }
    let mut cookie = Cookie {
        key,
        value,
        ..Cookie::default()
    };
    let Some(attributes) = attributes.map(str::trim).filter(|a| !a.is_empty()) else {
        return Some(cookie);
    };
    for av in attributes.split(';') {
        let av = av.trim();
        if av.is_empty() {
            continue;
        }
        let (name, value) = match av.find('=') {
            Some(i) => (&av[..i], Some(av[i + 1..].trim())),
            None => (av, None),
        };
        let value = value.filter(|v| !v.is_empty());
        match name.trim().to_ascii_lowercase().as_str() {
            "expires" => {
                if let Some(at) = value.and_then(parse_cookie_date) {
                    cookie.expires = When::At(at);
                }
            }
            "max-age" => {
                if let Some(v) = value
                    && let Some(digits) = v.strip_prefix('-').or(Some(v))
                    && !digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit())
                {
                    cookie.max_age = Some(MaxAge::Seconds(omni_core::js::string_to_number(v)));
                }
            }
            "domain" => {
                if let Some(v) = value {
                    let domain = v.trim();
                    let domain = domain.strip_prefix('.').unwrap_or(domain);
                    if !domain.is_empty() {
                        cookie.domain = Some(domain.to_lowercase());
                    }
                }
            }
            "path" => {
                cookie.path = value.filter(|v| v.starts_with('/')).map(str::to_owned);
            }
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            "samesite" => {
                cookie.same_site = match value.map(str::to_ascii_lowercase).as_deref() {
                    Some("strict") => Some("strict".into()),
                    Some("lax") => Some("lax".into()),
                    Some("none") => Some("none".into()),
                    _ => None,
                };
            }
            _ => cookie
                .extensions
                .get_or_insert_with(Vec::new)
                .push(av.to_owned()),
        }
    }
    Some(cookie)
}

/// tough-cookie `CookieJar` over a `MemoryCookieStore`.
#[derive(Clone, Debug, PartialEq)]
pub struct CookieJar {
    reject_public_suffixes: bool,
    loose: bool,
    allow_special_use_domain: bool,
    prefix_security: String,
    cookies: Vec<Cookie>,
}

impl Default for CookieJar {
    fn default() -> Self {
        Self {
            reject_public_suffixes: true,
            loose: false,
            allow_special_use_domain: true,
            prefix_security: "silent".into(),
            cookies: Vec::new(),
        }
    }
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// `CookieJar.deserializeSync(value)`.
    pub fn from_json(value: &Value) -> Result<Self, CookieJarError> {
        let obj = value.as_object();
        let boolean = |name: &str, default: bool| {
            obj.and_then(|o| o.get(name))
                .and_then(Value::as_bool)
                .unwrap_or(default)
        };
        let prefix_security = match obj
            .and_then(|o| o.get("prefixSecurity"))
            .and_then(Value::as_str)
        {
            Some(p @ ("strict" | "unsafe-disabled")) => p.to_owned(),
            _ => "silent".to_owned(),
        };
        let mut jar = Self {
            reject_public_suffixes: boolean("rejectPublicSuffixes", true),
            loose: boolean("enableLooseMode", false),
            allow_special_use_domain: boolean("allowSpecialUseDomain", true),
            prefix_security,
            cookies: Vec::new(),
        };
        let cookies = obj
            .and_then(|o| o.get("cookies"))
            .and_then(Value::as_array)
            .ok_or(CookieJarError::NoCookies)?;
        for item in cookies {
            let cookie = Cookie::from_json(item);
            if cookie.domain.is_some() && cookie.path.is_some() {
                jar.put(cookie);
            }
        }
        Ok(jar)
    }

    /// `CookieJar#serializeSync()`.
    pub fn to_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("version".into(), Value::String(TOUGH_COOKIE_VERSION.into()));
        obj.insert(
            "storeType".into(),
            Value::String("MemoryCookieStore".into()),
        );
        obj.insert(
            "rejectPublicSuffixes".into(),
            Value::Bool(self.reject_public_suffixes),
        );
        obj.insert("enableLooseMode".into(), Value::Bool(self.loose));
        obj.insert(
            "allowSpecialUseDomain".into(),
            Value::Bool(self.allow_special_use_domain),
        );
        obj.insert(
            "prefixSecurity".into(),
            Value::String(self.prefix_security.clone()),
        );
        obj.insert(
            "cookies".into(),
            Value::Array(self.cookies.iter().map(Cookie::to_json).collect()),
        );
        Value::Object(obj)
    }

    pub fn clear(&mut self) {
        self.cookies.clear();
    }

    /// Number of stored cookies (tests and diagnostics).
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Replaces `(domain, path, key)` in place, else appends.
    fn put(&mut self, cookie: Cookie) {
        // Session cookies are long random tokens; shorter values are locales,
        // flags and similar text that scrubbing would only obscure.
        if cookie.value.len() >= SCRUBBED_COOKIE_MIN_BYTES {
            omni_core::secrets::register_runtime(&cookie.value);
        }
        match self
            .cookies
            .iter_mut()
            .find(|c| c.domain == cookie.domain && c.path == cookie.path && c.key == cookie.key)
        {
            Some(existing) => *existing = cookie,
            None => self.cookies.push(cookie),
        }
    }

    /// `setCookieSync(header, url)`.
    pub fn set_cookie(
        &mut self,
        header: &str,
        url: &Url,
        now_ms: i64,
    ) -> Result<(), CookieJarError> {
        let mut cookie = parse_set_cookie(header, self.loose).ok_or(CookieJarError::Parse)?;
        let host = host_of(url);
        if self.reject_public_suffixes
            && let Some(domain) = &cookie.domain
        {
            let canonical = canonical_domain(domain);
            if !canonical.contains('.') && !is_ip(&canonical) {
                return Err(CookieJarError::PublicSuffix);
            }
        }
        if let Some(domain) = &cookie.domain {
            if !domain_match(&host, &canonical_domain(domain)) {
                return Err(CookieJarError::Domain);
            }
            if cookie.host_only.is_none() {
                cookie.host_only = Some(false);
            }
        } else {
            cookie.host_only = Some(true);
            cookie.domain = Some(host);
        }
        if cookie.path.as_deref().is_none_or(|p| !p.starts_with('/')) {
            cookie.path = Some(default_path(url.path()));
            cookie.path_is_default = Some(true);
        }
        if self.prefix_security != "unsafe-disabled" {
            let secure_ok = !cookie.key.starts_with("__Secure-") || cookie.secure;
            let host_ok = !cookie.key.starts_with("__Host-")
                || (cookie.secure
                    && cookie.host_only == Some(true)
                    && cookie.path.as_deref() == Some("/"));
            if !secure_ok || !host_ok {
                // "silent" ignores the cookie; "strict" would reject the response.
                return if self.prefix_security == "silent" {
                    Ok(())
                } else {
                    Err(CookieJarError::Parse)
                };
            }
        }
        let now = When::At(now_ms as f64);
        let existing = self
            .cookies
            .iter()
            .find(|c| c.domain == cookie.domain && c.path == cookie.path && c.key == cookie.key);
        match existing {
            Some(old) => {
                cookie.creation = old.creation;
                cookie.last_accessed = now;
            }
            None => {
                cookie.creation = now;
                cookie.last_accessed = now;
            }
        }
        self.put(cookie);
        Ok(())
    }

    /// `getCookieStringSync(url)`: drops expired matches and updates `lastAccessed`.
    pub fn cookie_header(&mut self, url: &Url, now_ms: i64) -> String {
        let host = host_of(url);
        let path = if url.path().is_empty() {
            "/"
        } else {
            url.path()
        };
        let secure = potentially_trustworthy(url);
        let now = now_ms as f64;
        let mut matched: Vec<usize> = Vec::new();
        let mut expired: Vec<usize> = Vec::new();
        for (index, c) in self.cookies.iter().enumerate() {
            let Some(domain) = c.domain.as_deref() else {
                continue;
            };
            // MemoryCookieStore only searches the host and its parent domains.
            let indexed = domain == host || (host.ends_with(domain) && domain.contains('.'));
            let domain_ok = if c.host_only == Some(true) {
                domain == host
            } else {
                indexed && domain_match(&host, domain)
            };
            let path_ok = c.path.as_deref().is_some_and(|p| path_match(path, p));
            if !domain_ok || !path_ok || (c.secure && !secure) {
                continue;
            }
            if c.expiry_time(now).is_some_and(|t| t <= now) {
                expired.push(index);
                continue;
            }
            matched.push(index);
        }
        let creation = |c: &Cookie| match c.creation {
            When::At(ms) => ms,
            _ => MAX_TIME_MS,
        };
        matched.sort_by(|&a, &b| {
            let (ca, cb) = (&self.cookies[a], &self.cookies[b]);
            let len = |c: &Cookie| c.path.as_deref().map_or(0, omni_core::js::utf16_len);
            len(cb)
                .cmp(&len(ca))
                .then(creation(ca).total_cmp(&creation(cb)))
                .then(a.cmp(&b))
        });
        for &index in &matched {
            self.cookies[index].last_accessed = When::At(now);
        }
        let header = matched
            .iter()
            .map(|&i| self.cookies[i].cookie_string())
            .collect::<Vec<_>>()
            .join("; ");
        for index in expired.into_iter().rev() {
            self.cookies.remove(index);
        }
        header
    }
}

const SCRUBBED_COOKIE_MIN_BYTES: usize = 20;

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn stored_cookie_values_are_scrubbed() {
        let mut jar = CookieJar::new();
        jar.set_cookie(
            "X-APPLE-WEBAUTH-TOKEN=cookie-marker-96-session-x; Domain=icloud.com; Path=/; Secure",
            &url("https://setup.icloud.com/setup/ws/1/accountLogin"),
            1_800_000_000_000,
        )
        .unwrap();
        jar.set_cookie(
            "dslang=US-EN-short; Domain=icloud.com; Path=/",
            &url("https://setup.icloud.com/setup/ws/1/accountLogin"),
            1_800_000_000_000,
        )
        .unwrap();
        assert_eq!(
            omni_core::secrets::scrub(
                "Cookie: X-APPLE-WEBAUTH-TOKEN=cookie-marker-96-session-x; dslang=US-EN-short"
            ),
            "Cookie: X-APPLE-WEBAUTH-TOKEN=[redacted]; dslang=US-EN-short"
        );
    }

    #[test]
    fn domain_and_host_only_cookies_match_like_tough_cookie() {
        let mut jar = CookieJar::new();
        let now = 1_800_000_000_000;
        jar.set_cookie(
            "X=1; Domain=icloud.com; Path=/; Secure; HttpOnly",
            &url("https://setup.icloud.com/setup/ws/1/requestPCS"),
            now,
        )
        .unwrap();
        jar.set_cookie(
            "h=2",
            &url("https://setup.icloud.com/setup/ws/1/validate"),
            now,
        )
        .unwrap();
        assert_eq!(
            jar.cookie_header(&url("https://setup.icloud.com/setup/ws/1/x"), now),
            "h=2; X=1"
        );
        assert_eq!(
            jar.cookie_header(&url("https://p01-ckdatabasews.icloud.com/x"), now),
            "X=1"
        );
        assert_eq!(
            jar.cookie_header(&url("http://setup.icloud.com/setup/ws/1/x"), now),
            "h=2"
        );
        assert_eq!(jar.cookie_header(&url("https://idmsa.apple.com/"), now), "");
    }

    #[test]
    fn rejects_foreign_and_public_suffix_domains() {
        let mut jar = CookieJar::new();
        let u = url("https://setup.icloud.com/");
        assert_eq!(
            jar.set_cookie("a=1; Domain=apple.com", &u, 0),
            Err(CookieJarError::Domain)
        );
        assert_eq!(
            jar.set_cookie("a=1; Domain=com", &u, 0),
            Err(CookieJarError::PublicSuffix)
        );
        assert_eq!(jar.set_cookie("novalue", &u, 0), Err(CookieJarError::Parse));
        assert!(jar.is_empty());
    }

    #[test]
    fn expires_cookies_lazily() {
        let mut jar = CookieJar::new();
        let u = url("https://setup.icloud.com/");
        let t0 = 1_800_000_000_000;
        jar.set_cookie("a=1; Max-Age=10", &u, t0).unwrap();
        jar.set_cookie("b=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT", &u, t0)
            .unwrap();
        assert_eq!(jar.cookie_header(&u, t0 + 5_000), "a=1");
        assert_eq!(jar.len(), 1);
        // Max-Age slides from the last access (tough-cookie expiryTime()).
        assert_eq!(jar.cookie_header(&u, t0 + 14_000), "a=1");
        assert_eq!(jar.cookie_header(&u, t0 + 100_000), "");
        assert!(jar.is_empty());
    }

    #[test]
    fn parses_rfc6265_dates() {
        assert_eq!(
            parse_cookie_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480_000.0)
        );
        assert_eq!(
            parse_cookie_date("Thu, 01-Jan-70 00:00:01 GMT"),
            Some(1000.0)
        );
        assert_eq!(parse_cookie_date("garbage"), None);
    }
}
