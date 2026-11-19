//! MCP server configuration and startup.
//!
//! Servers are declared per workspace in `.keel/mcp.toml`. Starting them is an
//! explicit CLI opt-in; this module still treats `enabled` as a secondary
//! filter. A failed server is a warning and contributes no tools, never a
//! harness error.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::time::Instant;

use crate::client::McpClient;
use crate::tool::McpTool;

/// Workspace-relative location of the server definitions.
pub const CONFIG_PATH: &str = ".keel/mcp.toml";

/// Untrusted config cannot make startup hang for longer than this.
pub const MAX_STARTUP_TIMEOUT_MS: u64 = 60_000;
pub const MAX_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Default)]
pub struct McpConfig {
    pub servers: BTreeMap<String, ServerConfig>,
    /// Per-server parse problems. One invalid server no longer disables every
    /// other server in the file.
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Environment variable names passed through to the child. This is hygiene,
    /// not a security boundary: the command itself is arbitrary.
    #[serde(default)]
    pub env: Vec<String>,
    /// MCP calls are barriers by default. Opt in per server only when the
    /// server is known to accept concurrent calls.
    #[serde(default)]
    pub parallel_safe: bool,
    #[serde(default = "default_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
}

impl ServerConfig {
    /// The configured startup budget, bounded so a hostile file cannot hang
    /// keel for days.
    pub fn startup_timeout(&self) -> Duration {
        Duration::from_millis(self.startup_timeout_ms.clamp(1, MAX_STARTUP_TIMEOUT_MS))
    }
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
    #[error("invalid MCP config `{path}`: {message}")]
    Shape { path: String, message: String },
}

impl McpConfig {
    /// Reads `.keel/mcp.toml` under `workspace`; a missing file means no MCP
    /// servers. A bad entry is reported per server and skipped.
    pub fn from_workspace(workspace: &Path) -> Result<Self, ConfigError> {
        Self::load(config_path(workspace))
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let path = path.display().to_string();
        let root: toml::Value = text.parse().map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        let table = root.as_table().ok_or_else(|| ConfigError::Shape {
            path: path.clone(),
            message: "the document root must be a table".to_owned(),
        })?;
        for key in table.keys() {
            if key != "servers" {
                return Err(ConfigError::Shape {
                    path,
                    message: format!("unknown top-level key `{key}`"),
                });
            }
        }

        let mut config = Self::default();
        let Some(servers) = table.get("servers") else {
            return Ok(config);
        };
        let servers = servers.as_table().ok_or_else(|| ConfigError::Shape {
            path: path.clone(),
            message: "`servers` must be a table".to_owned(),
        })?;
        for (name, value) in servers {
            match value.clone().try_into::<ServerConfig>() {
                Ok(server) => {
                    config.servers.insert(name.clone(), server);
                }
                Err(error) => config.warnings.push(format!(
                    "MCP server `{name}` was skipped: invalid config: {error}"
                )),
            }
        }
        Ok(config)
    }

    /// Starts every enabled server, lists its tools and filters names that
    /// would collide with `reserved` (the built-ins). Servers run in
    /// `current_dir`, normally the workspace root. The whole handshake and
    /// every `tools/list` page share one startup deadline.
    pub async fn connect(&self, reserved: &[String], current_dir: Option<&Path>) -> ServerSetup {
        let reserved: BTreeSet<&str> = reserved.iter().map(String::as_str).collect();
        let mut used = BTreeSet::new();
        let mut tools = Vec::new();
        let mut warnings = self.warnings.clone();
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
            if server.startup_timeout_ms > MAX_STARTUP_TIMEOUT_MS {
                warnings.push(format!(
                    "MCP server `{server_name}` startup_timeout_ms is capped at {MAX_STARTUP_TIMEOUT_MS}"
                ));
            }

            let timeout = server.startup_timeout();
            let deadline = Instant::now() + timeout;
            let client =
                match McpClient::start_until(server_name, server, deadline, timeout, current_dir)
                    .await
                {
                    Ok(client) => client,
                    Err(error) => {
                        warnings.push(format!(
                            "MCP server `{server_name}` did not start: {error}; no tools loaded"
                        ));
                        continue;
                    }
                };
            let remote_tools = match client.list_tools_until(deadline, timeout).await {
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
                let Some((name, mapped)) = qualified_name_with_mapping(server_name, &remote.name)
                else {
                    warnings.push(format!(
                        "MCP tool `{}` from `{server_name}` was skipped: its name is invalid or too long after adding the server prefix",
                        remote.name
                    ));
                    continue;
                };
                if mapped {
                    warnings.push(format!(
                        "MCP tool `{}` from `{server_name}` was registered as `{name}`: `.` and `:` are mapped to `_`",
                        remote.name
                    ));
                }
                if reserved.contains(name.as_str()) || used.contains(&name) {
                    warnings.push(format!(
                        "MCP tool `{name}` from `{server_name}` was skipped: the name is already registered"
                    ));
                    continue;
                }
                match McpTool::new(
                    name.clone(),
                    server_name.clone(),
                    remote,
                    client.clone(),
                    server.parallel_safe,
                ) {
                    Ok((tool, warning)) => {
                        used.insert(name);
                        tools.push(tool);
                        if let Some(warning) = warning {
                            warnings.push(warning);
                        }
                    }
                    Err(reason) => warnings.push(format!(
                        "MCP tool `{name}` from `{server_name}` was skipped: {reason}"
                    )),
                }
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
/// common provider limit of 64 characters. Dots and colons in the remote tool
/// name are mapped to `_`; other unsupported characters skip the tool.
pub fn qualified_name(server: &str, tool: &str) -> Option<String> {
    qualified_name_with_mapping(server, tool).map(|(name, _)| name)
}

fn qualified_name_with_mapping(server: &str, tool: &str) -> Option<(String, bool)> {
    if !is_valid_server_name(server) {
        return None;
    }
    let (tool, mapped) = map_tool_name(tool)?;
    let name = format!("{server}__{tool}");
    (name.len() <= 64).then_some((name, mapped))
}

fn map_tool_name(name: &str) -> Option<(String, bool)> {
    if name.is_empty() {
        return None;
    }
    let mut mapped = false;
    let mut output = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            output.push(character);
        } else if character == '.' || character == ':' {
            output.push('_');
            mapped = true;
        } else {
            return None;
        }
    }
    Some((output, mapped))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_is_empty() {
        let directory = tempfile::tempdir().unwrap();
        let config = McpConfig::from_workspace(directory.path()).unwrap();
        assert!(config.servers.is_empty());
        assert!(config.warnings.is_empty());
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
        assert_eq!(
            qualified_name("github", "issues.list:all").as_deref(),
            Some("github__issues_list_all")
        );
        assert!(qualified_name("bad server", "list").is_none());
        assert!(qualified_name("github", "bad/tool").is_none());
        assert!(qualified_name("github", &"x".repeat(60)).is_none());
    }

    #[test]
    fn one_bad_server_does_not_disable_the_others() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".keel")).unwrap();
        std::fs::write(
            directory.path().join(CONFIG_PATH),
            r#"
[servers.good]
command = "true"

[servers.bad]
command = "true"
secret = "nope"
"#,
        )
        .unwrap();
        let config = McpConfig::from_workspace(directory.path()).unwrap();
        assert!(config.servers.contains_key("good"));
        assert!(!config.servers.contains_key("bad"));
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("`bad`") && warning.contains("unknown field")),
            "{:?}",
            config.warnings
        );
    }

    #[test]
    fn a_long_startup_timeout_is_clamped() {
        let server = ServerConfig {
            command: "true".to_owned(),
            args: Vec::new(),
            enabled: true,
            env: Vec::new(),
            parallel_safe: false,
            startup_timeout_ms: u64::MAX,
        };
        assert_eq!(server.startup_timeout(), MAX_STARTUP_TIMEOUT);
    }

    #[test]
    fn a_syntax_error_is_still_a_config_error() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".keel")).unwrap();
        std::fs::write(directory.path().join(CONFIG_PATH), "[servers.test\n").unwrap();
        assert!(McpConfig::from_workspace(directory.path()).is_err());
    }
}
