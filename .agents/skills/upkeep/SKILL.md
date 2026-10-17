---
name: upkeep
description: Full maintenance pass on this repo - upgrade the Rust toolchain, every exact-pinned crate (including majors, researched via changelogs), frontend build tools, Dockerfile images and downloads, GitHub Actions, and LLM model IDs, then verify everything and auto commit/push. Use when the user invokes $upkeep or asks to update/upgrade dependencies, tooling, or "outdated stuff".
---

# Upkeep: full maintenance pass

Upgrade everything in this repo that has drifted, verify it all works, then commit and push to main (CI builds and deploys the Docker image). Be thorough: majors are in scope, but every major gets changelog research before its version is bumped.

Release-age policy: adopt only releases published at least 14 days ago (crates, toolchains, actions, images, tools). Skip yanked and pre-release versions.

## 0. Sync with remote first

`git fetch origin && git status`. If local main is behind, pull (rebase) **before** surveying anything. Upgrading against a stale base wastes the entire pass: the remote may already contain feature work or its own dependency bumps.

## 1. Survey (parallelize all of this)

- **Crates:** `python3 .agents/skills/upkeep/outdated.py` lists every exact `=` pin in the root `[workspace.dependencies]` that has a newer eligible crates.io release, marking semver-breaking bumps. It takes about two minutes (one request per second).
- **Advisories:** `cargo deny --locked check` (CI pin `cargo-deny@0.20.2`). Revisit every `ignore` entry in `deny.toml`; drop it when its reason no longer holds.
- **Rust toolchain:** `rust-toolchain.toml` `channel` vs the latest stable (`curl -s https://static.rust-lang.org/dist/channel-rust-stable.toml | grep -m1 '^version'`). Sibling Rust projects `beastie` and `deadpan` pin the same toolchain; check theirs (`../beastie/rust-toolchain.toml`, `../deadpan/rust-toolchain.toml`) and move together when they are ahead.
- **Frontend tools:** trunk, wasm-bindgen-cli and binaryen in `deploy/web-tools/install.sh` and `crates/omni-web/Trunk.toml` (`gh api repos/trunk-rs/trunk/releases/latest`, `repos/WebAssembly/binaryen/releases/latest`). wasm-bindgen-cli must equal the `wasm-bindgen` crate pin.
- **Docker downloads and images:** `Dockerfile` `FROM` images (`rust:<version>-bookworm`, `node:<LTS>-slim`, the `docker/dockerfile` syntax pin), cargo-chef (`deploy/build-tools/install-cargo-chef.sh`), sherpa-onnx (`deploy/sherpa-onnx/fetch-static-lib.sh`, must equal the `sherpa-onnx`/`sherpa-onnx-sys` pins), yt-dlp and the livestream model downloads. The runtime keeps Node only for `yt-dlp --js-runtimes node`: use the latest Active LTS (`curl -s https://raw.githubusercontent.com/nodejs/Release/main/schedule.json`).
- **GitHub Actions:** for each `uses:` in `.github/workflows/*.yml`, `gh api repos/<owner>/<repo>/releases/latest --jq .tag_name`, plus the `cargo-deny@` version given to `taiki-e/install-action`. Actions are pinned to full commit SHAs with a release comment; resolve the new tag to its SHA.
- **LLM model IDs:** defaults in `crates/omni-config/src/lib.rs`, the registry in `crates/omni-ai/src/registry.rs` and prices in `crates/omni-ai/src/costs.rs`, plus examples in `README.md` and `.env.example`. A model without a price records its cost as unknown, so add pricing with the ID.

## 2. Research majors before bumping

For each **breaking** crate bump and each toolchain, Leptos, axum, reqwest, rmcp or tokio minor that changes APIs, inspect actual usage (`rg` the crate's paths) and read the official changelog or migration guide. Use bounded parallel research only when several independent major migrations make it worthwhile. Compatible bumps need no separate research.

Model IDs: pick the newest **generally-available** model from the *same provider and tier* (flash-class stays flash-class). Confirm the ID against the provider's model list API or documentation; preview IDs get replaced by their GA successor.

## 3. Apply

- **Crates:** edit the exact `=x.y.z` pins in the root `Cargo.toml` only (crates use `workspace = true`), keep feature lists, and update the "published on or before" date in the pin comment. Then `cargo update --workspace` to re-resolve, and `cargo update` (no args) for transitive crates, since those are not pinned. Keep ecosystem families aligned (leptos/leptos_router/leptos_meta, tokio/tokio-util/tokio-stream, icu_*, sherpa-onnx and sherpa-onnx-sys, wasm-bindgen with web-sys/js-sys/wasm-bindgen-futures and the CLI pin).
- **Toolchain:** move together `rust-toolchain.toml` `channel`, every Dockerfile `rust:<version>-bookworm` image, and `rust-version` in `[workspace.package]` (minor only). New Clippy lints may fire; fix them rather than allowing them.
- **Frontend tools:** update versions and every per-architecture SHA-256 in `deploy/web-tools/install.sh`, and `crates/omni-web/Trunk.toml` `[tools]`. Install the same versions locally (`cargo install --locked trunk@<v> wasm-bindgen-cli@<v>`).
- **Downloads:** every pinned download carries a SHA-256; recompute it from the release artifact (`curl -sL -A 'OpenAI File Downloader, XaiImageApiFetch/1.0' <url> | shasum -a 256`) for both `x86_64` and `aarch64` where the script has both.
- **deny.toml:** keep the license allow list minimal; add a license only with a comment naming the crate that needs it.
- Fix code breakage from majors. When a changelog claims a rename, confirm it against the crate source in `~/.cargo/registry/src/` before editing.
- Frontend migrations (Leptos) can go to a subagent that owns only `crates/omni-web*` with its own `CARGO_TARGET_DIR`, while backend fixes proceed in parallel. Only one agent edits `Cargo.toml` and `Cargo.lock`.

## 4. Verify (all must pass)

```
cargo xtask gate                                # fmt, clippy native + wasm32, deps-check, tests
cargo deny --locked check
(cd crates/omni-web && trunk build --release --locked)
```

Then a runtime smoke test of the real binary (catches config, boot and runtime breaks tests miss):

```
LOG_LEVEL=info DB_NAME=<scratch>/smoke.db \
  FRONTEND_PORT=3199 timeout --signal=SIGTERM 15 \
  cargo run -p omni -- --side-effects=record --web-dist crates/omni-web/dist
```

Expect: config logged, tasks registered, server listening, graceful shutdown on SIGTERM. `curl -s localhost:3199/api/health` during the run should answer. The Docker image cannot be verified locally (no container runtime on this box); CI's `image` job and the in-image `omni doctor --image` cover it.

## 5. Ship

1. `git pull --rebase` (again: the remote may have moved during the pass)
2. Commit everything with a message summarizing toolchain/crate/tool/model changes (what and why, not a file list)
3. `git push`
4. Watch CI to completion: `gh run watch <id> --exit-status` (background). The `image` and `publish` jobs are the only verification of the Dockerfile; do not declare success until they are green. If CI fails, fix and push again.
