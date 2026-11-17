//! Retrievers that go through a third-party service (Wayback Machine, Jina
//! Reader).

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use jiff::civil::DateTime;
use omni_ai::costs::{NewCostEvent, current_cost_feature};
use omni_api::costs::{CostCategory, CostPriceStatus, CostUsage};
use omni_core::js::encode_uri_component;
use serde::Deserialize;

use super::html::{extract_domain, extract_title_from_html};
use super::{ArticleRetriever, RetrieverContext};
use crate::error::PressPodsError;
use crate::formatting::clean_text;
use crate::public_http::{
    PRESS_PODS_HTML_MAX_BYTES, PRESS_PODS_JSON_MAX_BYTES, PublicGet, fetch_public_json,
    fetch_public_text,
};
use crate::types::Article;

const WAYBACK_AVAILABILITY_API: &str = "https://archive.org/wayback/available";
const JINA_API_BASE: &str = "https://r.jina.ai";

/// Standard prepaid Jina API rate: US $0.05 per million tokens.
pub const JINA_READER_CENTS_PER_TOKEN: f64 = 5.0 / 1_000_000.0;

fn decode<T: for<'de> Deserialize<'de>>(
    value: serde_json::Value,
    operation: &str,
) -> Result<T, PressPodsError> {
    serde_json::from_value(value).map_err(|e| PressPodsError::invalid(operation, e.to_string()))
}

#[derive(Deserialize)]
struct WaybackResponse {
    #[allow(dead_code)]
    url: String,
    archived_snapshots: WaybackSnapshots,
}

#[derive(Deserialize)]
struct WaybackSnapshots {
    closest: Option<WaybackSnapshot>,
}

#[derive(Deserialize)]
struct WaybackSnapshot {
    #[allow(dead_code)]
    status: String,
    available: bool,
    url: String,
    timestamp: String,
}

/// `YYYYMMDDhhmmss` as local time (`new Date(y, m, d, ...)`).
pub fn parse_wayback_timestamp(timestamp: &str, tz: &jiff::tz::TimeZone) -> Option<i64> {
    if timestamp.len() < 8 || !timestamp.is_ascii() {
        return None;
    }
    let part = |start: usize, end: usize| -> Option<i64> {
        if timestamp.len() >= end {
            timestamp[start..end].parse::<i64>().ok()
        } else {
            Some(0)
        }
    };
    let year = i16::try_from(part(0, 4)?).ok()?;
    let month = part(4, 6)?;
    let day = part(6, 8)?;
    let (hour, minute, second) = (part(8, 10)?, part(10, 12)?, part(12, 14)?);
    // `new Date` rolls overflowing fields over; build from the first of the
    // month and add the rest as a span to match.
    let base = DateTime::new(year, 1, 1, 0, 0, 0, 0).ok()?;
    let span = jiff::Span::new()
        .try_months(month - 1)
        .ok()?
        .try_days(day - 1)
        .ok()?
        .try_hours(hour)
        .ok()?
        .try_minutes(minute)
        .ok()?
        .try_seconds(second)
        .ok()?;
    let dt = base.checked_add(span).ok()?;
    dt.to_zoned(tz.clone())
        .ok()
        .map(|z| z.timestamp().as_millisecond())
}

/// Most recent Internet Archive snapshot of the URL.
pub struct WaybackRetriever(pub Arc<RetrieverContext>);

impl ArticleRetriever for WaybackRetriever {
    fn name(&self) -> &str {
        "wayback"
    }

    fn retrieve<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'a str,
    ) -> BoxFuture<'a, Result<Article, PressPodsError>> {
        Box::pin(async move {
            let availability_url = format!(
                "{WAYBACK_AVAILABILITY_API}?url={}",
                encode_uri_component(url)
            );
            let raw = fetch_public_json(
                &self.0.public_http,
                &PublicGet {
                    url: &availability_url,
                    headers: vec![],
                    timeout: Duration::from_secs(10),
                    retries: 0,
                    max_bytes: PRESS_PODS_JSON_MAX_BYTES,
                    operation: "query Wayback availability",
                },
            )
            .await?;
            let response: WaybackResponse = decode(raw, "decode Wayback response")?;
            let Some(snapshot) = response.archived_snapshots.closest.filter(|s| s.available) else {
                return Err(PressPodsError::failed(
                    "retrieve article with Wayback",
                    "No archived snapshot available for this URL",
                ));
            };
            let html = fetch_public_text(
                &self.0.public_http,
                &PublicGet {
                    url: &snapshot.url,
                    headers: vec![("user-agent", user_agent.to_owned())],
                    timeout: Duration::from_secs(20),
                    retries: 0,
                    max_bytes: PRESS_PODS_HTML_MAX_BYTES,
                    operation: "fetch Wayback snapshot",
                },
            )
            .await?;
            Ok(Article {
                title: extract_title_from_html(&html),
                text: clean_text(&html)?,
                author: None,
                domain: extract_domain(url),
                published_at: parse_wayback_timestamp(&snapshot.timestamp, &self.0.tz),
                lead_image_url: None,
                url: url.to_owned(),
            })
        })
    }
}

#[derive(Deserialize)]
struct JinaResponse {
    data: JinaData,
}

#[derive(Deserialize)]
struct JinaData {
    title: Option<String>,
    /// Where the Reader puts the page for `x-respond-with: html`.
    html: Option<String>,
    /// The same page in older Reader responses.
    content: Option<String>,
    usage: Option<JinaUsage>,
}

#[derive(Deserialize)]
struct JinaUsage {
    tokens: Option<serde_json::Value>,
}

/// The Jina Reader API (a headless browser for JS-heavy pages). Only listed
/// when `JINA_API_KEY` is set.
pub struct JinaRetriever {
    pub ctx: Arc<RetrieverContext>,
    pub api_key: String,
}

impl ArticleRetriever for JinaRetriever {
    fn name(&self) -> &str {
        "jina"
    }

    fn retrieve<'a>(
        &'a self,
        url: &'a str,
        _user_agent: &'a str,
    ) -> BoxFuture<'a, Result<Article, PressPodsError>> {
        Box::pin(async move {
            let jina_url = format!("{JINA_API_BASE}/{url}");
            let raw = fetch_public_json(
                &self.ctx.public_http,
                &PublicGet {
                    url: &jina_url,
                    headers: vec![
                        ("accept", "application/json".to_owned()),
                        ("authorization", format!("Bearer {}", self.api_key)),
                        ("x-respond-with", "html".to_owned()),
                        (
                            "x-target-selector",
                            "article, main, [role=main], .article-body, .post-content, .entry-content".to_owned(),
                        ),
                        (
                            "x-remove-selector",
                            "nav, footer, header, aside, .sidebar, .comments, .related, .social-share, .advertisement, [role=navigation], [role=banner], [role=contentinfo]".to_owned(),
                        ),
                    ],
                    timeout: Duration::from_secs(30),
                    retries: 0,
                    max_bytes: PRESS_PODS_JSON_MAX_BYTES,
                    operation: "retrieve article with Jina",
                },
            )
            .await?;
            let response: JinaResponse = decode(raw, "decode Jina Reader response")?;
            let html = response
                .data
                .html
                .filter(|html| !html.is_empty())
                .or(response.data.content)
                .unwrap_or_default();
            if omni_core::js::utf16_len(&html) < 100 {
                return Err(PressPodsError::failed(
                    "retrieve article with Jina",
                    "Jina returned empty or too short content",
                ));
            }
            let tokens = response
                .data
                .usage
                .and_then(|u| u.tokens)
                .and_then(|t| t.as_f64())
                .filter(|t| t.is_finite() && *t >= 0.0);
            self.ctx
                .costs
                .record(NewCostEvent {
                    category: CostCategory::Retrieval,
                    feature: current_cost_feature("press-pods").to_owned(),
                    operation: "retrieve-article".to_owned(),
                    service: "jina".to_owned(),
                    model: Some("reader".to_owned()),
                    cost_cents: tokens.map(|t| t * JINA_READER_CENTS_PER_TOKEN),
                    price_status: if tokens.is_some() {
                        CostPriceStatus::Estimated
                    } else {
                        CostPriceStatus::Unknown
                    },
                    usage: CostUsage {
                        requests: Some(1.0),
                        output_tokens: tokens,
                        ..CostUsage::default()
                    },
                    event_id: None,
                    incurred_at: None,
                    run_id: None,
                })
                .await;
            let title = response
                .data
                .title
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())
                .or_else(|| extract_title_from_html(&html));
            Ok(Article {
                title,
                text: clean_text(&html)?,
                author: None,
                domain: extract_domain(url),
                published_at: None,
                lead_image_url: None,
                url: url.to_owned(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayback_timestamps_are_local_time() {
        let tz = jiff::tz::TimeZone::get("America/Vancouver").unwrap();
        assert_eq!(
            parse_wayback_timestamp("20260714070431", &tz),
            Some(1_784_037_871_000)
        );
        assert!(parse_wayback_timestamp("20260714", &tz).is_some());
        assert_eq!(parse_wayback_timestamp("2026", &tz), None);
        assert_eq!(parse_wayback_timestamp("2026xx14", &tz), None);
    }
}
