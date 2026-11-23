//! `deps-check`: the crate dependency direction rules of docs/architecture.md
//! ("Crate layout"), checked over `cargo metadata`, plus the one-binary
//! integration-test layout (`test_layout`).

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{cargo_command, test_layout};

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    dependencies: Vec<Dependency>,
    manifest_path: std::path::PathBuf,
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Target {
    name: String,
    kind: Vec<String>,
    src_path: std::path::PathBuf,
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
    ("omni-image", &[]),
    ("omni-recipe-format", &[]),
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
    "omni-recipes-web",
    "omni-testkit",
    "xtask",
];

/// Frontend crates may use only the wasm-safe foundation crates (and each
/// other) from the workspace.
const FRONTEND: &[&str] = &[
    "omni-web",
    "omni-web-kit",
    "omni-web-pages",
    "omni-recipes-web",
];

/// Pure crates that build for wasm32: the wire DTOs and the recipe format.
const FRONTEND_FOUNDATION: &[&str] = &["omni-api", "omni-recipe-format"];

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
                if !FRONTEND_FOUNDATION.contains(&dep.as_str()) && !FRONTEND.contains(&dep.as_str())
                {
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
    let output = cargo_command()
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
    let mut layout_violations = Vec::new();
    for package in &metadata.packages {
        let Some(directory) = package.manifest_path.parent() else {
            continue;
        };
        let (files, root) = test_layout::read(directory)?;
        let targets = package
            .targets
            .iter()
            .filter(|target| target.kind.iter().any(|kind| kind == "test"))
            .map(|target| {
                let path = target
                    .src_path
                    .strip_prefix(directory)
                    .unwrap_or(&target.src_path);
                (target.name.clone(), path.to_string_lossy().into_owned())
            })
            .collect();
        layout_violations.extend(test_layout::violations(&test_layout::Layout {
            package: &package.name,
            targets,
            files,
            root,
        }));
    }
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
    let mut found = violations(&packages);
    found.extend(layout_violations);
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
            ("omni-recipe-format", &[]),
            (
                "omni-recipes-web",
                &[("omni-recipe-format", None), ("omni-core", None)],
            ),
        ]);
        assert_eq!(
            violations(&packages),
            vec![
                "subsystem omni-live -> subsystem omni-email is not allowlisted".to_owned(),
                "foundation omni-mcp-kit -> omni-email is not allowed".to_owned(),
                "omni-media -> omni-testkit must be a dev-dependency".to_owned(),
                "frontend omni-recipes-web -> omni-core is not allowed".to_owned(),
            ]
        );
    }
}
