# Exa podcast transcripts

Two read-only MCP tools let an assistant (Moss, for podcast recommendations)
search what was said on podcasts and read full episode transcripts. They use
Exa's podcast index: about 3.2 million episodes from 119,000 shows, with full
transcripts labeled by speaker, updated as episodes air
([announcement](https://exa.ai/blog/podcast-search),
[library](https://exa.ai/library/podcast)).

Code: `crates/omni-podcasts/src/exa.rs` (client and pure helpers) and the
handlers in `crates/omni-podcasts/src/mcp.rs`. Tests:
`crates/omni-podcasts/tests/exa_transcripts.rs` (wiremock; no network).

## Setup

Set `EXA_API_KEY` in the service environment. Without it both tools fail with
`Exa podcast transcripts are not configured: EXA_API_KEY is unset`; nothing
else in Omni changes. The podcast index has no separate key, endpoint or
surcharge.

## Exa requests

Both calls go through the SSRF-guarded public client to `https://api.exa.ai`
with the `x-api-key` header, a 30 s timeout, a bounded body (2 MiB for search,
12 MiB for contents), no redirects and at most three concurrent requests.
Nothing is retried automatically. Each successful request's
`costDollars.total` is recorded as an `exa` cost event (feature
`podcast-transcripts`); a failed request records none. Exa's request ids,
error tags and content source reach the caller only when they are short plain
tokens.

- **Search** is `POST /search` with `type: "auto"`, `numResults` 1 to 10,
  `includeDomains: ["exa.ai/library/podcast"]`, optional
  `startPublishedDate`/`endPublishedDate`, and
  `contents.highlights.maxCharacters`. Results whose URL is not under
  `https://exa.ai/library/podcast/` are dropped and counted in
  `droppedResults`.
- **Get** is `POST /contents` for one canonical page URL with
  `text.maxCharacters: 1000001` and `extras.links: 51`. The tool accepts only
  `https://exa.ai/library/podcast/<showId>` and
  `https://exa.ai/library/podcast/<showId>/episode/<episodeId>` (ids are
  1 to 64 of `[A-Za-z0-9_-]`), so it cannot be used as a general web fetcher.

## What the tools return

- **Identity.** `kind` (`episode`, `show`, or `other` for search hits that are
  neither), `showId` and `episodeId` come only from the page URL. Exa's own
  `id` is returned as `exaId`. Omni does not map Exa pages to RSS GUIDs; an
  episode recommendation still goes through `podcast_recommendation_record`,
  which verifies the episode in the show's own feed.
- **Dates.** `publishedDate` is Exa's publication date for the page, passed
  through unchanged (it may be null). Exa's API reports no crawl date
  (`startCrawlDate`/`endCrawlDate` are deprecated and ignored), so none is
  shown. `contentSource` on a get is Exa's `cached` or `crawled` for that read.
- **Text.** Search returns at most three Exa-selected highlights per result,
  clipped to `highlightChars` (100 to 2,000), with `highlightsTruncated`.
  Highlights are excerpts, not the transcript. Get returns the page text
  verbatim: episode pages carry Exa's transcript format (speaker names and
  timestamps as Exa writes them), show pages carry the description, hosts,
  publisher and Exa's list of up to 30 recent episodes. Omni adds and infers no
  speakers or timestamps.
- **Links.** `links` are up to 50 page links Exa extracted (a show page's RSS
  feed and platform links when present), unverified, with `linksTruncated`.
- **Paging.** Offsets count Unicode characters. Each get returns `maxChars`
  (1,000 to 40,000, default 20,000) from `offset`, with `totalChars` and
  `nextOffset` (null at the end). Omni retrieves at most 1,000,000 characters
  per page; `sourceTruncated` marks a page with more. A successful page is
  cached in memory for 15 minutes (32 MiB total), and concurrent reads of an
  uncached page share one request, so paging makes one Exa request.
- **Unavailable.** `status: "unavailable"` with `reason`
  `exa_status_error` (with Exa's `exaErrorTag` and `exaHttpStatus`),
  `not_returned` or `empty_text`. It means Exa returned no usable text, not
  that the episode has no transcript. Unavailable results are not cached.
  HTTP failures are tool errors naming only the status and Exa's error tag.

## Boundaries

The tools write nothing but cost events. They are not used by the
`PodcastRecs` pipeline, schedules, recommendation history, the podcast queue
or any acquisition path, and they do not change the legacy recommendation
publishers or the Moss shadow pilot.
