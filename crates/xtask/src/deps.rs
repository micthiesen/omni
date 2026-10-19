//! `deps-check`: the crate dependency direction rules of docs/architecture.md
//! ("Crate layout"), checked over `cargo metadata`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::repo_root;

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
    kind: Option<String>,
}

/// Foundation crates and the foundation crates each may depend on (normal and build
/// dependencies; dev-dependencies may additionally use `omni-testkit`).
const FOUNDATION: &[(&str, &[&str])] = &[
    ("omni-core", &[]),
    ("omni-api", &[]),
    ("omni-config", &["omni-core"]),
    ("omni-store", &["omni-core", "omni-config"]),
    ("omni-http", &["omni-core", "omni-config"]),
    ("omni-mcp-kit", &["omni-core", "omni-api"]),
    (
        "omni-tasks",
        &["omni-core", "omni-config", "omni-store", "omni-api"],
    ),
    (
        "omni-alerts",
        &[
            "omni-core",
            "omni-config",
            "omni-http",
            "omni-store",
            "omni-tasks",
        ],
    ),
    ("omni-mailer", &["omni-core", "omni-config", "omni-http"]),
    (
        "omni-ai",
        &[
            "omni-core",
            "omni-config",
            "omni-http",
            "omni-store",
            "omni-tasks",
            "omni-api",
        ],
    ),
    (
        "omni-server-kit",
        &["omni-core", "omni-config", "omni-api", "omni-tasks"],
    ),
    (
        "omni-runtime",
        &[
            "omni-core",
            "omni-config",
            "omni-store",
            "omni-tasks",
            "omni-http",
            "omni-alerts",
            "omni-mailer",
            "omni-ai",
            "omni-server-kit",
            "omni-mcp-kit",
            "omni-api",
        ],
    ),
];

/// Allowed subsystem-to-subsystem edges.
const SUBSYSTEM_ALLOWLIST: &[(&str, &str)] = &[
    ("omni-ios-controls", "omni-live"),
    ("omni-parcel", "omni-email"),
    ("omni-calendar", "omni-email"),
];

/// Crates outside the backend layering (app binary, frontend, tooling).
const OUTSIDE: &[&str] = &[
    "omni",
    "omni-web",
    "omni-web-kit",
    "omni-web-pages",
    "omni-testkit",
    "xtask",
];

/// Frontend crates may use only `omni-api` (and each other) from the workspace.
const FRONTEND: &[&str] = &["omni-web", "omni-web-kit", "omni-web-pages"];

/// Package name -> `(dependency, kind)` pairs.
pub type Graph = BTreeMap<String, Vec<(String, Option<String>)>>;

/// Every rule violation in `packages`.
pub fn violations(packages: &Graph) -> Vec<String> {
    let foundation: BTreeMap<&str, &[&str]> = FOUNDATION.iter().copied().collect();
    let is_workspace = |name: &str| packages.contains_key(name);
    let is_subsystem = |name: &str| {
        name.starts_with("omni-") && !foundation.contains_key(name) && !OUTSIDE.contains(&name)
    };
    let mut found = Vec::new();
    for (name, deps) in packages {
        for (dep, kind) in deps {
            if !is_workspace(dep) {
                continue;
            }
            let dev = kind.as_deref() == Some("dev");
            if dep == "omni-testkit" {
                if !dev {
                    found.push(format!("{name} -> omni-testkit must be a dev-dependency"));
                }
                continue;
            }
            if dev && name != "omni-testkit" && !foundation.contains_key(name.as_str()) {
                continue;
            }
            if let Some(allowed) = foundation.get(name.as_str()) {
                if !allowed.contains(&dep.as_str()) {
                    found.push(format!("foundation {name} -> {dep} is not allowed"));
                }
            } else if FRONTEND.contains(&name.as_str()) {
                if dep != "omni-api" && !FRONTEND.contains(&dep.as_str()) {
                    found.push(format!("frontend {name} -> {dep} is not allowed"));
                }
            } else if is_subsystem(name) {
                if dep == "omni" {
                    found.push(format!("{name} -> omni is not allowed"));
                } else if is_subsystem(dep)
                    && !SUBSYSTEM_ALLOWLIST.contains(&(name.as_str(), dep.as_str()))
                {
                    found.push(format!(
                        "subsystem {name} -> subsystem {dep} is not allowlisted"
                    ));
                }
            }
        }
    }
    found
}

pub fn deps_check() -> Result<()> {
    let output = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(repo_root())
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
    let packages = metadata
        .packages
        .into_iter()
        .map(|p| {
            let deps = p
                .dependencies
                .into_iter()
                .map(|d| (d.name, d.kind))
                .collect();
            (p.name, deps)
        })
        .collect();
    let found = violations(&packages);
    if found.is_empty() {
        println!("deps-check: ok");
        return Ok(());
    }
    for violation in &found {
        println!("{violation}");
    }
    bail!("{} dependency rule violations", found.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Edges<'a> = &'a [(&'a str, Option<&'a str>)];

    fn graph(edges: &[(&str, Edges<'_>)]) -> Graph {
        edges
            .iter()
            .map(|(name, deps)| {
                (
                    (*name).to_owned(),
                    deps.iter()
                        .map(|(d, k)| ((*d).to_owned(), k.map(str::to_owned)))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn flags_cross_subsystem_and_upward_edges() {
        let packages = graph(&[
            ("omni-core", &[]),
            ("omni-testkit", &[]),
            ("omni-email", &[("omni-core", None)]),
            (
                "omni-parcel",
                &[("omni-email", None), ("omni-testkit", Some("dev"))],
            ),
            ("omni-live", &[("omni-email", None)]),
            ("omni-mcp-kit", &[("omni-email", None)]),
            ("omni-media", &[("omni-testkit", None)]),
        ]);
        assert_eq!(
            violations(&packages),
            vec![
                "subsystem omni-live -> subsystem omni-email is not allowlisted".to_owned(),
                "foundation omni-mcp-kit -> omni-email is not allowed".to_owned(),
                "omni-media -> omni-testkit must be a dev-dependency".to_owned(),
            ]
        );
    }
}
