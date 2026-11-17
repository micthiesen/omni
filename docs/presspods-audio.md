# PressPods TTS audio: models and post-processing

The post-processing chain (`crates/omni-presspods/src/speech/audio_chain.rs`) is where most
of the perceived quality of an episode is won or lost, and it's an ongoing
refinement target. This doc records what the chain does, why each stage exists,
the artifacts we've found and fixed (with the measurement method, so future
issues can be diagnosed the same way), and the known next steps.

## Speech server

Higgs TTS and the STT verifier run on one mlx-audio server: Michael's MacBook
(LaunchAgent `com.mlxaudio.server`, port 8000), reached at `PRESSPODS_TTS_URL`.
Production sets that variable in `~/compose/docker-compose.yml` on Boris. The
Mac gets its address from DHCP, so the address can change (it moved from
`10.10.1.90` to `10.10.1.113` on 2026-11-16), and the Mac can be asleep or off
the network. Both show up the same way, as a connection that cannot be made.

The queue worker therefore checks the server before a job starts
(`TtsFactory::preflight`, `GET /v1/models` with a 5-second timeout, Higgs
only). Any HTTP answer passes. A connection failure or timeout defers the job:

- Nothing is retrieved, no model is called, and the job's attempt count does
  not change. The job stays `queued` with `lastError` set to
  `speech server unreachable at <origin>: <reason>`.
- The next check is 2 minutes later, doubling to a cap of 30 minutes (2, 4, 8,
  16, 30, 30, ...). The worker sweeps every 5 minutes, so a short wait ends at
  the next sweep. One failed check defers every job due in that pass.
- The attempt history gets one `retrying` row with `operation: "TTS
  preflight"` when the outage starts for a job, not one per check.
- When the server has been unreachable for
  `PRESSPODS_TTS_UNREACHABLE_DEADLINE_HOURS` (default 24) since the job's first
  failed check, the job fails with the same message plus `(gave up after N h
  unreachable)`, a `failed` history row and the usual failure alert. A manual
  retry starts a new outage clock.
- A check that passes ends the outage for that job; a later one starts again
  from 2 minutes and a new deadline.

A connection failure during synthesis (no route, refused, no response within
the 4-minute chunk timeout) is reported with the same message and the
operation `synthesize Higgs chunk`. Work was already paid for, so it uses an
attempt and takes the ordinary retry backoff; the next attempt then meets the
preflight. After an outright connection failure the remaining takes of that
chunk are skipped; a timeout keeps them, since it may be one slow take. An
error the server itself answers with (a 400, a 5xx) keeps its existing
classification.

If jobs sit deferred, check that the Mac is awake and that
`PRESSPODS_TTS_URL` still names its address
(`curl -m 5 http://<mlx-host>:8000/v1/models`).

## TTS providers and what their raw audio looks like

| Provider | Native output | Bandwidth | Noise floor | Quirks |
|---|---|---|---|---|
| Higgs v3 (self-hosted mlx-audio, default) | 24 kHz MP3 ~128k mono | content shelf ~11 kHz | audible hiss (needs denoise) | random voice per request unless ref-cloned; unreliable length (truncation/runaway); comb-structured sibilance (see below) |
| ElevenLabs v3 | 44.1 kHz MP3 | full | clean | none of the above; costs $0.10/1k chars |

Two provider flags drive the chain: `needs_denoise` (Higgs) and
`verify_chunk_length` (Higgs; length-retry lives in `synthesize.rs`, not the
audio chain).

The key fact about Higgs: **everything above ~11 kHz in a finished episode is
by definition an artifact**, because the model itself produces nothing up
there. That makes the high band a clean diagnostic window: any energy that
shows up above the shelf was manufactured by our own processing.

## The chain

Per chunk (`AudioChain::prepare_chunk`):

1. **Denoise + band-edge shelf** (Higgs only): `highpass=f=80` rumble cut,
   then RNNoise (`arnndn` with `assets/press-pods/denoise.rnnn`), then the
   `FIZZ_SHELF` FIR (see the comb-sibilance section below). RNNoise over
   `afftdn` because spectral subtraction leaves a metallic "musical noise"
   tang on voice. RNNoise only runs at 48 kHz, so the filter chain upsamples
   to 48k first (see the resampling section below).
2. **Edge trim + fades**: `silenceremove` at each end (via an `areverse`
   sandwich), 12 ms fades so butt-joins never click.
3. **Per-chunk leveling** to −19 LUFS (two-pass *linear* loudnorm) so no chunk
   sits quieter than its neighbors.

Per episode (`AudioChain::assemble_episode`):

4. **Concat** chunk/gap WAVs (stream copy, no re-encode).
5. **Master** to −16 LUFS / −1.5 dBTP (two-pass linear loudnorm, the podcast
   delivery convention).
6. **Intro join + single encode**: intro jingle loudness-matched and
   concatenated in one filtergraph, one 96k mono MP3 encode. A single final
   encode replaces the old three-generation MP3 chain.

Two-pass *linear* loudnorm everywhere: pass 1 measures, pass 2 applies one
static gain. One-pass loudnorm runs a dynamic AGC that pumps and lifts quiet
passages. Note that when the linear gain would violate TP=−1.5, loudnorm
scales back / falls back internally, so episodes land around −17 LUFS on peaky
speech instead of clipping. Don't "fix" that by replacing the apply-pass with
a plain `volume` gain: we tested it, and the TP clamp makes it undershoot
loudness by ~3 dB with no audible benefit.

## Fixed: the ~13 kHz sibilance "ring" (resampler imaging, 2026-07)

**Symptom**: a faint, high-pitched metallic ring shadowing every sibilant:
"a spoon rattling very quietly in a ceramic cup." Peaked at −37 dBFS in a
−16 LUFS episode; clearly audible on headphones.

**Diagnosis** (the method matters more than the instance):

- Spectrogram of the episode showed speech content ending hard at ~11 kHz
  (Higgs's shelf) with a persistent speckled band at **~13.26 kHz** riding
  above it, 20 dB of spectral prominence.
- STFT band-energy correlation: the 13.26 kHz band tracked the sibilance band
  (8.3–9.5 kHz) at r=0.70, but ~0 against overall speech energy or a control
  band. So: a sibilance-shadow, not noise.
- A raw chunk fetched straight from the mlx server was **clean** above
  11.5 kHz (−140 dB). The artifact was created by us.
- Stage-by-stage replay found it in the very first step: a bare
  `ffmpeg -i raw.mp3 -ar 44100` already produced the bump. **swresample's
  default anti-imaging filter is weak**; upsampling 24 kHz output mirrors the
  vocoder's 10.7 kHz band-edge energy around 12 kHz → ~13.3 kHz images at
  only ~35 dB below source. The chain then compounded this across its many
  conversions: 24k→48k (auto-inserted for `arnndn`), 48k→44.1k out, and a
  192 kHz round-trip *inside every loudnorm pass* (loudnorm always resamples
  internally, even in linear mode).

**Fix**: every `aresample` in `audio_chain.rs` carries
`filter_size=256:cutoff=0.95` (`RESAMPLE_HQ`), including explicit inserts
that pre-empt ffmpeg's auto-inserted default-quality conversions (before
`arnndn`, after each loudnorm, both branches of the intro filtergraph).
Stock swr options only; the Docker image's Debian ffmpeg doesn't ship soxr.

**Measured result** (same raw chunk through the full chain incl. final MP3
encode): artifact band in sibilant frames −95 dB → **−136 dB** (below the
noise floor, zero spectral prominence); loudness/TP/LRA byte-identical to the
old chain. CPU cost negligible.

**Rule going forward**: any new `aresample`, `-ar`, or filter that forces a
sample rate (check the filter's supported rates: `arnndn`=48k,
`loudnorm`=192k internally) must use or be followed by `RESAMPLE_HQ`
parameters, or it will re-introduce imaging.

## Diagnostic toolkit

Throwaway scripts (a scratch Rust example or `#[ignore]` test run with
`dotenvx run -- cargo …` for anything needing `.env`;
plain `uv run --with numpy --with scipy python …` for analysis). Patterns that
paid off:

Spectrogram overview (artifacts above the ~11 kHz Higgs shelf jump out):

```bash
ffmpeg -y -i episode.mp3 -lavfi \
  "showspectrumpic=s=3600x1024:legend=1:scale=log:fscale=lin:stop=20000" spec.png
```

Isolate and amplify the high band to make a suspected artifact listenable
(ground truth for "is this what I'm hearing?"):

```bash
ffmpeg -y -ss 12 -i episode.mp3 -af \
  "highpass=f=12000,highpass=f=12000,highpass=f=12000,highpass=f=12000,volume=30dB" \
  artifact-isolated.wav
```

Measure how loud the artifact actually is (skip the full-band intro jingle):

```bash
ffmpeg -y -ss 12 -i episode.mp3 -af \
  "highpass=f=12000,highpass=f=12000,highpass=f=12000,highpass=f=12000" -f null - \
  # then astats: Peak level ≳ -45 dBFS = audible on headphones
```

Fetch a raw chunk from the Higgs server to separate model artifacts from
chain artifacts (sibilance-heavy text maximizes the high band):

```bash
curl -s -m 240 -X POST http://<mlx-host>:8000/v1/audio/speech \
  -H 'Content-Type: application/json' -d '{
    "model": "bosonai/higgs-audio-v3-tts-4b",
    "input": "The scientists insisted that the specifics stay strictly secret across successive sessions.",
    "gender": "male", "speed": 0.9, "max_tokens": 3000, "response_format": "mp3"
  }' -o raw_chunk.mp3
```

For narrowband suspects, the decisive test is **band-energy correlation over
time** (STFT, compare the suspect band's envelope against sibilance / speech /
control bands) plus **mirror arithmetic**: an image of source content at `f`
created when upsampling from rate `r` lands at `r − f`. If the suspect
frequency's mirror lands in a band that correlates with it, it's resampler
imaging; if it correlates with nothing, look at noise-shaping/codec artifacts
instead.

## Fixed: exposed band-edge fizz, the second "ring" (FIZZ_SHELF, 2026-07)

After the imaging fix, a fainter version of the same metallic-rattle
perception persisted. Analysis of three post-fix episodes confirmed the
resampler fix was live (13 kHz band flat, zero sibilance correlation) and
found the actual source: **Higgs's vocoder synthesizes sibilance as a dense
comb of narrowband peaks** (~20–100 Hz spacing) instead of smooth noise,
running from ~7 kHz right up to its 11 kHz band edge. Below ~9.5 kHz the comb
is masked by the sibilance it rides on. Above it, the sibilance rolloff drops
faster than the comb does, so the 9.8–11 kHz remainder pokes within 6–8 dB of
the speech mids at the worst moments (r=0.90 with sibilance), audible as the
rattle. The original pre-fix complaint was almost certainly both layers
stacked (comb + its 13 kHz mirror image); the imaging fix removed one layer.

**Fix**: `FIZZ_SHELF` in `audio_chain.rs`: a steep linear-phase FIR shelf
(`firequalizer`: 0 dB below 9.6 kHz, ramping to −30 dB by 10.3 kHz) appended
to the Higgs denoise chain. Measured on a full episode: fizz band in sibilant
frames −62 dB → −88 dB, with the 9.0–9.5 kHz band bit-identical. A more
aggressive corner (9.2 kHz) bought 4 dB more reduction but cost 3.8 dB of
real sibilance, so it was rejected.

Note per-generation variance is real but small: episodes that "sound clean"
and episodes that "ring" measured statistically identical here; perception
depends on content (sibilance density) and listening conditions.

## Fixed: Higgs truncation slipping past the duration check (STT verify, 2026-07)

Higgs (autoregressive) silently truncates: it emits a natural-sounding read of
only the first ~half of a chunk and stops. `synthesize.rs` had only a
duration-band check (seconds-of-audio per input char, `[0.03, 0.15]`) to catch
this, and it wasn't enough.

**Diagnosis.** Pulled the six post-Higgs episodes off `omni.boris` and looked at
the stored per-chunk stats, then STT-transcribed the suspicious chunks and
diffed word coverage against the input text. On a 5-chunk sample:

| chunk        | s/char | duration verdict     | STT coverage | word ratio |
| ------------ | ------ | -------------------- | ------------ | ---------- |
| complete ref | 0.065  | in-band              | 100%         | 1.00       |
| truncated    | 0.027  | out (shipped best)   | 48%          | 0.36       |
| truncated    | 0.030  | **in-band, passed** | 64%          | 0.45       |
| truncated    | 0.025  | out (shipped best)   | 58%          | 0.37       |
| truncated    | 0.035  | **in-band, passed** | 66%          | 0.47       |

Two chunks missing ~half their content passed the duration band: a truncated
read and a fast read overlap in seconds/char, so duration can't separate them.
Word coverage can: complete reads recover ~all input words (~1.0), truncated
ones a fraction (≤0.66), with a wide gap between the two populations.

**Fix.** An STT round-trip is now the primary chunk verifier (`coverage.rs` +
`stt.rs`, wired into `synthesize.rs`'s retry loop). Each take is transcribed and
scored on word `coverage` and `word_ratio` (transcript/input word count);
`is_content_complete` accepts coverage ≥ 0.75, or coverage ≥ 0.68 for short
(≤60-word) chunks when the word ratio is also ≥ 0.78. The secondary pass handles complete, number-heavy
reads where STT emits digits for spoken number words, while remaining above the
≤0.47 ratios measured for true truncations. A ratio ≤ 1.8 is always required
(the upper guard also catches runaway loops, which keep coverage high).
Below-bar takes are re-synthesized (up to 3); the best is kept and flagged in
the UI. The duration band is retained as the **fallback** verifier for when no
STT endpoint is configured.

STT runs on the same mlx-audio host as Higgs TTS (`PRESSPODS_STT_URL` defaults
to `PRESSPODS_TTS_URL`) via its OpenAI-compatible `/v1/audio/transcriptions`,
using `mlx-community/parakeet-tdt-0.6b-v3` (~0.3 s/chunk, $0). Note
`whisper-large-v3-turbo` on this mlx build 500s on that endpoint; parakeet
works and is faster. `coverage.rs` is pure and unit-tested against the measured
truncated/complete populations above.

## Playback speed: +10% via `atempo`

Narration is sped up 10% (`SPEED_MULTIPLIER = 1.1` in `audio_chain.rs`) with a
pitch-preserving `atempo` time-stretch, applied as the first filter in
`prepare_chunk` so every returned duration, and the chapter/chunk offsets
derived from it, already reflects the sped audio. This is decoupled from the
model's own speed handling (Higgs stays at its quality-tuned `speed=0.9`); the
intro jingle is joined later in `assemble_episode` and is not sped. The
duration-band fallback constants in `synthesize.rs` (`MIN_SEC_PER_CHAR`, `MAX_SEC_PER_CHAR`,
`IDEAL_SEC_PER_CHAR`) are divided by
`SPEED_MULTIPLIER` to stay honest; STT coverage is speed-invariant.

## Known next steps

- **In-band comb sibilance (< 9.5 kHz)**: still present and inherently a
  model artifact: post-processing can't notch a moving comb out of the band
  that carries real sibilance without dulling it. If it's still bothersome,
  the levers are, in order: (1) a dynamic de-esser-style band compressor on
  8–9.5 kHz (only engages during sibilant bursts; `acrossover` +
  `sidechaincompress` filtergraph; complex, prototype first); (2) source
  side: different Higgs quantization/settings on the mlx server, a reference
  clip with softer sibilance, or switching the provider back to ElevenLabs
  (clean sibilance, $0.10/1k chars).
- **STT round-trip verify** (shipped 2026-07, was "Whisper round-trip"): see
  the section below. The duration band is now the fallback verifier.
- **MP3 96k pre-echo**: not currently audible, but if a "smear before
  transients" complaint ever comes in, suspect the encoder before the chain:
  test by encoding a mastered WAV at 128k/V2 and comparing.
