//! URL to finished episode: retrieval, rating,
//! narration cleaning, TTS, audio finalization, the episode row, then a
//! best-effort notification. Errors propagate to the task, which classifies
//! them and requeues or fails the job.

use std::sync::Arc;

use omni_alerts::{PushoverChannel, PushoverMessage};

use crate::audio::{audio_duration_seconds, tag_episode_audio};
use crate::costs::CostCounter;
use crate::error::PressPodsError;
use crate::formatting::{FinalTextInput, build_final_text};
use crate::model::{PressPodsEpisode, RetrieverOutcome};
use crate::persistence::secure_id;
use crate::retrievers::{
    BestArticle, RETRIEVER_TIMEOUT, rate_retrieved_articles, run_article_retrievers, select_best,
};
use crate::service::PressPods;
use crate::speech::synthesize::Synthesizer;
use crate::storage::checkpoint_work_id;
use crate::types::{Article, RetrieverResult, retriever_outcomes, summarize_retriever_attempts};
use crate::url::normalize_url;
pub use omni_core::js::to_fixed as js_to_fixed;

const LOG: &str = "PressPods";

/// A fallback title from a URL: `"Host - Path / Segments"`.
pub fn title_from_url(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return "Untitled".to_owned();
    };
    let host = parsed.host_str().unwrap_or("");
    let host = host.strip_prefix("www.").unwrap_or(host);
    let mut chars = host.chars();
    let mut title = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
        None => String::new(),
    };
    let segments: Vec<String> = parsed
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|segment| {
            let spaced = segment.replace('-', " ");
            let mut out = String::with_capacity(spaced.len());
            let mut prev_word = false;
            for c in spaced.chars() {
                let word = c.is_ascii_alphanumeric() || c == '_';
                if word && !prev_word {
                    out.extend(c.to_uppercase());
                } else {
                    out.push(c);
                }
                prev_word = word;
            }
            out
        })
        .collect();
    if !segments.is_empty() {
        title.push_str(" - ");
        title.push_str(&segments.join(" / "));
    }
    title
}

/// `m:ss`, `"0:00"` when unknown or zero.
pub fn format_duration(seconds: Option<f64>) -> String {
    match seconds.filter(|s| *s != 0.0 && !s.is_nan()) {
        None => "0:00".to_owned(),
        Some(s) => format!("{}:{:02}", (s / 60.0).floor(), (s % 60.0).floor()),
    }
}

/// The Pushover body for a finished episode.
pub fn availability_message(episode: &PressPodsEpisode) -> String {
    let total_cents = episode
        .costs
        .as_ref()
        .map(|c| c.llm_cents + c.tts_cents)
        .unwrap_or(0.0);
    format!(
        "'{}' from '{}' is now available.\n{} · {} · US${:.2}",
        episode.title,
        episode.domain.as_deref().unwrap_or("unknown"),
        format_duration(episode.duration_seconds),
        episode.voice_name.as_deref().unwrap_or("undefined"),
        js_to_fixed(total_cents / 100.0, 2)
    )
}

impl PressPods {
    /// Runs every retriever, rates the results and picks the best article.
    pub async fn article_from_url(
        &self,
        url: &str,
        costs: &CostCounter,
    ) -> Result<BestArticle, PressPodsError> {
        select_best(self.rated_results(url, costs).await?)
    }

    /// Runs every retriever and rates the results.
    async fn rated_results(
        &self,
        url: &str,
        costs: &CostCounter,
    ) -> Result<Vec<RetrieverResult>, PressPodsError> {
        self.deps.url_guard.check(url).await?;
        let retrievers = self.deps.retrievers.for_url(url);
        let retrieved = run_article_retrievers(url, &retrievers, RETRIEVER_TIMEOUT).await;
        let agents = self.deps.agents.clone();
        let costs = costs.clone();
        Ok(rate_retrieved_articles(retrieved, move |article: Article| {
            let agents = agents.clone();
            let costs = costs.clone();
            async move {
                match tokio::time::timeout(
                    RETRIEVER_TIMEOUT,
                    agents.article_metadata(&article, &costs),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(PressPodsError::timeout(crate::retrievers::RATING_OPERATION)),
                }
            }
        })
        .await)
    }

    /// The whole pipeline for one submitted URL.
    pub async fn create_episode_from_url(
        &self,
        url: &str,
        run_id: Option<String>,
    ) -> Result<PressPodsEpisode, PressPodsError> {
        self.create_episode_traced(url, run_id).await.0
    }

    /// [`Self::create_episode_from_url`] plus every retriever's outcome for
    /// the attempt history (empty when the attempt ended before retrieval
    /// finished), whether or not an episode came out.
    pub async fn create_episode_traced(
        &self,
        url: &str,
        run_id: Option<String>,
    ) -> (
        Result<PressPodsEpisode, PressPodsError>,
        Vec<RetrieverOutcome>,
    ) {
        let mut outcomes = Vec::new();
        let result = self.create_episode(url, run_id, &mut outcomes).await;
        (result, outcomes)
    }

    async fn create_episode(
        &self,
        url: &str,
        run_id: Option<String>,
        outcomes: &mut Vec<RetrieverOutcome>,
    ) -> Result<PressPodsEpisode, PressPodsError> {
        let deps = &self.deps;
        let start = deps.clock.now_ms();
        let costs = CostCounter::new();
        let normalized_url = normalize_url(url);
        let work_id = checkpoint_work_id(&normalized_url);

        let results = self.rated_results(url, &costs).await?;
        *outcomes = retriever_outcomes(&results, None);
        let best = select_best(results)?;
        *outcomes = retriever_outcomes(&best.all_results, Some(&best.retriever_name));
        let retrieved = &best.article;
        let metadata = &best.metadata;
        tracing::info!(
            target: LOG,
            title = retrieved.title.as_deref().unwrap_or(""),
            chars = omni_core::js::utf16_len(&retrieved.text),
            retriever = best.retriever_name.as_str(),
            "Article retrieved"
        );

        let title = metadata
            .title
            .clone()
            .or_else(|| retrieved.title.clone())
            .unwrap_or_else(|| title_from_url(url));
        let author = metadata
            .author
            .clone()
            .or_else(|| retrieved.author.clone())
            .filter(|a| !a.to_lowercase().contains("unknown"));
        let published_at = metadata.published_at.or(retrieved.published_at);
        let lead_image_url = metadata
            .lead_image_url
            .clone()
            .or_else(|| retrieved.lead_image_url.clone());
        let tz = jiff::tz::TimeZone::get(&deps.config.tz).unwrap_or(jiff::tz::TimeZone::UTC);
        let text = build_final_text(&FinalTextInput {
            title: Some(&title),
            domain: metadata
                .publication
                .as_deref()
                .or(retrieved.domain.as_deref()),
            author: author.as_deref().unwrap_or("Anonymous"),
            coauthors: metadata.coauthors.as_deref().unwrap_or(&[]),
            date_published_ms: published_at,
            text: &retrieved.text,
            tz: &tz,
        });
        let article = Article {
            title: Some(title.clone()),
            text,
            author: author.clone(),
            domain: retrieved.domain.clone(),
            url: url.to_owned(),
            published_at,
            lead_image_url: lead_image_url.clone(),
        };

        let content = deps.agents.cleaned_article(&article, &costs).await?;
        tracing::info!(target: LOG, content_length = omni_core::js::utf16_len(&content), "Narration text ready");
        #[allow(clippy::cast_precision_loss)]
        let retriever_seconds = (deps.clock.now_ms() - start) as f64 / 1000.0;

        let provider = deps.tts.create(metadata.author_gender)?;
        let stt = if provider.verify_chunk_content() {
            deps.stt.clone()
        } else {
            None
        };
        let synthesizer = Synthesizer {
            chain: deps.chain.clone(),
            storage: deps.audio.clone(),
            costs: costs.clone(),
            recorder: deps.costs.clone(),
            clock: deps.clock.clone(),
            intro_path: deps.intro_path.clone(),
        };
        let synthesis = synthesizer
            .synthesize_speech(Arc::clone(&provider), stt, &content, Some(&work_id))
            .await?;

        // Measure before tagging so chapter end times are right (tags do not
        // change the duration).
        let duration_seconds = audio_duration_seconds(&synthesis.audio);
        let audio = tag_episode_audio(
            &deps.public_http,
            synthesis.audio,
            lead_image_url.as_deref(),
            &synthesis.chapters,
            duration_seconds,
        )
        .await;

        let episode_id = secure_id();
        let episode = PressPodsEpisode {
            audio_file: format!("{episode_id}.mp3"),
            episode_id,
            title,
            author,
            author_gender: metadata.author_gender,
            publication: metadata.publication.clone(),
            domain: article.domain.clone(),
            article_url: url.to_owned(),
            normalized_url: Some(normalized_url.clone()),
            lead_image_url,
            excerpt: metadata.short_summary.clone(),
            content,
            voice_name: Some(synthesis.voice_name),
            voice_provider: Some(synthesis.voice_provider),
            synthesized_seconds: Some(synthesis.synthesized_seconds),
            chapters: Some(synthesis.chapters),
            chunks: Some(synthesis.chunks),
            duration_seconds,
            file_bytes: i64::try_from(audio.len()).unwrap_or(i64::MAX),
            retriever_name: Some(best.retriever_name.clone()),
            retriever_seconds: Some(retriever_seconds),
            retriever_attempts: Some(summarize_retriever_attempts(&best.all_results)),
            costs: Some(costs.costs()),
            created_at: deps.clock.now_ms(),
            published_at,
            run_id,
            extra: Default::default(),
        };
        self.persist_episode_with_audio(&episode, &audio).await?;

        // Resubmit-as-retry: the newest take replaces older episodes for the
        // same canonical URL, right after the new row lands.
        self.replace_older_episodes(&normalized_url, &episode.episode_id)
            .await?;
        deps.audio.clear_chunk_checkpoints(&work_id).await;

        tracing::info!(target: LOG, costs = ?episode.costs, "Episode created for \"{}\"", episode.title);
        self.notify_episode_available(&episode).await;
        Ok(episode)
    }

    /// Writes the file, then the row that references it. When the row cannot
    /// land the file is removed again, so a persistence outage cannot
    /// accumulate invisible orphan audio.
    pub async fn persist_episode_with_audio(
        &self,
        episode: &PressPodsEpisode,
        audio: &[u8],
    ) -> Result<(), PressPodsError> {
        let persistence = self.deps.persistence.clone();
        let row = episode.clone();
        self.persist_episode_with(episode, audio, async move {
            persistence.upsert_episode(row).await
        })
        .await
    }

    /// [`Self::persist_episode_with_audio`] with an explicit row write (tests
    /// inject a failing one).
    pub async fn persist_episode_with<F>(
        &self,
        episode: &PressPodsEpisode,
        audio: &[u8],
        persist: F,
    ) -> Result<(), PressPodsError>
    where
        F: std::future::Future<Output = Result<(), PressPodsError>>,
    {
        self.deps
            .audio
            .save_episode_audio(&episode.audio_file, audio)
            .await?;
        if let Err(error) = persist.await {
            self.deps
                .audio
                .delete_episode_audio(&episode.audio_file)
                .await;
            return Err(error);
        }
        Ok(())
    }

    /// Drops episodes sharing the canonical URL other than `keep_episode_id`,
    /// with their audio. Runs on the happy path and on crash recovery.
    pub async fn replace_older_episodes(
        &self,
        normalized_url: &str,
        keep_episode_id: &str,
    ) -> Result<(), PressPodsError> {
        let replaced = self
            .deps
            .persistence
            .delete_episodes_by_normalized_url_except(normalized_url, keep_episode_id)
            .await?;
        for old in replaced {
            self.deps.audio.delete_episode_audio(&old.audio_file).await;
            tracing::info!(target: LOG, "Replaced older episode {} for the same article", old.episode_id);
        }
        Ok(())
    }

    /// Best-effort: the episode exists and the feed picks it up regardless.
    async fn notify_episode_available(&self, episode: &PressPodsEpisode) {
        let message = PushoverMessage {
            title: Some("Episode Now Available".to_owned()),
            message: availability_message(episode),
            url: Some(format!("{}/pods", self.deps.config.recs_public_url)),
            url_title: Some("Open PressPods".to_owned()),
            ..PushoverMessage::default()
        };
        if let Err(error) = self
            .deps
            .pushover
            .send(PushoverChannel::PressPods, message)
            .await
        {
            tracing::warn!(target: LOG, %error, "Failed to send episode notification");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_from_urls() {
        assert_eq!(
            title_from_url("https://www.example.com/some-long-story/part-2"),
            "Example.com - Some Long Story / Part 2"
        );
        assert_eq!(title_from_url("https://example.com/"), "Example.com");
        assert_eq!(title_from_url("not a url"), "Untitled");
    }

    #[test]
    fn to_fixed_rounds_exact_ties_up_like_js() {
        assert_eq!(js_to_fixed(0.125, 2), "0.13");
        assert_eq!(js_to_fixed(0.0, 2), "0.00");
        assert_eq!(js_to_fixed(1.005, 2), "1.00");
        assert_eq!(js_to_fixed(9.995, 2), "9.99");
        assert_eq!(js_to_fixed(9.999, 2), "10.00");
        assert_eq!(js_to_fixed(0.1234, 2), "0.12");
        assert_eq!(js_to_fixed(2.5, 0), "3");
        assert_eq!(js_to_fixed(-0.001, 2), "-0.00");
        assert_eq!(js_to_fixed(-0.0, 2), "0.00");
    }

    #[test]
    fn durations_format() {
        assert_eq!(format_duration(None), "0:00");
        assert_eq!(format_duration(Some(0.0)), "0:00");
        assert_eq!(format_duration(Some(523.9)), "8:43");
    }
}
