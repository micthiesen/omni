# PressPods article retrieval

How a submitted URL becomes the article text that is narrated. The code is
`crates/omni-presspods/src/retrievers/` and `PressPods::article_from_url`
(`pipeline.rs`). Audio is covered in `docs/presspods-audio.md`.

## The candidate set

Every retriever runs for every article, all at the same time, and each one is a
candidate. There is no fallback order.

| Name | What it does |
| --- | --- |
| `postlight` | Fetches the page, strips page chrome, runs the Readability scorer, takes Mercury-style metadata |
| `readability` | Fetches the page, plain Mozilla Readability |
| `extractus` | Fetches the page, JSON-LD `articleBody` first, else the densest content container |
| `wayback` | The newest Internet Archive snapshot, whole page cleaned |
| `fetch` | Fetches the page, whole page cleaned |
| `jina` | Jina Reader (a hosted headless browser); only with `JINA_API_KEY` |
| `obscura` | The page as Omni's own headless Obscura browser renders it; off with `PRESSPODS_OBSCURA_ENABLED=false` |

An X or Twitter status URL uses the `x` retriever alone.

## Scoring

1. Each retriever has 60 seconds (`RETRIEVER_TIMEOUT`). One that fails or
   times out is recorded as a failed candidate and the others carry on.
2. The metadata model (`PRESSPODS_METADATA_MODEL`) sees each distinct
   extraction once: title, author, domain, URL, date, lead image and the
   extracted text. It never sees which retriever produced it. Retrievers that
   produced identical text and metadata share one rating. Up to four ratings
   run at once, each with its own 60-second bound.
3. The model marks an extraction invalid (error page, login or paywall with no
   content, empty page) or rates the extraction quality from 0 to 10.
4. The highest rating wins. On a tie the earlier retriever in the table wins.

So the retrieval stage is bounded by 60 seconds of fetching plus the ratings
(60 seconds each, four at a time).

## The Obscura candidate

Obscura is the same headless browser the Amazon tools use
(`omni_core::obscura`, `obscura mcp --stealth`, a pinned binary in the image).
It exists for pages whose article only appears after their scripts run, where
a plain fetch gets a preview or an empty shell.

- It is last in the set, so it is recorded as the winner only when its
  extraction rated strictly higher than every cheaper retriever's. When it
  renders the same article the fetch already had, the two share one rating and
  the fetch-based retriever wins the tie.
- Each article gets a fresh browser process, which is ended afterwards. No
  cookies or page state carry over.
- The browser navigates to the submitted URL and reads the DOM. It never
  clicks, types, submits a form, logs in or downloads a file.
- The submitted URL passes the same public-URL check as every other
  retriever, Obscura's own block on loopback, private and link-local addresses
  stays on for the page and everything it loads, and a page that ends up off
  the public web after redirects is discarded.
- Bounds: 30 seconds to navigate, then up to 20 seconds for the page to
  settle (4 seconds without its text changing), 55 seconds overall, and a
  5,000,000-character cap on the rendered document.
- The rendered HTML goes through the `postlight` parser, then the
  `readability` parser if that finds nothing.
- A page that is still a Cloudflare or DataDome bot challenge after 8 seconds
  is a failed candidate (`the site answered the browser with a bot
  challenge`). Obscura did not pass either kind in testing on 2026-11-16.

## What is recorded

An episode stores `retrieverName` (the winner), `retrieverSeconds` and
`retrieverAttempts`: one entry per candidate with either its `contentRating`
and `textChars` or its error. The episode page and `GET
/api/press-pods/episodes` show them. To see whether a retriever earns its
place, count how often it is `retrieverName` and how its ratings compare.

A job whose every candidate failed stores each retriever's reason in
`lastError` (retrievers that failed the same way share an entry), for
example `All article retrievers failed (postlight, readability: HTTP 401;
wayback: No archived snapshot available for this URL; obscura: the site
answered the browser with a bot challenge)`. It is not retried, with one
exception: when an extraction existed but the rating model was temporarily
unavailable (429, 5xx, network), the job is requeued with the usual backoff.

## Attempt history

A job row is deleted when its episode lands, when it is dismissed, and its
error is cleared when it is retried or resubmitted. The evidence of what went
wrong is therefore kept separately, in `press-pods-job-attempt` rows
(`PressPodsJobAttempt` in `model.rs`):

- One row is appended for every failed attempt, in the same transaction as
  the job change: `outcome: "retrying"` when the job was requeued with
  backoff, `"failed"` when it failed for good. A crash or restart mid-run
  counts as an attempt and gets a row too.
- When a job produces an episode and the article has an unresolved failure
  in the history (or the job itself failed earlier), a `"succeeded"` row with
  the `episodeId` is appended. An article that works first time adds nothing.
- A row holds `jobId`, `url`, `normalizedUrl`, `domain` (lowercase host
  without `www.`), `attempt` (the number within the job's retry budget; a
  manual retry or resubmit starts again at 1), `at`, the stored error (at
  most 500 characters), `operation` (the pipeline step that failed), `runId`
  and `retrievers`: per retriever its `name`, `ok`, whether it was
  `selected`, an `errorClass` (`http_status`, `timeout`, `network`,
  `invalid_article`, `rating_failed`, `other`), `httpStatus`, a short `error`,
  `textChars` and `contentRating`. `retrievers` is absent when the attempt
  ended before retrieval finished. No article text, HTML or audio is stored,
  and a retriever's `error` never holds a response body. The attempt's own
  `error` is the job's `lastError`, which for a TTS, STT or model status
  error includes up to 200 characters of that service's error response.
  `url` keeps the submitted query string, as the job row does.
- Rows are never updated or deleted by PressPods, and nothing ties them to
  the job row: deleting, dismissing, retrying or resubmitting a job leaves
  them in place. They are not in the data manager.
- A failure is resolved by the earliest `succeeded` row for the same
  `normalizedUrl` at or after it. This is computed when the history is read
  (`resolvedAt`, `resolvedByEpisodeId`), so rows stay append-only.
- Each row expires 180 days after it was written (`ATTEMPT_HISTORY_TTL_MS`,
  the docstore's row expiry); the store maintenance sweep removes it.

Read it with `presspods_list` and `resource: "history"`, newest first, with
optional `query` (URL text), `domain` (subdomains match), and `since` and
`until` days in the server's time zone. There is no HTTP route or page for it.

## Known limits

- DataDome sites (wsj.com, reuters.com) answer every retriever, the browser
  included, with a bot challenge.
- nytimes.com serves a preview to a plain fetch and replaces it with a
  subscription wall in a fresh browser session, gift links included.
- The removepaywall.com retriever was removed on 2026-11-16: it returned its
  own search page for every one of 41 stored attempts.
