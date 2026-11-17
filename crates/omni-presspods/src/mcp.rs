//! PressPods MCP tools. Metadata (names, schemas, annotations, policy) is
//! declared in [`defs`].

pub mod defs;

use omni_mcp_kit::{McpTool, ToolError, ToolMetaError, paginate, truncate_utf16, typed_tool};
use serde::{Deserialize, Serialize};

use crate::error::PressPodsError;
use crate::model::{
    AttemptOutcome, JobStatus, PressPodsEpisode, PressPodsJob, RetrieverErrorClass,
    RetrieverOutcome,
};
use crate::persistence::{AttemptHistoryEntry, job_normalized_url};
use crate::routes::{cost_cents, job_dto, js_numbers};
use crate::service::PressPods;
use crate::storage::checkpoint_work_id;
use crate::submit::first_line_public_url;

/// An episode for MCP (no audio file name or lead image).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpEpisode {
    pub episode_id: String,
    pub title: String,
    pub author: Option<String>,
    pub publication: Option<String>,
    pub domain: Option<String>,
    pub article_url: String,
    pub excerpt: Option<String>,
    pub voice_name: Option<String>,
    pub voice_provider: Option<String>,
    pub duration_seconds: Option<f64>,
    pub file_bytes: i64,
    pub retriever_name: Option<String>,
    pub cost_cents: Option<f64>,
    pub created_at: i64,
    pub published_at: Option<i64>,
    pub run_id: Option<String>,
    pub chapter_count: usize,
}

fn mcp_episode(e: &PressPodsEpisode) -> McpEpisode {
    McpEpisode {
        episode_id: e.episode_id.clone(),
        title: e.title.clone(),
        author: e.author.clone(),
        publication: e.publication.clone(),
        domain: e.domain.clone(),
        article_url: e.article_url.clone(),
        excerpt: e.excerpt.clone(),
        voice_name: e.voice_name.clone(),
        voice_provider: e.voice_provider.clone(),
        duration_seconds: e.duration_seconds,
        file_bytes: e.file_bytes,
        retriever_name: e.retriever_name.clone(),
        cost_cents: cost_cents(e.costs.as_ref()),
        created_at: e.created_at,
        published_at: e.published_at,
        run_id: e.run_id.clone(),
        chapter_count: e.chapters.as_ref().map_or(0, Vec::len),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListResource {
    Episodes,
    Jobs,
    History,
}

/// One retriever's outcome in a history item.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpRetrieverOutcome {
    pub name: String,
    pub ok: bool,
    pub selected: bool,
    pub error_class: Option<RetrieverErrorClass>,
    pub http_status: Option<u16>,
    pub error: Option<String>,
    pub text_chars: Option<i64>,
    pub content_rating: Option<f64>,
}

fn mcp_retriever_outcome(o: &RetrieverOutcome) -> McpRetrieverOutcome {
    McpRetrieverOutcome {
        name: o.name.clone(),
        ok: o.ok,
        selected: o.selected.unwrap_or(false),
        error_class: o.error_class,
        http_status: o.http_status,
        error: o.error.clone(),
        text_chars: o.text_chars,
        content_rating: o.content_rating,
    }
}

/// One recorded job attempt for MCP.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpAttempt {
    pub attempt_id: String,
    pub job_id: String,
    pub url: String,
    pub domain: Option<String>,
    pub attempt: i64,
    pub at: i64,
    pub outcome: AttemptOutcome,
    pub error: Option<String>,
    pub operation: Option<String>,
    pub retrievers: Option<Vec<McpRetrieverOutcome>>,
    pub run_id: Option<String>,
    pub episode_id: Option<String>,
    pub resolved_at: Option<i64>,
    pub resolved_by_episode_id: Option<String>,
}

fn mcp_attempt(entry: &AttemptHistoryEntry) -> McpAttempt {
    let a = &entry.attempt;
    McpAttempt {
        attempt_id: a.attempt_id.clone(),
        job_id: a.job_id.clone(),
        url: a.url.clone(),
        domain: a.domain.clone(),
        attempt: a.attempt,
        at: a.at,
        outcome: a.outcome,
        error: a.error.clone(),
        operation: a.operation.clone(),
        retrievers: a
            .retrievers
            .as_ref()
            .map(|all| all.iter().map(mcp_retriever_outcome).collect()),
        run_id: a.run_id.clone(),
        episode_id: a.episode_id.clone(),
        resolved_at: entry.resolved_at,
        resolved_by_episode_id: entry.resolved_by_episode_id.clone(),
    }
}

/// Start of a `YYYY-MM-DD` day in `tz`, plus `days`, as epoch milliseconds.
fn day_start_ms(date: &str, days: i64, tz: &jiff::tz::TimeZone) -> Result<i64, ToolError> {
    let invalid = || ToolError::input(format!("\"{date}\" is not a YYYY-MM-DD date"));
    let day: jiff::civil::Date = date.parse().map_err(|_| invalid())?;
    let day = day
        .checked_add(jiff::Span::new().days(days))
        .map_err(|_| invalid())?;
    let start = day.to_zoned(tz.clone()).map_err(|_| invalid())?;
    Ok(start.timestamp().as_millisecond())
}

/// The `domain` filter as a bare host: a pasted URL is reduced to its host,
/// and case, a leading `www.` and a trailing dot are ignored.
fn wanted_domain(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains("://")
        && let Some(host) = crate::url::url_domain(raw)
    {
        return host;
    }
    let lowered = raw.to_lowercase();
    let host = lowered.strip_prefix("www.").unwrap_or(&lowered);
    host.strip_suffix('.').unwrap_or(host).to_owned()
}

/// `domain` matches itself and its subdomains, ignoring case and `www.`.
fn domain_matches(row: Option<&str>, wanted: &str) -> bool {
    row.is_some_and(|domain| {
        domain == wanted
            || domain
                .strip_suffix(wanted)
                .is_some_and(|rest| rest.ends_with('.'))
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListInput {
    resource: ListResource,
    status: Option<JobStatus>,
    query: Option<String>,
    domain: Option<String>,
    since: Option<String>,
    until: Option<String>,
    #[serde(default)]
    cursor: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    25
}

/// `{resource, items, nextCursor, total}`, in this key order.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListOutput {
    resource: ListResource,
    items: serde_json::Value,
    next_cursor: Option<usize>,
    total: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodeInput {
    episode_id: String,
}

#[derive(Serialize)]
struct EpisodeOutput {
    episode: McpEpisode,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptInput {
    episode_id: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_max_chars")]
    max_chars: usize,
}

fn default_max_chars() -> usize {
    4000
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptOutput {
    episode_id: String,
    title: String,
    offset: usize,
    text: String,
    next_offset: Option<usize>,
    total_chars: usize,
    truncated: bool,
}

#[derive(Deserialize)]
struct SubmitInput {
    url: String,
}

#[derive(Serialize)]
struct JobOutput {
    job: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(tag = "resource", rename_all = "lowercase")]
enum ResourceInput {
    Episode {
        #[serde(rename = "episodeId")]
        episode_id: String,
    },
    Job {
        #[serde(rename = "jobId")]
        job_id: String,
    },
}

#[derive(Serialize)]
struct DeleteOutput {
    resource: &'static str,
    deleted: bool,
}

fn execute(error: &PressPodsError) -> ToolError {
    ToolError::execute_from(error)
}

fn job_value(job: &PressPodsJob) -> Result<serde_json::Value, ToolError> {
    serde_json::to_value(job_dto(job))
        .map(js_numbers)
        .map_err(|e| ToolError::output(e.to_string()))
}

fn contains_ci(value: Option<&str>, needle: &str) -> bool {
    value.is_some_and(|v| v.to_lowercase().contains(needle))
}

/// The six PressPods tools.
pub fn tools(service: &PressPods) -> Result<Vec<McpTool>, ToolMetaError> {
    let list = {
        let service = service.clone();
        typed_tool(&defs::PRESSPODS_LIST, move |input: ListInput, _cx| {
            let service = service.clone();
            async move {
                let needle = input
                    .query
                    .as_deref()
                    .map(str::trim)
                    .filter(|q| !q.is_empty())
                    .map(str::to_lowercase);
                let persistence = service.persistence();
                let page = match input.resource {
                    ListResource::Episodes => {
                        let mut values = persistence
                            .get_all_episodes()
                            .await
                            .map_err(|e| execute(&e))?;
                        if let Some(needle) = &needle {
                            values.retain(|e| {
                                contains_ci(Some(&e.title), needle)
                                    || contains_ci(e.author.as_deref(), needle)
                                    || contains_ci(e.publication.as_deref(), needle)
                                    || contains_ci(e.domain.as_deref(), needle)
                            });
                        }
                        let items: Vec<McpEpisode> = values.iter().map(mcp_episode).collect();
                        let page = paginate(items, input.cursor, input.limit);
                        (
                            serde_json::to_value(page.items),
                            page.next_cursor,
                            page.total,
                        )
                    }
                    ListResource::Jobs => {
                        let mut values =
                            persistence.get_all_jobs().await.map_err(|e| execute(&e))?;
                        if let Some(status) = input.status {
                            values.retain(|j| j.status == status);
                        }
                        if let Some(needle) = &needle {
                            values.retain(|j| contains_ci(Some(&j.url), needle));
                        }
                        let items: Vec<omni_api::presspods::PressPodsJob> =
                            values.iter().map(job_dto).collect();
                        let page = paginate(items, input.cursor, input.limit);
                        (
                            serde_json::to_value(page.items),
                            page.next_cursor,
                            page.total,
                        )
                    }
                    ListResource::History => {
                        let tz = jiff::tz::TimeZone::get(&service.config().tz)
                            .unwrap_or(jiff::tz::TimeZone::UTC);
                        let since = input
                            .since
                            .as_deref()
                            .map(|date| day_start_ms(date, 0, &tz))
                            .transpose()?;
                        let until = input
                            .until
                            .as_deref()
                            .map(|date| day_start_ms(date, 1, &tz))
                            .transpose()?;
                        let domain = input
                            .domain
                            .as_deref()
                            .map(wanted_domain)
                            .filter(|d| !d.is_empty());
                        let mut values = persistence
                            .get_attempt_history()
                            .await
                            .map_err(|e| execute(&e))?;
                        values.retain(|entry| {
                            let a = &entry.attempt;
                            since.is_none_or(|since| a.at >= since)
                                && until.is_none_or(|until| a.at < until)
                                && domain
                                    .as_deref()
                                    .is_none_or(|d| domain_matches(a.domain.as_deref(), d))
                                && needle
                                    .as_deref()
                                    .is_none_or(|needle| contains_ci(Some(&a.url), needle))
                        });
                        let items: Vec<McpAttempt> = values.iter().map(mcp_attempt).collect();
                        let page = paginate(items, input.cursor, input.limit);
                        (
                            serde_json::to_value(page.items),
                            page.next_cursor,
                            page.total,
                        )
                    }
                };
                let (items, next_cursor, total) = page;
                Ok(ListOutput {
                    resource: input.resource,
                    items: js_numbers(items.map_err(|e| ToolError::output(e.to_string()))?),
                    next_cursor,
                    total,
                })
            }
        })?
    };

    let episode_get = {
        let service = service.clone();
        typed_tool(
            &defs::PRESSPODS_EPISODE_GET,
            move |input: EpisodeInput, _cx| {
                let service = service.clone();
                async move {
                    let episode = service
                        .persistence()
                        .get_episode(&input.episode_id)
                        .await
                        .map_err(|e| execute(&e))?
                        .ok_or_else(|| ToolError::execute("PressPods episode not found"))?;
                    Ok(EpisodeOutput {
                        episode: mcp_episode(&episode),
                    })
                }
            },
        )?
    };

    let transcript = {
        let service = service.clone();
        typed_tool(
            &defs::PRESSPODS_TRANSCRIPT_READ,
            move |input: TranscriptInput, _cx| {
                let service = service.clone();
                async move {
                    let episode = service
                        .persistence()
                        .get_episode(&input.episode_id)
                        .await
                        .map_err(|e| execute(&e))?
                        .ok_or_else(|| ToolError::execute("PressPods episode not found"))?;
                    let total = omni_core::js::utf16_len(&episode.content);
                    if input.offset > total {
                        return Err(ToolError::execute(
                            "Transcript offset is beyond the end of the episode",
                        ));
                    }
                    let rest = omni_core::js::utf16_slice(&episode.content, input.offset, total);
                    let (text, truncated) = truncate_utf16(&rest, input.max_chars);
                    let next_offset =
                        truncated.then(|| input.offset + omni_core::js::utf16_len(&text));
                    Ok(TranscriptOutput {
                        episode_id: input.episode_id,
                        title: episode.title,
                        offset: input.offset,
                        text,
                        next_offset,
                        total_chars: total,
                        truncated,
                    })
                }
            },
        )?
    };

    let submit = {
        let service = service.clone();
        typed_tool(&defs::PRESSPODS_SUBMIT, move |input: SubmitInput, _cx| {
            let service = service.clone();
            async move {
                let url = first_line_public_url(&input.url).map_err(ToolError::input)?;
                let job = service
                    .submit_episode_url(&url)
                    .await
                    .map_err(|e| execute(&e))?;
                Ok(JobOutput {
                    job: job_value(&job)?,
                })
            }
        })?
    };

    let retry = {
        let service = service.clone();
        typed_tool(&defs::PRESSPODS_RETRY, move |input: ResourceInput, _cx| {
            let service = service.clone();
            async move {
                let persistence = service.persistence();
                let job = match input {
                    ResourceInput::Episode { episode_id } => {
                        let episode = persistence
                            .get_episode(&episode_id)
                            .await
                            .map_err(|e| execute(&e))?
                            .ok_or_else(|| ToolError::execute("PressPods episode not found"))?;
                        service
                            .submit_episode_url(&episode.article_url)
                            .await
                            .map_err(|e| execute(&e))?
                    }
                    ResourceInput::Job { job_id } => {
                        let existing = persistence
                            .get_job(&job_id)
                            .await
                            .map_err(|e| execute(&e))?
                            .ok_or_else(|| ToolError::execute("PressPods job not found"))?;
                        if existing.status != JobStatus::Failed {
                            return Err(ToolError::execute(
                                "Only failed PressPods jobs can be retried",
                            ));
                        }
                        let requeued = persistence
                            .requeue_job_now(&job_id)
                            .await
                            .map_err(|e| execute(&e))?
                            .ok_or_else(|| {
                                ToolError::execute("PressPods job could not be retried")
                            })?;
                        service.kick_worker().map_err(|e| execute(&e))?;
                        requeued
                    }
                };
                Ok(JobOutput {
                    job: job_value(&job)?,
                })
            }
        })?
    };

    let delete = {
        let service = service.clone();
        typed_tool(&defs::PRESSPODS_DELETE, move |input: ResourceInput, _cx| {
            let service = service.clone();
            async move {
                let persistence = service.persistence();
                let resource = match input {
                    ResourceInput::Episode { episode_id } => {
                        let episode = persistence
                            .delete_episode(&episode_id)
                            .await
                            .map_err(|e| execute(&e))?
                            .ok_or_else(|| ToolError::execute("PressPods episode not found"))?;
                        service
                            .audio()
                            .delete_episode_audio(&episode.audio_file)
                            .await;
                        "episode"
                    }
                    ResourceInput::Job { job_id } => {
                        let job = persistence
                            .get_job(&job_id)
                            .await
                            .map_err(|e| execute(&e))?
                            .ok_or_else(|| ToolError::execute("PressPods job not found"))?;
                        if job.status == JobStatus::Processing {
                            return Err(ToolError::execute(
                                "A processing PressPods job cannot be dismissed",
                            ));
                        }
                        persistence
                            .delete_job(&job.job_id)
                            .await
                            .map_err(|e| execute(&e))?;
                        service
                            .audio()
                            .clear_chunk_checkpoints(&checkpoint_work_id(&job_normalized_url(&job)))
                            .await;
                        "job"
                    }
                };
                Ok(DeleteOutput {
                    resource,
                    deleted: true,
                })
            }
        })?
    };

    Ok(vec![list, episode_get, transcript, submit, retry, delete])
}
