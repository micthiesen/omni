//! The live Parcel carrier list:
//! a 24-hour in-memory cache refreshed under a mutex (concurrent first
//! refreshes coalesce), bounded reads, and a stale-cache fallback when a
//! refresh fails. The email pipeline sees the list minus the blacklist below;
//! `parcels_add` validates against everything Parcel supports.

use std::collections::HashSet;
use std::time::Duration;

use omni_core::clock::SharedClock;
use omni_http::public::PublicHttpClient;
use omni_http::{HttpError, Method, Url};
use regex::Regex;
use serde_json::Value;
use tokio::sync::Mutex;

const LOG: &str = "Main:ParcelTracker";
pub const CARRIER_LIST_URL: &str = "https://api.parcel.app/external/supported_carriers.json";
pub const CARRIER_LIST_MAX_BYTES: usize = 2 * 1024 * 1024;
const CACHE_TTL_MS: i64 = 24 * 60 * 60 * 1000;
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// One supported carrier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarrierEntry {
    pub code: String,
    pub name: String,
}

#[derive(Debug, thiserror::Error)]
enum CarrierListError {
    #[error("Carrier list failed: {0}")]
    Http(#[from] HttpError),
    #[error("Carrier list failed: {0}")]
    Decode(String),
}

struct Cache {
    carriers: Option<Vec<CarrierEntry>>,
    cached_at: i64,
}

/// The carrier list cache; share one per process.
pub struct CarrierDirectory {
    http: PublicHttpClient,
    url: Url,
    max_bytes: usize,
    clock: SharedClock,
    cache: Mutex<Cache>,
}

impl CarrierDirectory {
    pub fn new(http: PublicHttpClient, clock: SharedClock) -> Result<Self, HttpError> {
        let url = Url::parse(CARRIER_LIST_URL).map_err(|e| HttpError::InvalidUrl(e.to_string()))?;
        Ok(Self::with_limits(http, clock, url, CARRIER_LIST_MAX_BYTES))
    }

    /// Custom URL and byte cap (tests).
    pub fn with_limits(
        http: PublicHttpClient,
        clock: SharedClock,
        url: Url,
        max_bytes: usize,
    ) -> Self {
        Self {
            http,
            url,
            max_bytes,
            clock,
            cache: Mutex::new(Cache {
                carriers: None,
                cached_at: 0,
            }),
        }
    }

    /// `code: name` lines (empty when unavailable).
    pub async fn prompt_codes(&self) -> String {
        self.carriers()
            .await
            .unwrap_or_default()
            .iter()
            .map(|c| format!("{}: {}", c.code, c.name))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every carrier Parcel supports, blacklisted ones included, as code to
    /// name; `None` when the list is unavailable.
    pub async fn supported(&self) -> Option<std::collections::HashMap<String, String>> {
        self.all_carriers()
            .await
            .map(|carriers| carriers.into_iter().map(|c| (c.code, c.name)).collect())
    }

    /// `None` when the list is unavailable
    /// (fetch failed and nothing cached).
    pub async fn valid_codes(&self) -> Option<HashSet<String>> {
        self.carriers()
            .await
            .map(|carriers| carriers.into_iter().map(|c| c.code).collect())
    }

    /// Case-insensitive word-boundary patterns.
    pub async fn name_patterns(&self) -> Vec<Regex> {
        self.carriers()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|c| carrier_name_pattern(&c.name))
            .collect()
    }

    /// Carrier names by code (empty when unavailable).
    pub async fn names(&self) -> std::collections::HashMap<String, String> {
        self.carriers()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|c| (c.code, c.name))
            .collect()
    }

    /// The pipeline's carriers: the supported list minus the blacklist.
    async fn carriers(&self) -> Option<Vec<CarrierEntry>> {
        self.all_carriers().await.map(|carriers| {
            carriers
                .into_iter()
                .filter(|c| !is_blacklisted_carrier(&c.code))
                .collect()
        })
    }

    async fn all_carriers(&self) -> Option<Vec<CarrierEntry>> {
        let mut cache = self.cache.lock().await;
        let now = self.clock.now_ms();
        if let Some(carriers) = &cache.carriers
            && now - cache.cached_at < CACHE_TTL_MS
        {
            return Some(carriers.clone());
        }
        match self.fetch().await {
            Ok(carriers) => {
                cache.carriers = Some(carriers.clone());
                cache.cached_at = now;
                Some(carriers)
            }
            Err(error) => {
                tracing::warn!(target: LOG, "Failed to fetch Parcel carrier list: {error}");
                cache.carriers.clone()
            }
        }
    }

    async fn fetch(&self) -> Result<Vec<CarrierEntry>, CarrierListError> {
        let response = self
            .http
            .request(Method::GET, self.url.clone())
            .timeout(FETCH_TIMEOUT)
            .send_bounded(self.max_bytes)
            .await?;
        if !response.status.is_success() {
            return Err(CarrierListError::Http(HttpError::Status {
                status: response.status.as_u16(),
                body: String::from_utf8_lossy(&response.body)
                    .chars()
                    .take(4096)
                    .collect(),
            }));
        }
        let decoded: Value = serde_json::from_slice(&response.body)
            .map_err(|e| CarrierListError::Decode(e.to_string()))?;
        decode_carriers(&decoded).map_err(CarrierListError::Decode)
    }
}

/// `new RegExp(`\\b${escapeRegExp(name)}\\b`, "i")`: JS `\b` without the
/// `u` flag is an ASCII word boundary.
pub fn carrier_name_pattern(name: &str) -> Option<Regex> {
    match Regex::new(&format!(r"(?i)(?-u:\b){}(?-u:\b)", regex::escape(name))) {
        Ok(pattern) => Some(pattern),
        Err(error) => {
            tracing::warn!(target: LOG, "Skipping carrier name pattern {name:?}: {error}");
            None
        }
    }
}

/// Decodes `{code: name | {name?}}`, keeping entries with a usable name.
pub fn decode_carriers(value: &Value) -> Result<Vec<CarrierEntry>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "expected an object of carriers".to_owned())?;
    let mut carriers = Vec::new();
    for (code, entry) in object {
        let name = match entry {
            Value::String(name) => Some(name.clone()),
            Value::Object(fields) => match fields.get("name") {
                None => None,
                Some(Value::String(name)) => Some(name.clone()),
                Some(_) => return Err(format!("carrier {code:?}: name must be a string")),
            },
            _ => return Err(format!("carrier {code:?}: expected a string or an object")),
        };
        if let Some(name) = name {
            carriers.push(CarrierEntry {
                code: code.clone(),
                name,
            });
        }
    }
    Ok(carriers)
}

// Maintenance: to prune this list, fetch the supported carriers JSON and diff it
// against the entries below. Remove blacklisted codes Parcel has dropped, and
// consider blacklisting new codes clearly irrelevant for a Canadian recipient
// (regional last-mile carriers in distant countries, freight/B2B services,
// non-shipping platforms). Keep international postal services and cross-border
// shippers: they can carry inbound packages.
const BLACKLISTED_PREFIXES: &[&str] = &[
    "amzl",   // Amazon regional
    "amship", // Amazon Shipping
];

const BLACKLISTED_CARRIERS: &[&str] = &[
    // Food delivery / non-shipping
    "doordash",
    "pholder", // Placeholder Deliveries
    // Freight / B2B logistics (not consumer parcels)
    "abf",      // ABF Freight
    "ceva",     // Ceva Logistics
    "dachser",  // Dachser
    "dsv",      // DSV
    "geodis",   // Geodis
    "mscgva",   // MSC (shipping line)
    "pilot",    // Pilot Freight
    "safmar",   // Safmarine (shipping line)
    "sch",      // DB Schenker
    "seabour",  // Seabourne Logistics
    "straight", // Straightship
    "pfl",      // Parcel Freight Logistics
    "syncreon", // Syncreon
    // Russia / CIS
    "rp",       // Russian Post
    "ems",      // EMS Russian Post
    "edos",     // CDEK
    "boxb",     // Boxberry
    "shiptor",  // Shiptor
    "fivepost", // 5post
    "dellin",   // Delovie Linii
    "pec",      // PEC
    "energia",  // TK Energia
    "major",    // Major Express
    "blp",      // Belpost (Belarus)
    "kz",       // Kazpost
    "azer",     // Azerpost
    "moldov",   // Moldova Post
    "newp",     // Nova Poshta (Ukraine)
    "ukr",      // Ukrpost
    // Middle East / Africa
    "naqel",    // Naqel Express
    "smsa",     // SMSA Express
    "saudi",    // Saudi Post
    "emirates", // Emirates Post
    "imile",    // iMile
    "jordan",   // Jordan Post
    "safr",     // South African Post Office
    "il",       // Israel Post
    // South / SE Asia (regional last-mile)
    "dtdc",     // DTDC India
    "bluedart", // Blue Dart (India)
    "in",       // India Post
    "kerry",    // Kerry Express (Thailand)
    "thai",     // Thailand Post
    "skynetm",  // Skynet Malaysia
    "malpos",   // Malaysia Post
    "phlpost",  // Philpost
    "indon",    // Indonesia Post
    "bluecare", // Bluecare Express
    // Latin America (regional)
    "oca",     // OCA Argentina
    "chilex",  // Chilexpress
    "colomb",  // Colombia post (4-72)
    "corm",    // Correos de Mexico
    "estafe",  // Estafeta (Mexico)
    "redpack", // Redpack (Mexico)
    "paquet",  // Paquetexpress (Mexico)
    "serpost", // Serpost (Peru)
    "corurg",  // Correo Uruguayo
    "corbra",  // Correios (Brazil)
    "vasp",    // Vasp Expresso (Brazil)
    // Eastern Europe (regional last-mile)
    "econt",   // Econt Express (Bulgaria)
    "bolg",    // Bulgarian Post
    "serbia",  // Serbia Post
    "hr",      // Hrvatska pošta (Croatia)
    "hrpar",   // HR Parcel (Croatia)
    "hung",    // Magyar Posta (Hungary)
    "ceska",   // Česká pošta
    "slovak",  // Slovenská pošta
    "slv",     // Pošta Slovenije
    "litva",   // Lietuvos paštas
    "ee",      // Eesti Post (Estonia)
    "lv",      // Latvijas Pasts (Latvia)
    "cypr",    // Cyprus Post
    "geniki",  // Geniki Taxydromiki (Greece)
    "elta",    // Elta (Greece)
    "venipak", // Venipak (Baltics)
    // Oceania (regional last-mile)
    "airroad",   // AirRoad (AU)
    "star",      // StarTrack Express (AU)
    "fastau",    // Fastway AU
    "tntau",     // TNT Australia
    "couple",    // Couriers Please (AU)
    "northline", // Northline (AU)
    "allied",    // Allied Express (AU)
    "sendle",    // Sendle (AU)
    "coup",      // CourierPost (NZ)
    "fastnz",    // Fastway NZ
    "pbt",       // PBT New Zealand
    "parcelpnt", // ParcelPoint (AU)
    // Spain (domestic last-mile)
    "acs",       // ACS Courier (Greece)
    "asmred",    // GLS Spain
    "celeritas", // Celeritas
    "chrexp",    // Correos Express
    "cor",       // Correos
    "envia",     // Ontime - Envialia
    "mrw",       // MRW
    "nacex",     // Nacex
    "seur",      // SEUR
    "tipsac",    // Tipsa
    "tourline",  // CTT Express (Spain/Portugal)
    "zel",       // Zeleris
    // Italy (domestic last-mile)
    "bartol", // Bartolini
    "glsit",  // GLS Italy
    // Malta / Turkey / Pakistan
    "malta", // MaltaPost
    "turk",  // PTT (Turkey)
    "pk",    // Pakistan Post
    // UK / Germany heavy goods & niche
    "arrowxl",  // Arrow XL (UK heavy goods)
    "dx",       // DX (UK)
    "her2mann", // Hermes 2-Mann-Handling (German heavy goods)
    // Niche cargo
    "hawai",     // Hawaiian Air Cargo
    "koreanair", // Korean Air Cargo
];

/// Irrelevant carriers never offered to the extraction model.
pub fn is_blacklisted_carrier(code: &str) -> bool {
    BLACKLISTED_PREFIXES
        .iter()
        .any(|prefix| code.starts_with(prefix))
        || BLACKLISTED_CARRIERS.contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carrier_name_patterns_compile_and_match_on_word_boundaries() {
        let purolator = carrier_name_pattern("Purolator").expect("compiles");
        assert!(purolator.is_match("Your PUROLATOR shipment"));
        assert!(!purolator.is_match("Purolatorx"));
        let canada_post = carrier_name_pattern("Canada Post").expect("compiles");
        assert!(canada_post.is_match("Shipped via canada post today"));
        let dotted = carrier_name_pattern("A.B. Express").expect("compiles");
        assert!(dotted.is_match("by A.B. Express today"));
        assert!(!dotted.is_match("by AxB. Express today"));
        // Like JS, a name ending in punctuation needs a word character after it.
        let bracketed = carrier_name_pattern("Express (CA)").expect("compiles");
        assert!(!bracketed.is_match("via Express (CA) today"));
        // Non-ASCII letters are not word characters for a JS `\b`.
        let accented = carrier_name_pattern("Posta").expect("compiles");
        assert!(accented.is_match("éPosta"));
    }
}
