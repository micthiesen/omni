//! Pure decisions over Amazon pages: URLs, page-state classification, and
//! turning the extraction scripts' untrusted output into bounded results.

use serde::{Deserialize, Serialize};
use url::form_urlencoded;

/// The page-state probe; returns [`PageState`] as JSON.
pub const STATE_SCRIPT: &str = include_str!("js/state.js");
/// Search-result extraction; returns [`RawSearch`] as JSON.
pub const SEARCH_SCRIPT: &str = include_str!("js/search.js");
/// Product-page extraction; returns [`RawProduct`] as JSON.
pub const PRODUCT_SCRIPT: &str = include_str!("js/product.js");
/// Presses "Continue shopping" on Amazon's interstitial.
pub const CONTINUE_SCRIPT: &str = "(() => { const b = document.querySelector('form[action*=\"validateCaptcha\"] button'); if (b) b.click(); return !!b; })()";

const MAX_TITLE: usize = 300;
const MAX_SHORT: usize = 120;
const MAX_BULLET: usize = 500;
const MAX_BULLETS: usize = 10;
const MAX_DETAIL_NAME: usize = 80;
const MAX_DETAIL_VALUE: usize = 300;
const MAX_DETAILS: usize = 30;
const MAX_REVIEWS: usize = 8;
const MAX_REVIEW_BODY: usize = 1_200;
const MAX_SUMMARY: usize = 1_000;
pub const MAX_RESULTS: usize = 48;

/// Which Amazon storefront to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Store {
    #[default]
    Ca,
    Com,
}

impl Store {
    pub fn origin(self) -> &'static str {
        match self {
            Self::Ca => "https://www.amazon.ca",
            Self::Com => "https://www.amazon.com",
        }
    }

    pub fn currency(self) -> &'static str {
        match self {
            Self::Ca => "CAD",
            Self::Com => "USD",
        }
    }

    pub fn home_url(self) -> String {
        format!("{}/", self.origin())
    }

    pub fn product_url(self, asin: &Asin) -> String {
        format!("{}/dp/{}", self.origin(), asin.as_str())
    }
}

/// Search result ordering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sort {
    #[default]
    Relevance,
    PriceLow,
    PriceHigh,
    Rating,
    Newest,
}

impl Sort {
    fn param(self) -> Option<&'static str> {
        match self {
            Self::Relevance => None,
            Self::PriceLow => Some("price-asc-rank"),
            Self::PriceHigh => Some("price-desc-rank"),
            Self::Rating => Some("review-rank"),
            Self::Newest => Some("date-desc-rank"),
        }
    }
}

/// The search results page URL.
pub fn search_url(store: Store, query: &str, page: u32, sort: Sort) -> String {
    let mut pairs = form_urlencoded::Serializer::new(String::new());
    pairs.append_pair("k", query);
    if let Some(sort) = sort.param() {
        pairs.append_pair("s", sort);
    }
    if page > 1 {
        pairs.append_pair("page", &page.to_string());
    }
    format!("{}/s?{}", store.origin(), pairs.finish())
}

/// A ten-character Amazon Standard Identification Number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asin(String);

impl Asin {
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_uppercase();
        (value.len() == 10 && value.bytes().all(|b| b.is_ascii_alphanumeric()))
            .then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What the state probe saw.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageState {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub captcha: bool,
    #[serde(default)]
    pub continue_button: bool,
    #[serde(default)]
    pub search: bool,
    #[serde(default)]
    pub product: bool,
    #[serde(default)]
    pub not_found: bool,
}

/// The page a load expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    Search,
    Product,
}

/// What to do with a loaded page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ready,
    NotFound,
    /// An image CAPTCHA: warming up again will not help.
    Captcha,
    /// Bot check or interstitial: warm up and retry.
    Blocked,
}

impl PageState {
    pub fn verdict(&self, kind: PageKind) -> Verdict {
        let ready = match kind {
            PageKind::Search => self.search,
            PageKind::Product => self.product,
        };
        if ready {
            Verdict::Ready
        } else if self.captcha {
            Verdict::Captcha
        } else if self.not_found && kind == PageKind::Product {
            Verdict::NotFound
        } else {
            Verdict::Blocked
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawSearch {
    #[serde(default)]
    pub total: Option<String>,
    #[serde(default)]
    pub results: Vec<RawResult>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawResult {
    #[serde(default)]
    pub asin: String,
    pub title: Option<String>,
    pub brand: Option<String>,
    pub price: Option<String>,
    pub list_price: Option<String>,
    pub rating: Option<String>,
    pub review_count: Option<String>,
    #[serde(default)]
    pub sponsored: bool,
    pub delivery: Option<String>,
    pub badge: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawProduct {
    pub title: Option<String>,
    pub brand: Option<String>,
    pub price: Option<String>,
    pub list_price: Option<String>,
    pub rating: Option<String>,
    pub review_count: Option<String>,
    pub availability: Option<String>,
    pub seller: Option<String>,
    pub ships_from: Option<String>,
    #[serde(default)]
    pub bullets: Vec<String>,
    #[serde(default)]
    pub details: Vec<(String, String)>,
    #[serde(default)]
    pub reviews: Vec<RawReview>,
    pub summary: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawReview {
    pub title: Option<String>,
    pub rating: Option<String>,
    pub date: Option<String>,
    #[serde(default)]
    pub verified: bool,
    pub body: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResults {
    pub store: Store,
    pub currency: &'static str,
    pub url: String,
    pub page: u32,
    pub total: Option<String>,
    pub sponsored_hidden: u32,
    pub results: Vec<SearchResult>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub asin: String,
    pub url: String,
    pub title: Option<String>,
    pub brand: Option<String>,
    pub price: Option<String>,
    pub list_price: Option<String>,
    pub rating: Option<f64>,
    pub review_count: Option<String>,
    pub sponsored: bool,
    pub badge: Option<String>,
    pub delivery: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Product {
    pub asin: String,
    pub store: Store,
    pub currency: &'static str,
    pub url: String,
    pub title: String,
    pub brand: Option<String>,
    pub price: Option<String>,
    pub list_price: Option<String>,
    pub rating: Option<f64>,
    pub review_count: Option<u64>,
    pub availability: Option<String>,
    pub seller: Option<String>,
    pub ships_from: Option<String>,
    pub bullets: Vec<String>,
    pub details: Vec<Detail>,
    pub review_summary: Option<String>,
    pub reviews: Vec<Review>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Detail {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub title: Option<String>,
    pub rating: Option<f64>,
    pub date: Option<String>,
    pub verified: bool,
    pub body: Option<String>,
}

/// Search options after input validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    pub store: Store,
    pub page: u32,
    pub limit: usize,
    pub include_sponsored: bool,
}

/// Bounded, deduplicated results from the search script's output.
pub fn search_results(raw: RawSearch, url: String, options: &SearchOptions) -> SearchResults {
    let mut seen = Vec::new();
    let mut hidden = 0;
    let mut results = Vec::new();
    for item in raw.results {
        let Some(asin) = Asin::parse(&item.asin) else {
            continue;
        };
        if seen.contains(&asin) {
            continue;
        }
        seen.push(asin.clone());
        if item.sponsored && !options.include_sponsored {
            hidden += 1;
            continue;
        }
        if results.len() >= options.limit {
            continue;
        }
        results.push(SearchResult {
            url: options.store.product_url(&asin),
            asin: asin.0,
            title: bounded(item.title, MAX_TITLE),
            brand: bounded(item.brand, MAX_SHORT),
            price: bounded(item.price, MAX_SHORT),
            list_price: bounded(item.list_price, MAX_SHORT),
            rating: item.rating.as_deref().and_then(rating),
            review_count: bounded(
                item.review_count
                    .map(|c| c.trim_matches(|ch| ch == '(' || ch == ')').to_owned()),
                MAX_SHORT,
            ),
            sponsored: item.sponsored,
            badge: bounded(item.badge, MAX_SHORT),
            delivery: bounded(item.delivery, MAX_SHORT * 2),
        });
    }
    SearchResults {
        store: options.store,
        currency: options.store.currency(),
        url,
        page: options.page,
        total: bounded(raw.total, MAX_SHORT),
        sponsored_hidden: hidden,
        results,
    }
}

/// The product, or `None` when the page had no title.
pub fn product(raw: RawProduct, asin: &Asin, store: Store) -> Option<Product> {
    let title = bounded(raw.title, MAX_TITLE)?;
    let mut details: Vec<Detail> = Vec::new();
    for (name, value) in raw.details {
        let (Some(name), Some(value)) = (
            bounded(Some(name), MAX_DETAIL_NAME),
            bounded(Some(value), MAX_DETAIL_VALUE),
        ) else {
            continue;
        };
        if details.len() >= MAX_DETAILS
            || is_noise_detail(&name)
            || details.iter().any(|d| d.name.eq_ignore_ascii_case(&name))
        {
            continue;
        }
        details.push(Detail { name, value });
    }
    let mut bullets: Vec<String> = Vec::new();
    for bullet in raw.bullets {
        if let Some(bullet) = bounded(Some(bullet), MAX_BULLET)
            && bullets.len() < MAX_BULLETS
            && !bullets.contains(&bullet)
        {
            bullets.push(bullet);
        }
    }
    let reviews = raw
        .reviews
        .into_iter()
        .filter(|r| r.body.as_deref().is_some_and(|b| !b.trim().is_empty()))
        .take(MAX_REVIEWS)
        .map(|r| Review {
            title: bounded(r.title, MAX_SHORT * 2),
            rating: r.rating.as_deref().and_then(rating),
            date: bounded(r.date, MAX_SHORT),
            verified: r.verified,
            body: bounded(r.body, MAX_REVIEW_BODY),
        })
        .collect();
    Some(Product {
        url: store.product_url(asin),
        asin: asin.as_str().to_owned(),
        store,
        currency: store.currency(),
        title,
        brand: bounded(raw.brand.map(|b| brand(&b)), MAX_SHORT),
        price: bounded(raw.price, MAX_SHORT),
        list_price: bounded(raw.list_price, MAX_SHORT),
        rating: raw.rating.as_deref().and_then(rating),
        review_count: raw.review_count.as_deref().and_then(count),
        availability: bounded(raw.availability, MAX_SHORT * 2),
        seller: bounded(raw.seller, MAX_SHORT),
        ships_from: bounded(raw.ships_from, MAX_SHORT),
        bullets,
        details,
        review_summary: bounded(raw.summary, MAX_SUMMARY),
        reviews,
    })
}

/// Rows that repeat fields reported elsewhere.
fn is_noise_detail(name: &str) -> bool {
    ["Customer Reviews", "Customer reviews"].contains(&name)
}

/// "4.5 out of 5 stars" -> 4.5.
pub fn rating(text: &str) -> Option<f64> {
    let first = text.split_whitespace().next()?;
    let value: f64 = first.replace(',', ".").parse().ok()?;
    (0.0..=5.0).contains(&value).then_some(value)
}

/// "(13,249)" or "13,249 ratings" -> 13249; abbreviated counts ("13K") are
/// not exact, so they are dropped.
pub fn count(text: &str) -> Option<u64> {
    let token = text.split_whitespace().next()?;
    if token.chars().any(char::is_alphabetic) {
        return None;
    }
    let digits: String = token.chars().filter(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// "Visit the UGREEN Store" / "Brand: UGREEN" -> "UGREEN".
pub fn brand(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_prefix("Brand:").unwrap_or(text).trim();
    text.strip_prefix("Visit the ")
        .and_then(|rest| rest.strip_suffix(" Store"))
        .unwrap_or(text)
        .to_owned()
}

/// Whitespace-collapsed, non-empty, at most `max` characters.
fn bounded(value: Option<String>, max: usize) -> Option<String> {
    let value = value?;
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= max {
        return Some(collapsed);
    }
    let mut cut: String = collapsed.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    Some(cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_encodes_query_sort_and_page() {
        assert_eq!(
            search_url(Store::Ca, "usb-c charger & cable", 1, Sort::Relevance),
            "https://www.amazon.ca/s?k=usb-c+charger+%26+cable"
        );
        assert_eq!(
            search_url(Store::Com, "socks", 3, Sort::PriceLow),
            "https://www.amazon.com/s?k=socks&s=price-asc-rank&page=3"
        );
    }

    #[test]
    fn asin_accepts_only_ten_alphanumerics() {
        assert_eq!(
            Asin::parse(" b091bgmkys ").map(|a| a.0),
            Some("B091BGMKYS".into())
        );
        assert!(Asin::parse("B091BGMKY").is_none());
        assert!(Asin::parse("B091BGMKY/").is_none());
        assert!(Asin::parse("../../etc/x").is_none());
    }

    #[test]
    fn verdicts() {
        let blocked = PageState {
            title: "Sorry! Something went wrong!".into(),
            ..PageState::default()
        };
        assert_eq!(blocked.verdict(PageKind::Search), Verdict::Blocked);
        let interstitial = PageState {
            continue_button: true,
            ..PageState::default()
        };
        assert_eq!(interstitial.verdict(PageKind::Product), Verdict::Blocked);
        let captcha = PageState {
            captcha: true,
            continue_button: true,
            ..PageState::default()
        };
        assert_eq!(captcha.verdict(PageKind::Search), Verdict::Captcha);
        let missing = PageState {
            not_found: true,
            ..PageState::default()
        };
        assert_eq!(missing.verdict(PageKind::Product), Verdict::NotFound);
        assert_eq!(missing.verdict(PageKind::Search), Verdict::Blocked);
        let product = PageState {
            product: true,
            ..PageState::default()
        };
        assert_eq!(product.verdict(PageKind::Product), Verdict::Ready);
    }

    #[test]
    fn parses_ratings_counts_and_brands() {
        assert_eq!(rating("4.5 out of 5 stars"), Some(4.5));
        assert_eq!(rating("nope"), None);
        assert_eq!(rating("9 out of 5"), None);
        assert_eq!(count("(13,249)"), Some(13_249));
        assert_eq!(count("22,941 ratings"), Some(22_941));
        assert_eq!(count("13K ratings"), None);
        assert_eq!(brand("Visit the UGREEN Store"), "UGREEN");
        assert_eq!(brand("Brand: Anker"), "Anker");
        assert_eq!(brand("Lenovo"), "Lenovo");
    }

    #[test]
    fn search_results_dedupe_hide_sponsored_and_limit() {
        let item = |asin: &str, sponsored: bool| RawResult {
            asin: asin.into(),
            title: Some(format!("  Item   {asin} ")),
            rating: Some("4.4 out of 5 stars".into()),
            review_count: Some("(11.4K)".into()),
            sponsored,
            ..RawResult::default()
        };
        let raw = RawSearch {
            total: Some("1-48 of over 1,000 results for".into()),
            results: vec![
                item("AAAAAAAAA1", true),
                item("AAAAAAAAA2", false),
                item("AAAAAAAAA2", false),
                item("bad", false),
                item("AAAAAAAAA3", false),
                item("AAAAAAAAA4", false),
            ],
        };
        let options = SearchOptions {
            store: Store::Ca,
            page: 1,
            limit: 2,
            include_sponsored: false,
        };
        let out = search_results(raw, "u".into(), &options);
        assert_eq!(out.sponsored_hidden, 1);
        assert_eq!(
            out.results
                .iter()
                .map(|r| r.asin.as_str())
                .collect::<Vec<_>>(),
            ["AAAAAAAAA2", "AAAAAAAAA3"]
        );
        assert_eq!(out.results[0].title.as_deref(), Some("Item AAAAAAAAA2"));
        assert_eq!(out.results[0].review_count.as_deref(), Some("11.4K"));
        assert_eq!(out.results[0].url, "https://www.amazon.ca/dp/AAAAAAAAA2");
        assert_eq!(out.currency, "CAD");
    }

    #[test]
    fn product_bounds_and_cleans_fields() {
        let asin = Asin::parse("B091BGMKYS").unwrap();
        assert!(product(RawProduct::default(), &asin, Store::Ca).is_none());
        let raw = RawProduct {
            title: Some("Charger".into()),
            brand: Some("Visit the UGREEN Store".into()),
            review_count: Some("(13,249)".into()),
            bullets: vec!["a".into(), "a".into(), "x".repeat(900)],
            details: vec![
                ("Brand".into(), "UGREEN".into()),
                ("brand".into(), "dup".into()),
                ("Customer Reviews".into(), "4.5".into()),
            ],
            reviews: vec![
                RawReview {
                    body: Some(" ".into()),
                    ..RawReview::default()
                },
                RawReview {
                    rating: Some("5 out of 5 stars".into()),
                    body: Some("Works.".into()),
                    verified: true,
                    ..RawReview::default()
                },
            ],
            ..RawProduct::default()
        };
        let p = product(raw, &asin, Store::Ca).unwrap();
        assert_eq!(p.brand.as_deref(), Some("UGREEN"));
        assert_eq!(p.review_count, Some(13_249));
        assert_eq!(p.bullets.len(), 2);
        assert_eq!(p.bullets[1].chars().count(), MAX_BULLET);
        assert_eq!(
            p.details,
            vec![Detail {
                name: "Brand".into(),
                value: "UGREEN".into()
            }]
        );
        assert_eq!(p.reviews.len(), 1);
        assert_eq!(p.reviews[0].rating, Some(5.0));
    }
}
