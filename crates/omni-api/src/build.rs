//! The running build's identity, carried by every dashboard snapshot and by
//! `/api/health`, so an open page can tell that a deploy replaced it.

use serde::{Deserialize, Serialize};

/// Computed once per server process.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildIdentity {
    /// The container image revision, or a fingerprint of the executable.
    pub server: String,
    /// A hash of the served `index.html` (it names every hashed asset).
    pub frontend: String,
}

/// What differs between the build a page loaded with and the current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildChange {
    Frontend,
    Server,
    Both,
}

impl BuildChange {
    /// What changed, for the update tooltip.
    pub fn describe(self) -> &'static str {
        match self {
            BuildChange::Frontend => "app",
            BuildChange::Server => "server",
            BuildChange::Both => "app and server",
        }
    }
}

impl BuildIdentity {
    /// `None` when `current` is the build `self` (the one loaded) came from.
    pub fn change_to(&self, current: &BuildIdentity) -> Option<BuildChange> {
        match (
            self.frontend != current.frontend,
            self.server != current.server,
        ) {
            (false, false) => None,
            (true, false) => Some(BuildChange::Frontend),
            (false, true) => Some(BuildChange::Server),
            (true, true) => Some(BuildChange::Both),
        }
    }
}

/// `GET /api/health`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    /// Always `"ok"`.
    pub status: String,
    pub build: BuildIdentity,
}

/// `GET /api/health/deep`: 200 when every critical check passes, else 503.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepHealthResponse {
    pub ok: bool,
    pub build: BuildIdentity,
    pub checks: Vec<HealthCheckView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthCheckView {
    pub name: String,
    pub ok: bool,
    /// A failing critical check means this build does not work (a deploy
    /// rolls back); the others report things outside Omni's control.
    pub critical: bool,
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(server: &str, frontend: &str) -> BuildIdentity {
        BuildIdentity {
            server: server.to_owned(),
            frontend: frontend.to_owned(),
        }
    }

    #[test]
    fn change_to_names_each_changed_part() {
        let loaded = id("s1", "f1");
        assert_eq!(loaded.change_to(&id("s1", "f1")), None);
        assert_eq!(
            loaded.change_to(&id("s1", "f2")),
            Some(BuildChange::Frontend)
        );
        assert_eq!(loaded.change_to(&id("s2", "f1")), Some(BuildChange::Server));
        assert_eq!(loaded.change_to(&id("s2", "f2")), Some(BuildChange::Both));
    }
}
