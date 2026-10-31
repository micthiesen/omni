# CI and build caching

`.github/workflows/ci.yml` runs on every push to `main` (which deploys) and
on pull requests. The repository is public, so the standard GitHub-hosted
runners are free; Linux runners have 4 cores. Change the workflow and this
file together. The caching scheme comes from the wt repository's CI.

## Jobs

| Job | What it does |
|---|---|
| `lint` | `cargo fmt --check`, clippy (native and wasm32), `deps-check`, `cargo deny` |
| `test` | `cargo nextest run --workspace --profile ci` |
| `server` | Release build of `omni`, `omni-voice-enroll` and `omni-intel-doctor` in `rust:1.97.1-bookworm`, uploaded as the `server` artifact |
| `image` | Docker build with `SERVER_SOURCE=prebuilt`, packaging the `server` artifact; layers go to the GitHub Actions cache |
| `publish` | `main` only, after `lint`, `test` and `image`: the same build (fully cached) pushed to GHCR, which Boris deploys |
| `prune` | `main` only: deletes superseded Cargo cache entries |
| `notify` | Pushover on a failed `main` run |

`lint`, `test` and `server` start together, so a push reports in about the
time of `server` plus the image packaging.

## Caching

Each Rust job restores and saves its whole `target/` directory and the Cargo
registry through `.github/actions/cargo-cache`, under a flavor name (`lint`,
`test`, `release-server`). These rules let a job reuse `main`'s compiled
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
   for a plain local `docker build`.
6. **Superseded entries are pruned.** `prune` keeps only the newest entry of
   each Cargo cache name, across toolchains. A cache grows with each
   dependency bump until the toolchain changes; check `gh cache list` sizes
   if the repository nears its 10 GB quota. The Docker layer cache (`type=gha,scope=omni`) is
   left to GitHub's least-recently-used eviction; with the server built
   outside Docker it holds only downloads, the web bundle and the runtime.
