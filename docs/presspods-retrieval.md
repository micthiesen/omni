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

## Known limits

- DataDome sites (wsj.com, reuters.com) answer every retriever, the browser
  included, with a bot challenge.
- nytimes.com serves a preview to a plain fetch and replaces it with a
  subscription wall in a fresh browser session, gift links included.
- The removepaywall.com retriever was removed on 2026-11-16: it returned its
  own search page for every one of 41 stored attempts.
