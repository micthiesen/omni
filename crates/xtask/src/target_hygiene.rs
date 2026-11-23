//! `hygiene`: keep abandoned build directories from filling the disk, without
//! ever deleting from a build directory someone is using.
//!
//! Deleting artifacts from the directory in use forces the next build to start
//! cold (15 minutes or more for this workspace), and a size-based sweep did that
//! on every gate once one full build passed the limit. So this check removes
//! only whole directories under `target/agents/` that nothing has used for
//! `OMNI_TARGET_STALE_HOURS` (default 48): neither their `.omni-last-used`
//! stamp, which every run writes into the directory in use, nor any profile's
//! `deps/` directory, which every build rewrites, changed in that time. The
//! directory in use (`CARGO_TARGET_DIR`, else `target/`) is never deleted from.
//! Then it measures every Cargo profile directory (one holding `.fingerprint`)
//! and, above `OMNI_TARGET_LIMIT_GIB` (default 80), names the largest build
//! directory and suggests `cargo clean`. Disk usage alone never fails the gate.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};

const DEFAULT_LIMIT_GIB: u64 = 80;
const LIMIT_VARIABLE: &str = "OMNI_TARGET_LIMIT_GIB";
const DEFAULT_STALE_HOURS: u64 = 48;
const STALE_VARIABLE: &str = "OMNI_TARGET_STALE_HOURS";
/// Written into the directory in use on every run.
const STAMP: &str = ".omni-last-used";
const GIB: u64 = 1 << 30;
/// `target/agents/<name>/<triple>/<profile>` is the deepest layout in use.
const MAX_DEPTH: usize = 5;

/// Removes stale agent build directories and reports Cargo artifact usage.
pub fn maintain(workspace: &Path) -> Result<()> {
    let workspace = &workspace
        .canonicalize()
        .with_context(|| format!("resolving {}", workspace.display()))?;
    let limit = positive(
        std::env::var(LIMIT_VARIABLE).ok().as_deref(),
        DEFAULT_LIMIT_GIB,
    );
    let stale_hours = positive(
        std::env::var(STALE_VARIABLE).ok().as_deref(),
        DEFAULT_STALE_HOURS,
    );
    let target = workspace.join("target");
    let in_use = in_use_directory(workspace);
    stamp(&in_use)?;
    let in_use = in_use.canonicalize().unwrap_or(in_use);

    let stale_after = Duration::from_secs(stale_hours * 3600);
    let agents = target.join("agents");
    for directory in stale_agent_directories(&agents, &in_use, SystemTime::now(), stale_after)? {
        let bytes = artifact_bytes(std::slice::from_ref(&directory))?;
        std::fs::remove_dir_all(&directory)
            .with_context(|| format!("removing {}", directory.display()))?;
        println!(
            "hygiene: removed {} ({}, unused for over {stale_hours} h)",
            directory.display(),
            gib(bytes)
        );
    }

    let directories = build_directories(&target, &in_use);
    let mut sizes = Vec::new();
    for directory in &directories {
        sizes.push((profile_bytes(directory, &agents)?, directory));
    }
    let total: u64 = sizes.iter().map(|(bytes, _)| bytes).sum();
    println!(
        "hygiene: {} of Cargo artifacts (limit {limit} GiB)",
        gib(total)
    );
    if total > limit * GIB
        && let Some((bytes, largest)) = sizes.iter().max_by_key(|(bytes, _)| *bytes)
    {
        println!(
            "hygiene: above {limit} GiB; the largest build directory is {} ({}). \
             Run `cargo clean` there when no build is using it.",
            largest.display(),
            gib(*bytes)
        );
    }
    Ok(())
}

/// A positive whole number from `value`; unset or unparsable keeps `default`.
fn positive(value: Option<&str>, default: u64) -> u64 {
    value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|number| *number > 0)
        .unwrap_or(default)
}

/// `CARGO_TARGET_DIR` (relative to the workspace) when set, else `target/`.
fn in_use_directory(workspace: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        Some(custom) if custom.is_absolute() => custom,
        Some(custom) => workspace.join(custom),
        None => workspace.join("target"),
    }
}

fn stamp(directory: &Path) -> Result<()> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let path = directory.join(STAMP);
    std::fs::write(&path, b"").with_context(|| format!("writing {}", path.display()))
}

/// Directories under `agents` whose stamp, own entry and every profile `deps/`
/// were last modified before `now - stale_after`, excluding `in_use` and any
/// directory that contains it.
fn stale_agent_directories(
    agents: &Path,
    in_use: &Path,
    now: SystemTime,
    stale_after: Duration,
) -> Result<Vec<PathBuf>> {
    let Ok(entries) = std::fs::read_dir(agents) else {
        return Ok(Vec::new());
    };
    let cutoff = now
        .checked_sub(stale_after)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut stale = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", agents.display()))?;
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let directory = entry.path();
        let canonical = directory
            .canonicalize()
            .unwrap_or_else(|_| directory.clone());
        if in_use.starts_with(&canonical) {
            continue;
        }
        if last_used(&directory)? < cutoff {
            stale.push(directory);
        }
    }
    stale.sort();
    Ok(stale)
}

/// The newest of the directory's own entry, its stamp and its profiles' `deps/`.
fn last_used(directory: &Path) -> Result<SystemTime> {
    let mut candidates = vec![directory.to_path_buf(), directory.join(STAMP)];
    for profile in profile_directories(&[directory.to_path_buf()])? {
        candidates.push(profile.join("deps"));
    }
    Ok(candidates
        .iter()
        .filter_map(|path| {
            path.metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
        })
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH))
}

/// `target/`, each directory under `target/agents/`, and the directory in use
/// when it lies outside `target/`.
fn build_directories(target: &Path, in_use: &Path) -> Vec<PathBuf> {
    let mut directories = vec![target.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(target.join("agents")) {
        let mut agents: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect();
        agents.sort();
        directories.extend(agents);
    }
    let target = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    if !in_use.starts_with(&target) {
        directories.push(in_use.to_path_buf());
    }
    directories
}

/// Bytes in the profile directories below `directory`, leaving out `agents`
/// (measured as directories of their own).
fn profile_bytes(directory: &Path, agents: &Path) -> Result<u64> {
    let mut total = 0;
    for profile in profile_directories(&[directory.to_path_buf()])? {
        if !directory.starts_with(agents) && profile.starts_with(agents) {
            continue;
        }
        total += tree_bytes(&profile)?;
    }
    Ok(total)
}

/// Bytes in every Cargo profile directory below `roots`.
fn artifact_bytes(roots: &[PathBuf]) -> Result<u64> {
    let mut total = 0;
    for profile in profile_directories(roots)? {
        total += tree_bytes(&profile)?;
    }
    Ok(total)
}

/// Directories holding `.fingerprint` below `roots`, without descending into them.
fn profile_directories(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut profiles = Vec::new();
    let mut pending: Vec<(PathBuf, usize)> = roots.iter().map(|root| (root.clone(), 0)).collect();
    while let Some((directory, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", directory.display()))?;
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let path = entry.path();
            if path.join(".fingerprint").is_dir() {
                profiles.push(path);
            } else if depth + 1 < MAX_DEPTH {
                pending.push((path, depth + 1));
            }
        }
    }
    profiles.sort();
    profiles.dedup();
    Ok(profiles)
}

fn tree_bytes(root: &Path) -> Result<u64> {
    let mut total = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let read = || format!("reading {}", directory.display());
        for entry in std::fs::read_dir(&directory).with_context(read)? {
            let entry = entry.with_context(read)?;
            let kind = entry.file_type().with_context(read)?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                total += entry.metadata().with_context(read)?.len();
            }
        }
    }
    Ok(total)
}

fn gib(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64 / GIB as f64;
    format!("{value:.1} GiB")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);
    const STALE: Duration = Duration::from_secs(48 * 3600);

    fn write(path: &Path, bytes: usize) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, vec![0; bytes])
    }

    fn age(path: &Path, now: SystemTime, by: Duration) -> std::io::Result<()> {
        std::fs::File::open(path)?.set_modified(now - by)
    }

    /// An agent build directory with one native profile, every entry aged `by`.
    fn agent(target: &Path, name: &str, now: SystemTime, by: Duration) -> Result<PathBuf> {
        let directory = target.join("agents").join(name);
        std::fs::create_dir_all(directory.join("debug/.fingerprint"))?;
        write(&directory.join("debug/deps/libx.rlib"), 100)?;
        write(
            &directory.join("debug/incremental/x-1/query-cache.bin"),
            100,
        )?;
        age(&directory.join("debug/deps"), now, by)?;
        age(&directory, now, by)?;
        Ok(directory)
    }

    #[test]
    fn a_stale_agent_directory_is_removed_whole() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let target = scratch.path().join("target");
        let now = SystemTime::now();
        let old = agent(&target, "old", now, STALE + HOUR)?;
        let in_use = target.clone();

        let stale = stale_agent_directories(&target.join("agents"), &in_use, now, STALE)?;
        assert_eq!(stale, vec![old]);
        Ok(())
    }

    #[test]
    fn a_recently_used_agent_directory_is_kept() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let target = scratch.path().join("target");
        let now = SystemTime::now();
        // Built an hour ago.
        let built = agent(&target, "built", now, STALE + HOUR)?;
        age(&built.join("debug/deps"), now, HOUR)?;
        // Not built for days, but a gate ran in it an hour ago.
        let stamped = agent(&target, "stamped", now, STALE + HOUR)?;
        write(&stamped.join(STAMP), 0)?;
        age(&stamped.join(STAMP), now, HOUR)?;
        age(&stamped, now, STALE + HOUR)?;
        // A wasm32 profile counts like the native one.
        let wasm = agent(&target, "wasm", now, STALE + HOUR)?;
        let wasm_profile = wasm.join("wasm32-unknown-unknown/debug");
        std::fs::create_dir_all(wasm_profile.join(".fingerprint"))?;
        write(&wasm_profile.join("deps/liby.rlib"), 100)?;
        age(&wasm, now, STALE + HOUR)?;
        age(&wasm.join("wasm32-unknown-unknown"), now, STALE + HOUR)?;

        let stale = stale_agent_directories(&target.join("agents"), &target, now, STALE)?;
        assert!(stale.is_empty(), "{stale:?}");
        Ok(())
    }

    #[test]
    fn the_directory_in_use_is_never_touched() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let target = scratch.path().join("target");
        let now = SystemTime::now();
        let current = agent(&target, "current", now, STALE * 10)?;
        let in_use = current.canonicalize()?;

        let stale = stale_agent_directories(&target.join("agents"), &in_use, now, STALE)?;
        assert!(stale.is_empty(), "{stale:?}");

        // The stamp is the only write to the directory in use.
        stamp(&in_use)?;
        assert!(in_use.join(STAMP).is_file());
        assert!(in_use.join("debug/deps/libx.rlib").is_file());
        assert!(
            in_use
                .join("debug/incremental/x-1/query-cache.bin")
                .is_file()
        );
        Ok(())
    }

    #[test]
    fn only_cargo_profile_directories_are_measured() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let target = scratch.path().join("target");
        std::fs::create_dir_all(target.join("debug/.fingerprint"))?;
        write(&target.join("debug/deps/libcore.rlib"), 1000)?;
        std::fs::create_dir_all(target.join("wasm32-unknown-unknown/debug/.fingerprint"))?;
        write(&target.join("wasm32-unknown-unknown/debug/app.wasm"), 500)?;
        // Per-agent build directories nest a whole target directory.
        let agent = target.join("agents/build");
        std::fs::create_dir_all(agent.join("debug/.fingerprint"))?;
        write(&agent.join("debug/deps/libx.rlib"), 200)?;
        std::fs::create_dir_all(agent.join("wasm32-unknown-unknown/debug/.fingerprint"))?;
        write(&agent.join("wasm32-unknown-unknown/debug/y.wasm"), 100)?;
        // Evidence from other tools lives beside the profiles.
        write(&target.join("evidence/run/capture.png"), 4000)?;

        assert_eq!(artifact_bytes(std::slice::from_ref(&target))?, 1800);
        assert_eq!(artifact_bytes(&[scratch.path().join("missing")])?, 0);
        // Each build directory is measured on its own.
        let agents = target.join("agents");
        assert_eq!(profile_bytes(&target, &agents)?, 1500);
        assert_eq!(profile_bytes(&agent, &agents)?, 300);
        Ok(())
    }

    #[test]
    fn limits_default_when_unset_or_invalid() {
        assert_eq!(positive(None, DEFAULT_LIMIT_GIB), 80);
        assert_eq!(positive(Some("12"), DEFAULT_LIMIT_GIB), 12);
        assert_eq!(positive(Some(" 60 "), DEFAULT_LIMIT_GIB), 60);
        assert_eq!(positive(Some("0"), DEFAULT_STALE_HOURS), 48);
        assert_eq!(positive(Some("lots"), DEFAULT_STALE_HOURS), 48);
    }
}
