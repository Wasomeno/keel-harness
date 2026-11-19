//! MCP server configuration and startup.
//!
//! Servers are declared per workspace in `.keel/mcp.toml`. A server is only
//! started when its `enabled` flag is true; a failed server is a warning and
//! contributes no tools, never a harness error.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::client::McpClient;
use crate::tool::McpTool;

/// Workspace-relative location of the server definitions.
pub const CONFIG_PATH: &str = ".keel/mcp.toml";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Environment variable names passed through to the child. Secrets belong
    /// here, never in `args`.
    #[serde(default)]
    pub env: Vec<String>,
    /// MCP calls are barriers by default. Opt in per server only when the
    /// server is known to accept concurrent calls.
    #[serde(default)]
    pub parallel_safe: bool,
    #[serde(default = "default_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
}

const fn enabled_by_default() -> bool {
    true
}

const fn default_startup_timeout_ms() -> u64 {
    10_000
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read MCP config `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse MCP config `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
}

impl McpConfig {
    /// Reads `.keel/mcp.toml` under `workspace`; a missing file means no MCP
    /// servers. Parse and permission errors are returned to the caller.
    pub fn from_workspace(workspace: &Path) -> Result<Self, ConfigError> {
        Self::load(config_path(workspace))
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                    path: path.display().to_string(),
                    source,
                })?;
                Ok(config)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Starts every enabled server, lists its tools and filters names that
    /// would collide with `reserved` (the built-ins). Servers run in
    /// `current_dir`, normally the workspace root.
    pub async fn connect(&self, reserved: &[String], current_dir: Option<&Path>) -> ServerSetup {
        let reserved: BTreeSet<&str> = reserved.iter().map(String::as_str).collect();
        let mut used = BTreeSet::new();
        let mut tools = Vec::new();
        let mut warnings = Vec::new();
        let mut connected = Vec::new();

        for (server_name, server) in &self.servers {
            if !server.enabled {
                continue;
            }
            if !is_valid_server_name(server_name) {
                warnings.push(format!(
                    "MCP server name `{server_name}` is invalid; use 1-32 letters, digits, `_` or `-`, starting with a letter or digit"
                ));
                continue;
            }
            if server.command.trim().is_empty() {
                warnings.push(format!(
                    "MCP server `{server_name}` has an empty command; no tools loaded"
                ));
                continue;
            }

            let timeout = std::time::Duration::from_millis(server.startup_timeout_ms.max(1));
            let client = match McpClient::start(server_name, server, timeout, current_dir).await {
                Ok(client) => client,
                Err(error) => {
                    warnings.push(format!(
                        "MCP server `{server_name}` did not start: {error}; no tools loaded"
                    ));
                    continue;
                }
            };
            let remote_tools = match client.list_tools(timeout).await {
                Ok(tools) => tools,
                Err(error) => {
                    warnings.push(format!(
                        "MCP server `{server_name}` did not list tools: {error}; no tools loaded"
                    ));
                    continue;
                }
            };

            connected.push(server_name.clone());
            let client = std::sync::Arc::new(client);
            for remote in remote_tools {
                let Some(name) = qualified_name(server_name, &remote.name) else {
                    warnings.push(format!(
                        "MCP tool `{}` from `{server_name}` was skipped: its name is invalid or too long after adding the server prefix",
                        remote.name
                    ));
                    continue;
                };
                if reserved.contains(name.as_str()) || !used.insert(name.clone()) {
                    warnings.push(format!(
                        "MCP tool `{name}` from `{server_name}` was skipped: the name is already registered"
                    ));
                    continue;
                }
                used.insert(name.clone());
                tools.push(McpTool::new(
                    name,
                    server_name.clone(),
                    remote,
                    client.clone(),
                    server.parallel_safe,
                ));
            }
        }

        ServerSetup {
            tools,
            warnings,
            connected,
        }
    }
}

#[derive(Debug)]
pub struct ServerSetup {
    pub tools: Vec<McpTool>,
    pub warnings: Vec<String>,
    /// Servers that completed initialize and tools/list.
    pub connected: Vec<String>,
}

impl ServerSetup {
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }
}

pub fn config_path(workspace: &Path) -> PathBuf {
    workspace.join(CONFIG_PATH)
}

/// The registry namespace is `server__tool`. Tool names must stay within the
/// common provider limit of 64 characters.
pub fn qualified_name(server: &str, tool: &str) -> Option<String> {
    if !is_valid_server_name(server) || !is_valid_tool_name(tool) {
        return None;
    }
    let name = format!("{server}__{tool}");
    (name.len() <= 64).then_some(name)
}

fn is_valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_is_empty() {
        let directory = tempfile::tempdir().unwrap();
        let config = McpConfig::from_workspace(directory.path()).unwrap();
        assert!(config.servers.is_empty());
    }

    #[test]
    fn parses_a_server_with_an_environment_allowlist() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".keel")).unwrap();
        std::fs::write(
            directory.path().join(CONFIG_PATH),
            r#"
[servers.github]
command = "github-mcp"
args = ["--stdio"]
env = ["GITHUB_TOKEN"]
parallel_safe = true
"#,
        )
        .unwrap();

        let config = McpConfig::from_workspace(directory.path()).unwrap();
        let server = &config.servers["github"];
        assert_eq!(server.command, "github-mcp");
        assert_eq!(server.args, ["--stdio"]);
        assert_eq!(server.env, ["GITHUB_TOKEN"]);
        assert!(server.enabled);
        assert!(server.parallel_safe);
        assert_eq!(server.startup_timeout_ms, 10_000);
    }

    #[test]
    fn qualified_names_use_the_server_namespace() {
        assert_eq!(
            qualified_name("github", "list_issues").as_deref(),
            Some("github__list_issues")
        );
        assert!(qualified_name("bad server", "list").is_none());
        assert!(qualified_name("github", "bad/tool").is_none());
        assert!(qualified_name("github", &"x".repeat(60)).is_none());
    }

    #[test]
    fn an_unknown_config_key_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".keel")).unwrap();
        std::fs::write(
            directory.path().join(CONFIG_PATH),
            "[servers.test]\ncommand = \"true\"\nsecret = \"nope\"\n",
        )
        .unwrap();
        assert!(McpConfig::from_workspace(directory.path()).is_err());
    }
}
