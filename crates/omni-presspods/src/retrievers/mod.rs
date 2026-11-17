//! Article retrieval.
//!
//! Every retriever runs independently (all at once, 60 s each); the
//! metadata model rates each distinct extraction once (0-10) and the best
//! usable article wins. One bad retriever never hurts the outcome; it just
//! loses the rating contest.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{self, StreamExt as _};
use jiff::tz::TimeZone;
use omni_ai::costs::CostRecorder;
use omni_http::HttpClient;
use omni_http::public::PublicHttpClient;

use crate::error::PressPodsError;
use crate::types::{Article, MetadataInfo, RetrieverResult, failure_reason, text_chars};
use crate::url::X_HOSTS;

pub mod html;
pub mod local;
pub mod obscura;
pub mod proxies;
pub mod x;

const LOG: &str = "PressPods";

/// Mobile Safari, which most sites serve their simplest markup to.
pub const USER_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 14_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/14.0 Mobile/15A372 Safari/604.1";

/// The operation of a rating outcome fanned out to a retriever. The stored
/// attempt error is the rating cause itself (`"Invalid article"`, the model
/// error), with no operation prefix.
pub const RATING_OPERATION: &str = "rate retrieved PressPods article";
/// The rating outcome of an extraction the model judged not to be an article.
pub const INVALID_ARTICLE: &str = "Invalid article";

/// Per-retriever (and per-rating) deadline.
pub const RETRIEVER_TIMEOUT: Duration = Duration::from_secs(60);
const RATING_CONCURRENCY: usize = 4;

/// One extraction strategy.
pub trait ArticleRetriever: Send + Sync {
    /// Persisted as `retrieverName`.
    fn name(&self) -> &str;
    fn retrieve<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'a str,
    ) -> BoxFuture<'a, Result<Article, PressPodsError>>;
}

/// What the retrievers share.
#[derive(Clone)]
pub struct RetrieverContext {
    /// SSRF-guarded client for article and proxy fetches.
    pub public_http: PublicHttpClient,
    /// Plain client for the fixed FxTwitter API.
    pub http: HttpClient,
    pub jina_api_key: Option<String>,
    /// The headless browser behind the `obscura` retriever; `None` leaves it out.
    pub renderer: Option<Arc<dyn obscura::PageRenderer>>,
    pub costs: CostRecorder,
    pub tz: TimeZone,
}

/// An X/Twitter status permalink (any username, `i/web`).
pub fn is_x_status_url(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    let host = url.host_str().unwrap_or("").to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if !X_HOSTS.contains(&host) {
        return false;
    }
    static PREFIX: std::sync::LazyLock<Option<regex::Regex>> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^/(?:[^/]+/status|i/web/status)/\d+").ok());
    PREFIX.as_ref().is_some_and(|re| re.is_match(url.path()))
}

/// The X retriever alone for status URLs, else the
/// generic set (plus Jina when its key is configured and Obscura when its
/// renderer is). Order matters only on a rating tie, which the earlier
/// retriever wins: Obscura is last, so it is recorded as the winner only when
/// its extraction rated strictly higher than every cheaper one.
pub fn article_retrievers(
    ctx: &Arc<RetrieverContext>,
    url: &str,
) -> Vec<Arc<dyn ArticleRetriever>> {
    if is_x_status_url(url) {
        return vec![Arc::new(x::XRetriever(ctx.clone()))];
    }
    let mut retrievers: Vec<Arc<dyn ArticleRetriever>> = vec![
        Arc::new(local::PostlightRetriever(ctx.clone())),
        Arc::new(local::ReadabilityRetriever(ctx.clone())),
        Arc::new(local::ExtractusRetriever(ctx.clone())),
        Arc::new(proxies::WaybackRetriever(ctx.clone())),
        Arc::new(local::FetchRetriever(ctx.clone())),
    ];
    if let Some(key) = ctx.jina_api_key.as_ref().filter(|k| !k.is_empty()) {
        retrievers.push(Arc::new(proxies::JinaRetriever {
            ctx: ctx.clone(),
            api_key: key.clone(),
        }));
    }
    if let Some(renderer) = ctx.renderer.as_ref() {
        retrievers.push(Arc::new(obscura::ObscuraRetriever {
            ctx: ctx.clone(),
            renderer: renderer.clone(),
        }));
    }
    retrievers
}

/// One retriever's raw outcome, before rating.
#[derive(Debug)]
pub enum Retrieved {
    Success {
        retriever_name: String,
        article: Article,
    },
    Failure {
        retriever_name: String,
        error: PressPodsError,
    },
}

impl Retrieved {
    pub fn retriever_name(&self) -> &str {
        match self {
            Retrieved::Success { retriever_name, .. }
            | Retrieved::Failure { retriever_name, .. } => retriever_name,
        }
    }
}

/// Runs every retriever at once (each bounded by `timeout`, a timed-out one
/// is dropped) and returns one outcome per retriever in input order.
pub async fn run_article_retrievers(
    url: &str,
    retrievers: &[Arc<dyn ArticleRetriever>],
    timeout: Duration,
) -> Vec<Retrieved> {
    let url: Arc<str> = Arc::from(url);
    let owned: Vec<Arc<dyn ArticleRetriever + 'static>> = retrievers.to_vec();
    let runs: Vec<_> = owned
        .into_iter()
        .map(|retriever| run_one(retriever, Arc::clone(&url), timeout))
        .collect();
    let runs_len = runs.len().max(1);
    stream::iter(runs).buffered(runs_len).collect().await
}

async fn run_one(
    retriever: Arc<dyn ArticleRetriever>,
    url: Arc<str>,
    timeout: Duration,
) -> Retrieved {
    let retriever_name = retriever.name().to_owned();
    match tokio::time::timeout(timeout, retriever.retrieve(&url, USER_AGENT)).await {
        Ok(Ok(article)) => Retrieved::Success {
            retriever_name,
            article,
        },
        Ok(Err(error)) => Retrieved::Failure {
            retriever_name,
            error,
        },
        Err(_) => Retrieved::Failure {
            error: PressPodsError::timeout(format!("retrieve article with {retriever_name}")),
            retriever_name,
        },
    }
}

/// Identity of an extraction for rating dedup: everything the rating prompt
/// sees, with line endings normalized and the text trimmed.
#[derive(Clone, PartialEq, Eq, Hash)]
struct RatingKey {
    title: Option<String>,
    text: String,
    author: Option<String>,
    domain: Option<String>,
    url: String,
    published_at: Option<i64>,
    lead_image_url: Option<String>,
}

fn rating_key(article: &Article) -> RatingKey {
    RatingKey {
        title: article.title.clone(),
        text: article
            .text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .trim()
            .to_owned(),
        author: article.author.clone(),
        domain: article.domain.clone(),
        url: article.url.clone(),
        published_at: article.published_at,
        lead_image_url: article.lead_image_url.clone(),
    }
}

/// `(result index, retriever name, article)` of one rating group member.
type GroupMember = (usize, String, Article);

/// Rates each distinct extraction once and fans the verdict back out to every
/// retriever that produced it. Invalid articles and rating failures become
/// failures of each matching retriever. Order and count are preserved.
pub async fn rate_retrieved_articles<F, Fut>(
    retrieved: Vec<Retrieved>,
    rate: F,
) -> Vec<RetrieverResult>
where
    F: Fn(Article) -> Fut,
    Fut: Future<Output = Result<MetadataInfo, PressPodsError>>,
{
    let mut slots: Vec<Option<RetrieverResult>> = Vec::with_capacity(retrieved.len());
    let mut groups: Vec<(Article, Vec<GroupMember>)> = Vec::new();
    let mut by_key: HashMap<RatingKey, usize> = HashMap::new();
    for (index, item) in retrieved.into_iter().enumerate() {
        match item {
            Retrieved::Failure {
                retriever_name,
                error,
            } => slots.push(Some(RetrieverResult::Failure {
                retriever_name,
                error,
                text_chars: None,
            })),
            Retrieved::Success {
                retriever_name,
                article,
            } => {
                slots.push(None);
                let key = rating_key(&article);
                let group = *by_key.entry(key).or_insert_with(|| {
                    groups.push((article.clone(), Vec::new()));
                    groups.len() - 1
                });
                if let Some((_, members)) = groups.get_mut(group) {
                    members.push((index, retriever_name, article));
                }
            }
        }
    }

    let rated: Vec<(Result<MetadataInfo, PressPodsError>, Vec<GroupMember>)> = stream::iter(groups)
        .map(|(representative, members)| {
            let fut = rate(representative);
            async move { (fut.await, members) }
        })
        .buffer_unordered(RATING_CONCURRENCY)
        .collect()
        .await;

    for (verdict, members) in rated {
        for (index, retriever_name, article) in members {
            let result = match &verdict {
                Ok(metadata) if metadata.is_valid_article => RetrieverResult::Success {
                    retriever_name,
                    article,
                    metadata: metadata.clone(),
                },
                Ok(_) => RetrieverResult::Failure {
                    retriever_name,
                    error: PressPodsError::failed(RATING_OPERATION, INVALID_ARTICLE),
                    text_chars: Some(text_chars(&article.text)),
                },
                // A model outage is not a verdict on the article: keep
                // whether it is worth another attempt.
                Err(error) => RetrieverResult::Failure {
                    retriever_name,
                    error: PressPodsError::Failed {
                        operation: RATING_OPERATION.to_owned(),
                        message: error.cause_message(),
                        retryable: Some(error.is_retryable()),
                    },
                    text_chars: Some(text_chars(&article.text)),
                },
            };
            if let Some(slot) = slots.get_mut(index) {
                *slot = Some(result);
            }
        }
    }
    slots.into_iter().flatten().collect()
}

/// The winning extraction plus every retriever's outcome.
#[derive(Debug)]
pub struct BestArticle {
    pub article: Article,
    pub metadata: MetadataInfo,
    pub retriever_name: String,
    pub all_results: Vec<RetrieverResult>,
}

/// Why no candidate could be used: every retriever's reason in the stored
/// summary, retryable only when an extraction existed but the rating model
/// was temporarily unavailable.
fn all_failed(all_results: &[RetrieverResult]) -> PressPodsError {
    // Retrievers that failed the same way share one entry, so the stored
    // summary has room for all of them.
    let mut reasons: Vec<(String, Vec<&str>)> = Vec::new();
    let mut rating_outage = false;
    for result in all_results {
        if let RetrieverResult::Failure {
            retriever_name,
            error,
            ..
        } = result
        {
            tracing::warn!(target: LOG, error = %error, "Retriever {retriever_name} failed:");
            let reason = failure_reason(error);
            match reasons.iter_mut().find(|(known, _)| *known == reason) {
                Some((_, names)) => names.push(retriever_name),
                None => reasons.push((reason, vec![retriever_name])),
            }
            rating_outage |= error.operation() == RATING_OPERATION && error.is_retryable();
        }
    }
    let message = if reasons.is_empty() {
        "All article retrievers failed".to_owned()
    } else {
        let reasons: Vec<String> = reasons
            .iter()
            .map(|(reason, names)| format!("{}: {reason}", names.join(", ")))
            .collect();
        format!("All article retrievers failed ({})", reasons.join("; "))
    };
    PressPodsError::Failed {
        operation: "retrieve PressPods article".to_owned(),
        message,
        retryable: Some(rating_outage),
    }
}

/// Picks the highest-rated successful result (the first on ties). Logs every
/// candidate's outcome and errors when nothing succeeded.
pub fn select_best(all_results: Vec<RetrieverResult>) -> Result<BestArticle, PressPodsError> {
    let best = all_results
        .iter()
        .enumerate()
        .filter_map(|(i, r)| match r {
            RetrieverResult::Success { metadata, .. } => Some((i, metadata.content_rating)),
            RetrieverResult::Failure { .. } => None,
        })
        .fold(None::<(usize, f64)>, |best, (i, rating)| match best {
            Some((_, top)) if top >= rating => best,
            _ => Some((i, rating)),
        });
    let Some((article, metadata, retriever_name)) =
        best.and_then(|(index, _)| match all_results.get(index) {
            Some(RetrieverResult::Success {
                article,
                metadata,
                retriever_name,
            }) => Some((article.clone(), metadata.clone(), retriever_name.clone())),
            _ => None,
        })
    else {
        return Err(all_failed(&all_results));
    };
    let candidates = all_results
        .iter()
        .map(|result| match result {
            RetrieverResult::Success {
                retriever_name,
                article,
                metadata,
            } => format!(
                "{retriever_name}={} ({} chars)",
                metadata.content_rating,
                omni_core::js::utf16_len(&article.text)
            ),
            RetrieverResult::Failure { retriever_name, .. } => format!("{retriever_name}=failed"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!(target: LOG, candidates = candidates.as_str(), "Retriever {retriever_name} selected as best");
    Ok(BestArticle {
        article,
        metadata,
        retriever_name,
        all_results,
    })
}
