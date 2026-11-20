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

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Stdio transport: the program to run. Exactly one of `command` and `url`
    /// must be set.
    #[serde(default)]
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
    /// Streamable HTTP transport: the endpoint URL. Exactly one of `command`
    /// and `url` must be set.
    #[serde(default)]
    pub url: Option<String>,
    /// Extra request headers for the HTTP transport. Values are expanded once
    /// with `${VAR}` from the process environment when the file is loaded.
    /// Header values are redacted from `Debug` and from every error.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// Header values can carry credentials, so `Debug` shows names only.
impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let header_names = self.headers.keys().map(String::as_str).collect::<Vec<_>>();
        f.debug_struct("ServerConfig")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("enabled", &self.enabled)
            .field("env", &self.env)
            .field("parallel_safe", &self.parallel_safe)
            .field("startup_timeout_ms", &self.startup_timeout_ms)
            .field("url", &self.url)
            .field("headers", &header_names)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportKind {
    Stdio,
    Http,
}

impl ServerConfig {
    /// The configured startup budget, bounded so a hostile file cannot hang
    /// keel for days.
    pub fn startup_timeout(&self) -> Duration {
        Duration::from_millis(self.startup_timeout_ms.clamp(1, MAX_STARTUP_TIMEOUT_MS))
    }

    /// A server is exactly one transport. Programmatic configs are checked
    /// here too; file configs are already filtered by [`parse`].
    pub(crate) fn transport_kind(&self) -> Result<TransportKind, String> {
        match (self.command.trim().is_empty(), self.url.as_deref()) {
            (false, None) => Ok(TransportKind::Stdio),
            (false, Some(url)) if url.trim().is_empty() => {
                Err("it sets an empty `url`; use exactly one transport".to_owned())
            }
            (false, Some(_)) => {
                Err("it sets both `command` and `url`; configure exactly one transport".to_owned())
            }
            (true, Some(url)) if !url.trim().is_empty() => Ok(TransportKind::Http),
            (true, Some(_)) => {
                Err("it sets an empty `url`; configure a command or a url".to_owned())
            }
            (true, None) => Err(
                "it sets neither `command` nor `url`; configure exactly one transport".to_owned(),
            ),
        }
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
        Self::parse_with_lookup(text, path, &|name| std::env::var(name).ok())
    }

    fn parse_with_lookup(
        text: &str,
        path: &Path,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
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
                Ok(mut server) => match validate_server(name, &mut server, lookup) {
                    Ok(warnings) => {
                        config.servers.insert(name.clone(), server);
                        config.warnings.extend(warnings);
                    }
                    Err(reason) => config
                        .warnings
                        .push(format!("MCP server `{name}` was skipped: {reason}")),
                },
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
            let kind = match server.transport_kind() {
                Ok(kind) => kind,
                Err(reason) => {
                    warnings.push(format!("MCP server `{server_name}` was skipped: {reason}"));
                    continue;
                }
            };
            if kind == TransportKind::Http
                && let Err(reason) = validate_url(server.url.as_deref().unwrap_or_default())
            {
                warnings.push(format!("MCP server `{server_name}` was skipped: {reason}"));
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

/// Headers the transport owns. A configured value could silently break the
/// JSON-RPC exchange or smuggle a session, so they are rejected by name.
const RESERVED_HEADERS: [&str; 6] = [
    "content-type",
    "accept",
    "content-length",
    "host",
    "mcp-session-id",
    "mcp-protocol-version",
];

pub(crate) fn is_reserved_header(name: &str) -> bool {
    RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str())
}

/// RFC 7230 `token`: one or more `tchar` bytes.
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// The transport sends only `http` and `https` endpoints. Reject credentials
/// in the authority and fragments so a configured URL cannot be mistaken for
/// an interactive login flow or silently drop its tail.
pub(crate) fn validate_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url
        .chars()
        .any(|character| character.is_control() || character == ' ')
    {
        return Err("url contains whitespace or control characters".to_owned());
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err("url must start with `http://` or `https://`".to_owned());
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(format!(
            "url scheme `{scheme}` is not supported; use `http` or `https`"
        ));
    }
    if url.contains('#') {
        return Err("url must not contain a fragment".to_owned());
    }
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err("url has no host".to_owned());
    }
    if authority.contains('@') {
        return Err("url must not contain userinfo; configure credentials in `headers`".to_owned());
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err("url has an unterminated IPv6 host".to_owned());
        };
        let port = match after {
            "" => None,
            _ => Some(
                after
                    .strip_prefix(':')
                    .ok_or_else(|| "url has an invalid host".to_owned())?,
            ),
        };
        (host, port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err("url has no host".to_owned());
    }
    if let Some(port) = port {
        match port.parse::<u16>() {
            Ok(0) | Err(_) => return Err("url has an invalid port".to_owned()),
            Ok(_) => {}
        }
    }
    Ok(())
}

/// Expands `${VAR}` exactly once. A value that itself contains `${...}` is
/// inserted literally; a missing or empty variable rejects the server.
fn expand_header_value(
    raw: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut output = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err("value has an unterminated `${` placeholder".to_owned());
        };
        let name = &after[..end];
        if name.is_empty() {
            return Err("value has an empty `${}` placeholder".to_owned());
        }
        match lookup(name) {
            Some(value) if !value.is_empty() => output.push_str(&value),
            _ => {
                return Err(format!(
                    "value references `${{{name}}}`, which is not set or is empty"
                ));
            }
        }
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

/// Header values stay visible ASCII (plus horizontal tab) so the HTTP client
/// never rejects them at call time, and CR/LF cannot inject a header.
fn is_sendable_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
}

/// Turns one parsed server into either a valid entry plus non-fatal warnings,
/// or a reason it is skipped. This is the only place header values are
/// expanded, so a programmatically built config cannot expand twice.
fn validate_server(
    name: &str,
    server: &mut ServerConfig,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    match server.transport_kind()? {
        TransportKind::Stdio => {
            if !server.headers.is_empty() {
                warnings.push(format!(
                    "MCP server `{name}` sets `headers`, which apply only to URL servers; they were ignored"
                ));
                server.headers.clear();
            }
            Ok(warnings)
        }
        TransportKind::Http => {
            let url = server.url.as_deref().unwrap_or_default().trim().to_owned();
            validate_url(&url)?;
            server.url = Some(url);
            if !server.args.is_empty() {
                warnings.push(format!(
                    "MCP server `{name}` sets `args`, which apply only to stdio servers; they were ignored"
                ));
            }
            if !server.env.is_empty() {
                warnings.push(format!(
                    "MCP server `{name}` sets `env`, which applies only to stdio servers; it was ignored"
                ));
            }

            let mut expanded = BTreeMap::new();
            for (header, raw) in &server.headers {
                if !is_token(header) {
                    return Err(format!("header name `{header}` is not a valid HTTP token"));
                }
                if is_reserved_header(header) {
                    return Err(format!(
                        "header `{header}` is reserved by the transport and cannot be configured"
                    ));
                }
                let value = expand_header_value(raw, lookup)
                    .map_err(|message| format!("header `{header}` {message}"))?;
                if !is_sendable_header_value(&value) {
                    return Err(format!(
                        "header `{header}` contains a character that HTTP header values cannot carry"
                    ));
                }
                expanded.insert(header.clone(), value);
            }
            server.headers = expanded;
            Ok(warnings)
        }
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
            url: None,
            headers: BTreeMap::new(),
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

    fn parse_with_env(text: &str, vars: &[(&str, &str)]) -> McpConfig {
        let vars = vars
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned()))
            .collect::<BTreeMap<_, _>>();
        McpConfig::parse_with_lookup(text, Path::new("mcp.toml"), &|name| vars.get(name).cloned())
            .unwrap()
    }

    #[test]
    fn parses_a_url_server_with_headers() {
        let config = parse_with_env(
            r#"
[servers.remote]
url = "https://mcp.example.test/rpc"
parallel_safe = true

[servers.remote.headers]
Authorization = "${TOKEN}"
X-Team = "alpha"
"#,
            &[("TOKEN", "secret-value")],
        );

        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        let server = &config.servers["remote"];
        assert_eq!(server.url.as_deref(), Some("https://mcp.example.test/rpc"));
        assert_eq!(server.headers["Authorization"], "secret-value");
        assert_eq!(server.headers["X-Team"], "alpha");
        assert!(server.parallel_safe);
        assert_eq!(
            server.transport_kind().unwrap(),
            TransportKind::Http,
            "url servers use the HTTP transport"
        );

        let debug = format!("{server:?}");
        assert!(!debug.contains("secret-value"), "{debug}");
        assert!(debug.contains("Authorization"), "{debug}");
    }

    #[test]
    fn a_server_needs_exactly_one_transport() {
        let config = parse_with_env(
            r#"
[servers.good]
command = "true"

[servers.both]
command = "true"
url = "https://example.test/mcp"

[servers.neither]
enabled = true
"#,
            &[],
        );
        assert!(config.servers.contains_key("good"));
        assert!(!config.servers.contains_key("both"));
        assert!(!config.servers.contains_key("neither"));
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("`both`") && warning.contains("both `command`")),
            "{:?}",
            config.warnings
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("`neither`")
                    && warning.contains("neither `command`")),
            "{:?}",
            config.warnings
        );
    }

    #[test]
    fn args_and_env_on_a_url_server_warn_but_keep_the_server() {
        let config = parse_with_env(
            r#"
[servers.remote]
url = "http://127.0.0.1:9/mcp"
args = ["--flag"]
env = ["TOKEN"]
"#,
            &[],
        );
        assert!(config.servers.contains_key("remote"));
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("`remote`") && warning.contains("`args`")),
            "{:?}",
            config.warnings
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("`remote`") && warning.contains("`env`")),
            "{:?}",
            config.warnings
        );
    }

    #[test]
    fn invalid_urls_are_skipped_with_clear_warnings() {
        let config = parse_with_env(
            r#"
[servers.good]
command = "true"

[servers.scheme]
url = "ftp://example.test/mcp"

[servers.credentials]
url = "https://user:password@example.test/mcp"

[servers.fragment]
url = "https://example.test/mcp#part"

[servers.relative]
url = "example.test/mcp"

[servers.nohost]
url = "https:///mcp"
"#,
            &[],
        );
        assert!(config.servers.contains_key("good"));
        for name in ["scheme", "credentials", "fragment", "relative", "nohost"] {
            assert!(
                !config.servers.contains_key(name),
                "the invalid server `{name}` was loaded"
            );
            assert!(
                config
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(&format!("`{name}`"))),
                "no warning for `{name}`: {:?}",
                config.warnings
            );
        }
        let warnings = config.warnings.join("\n");
        assert!(warnings.contains("scheme `ftp`"), "{warnings}");
        assert!(warnings.contains("userinfo"), "{warnings}");
        assert!(warnings.contains("fragment"), "{warnings}");
        assert!(!warnings.contains("password"), "{warnings}");
    }

    #[test]
    fn unsafe_headers_skip_only_their_server() {
        let config = parse_with_env(
            r#"
[servers.good]
command = "true"

[servers.reserved]
url = "https://example.test/mcp"

[servers.reserved.headers]
Content-Type = "text/plain"

[servers.crlf]
url = "https://example.test/mcp"

[servers.crlf.headers]
X-Secret = "line\nInjected: yes"

[servers.badname]
url = "https://example.test/mcp"

[servers.badname.headers]
"bad header" = "value"

[servers.missing]
url = "https://example.test/mcp"

[servers.missing.headers]
X-Token = "${NOT_SET_ANYWHERE}"
"#,
            &[],
        );
        assert!(config.servers.contains_key("good"));
        for name in ["reserved", "crlf", "badname", "missing"] {
            assert!(
                !config.servers.contains_key(name),
                "the invalid server `{name}` was loaded"
            );
            assert!(
                config
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(&format!("`{name}`"))),
                "{:?}",
                config.warnings
            );
        }
        let warnings = config.warnings.join("\n");
        assert!(warnings.contains("reserved"), "{warnings}");
        assert!(warnings.contains("cannot carry"), "{warnings}");
        assert!(warnings.contains("valid HTTP token"), "{warnings}");
        assert!(warnings.contains("NOT_SET_ANYWHERE"), "{warnings}");
        assert!(!warnings.contains("line\nInjected"), "{warnings}");
        assert!(!warnings.contains("line\n"), "{warnings}");
    }

    #[test]
    fn header_values_expand_exactly_once() {
        let config = parse_with_env(
            r#"
[servers.remote]
url = "https://example.test/mcp"

[servers.remote.headers]
X-Token = "${TOKEN}"
X-Literal = "$TOKEN"
"#,
            &[("TOKEN", "${OTHER}"), ("OTHER", "not-expanded")],
        );
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        let headers = &config.servers["remote"].headers;
        assert_eq!(headers["X-Token"], "${OTHER}");
        assert_eq!(headers["X-Literal"], "$TOKEN");
    }

    #[test]
    fn an_empty_header_variable_skips_the_server() {
        let config = parse_with_env(
            r#"
[servers.good]
command = "true"

[servers.empty]
url = "https://example.test/mcp"

[servers.empty.headers]
X-Token = "${EMPTY}"
"#,
            &[("EMPTY", "")],
        );
        assert!(config.servers.contains_key("good"));
        assert!(!config.servers.contains_key("empty"));
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("`empty`") && warning.contains("empty")),
            "{:?}",
            config.warnings
        );
    }

    #[test]
    fn header_values_never_appear_in_warnings() {
        let config = parse_with_env(
            r#"
[servers.remote]
url = "https://example.test/mcp"

[servers.remote.headers]
X-Secret = "value-with-secret"
X-Broken = "${ABSENT}"
"#,
            &[],
        );
        assert!(!config.servers.contains_key("remote"));
        let warnings = config.warnings.join("\n");
        assert!(warnings.contains("ABSENT"), "{warnings}");
        assert!(!warnings.contains("value-with-secret"), "{warnings}");
    }

    #[test]
    fn stdio_servers_ignore_headers_with_a_warning() {
        let config = parse_with_env(
            r#"
[servers.local]
command = "true"

[servers.local.headers]
X-Token = "secret"
"#,
            &[],
        );
        let server = &config.servers["local"];
        assert!(server.headers.is_empty());
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("`local`") && warning.contains("`headers`")),
            "{:?}",
            config.warnings
        );
    }
}
