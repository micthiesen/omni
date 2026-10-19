# syntax=docker/dockerfile:1.27.1

# Stage graph (BuildKit builds independent stages in parallel):
#   downloads: cargo-chef, sherpa-lib, web-tools, yt-dlp, silero, speaker,
#              segmentation, parakeet
#   rust:      chef -> planner -> server (cook, then workspace build)
#                             \-> web (cook, then trunk) -> web-dist
#   runtime:   apt layer, then static assets, then the frequently changing outputs
# Every download is pinned and SHA-256 verified in its own stage, so it caches
# independently of the Rust sources and of every other download.
#
# The Rust stages rely on cargo-chef layers rather than cache mounts: CI exports
# layers to the GitHub Actions cache, but never cache mounts, so a mounted
# registry or target directory would rebuild every dependency there.

# --- Downloads (the Rust image already carries curl, bzip2 and sha256sum) ---
FROM rust:1.97.1-bookworm AS fetch
ENV PATH=/opt/build-tools:$PATH
COPY deploy/build-tools/fetch-verified.sh /opt/build-tools/

FROM fetch AS cargo-chef
COPY deploy/build-tools/install-cargo-chef.sh /opt/build-tools/
RUN install-cargo-chef.sh /out

# sherpa-onnx-sys links a prebuilt static library; supply the SHA-256-verified
# archive so its build script never downloads an unchecked copy.
FROM fetch AS sherpa-lib
COPY deploy/sherpa-onnx/fetch-static-lib.sh /usr/local/bin/fetch-sherpa-onnx
RUN fetch-sherpa-onnx /opt/sherpa-onnx

FROM fetch AS web-tools
COPY deploy/web-tools/install.sh /usr/local/bin/install-web-tools
RUN install-web-tools /out

FROM fetch AS yt-dlp
ARG TARGETARCH
ARG YT_DLP_VERSION=2026.08.19
RUN case "${TARGETARCH}" in \
    amd64) ASSET=yt-dlp_linux; SHA256=58162f9bfdc27458ea47bfcb311cf47028f17d8154a8bf7d689861d46399230a ;; \
    arm64) ASSET=yt-dlp_linux_aarch64; SHA256=b16e4dab368a816cd05d477d698a605a6ae87ccee1c8ffd38fa21d7254141fcc ;; \
    *) echo "Unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; \
  esac \
  && fetch-verified.sh \
    "https://github.com/yt-dlp/yt-dlp/releases/download/${YT_DLP_VERSION}/${ASSET}" \
    "${SHA256}" /out/yt-dlp \
  && chmod 755 /out/yt-dlp

# Livestream intelligence models, one stage each.
FROM fetch AS silero
RUN fetch-verified.sh \
  https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.int8.onnx \
  c36d490aff5ab924ca6c7aeec4d8f6bd3d22db6fa17611b9c5b17eae58ac3a20 \
  /models/silero_vad.int8.onnx

FROM fetch AS speaker
RUN fetch-verified.sh \
  https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx \
  357a834f702b80161e5b981182c038e18553c1f2ca752ed6cec2052365d4129b \
  /models/3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx

# pyannote segmentation 3.0 (MIT) for rolling-summary speaker diarization.
FROM fetch AS segmentation
RUN fetch-verified.sh \
    https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2 \
    24615ee884c897d9d2ba09bb4d30da6bb1b15e685065962db5b02e76e4996488 \
    /tmp/segmentation.tar.bz2 \
  && mkdir -p /models \
  && tar -xjf /tmp/segmentation.tar.bz2 -C /models \
    sherpa-onnx-pyannote-segmentation-3-0/model.onnx \
    sherpa-onnx-pyannote-segmentation-3-0/LICENSE \
  && rm /tmp/segmentation.tar.bz2

FROM fetch AS parakeet
RUN fetch-verified.sh \
    https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2 \
    5793d0fd397c5778d2cf2126994d58e9d56b1be7c04d13c7a15bb1b4eafb16bf \
    /tmp/parakeet.tar.bz2 \
  && mkdir -p /models \
  && tar -xjf /tmp/parakeet.tar.bz2 -C /models \
  && rm /tmp/parakeet.tar.bz2

# --- Rust toolchain with cargo-chef (dependency layers cached separately) ---
FROM rust:1.97.1-bookworm AS chef
COPY --from=cargo-chef /out/cargo-chef /usr/local/cargo/bin/cargo-chef
WORKDIR /src
# Install the pinned toolchain's components once for every later stage.
COPY rust-toolchain.toml ./
RUN rustup toolchain install

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

# --- Server binaries ---
FROM chef AS server
COPY --from=sherpa-lib /opt/sherpa-onnx /opt/sherpa-onnx
ENV SHERPA_ONNX_ARCHIVE_DIR=/opt/sherpa-onnx
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json \
    -p omni -p omni-live-intel
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
# Crates the server never links keep their cargo-chef skeletons, so frontend and
# xtask edits leave this layer cached.
COPY --exclude=omni-web --exclude=omni-web-kit --exclude=omni-web-pages \
  --exclude=xtask \
  crates ./crates
# One invocation with the cook's package set: dependency features then match the
# cooked layer exactly, and shared workspace crates compile once.
RUN cargo build --release --locked -p omni -p omni-live-intel \
      --bin omni --bin omni-voice-enroll --bin omni-intel-doctor \
    && mkdir -p /out \
    && install -m 0755 target/release/omni target/release/omni-voice-enroll \
      target/release/omni-intel-doctor /out/

# --- Web frontend ---
FROM chef AS web
RUN rustup target add wasm32-unknown-unknown
COPY --from=web-tools /out/ /usr/local/bin/
COPY --from=planner /src/recipe.json recipe.json
# trunk runs `cargo metadata`, which needs every workspace package downloaded;
# fetching beside the cook keeps that out of the per-source-change layer.
RUN cargo chef cook --profile wasm-release --target wasm32-unknown-unknown --locked \
    --recipe-path recipe.json -p omni-web \
  && cargo fetch --locked
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
# Only omni-web's path dependencies; server crates keep their skeletons.
COPY crates/omni-api ./crates/omni-api
COPY crates/omni-web-kit ./crates/omni-web-kit
COPY crates/omni-web-pages ./crates/omni-web-pages
COPY crates/omni-web ./crates/omni-web
RUN cd crates/omni-web && trunk build --release --offline --locked

# The SPA service serves .br/.gz siblings when the client accepts them.
FROM fetch AS web-dist
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
  rm -f /etc/apt/apt.conf.d/docker-clean \
  && apt-get update \
  && apt-get install -y --no-install-recommends brotli
COPY --from=web /src/crates/omni-web/dist /dist
RUN find /dist -type f \
    \( -name '*.html' -o -name '*.js' -o -name '*.css' -o -name '*.wasm' \) \
    -exec brotli -k -q 11 {} + -exec gzip -k -n -9 {} +

# Stdio MCP servers the gateway can run (gateway/bundled.rs), installed from the
# committed lockfile: exact versions, integrity hashes, no install scripts.
FROM node:24.19.0-slim AS mcp-stdio
WORKDIR /opt/mcp-stdio
COPY deploy/mcp-stdio/package.json deploy/mcp-stdio/package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
  && test -f node_modules/@karakeep/mcp/dist/index.js

# --- Runtime ---
# Node runs yt-dlp's JavaScript (`--js-runtimes node`) and the bundled stdio
# MCP servers.
FROM node:24.19.0-slim AS runtime

# ffmpeg: PressPods audio pipeline (arnndn denoise, loudnorm, intro concat) and
#   livestream audio capture
# CUPS + brlaser + ghostscript + pdfinfo: bounded, model-aware PDF conversion for
#   Brother printing. The compatible HL-L2360D profile is physically verified on
#   this HL-L2370DW, including duplex.
# ca-certificates: rustls verifies TLS against the system trust store.
# tini: PID 1 that forwards signals and reaps orphaned subprocesses.
# The apt cache mounts keep package lists and archives out of the image.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
  rm -f /etc/apt/apt.conf.d/docker-clean \
  && apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates \
    cups \
    ffmpeg \
    ghostscript \
    poppler-utils \
    printer-driver-brlaser \
    tini \
  && mkdir -p /usr/share/omni-printing /tmp/brlaser-ppd \
  && ppdc -d /tmp/brlaser-ppd /usr/share/cups/drv/brlaser.drv \
  && cp /tmp/brlaser-ppd/brl2360d.ppd /usr/share/omni-printing/brother-hll2370dw.ppd \
  && rm -rf /tmp/brlaser-ppd \
  && mkdir -p /data \
  && chown node:node /data

WORKDIR /app

# Least to most frequently changing, so a code change rebuilds and pushes only
# the last layers.
COPY --chown=node:node docs/licenses ./licenses
COPY --from=silero --chown=node:node /models/ ./assets/livestream-intelligence/models/
COPY --from=speaker --chown=node:node /models/ ./assets/livestream-intelligence/models/
COPY --from=segmentation --chown=node:node /models/ ./assets/livestream-intelligence/models/
COPY --from=parakeet --chown=node:node /models/ ./assets/livestream-intelligence/models/
COPY --from=yt-dlp /out/yt-dlp /usr/local/bin/yt-dlp
COPY --from=mcp-stdio --chown=node:node /opt/mcp-stdio ./mcp-stdio
COPY --chown=node:node assets ./assets
COPY --from=web-dist --chown=node:node /dist ./web
COPY --from=server /out/ /usr/local/bin/

ENV DOCKERIZED=true DB_NAME=/data/docstore.db OMNI_WEB_DIST=/app/web
USER node

# Fails the build when a runtime invariant is missing: ffmpeg arnndn,
# firequalizer and libmp3lame, the denoise model, the brlaser filter and PPD,
# cupsfilter, pdfinfo, the sherpa models and a runnable yt-dlp. It reads only
# the tool and model paths, so it needs no runtime secrets.
RUN omni doctor --image

# The commit this image was built from (CI passes it); after the doctor so a new
# revision rebuilds no layer. Open pages compare it to offer a reload.
ARG OMNI_REVISION=
ENV OMNI_IMAGE_REVISION=${OMNI_REVISION}

EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["omni", "healthcheck"]

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["omni"]
