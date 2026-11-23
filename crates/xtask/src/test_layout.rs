//! Part of `deps-check`: each crate's integration tests build as one binary.
//!
//! Every `tests/*.rs` file of a crate is a module of `tests/it.rs`, its only
//! test target (`autotests = false` plus one `[[test]]`). One binary per crate
//! instead of one per file cuts the test build roughly in half. Cargo would not
//! notice a new file nobody declares, so its tests would silently never run;
//! this check fails instead.

use std::path::Path;

use anyhow::{Context, Result};

/// The single integration-test target of a crate with `tests/*.rs` files.
pub const ROOT: &str = "it.rs";

/// What `deps-check` knows about one package's test layout.
pub struct Layout<'a> {
    pub package: &'a str,
    /// `(name, path relative to the package directory)` of each `test` target.
    pub targets: Vec<(String, String)>,
    /// Stems of the `tests/*.rs` files other than `it.rs`, sorted.
    pub files: Vec<String>,
    /// The text of `tests/it.rs`, when it exists.
    pub root: Option<String>,
}

/// Reads the layout of the package in `directory`.
pub fn read(directory: &Path) -> Result<(Vec<String>, Option<String>)> {
    let tests = directory.join("tests");
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&tests) {
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", tests.display()))?;
            let path = entry.path();
            if !entry.file_type().is_ok_and(|kind| kind.is_file())
                || path.extension().is_none_or(|extension| extension != "rs")
                || entry.file_name() == ROOT
            {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                files.push(stem.to_owned());
            }
        }
    }
    files.sort();
    let root = std::fs::read_to_string(tests.join(ROOT)).ok();
    Ok((files, root))
}

/// Every way `layout` breaks the one-binary rule.
pub fn violations(layout: &Layout<'_>) -> Vec<String> {
    let package = layout.package;
    let mut found = Vec::new();
    if layout.files.is_empty() && layout.root.is_none() {
        return found;
    }
    let expected = format!("tests/{ROOT}");
    let only_it = matches!(
        layout.targets.as_slice(),
        [(name, path)] if name == "it" && *path == expected
    );
    if !only_it {
        let names: Vec<&str> = layout
            .targets
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        found.push(format!(
            "{package}: integration tests must be one target `it` at {expected} with \
             `autotests = false`; found test targets {names:?}"
        ));
    }
    let Some(root) = &layout.root else {
        found.push(format!(
            "{package}: tests/ has test files but no {expected}"
        ));
        return found;
    };
    let declared = declared_modules(root);
    for file in &layout.files {
        if !declared.iter().any(|module| module == file) {
            found.push(format!(
                "{package}: tests/{file}.rs is not declared in {expected} (add `mod {file};`), \
                 so its tests would never run"
            ));
        }
    }
    found
}

/// Names of the top-level `mod <name>;` declarations in `source`.
fn declared_modules(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix("pub ").unwrap_or(line);
            let line = line.strip_prefix("pub(crate) ").unwrap_or(line);
            let name = line.strip_prefix("mod ")?.trim_end().strip_suffix(';')?;
            Some(name.trim().to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout<'a>(targets: &[(&str, &str)], files: &[&str], root: Option<&str>) -> Layout<'a> {
        Layout {
            package: "omni-x",
            targets: targets
                .iter()
                .map(|(name, path)| ((*name).to_owned(), (*path).to_owned()))
                .collect(),
            files: files.iter().map(|file| (*file).to_owned()).collect(),
            root: root.map(str::to_owned),
        }
    }

    #[test]
    fn a_declared_layout_passes() {
        let ok = layout(
            &[("it", "tests/it.rs")],
            &["alpha", "fake_caldav"],
            Some("//! doc\nmod fake_caldav;\npub mod alpha;\n"),
        );
        assert!(violations(&ok).is_empty());
        assert!(violations(&layout(&[], &[], None)).is_empty());
    }

    #[test]
    fn an_undeclared_test_file_fails() {
        let found = violations(&layout(
            &[("it", "tests/it.rs")],
            &["alpha", "beta"],
            Some("mod alpha;\n"),
        ));
        assert_eq!(found.len(), 1);
        assert!(
            found[0].contains("tests/beta.rs is not declared"),
            "{found:?}"
        );
    }

    #[test]
    fn auto_discovered_test_targets_fail() {
        // Without `autotests = false` Cargo adds every file as its own target.
        let found = violations(&layout(
            &[("alpha", "tests/alpha.rs"), ("it", "tests/it.rs")],
            &["alpha"],
            Some("mod alpha;\n"),
        ));
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("one target `it`"), "{found:?}");
    }

    #[test]
    fn test_files_without_a_root_fail() {
        let found = violations(&layout(&[("alpha", "tests/alpha.rs")], &["alpha"], None));
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn nested_module_lines_are_not_declarations() {
        assert_eq!(
            declared_modules("mod a;\n    mod b;\nmod c {\n}\n// mod d;\n"),
            vec!["a".to_owned()]
        );
    }
}
