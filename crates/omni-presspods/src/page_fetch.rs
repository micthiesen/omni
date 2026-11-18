//! The [`PageFetcher`] port: the retrievers' candidates for a public URL,
//! for subsystems that read a submitted page themselves.
//!
//! It runs the retriever set PressPods uses for the URL, at once and with
//! the same per-retriever deadline and SSRF-guarded client, and returns
//! every candidate. Nothing is rated, stored or queued, and no job history
//! is written. Pages are untrusted third-party content.

use std::sync::Arc;

use futures::future::{BoxFuture, join_all};
use omni_http::HttpError;
use omni_http::public::assert_public_http_url_syntax;
use omni_runtime::AppContext;
use omni_runtime::ports::{FetchedPage, PageFetcher, PortError};

use crate::error::PressPodsError;
use crate::retrievers::{
    ArticleRetriever, RETRIEVER_TIMEOUT, RetrievedPage, RetrieverContext, USER_AGENT,
    article_retrievers,
};
use crate::types::failure_reason;

/// The retriever that reads the submitted URL itself.
const DIRECT_SOURCE: &str = "fetch";

/// [`PageFetcher`] over the PressPods retrievers.
pub struct PressPodsPageFetcher {
    ctx: Arc<RetrieverContext>,
}

impl PressPodsPageFetcher {
    pub fn new(ctx: Arc<RetrieverContext>) -> Self {
        Self { ctx }
    }

    /// Production wiring; needs no PressPods credential.
    pub fn from_context(ctx: &AppContext) -> Self {
        Self::new(Arc::new(RetrieverContext::from_context(ctx)))
    }
}

async fn run_one(
    retriever: Arc<dyn ArticleRetriever>,
    url: &str,
) -> (String, Result<RetrievedPage, PressPodsError>) {
    let name = retriever.name().to_owned();
    let outcome =
        match tokio::time::timeout(RETRIEVER_TIMEOUT, retriever.retrieve_page(url, USER_AGENT))
            .await
        {
            Ok(outcome) => outcome,
            Err(_) => Err(PressPodsError::timeout(format!(
                "retrieve page with {name}"
            ))),
        };
    (name, outcome)
}

fn timed_out(error: &PressPodsError) -> bool {
    matches!(
        error,
        PressPodsError::Timeout { .. }
            | PressPodsError::Http {
                source: HttpError::Timeout,
                ..
            }
    )
}

/// Why no source had the page. The lead says what the host itself did;
/// each source's reason follows as an HTTP status or the first line of its
/// cause, so no response body is included.
fn no_page(failures: &[(String, PressPodsError)]) -> PortError {
    // Sources that failed the same way share one entry.
    let mut reasons: Vec<(String, Vec<&str>)> = Vec::new();
    for (name, error) in failures {
        let reason = failure_reason(error);
        match reasons.iter_mut().find(|(known, _)| *known == reason) {
            Some((_, names)) => names.push(name),
            None => reasons.push((reason, vec![name])),
        }
    }
    let reasons: Vec<String> = reasons
        .iter()
        .map(|(reason, names)| format!("{}: {reason}", names.join(", ")))
        .collect();
    let direct = failures
        .iter()
        .find(|(name, _)| name == DIRECT_SOURCE)
        .or(failures.first())
        .map(|(_, error)| error);
    let lead = match direct {
        Some(error) if timed_out(error) => "The host timed out; all page sources failed".to_owned(),
        Some(error) => match error.status_code() {
            Some(status) => {
                format!("The host refused the request (HTTP {status}); all page sources failed")
            }
            None => "All page sources failed".to_owned(),
        },
        None => "All page sources failed".to_owned(),
    };
    PortError::Failed {
        message: if reasons.is_empty() {
            lead
        } else {
            format!("{lead} ({})", reasons.join("; "))
        },
        transient: failures.iter().any(|(_, error)| error.is_retryable()),
    }
}

impl PageFetcher for PressPodsPageFetcher {
    fn fetch_pages<'a>(
        &'a self,
        url: &'a str,
    ) -> BoxFuture<'a, Result<Vec<FetchedPage>, PortError>> {
        Box::pin(async move {
            assert_public_http_url_syntax(url).map_err(|error| PortError::Failed {
                message: format!("Not a public http(s) URL: {error}"),
                transient: false,
            })?;
            let retrievers = article_retrievers(&self.ctx, url);
            let outcomes = join_all(
                retrievers
                    .into_iter()
                    .map(|retriever| run_one(retriever, url)),
            )
            .await;
            let mut pages = Vec::new();
            let mut failures = Vec::new();
            for (source, outcome) in outcomes {
                match outcome {
                    Ok(RetrievedPage {
                        html,
                        url,
                        article: Ok(article),
                    }) => pages.push(FetchedPage {
                        source,
                        url,
                        html,
                        title: article.title,
                        text: Some(article.text),
                        lead_image_url: article.lead_image_url,
                        author: article.author,
                    }),
                    // A page with nothing to extract is still the page.
                    Ok(RetrievedPage {
                        html: Some(html),
                        url,
                        article: Err(_),
                    }) => pages.push(FetchedPage {
                        source,
                        url,
                        html: Some(html),
                        ..FetchedPage::default()
                    }),
                    Ok(RetrievedPage {
                        article: Err(error),
                        ..
                    })
                    | Err(error) => failures.push((source, error)),
                }
            }
            if pages.is_empty() {
                return Err(no_page(&failures));
            }
            Ok(pages)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(failures: Vec<(&str, PressPodsError)>) -> (String, bool) {
        let owned: Vec<(String, PressPodsError)> = failures
            .into_iter()
            .map(|(name, error)| (name.to_owned(), error))
            .collect();
        match no_page(&owned) {
            PortError::Failed { message, transient } => (message, transient),
            other @ PortError::Unavailable(_) => (other.to_string(), false),
        }
    }

    fn status(status: u16) -> PressPodsError {
        PressPodsError::http(
            "fetch public PressPods HTML",
            HttpError::Status {
                status,
                body: "<html>SECRET CHALLENGE PAGE</html>".to_owned(),
            },
        )
    }

    #[test]
    fn a_refusal_names_the_status_and_never_the_body() {
        let (message, transient) = failure(vec![
            ("readability", status(403)),
            (
                "wayback",
                PressPodsError::failed("retrieve article with Wayback", "No archived snapshot"),
            ),
            ("fetch", status(403)),
        ]);
        assert_eq!(
            message,
            "The host refused the request (HTTP 403); all page sources failed \
             (readability, fetch: HTTP 403; wayback: No archived snapshot)"
        );
        assert!(!message.contains("SECRET"));
        assert!(!transient);
    }

    #[test]
    fn a_slow_host_is_a_timeout_worth_another_try() {
        for error in [
            PressPodsError::http("fetch public PressPods HTML", HttpError::Timeout),
            PressPodsError::timeout("retrieve page with fetch"),
        ] {
            let (message, _) = failure(vec![("fetch", error)]);
            assert!(
                message.starts_with("The host timed out; all page sources failed ("),
                "{message}"
            );
        }
        let (_, transient) = failure(vec![(
            "fetch",
            PressPodsError::http("fetch public PressPods HTML", HttpError::Timeout),
        )]);
        assert!(transient);
    }

    #[test]
    fn anything_else_is_all_sources_failed() {
        let (message, _) = failure(vec![(
            "fetch",
            PressPodsError::http("fetch", HttpError::Network("connection reset".to_owned())),
        )]);
        assert_eq!(
            message,
            "All page sources failed (fetch: network error: connection reset)"
        );
        assert_eq!(failure(vec![]).0, "All page sources failed");
    }
}
