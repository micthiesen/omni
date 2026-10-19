//! Stdio MCP servers bundled in the image.
//!
//! A stdio integration names one of these by id; it never names a command,
//! so the management UI cannot run arbitrary programs. Each server's npm
//! package is installed at image build from the lockfile in
//! `deploy/mcp-stdio/` (exact versions and integrity hashes, no install
//! scripts) and runs with Node from the runtime image.

use std::path::{Path, PathBuf};

use super::client::stdio::StdioCommand;

/// One bundled server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundledServer {
    pub id: String,
    pub name: String,
    pub package: String,
    /// Variables the server needs; their values are the integration's credentials.
    pub env: Vec<String>,
    program: PathBuf,
    args: Vec<String>,
    /// The installed entry point, when the server is a package file.
    entry: Option<PathBuf>,
}

impl BundledServer {
    /// Whether the server's entry point is installed.
    pub fn available(&self) -> bool {
        self.entry.as_deref().is_none_or(Path::is_file)
    }

    /// The installed entry point, when the server is a package file.
    pub fn entry(&self) -> Option<&Path> {
        self.entry.as_deref()
    }

    /// The command with `env` (only variables the server declares are passed).
    pub fn command(&self, env: &[(String, String)]) -> StdioCommand {
        StdioCommand {
            program: self.program.clone(),
            args: self.args.clone(),
            env: env
                .iter()
                .filter(|(name, _)| self.env.contains(name))
                .cloned()
                .collect(),
        }
    }
}

/// The servers this process can run.
#[derive(Clone, Debug, Default)]
pub struct BundledRegistry {
    servers: Vec<BundledServer>,
}

/// `/app/mcp-stdio` in the image, `deploy/mcp-stdio` in a checkout.
pub fn default_root(dockerized: bool) -> PathBuf {
    if dockerized {
        PathBuf::from("/app/mcp-stdio")
    } else {
        PathBuf::from("deploy/mcp-stdio")
    }
}

impl BundledRegistry {
    /// The production list, installed under `root`.
    pub fn installed(root: &Path) -> Self {
        let entry = root.join("node_modules/@karakeep/mcp/dist/index.js");
        Self {
            servers: vec![BundledServer {
                id: "karakeep".into(),
                name: "Karakeep".into(),
                package: "@karakeep/mcp@0.33.0".into(),
                env: vec!["KARAKEEP_API_ADDR".into(), "KARAKEEP_API_KEY".into()],
                program: PathBuf::from("node"),
                args: vec![entry.to_string_lossy().into_owned()],
                entry: Some(entry),
            }],
        }
    }

    /// A registry of arbitrary commands (tests).
    pub fn custom(
        servers: impl IntoIterator<Item = (String, Vec<String>, PathBuf, Vec<String>)>,
    ) -> Self {
        Self {
            servers: servers
                .into_iter()
                .map(|(id, env, program, args)| BundledServer {
                    name: id.clone(),
                    package: format!("test:{id}"),
                    id,
                    env,
                    program,
                    args,
                    entry: None,
                })
                .collect(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&BundledServer> {
        self.servers.iter().find(|server| server.id == id)
    }

    pub fn servers(&self) -> &[BundledServer] {
        &self.servers
    }
}
