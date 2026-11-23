# CI and build caching

`.github/workflows/ci.yml` runs on every push to `main` (which deploys) and
on pull requests, except pushes that touch only Markdown under `docs/`,
`AGENTS.md`, `README.md`, `.agents/`, `ios/`, `scripts/ios-live/`,
`scripts/brand/` or `slate/` (`paths-ignore`). Those never reach the image or a
Rust test, and a new image would restart production for nothing: Boris
recreates the container whenever `latest` changes. `docs/mcp-policy.json` and
`docs/licenses/` still trigger a run. The repository is public, so the standard GitHub-hosted
runners are free; Linux runners have 4 cores. Change the workflow and this
file together. The caching scheme comes from the wt repository's CI.

## Jobs

| Job | What it does |
|---|---|
| `lint` | `cargo fmt --check`, clippy (native, and wasm32 for the web crates, `omni-recipes-web` and `omni-recipe-format`), `deps-check`, `cargo deny` |
| `test` | `cargo nextest run --workspace --profile ci`, built (`--no-run`) and run as separate steps, after installing `tzdata-legacy` (Ubuntu 26.04 runners lack the backward-compatible zone names that the Debian runtime image has) |
| `server` | Release build of `omni`, `omni-voice-enroll` and `omni-intel-doctor` in `rust:1.97.1-bookworm`, uploaded as the `server` artifact |
| `web` | Both release trunk builds (`omni-web` and Fond's `omni-recipes-web`) with the pinned tools, compressed (`.br`, `.gz`) like the Dockerfile's `web-dist-build` stage, uploaded as the `web` artifact |
| `publish` | After `lint`, `test`, `server` and `web`: one Docker build with `SERVER_SOURCE=prebuilt` and `WEB_SOURCE=prebuilt` that packages both artifacts and, on `main`, pushes to GHCR, which Boris deploys; layers go to the GitHub Actions cache |
| `prune` | `main` only: deletes superseded Cargo cache entries |
| `notify` | Pushover on a failed `main` run |

`lint`, `test`, `server` and `web` start together, so a push reports in about
the time of the longest of them plus about a minute of packaging. Pull
requests run the same build without pushing.

## Caching

Each Rust job restores and saves its whole `target/` directory and the Cargo
registry through `.github/actions/cargo-cache`, under a flavor name (`lint`,
`test`, `release-server`, `web`). These rules let a job reuse `main`'s compiled
workspace crates and recompile only what changed:

1. **Only `main` saves.** GitHub scopes a cache to the ref that saved it; a
   pull request or branch save could never be restored elsewhere.
2. **Every commit has its own key; restores fall back to the newest.** The key
   is `cargo-<name>-<os>-<arch>-<toolchain hash>-<sha>`. `Swatinem/rust-cache`
   (used before) keeps only dependencies, so every push recompiled every
   workspace crate.
3. **Files are dated by their last commit, and changes since the cache are
   new** (`scripts/ci/restore-mtimes.py`, after the restore of a
   `fetch-depth: 0` checkout). Cargo treats a workspace crate as stale when a
   source file is newer than its cached output, and a checkout stamps every
   file with the current time. Commit times alone would let a commit dated
   before the cache was built (pushed during a run, or an older pull request
   commit) look fresh and ship stale code, so each save records its commit in
   `target/.cache-sha` and the script stamps every file changed since then
   with the current time; a cache without a known commit is deleted. The
   script fails on a shallow clone.
4. **No build input may vary per commit.** The image revision reaches the
   binary only at runtime (`OMNI_REVISION` build arg, `build_identity.rs`), and
   no build script reads the commit; anything that did would rebuild every
   dependent crate on every push. `CARGO_INCREMENTAL=0` keeps unreusable
   incremental output out of the cache.
5. **The server is built where it runs.** The `server` job uses the
   Dockerfile's Rust image, so the binaries link against the same Debian
   bookworm glibc as the `node:*-slim` runtime. The Dockerfile's default
   `SERVER_SOURCE=build` still compiles them itself (with cargo-chef layers)
   for a plain local `docker build`, and `WEB_SOURCE=build` runs trunk there.
6. **Superseded entries are pruned.** `prune` keeps only the newest entry of
   each Cargo cache name, across toolchains. A cache grows with each
   dependency bump until the toolchain changes; check `gh cache list` sizes
   if the repository nears its 10 GB quota. The `test` cache keeps the linked
   test executables: each crate's integration tests are one binary
   (`tests/it.rs`), so there are few of them, and unchanged crates then
   relink nothing. `lint` saves after `deps-check` so xtask's build is cached
   too. The Docker layer cache (`type=gha,scope=omni`) is left to GitHub's
   least-recently-used eviction; with the server and web bundles built outside
   Docker it holds only downloads and the runtime.
