use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use keel_core::{Harness, RunEvent, RunStop, Session, build_route};
use keel_llm_catalog::{Catalog, ModelSpec, Provider};
use keel_llm_route::{Client, CredentialStore, RetryPolicy, StoreResolver};
use keel_llm_types::{
    LlmEvent, MaxTokensField, Message, ProtocolId, ReplayTarget, Role, repair_transcript,
};
use keel_mcp::McpConfig;
use keel_tools::{
    PermissionDecision, PermissionGate, PermissionRequest, ToolRegistry, Workspace, builtin,
};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(message) = run(args).await {
        eprintln!("keel: {message}");
        std::process::exit(1);
    }
}

async fn run(args: Vec<String>) -> Result<(), String> {
    let Some(command) = args.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };

    match command {
        "providers" => list_providers(),
        "models" => list_models(args.get(1).map(String::as_str)),
        "tools" => list_tools(&args[1..]).await,
        "route" => show_route(&args[1..]),
        "chat" => chat(&args[1..]).await,
        "sessions" => sessions(&args[1..]),
        "auth" => auth(&args[1..]),
        "repair" => repair(&args[1..]),
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "version" | "-V" | "--version" => {
            println!("keel {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(format!("unknown command `{other}`; run `keel help`")),
    }
}

fn list_providers() -> Result<(), String> {
    let catalog = Catalog::builtin();
    println!(
        "{:<14} {:<30} {:>6}  {:<46} ENDPOINT",
        "PROVIDER", "NAME", "MODELS", "AUTH"
    );
    for provider in &catalog.providers {
        println!(
            "{:<14} {:<30} {:>6}  {:<46} {}",
            provider.id,
            provider.name,
            provider.models.len(),
            provider.auth.describe(),
            provider.base_url
        );
    }
    if let Some(notes) = &catalog.notes {
        println!("\n{notes}");
    }
    Ok(())
}

fn list_models(provider_filter: Option<&str>) -> Result<(), String> {
    let catalog = Catalog::builtin();
    if let Some(filter) = provider_filter {
        if catalog.provider(filter).is_none() {
            return Err(format!("unknown provider `{filter}`"));
        }
    }

    println!(
        "{:<36} {:<20} {:>9} {:>9}  {:<6} {:<6} {:<9} STATUS",
        "MODEL", "PROTOCOL", "CONTEXT", "MAX OUT", "TOOLS", "IMAGES", "REASONING"
    );
    for (provider, model) in catalog.models() {
        if let Some(filter) = provider_filter {
            if provider.id != filter {
                continue;
            }
        }
        println!(
            "{:<36} {:<20} {:>9} {:>9}  {:<6} {:<6} {:<9} {}",
            format!("{}/{}", provider.id, model.id),
            model.protocol.as_str(),
            model.context_window,
            model.max_output_tokens,
            yes_no(model.capabilities.tools),
            yes_no(model.capabilities.images),
            yes_no(model.capabilities.reasoning),
            protocol_status(model.protocol),
        );
    }
    println!("\ncontext and max output are token counts from the seed catalog.");
    Ok(())
}

async fn list_tools(args: &[String]) -> Result<(), String> {
    let mut workspace_path = PathBuf::from(".");
    let mut mcp = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--mcp" => mcp = true,
            "--workspace" | "-C" => {
                index += 1;
                workspace_path = PathBuf::from(args.get(index).ok_or(
                    "usage: keel tools [--mcp] [--workspace DIR]; --workspace needs a value",
                )?);
            }
            other => {
                return Err(format!(
                    "unknown argument `{other}`; usage: keel tools [--mcp] [--workspace DIR]"
                ));
            }
        }
        index += 1;
    }
    let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
    let mut registry = builtin::registry();
    let builtins: std::collections::BTreeSet<String> = registry
        .names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    add_mcp_tools(&mut registry, workspace.root(), mcp_requested(mcp)).await;

    println!(
        "{:<24} {:<10} {:<10} DESCRIPTION",
        "TOOL", "PARALLEL", "SOURCE"
    );
    for name in registry.names() {
        let tool = registry.get(name).expect("registered tool");
        let spec = tool.spec();
        let source = if builtins.contains(name) {
            "builtin"
        } else {
            "mcp"
        };
        println!(
            "{:<24} {:<10} {:<10} {}",
            spec.name,
            yes_no(tool.parallel_safe()),
            source,
            one_line(&spec.description, 120)
        );
    }
    println!("\n`search` requires the `rg` binary on PATH.");
    println!(
        "MCP servers are configured in `.keel/mcp.toml`; start them with `--mcp` or `KEEL_MCP=1`."
    );
    Ok(())
}

fn mcp_requested(flag: bool) -> bool {
    flag || std::env::var("KEEL_MCP").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}

/// Loads MCP tools when the user opted in, printing one warning per failed
/// server. Without the opt-in, config is only read and a single line says that
/// servers are configured but not started. A missing or malformed config yields
/// no MCP tools, never a CLI failure.
async fn add_mcp_tools(registry: &mut ToolRegistry, workspace: &std::path::Path, start: bool) {
    let config = match McpConfig::from_workspace(workspace) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("keel: warning: {error}; no MCP tools loaded");
            return;
        }
    };
    if !start {
        let configured = config
            .servers
            .values()
            .filter(|server| server.enabled)
            .count();
        if configured > 0 {
            eprintln!(
                "keel: MCP servers are configured but not started; pass --mcp or set KEEL_MCP=1 to start them"
            );
        }
        return;
    }
    let reserved = registry
        .names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let setup = config.connect(&reserved, Some(workspace)).await;
    for warning in setup.warnings {
        eprintln!("keel: warning: {warning}");
    }
    for tool in setup.tools {
        registry.register(tool);
    }
}

fn auth(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("set") => auth_set(&args[1..]),
        Some("remove" | "rm") => auth_remove(&args[1..]),
        Some("list" | "ls") => auth_list(&args[1..]),
        Some(other) => Err(format!(
            "unknown auth command `{other}`; usage: keel auth set <key> | remove <key> | list"
        )),
        None => Err("usage: keel auth set <key> | remove <key> | list".to_owned()),
    }
}

fn auth_set(args: &[String]) -> Result<(), String> {
    let key = args
        .first()
        .ok_or("usage: keel auth set <key> (the secret is read from stdin)")?;
    if args.len() > 1 {
        return Err(
            "the secret is read from stdin or a prompt; do not pass it on the command line"
                .to_owned(),
        );
    }
    let store = CredentialStore::discover().map_err(|error| error.to_string())?;
    CredentialStore::validate_key(key).map_err(|error| error.to_string())?;
    let secret = read_secret(key)?;
    store.set(key, &secret).map_err(|error| error.to_string())?;
    eprintln!(
        "keel: stored credential `{key}` in {}",
        store.path().display()
    );
    Ok(())
}

fn auth_remove(args: &[String]) -> Result<(), String> {
    let key = args.first().ok_or("usage: keel auth remove <key>")?;
    if args.len() > 1 {
        return Err("usage: keel auth remove <key>".to_owned());
    }
    let store = CredentialStore::discover().map_err(|error| error.to_string())?;
    if store.remove(key).map_err(|error| error.to_string())? {
        println!("removed credential `{key}`");
    } else {
        eprintln!("keel: credential `{key}` was not set");
    }
    Ok(())
}

fn auth_list(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("usage: keel auth list".to_owned());
    }
    let store = CredentialStore::discover().map_err(|error| error.to_string())?;
    let keys = store.list().map_err(|error| error.to_string())?;
    if keys.is_empty() {
        eprintln!("keel: no stored credentials in {}", store.path().display());
        return Ok(());
    }
    for key in &keys {
        println!("{key}");
    }
    eprintln!(
        "keel: {} credential(s) in {}",
        keys.len(),
        store.path().display()
    );
    Ok(())
}

fn read_secret(key: &str) -> Result<String, String> {
    use std::io::IsTerminal;

    let mut secret = if io::stdin().is_terminal() {
        rpassword::prompt_password(format!("secret for {key}: "))
            .map_err(|error| format!("read the secret from the terminal: {error}"))?
    } else {
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|error| format!("read the secret from stdin: {error}"))?;
        line
    };
    while secret.ends_with('\n') || secret.ends_with('\r') {
        secret.pop();
    }
    if secret.trim().is_empty() {
        return Err("the secret is empty".to_owned());
    }
    Ok(secret)
}

fn show_route(args: &[String]) -> Result<(), String> {
    let mut target: Option<String> = None;
    let mut workspace_path = PathBuf::from(".");
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" | "-C" => {
                index += 1;
                workspace_path = PathBuf::from(args.get(index).ok_or("--workspace needs a value")?);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}`"));
            }
            other => {
                if target.replace(other.to_owned()).is_some() {
                    return Err(format!("unexpected argument `{other}`"));
                }
            }
        }
        index += 1;
    }
    let target = target.ok_or("usage: keel route <provider>/<model> [--workspace DIR]")?;
    let (provider, model) = resolve(Catalog::builtin(), &target)?;
    let route = build_route(provider, model).map_err(|error| error.to_string())?;
    let compat = route.protocol.compat();

    println!("provider   {}", route.provider);
    println!("model      {}", route.model);
    println!("protocol   {}", route.protocol_id());
    println!("endpoint   {}", route.endpoint);
    println!("auth       {}", route.auth.describe());
    println!("framing    {:?}", route.framing);
    println!(
        "window     {} context, {} output",
        if route.context_window == 0 {
            "unknown".to_owned()
        } else {
            route.context_window.to_string()
        },
        if route.max_output_tokens == 0 {
            "unknown".to_owned()
        } else {
            route.max_output_tokens.to_string()
        }
    );
    println!(
        "max tokens {}",
        match compat.max_tokens_field {
            MaxTokensField::MaxTokens => "max_tokens",
            MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
        }
    );
    println!(
        "supports   tools={} images={} reasoning={}",
        compat.supports_tools, compat.supports_images, compat.supports_reasoning
    );
    match McpConfig::from_workspace(&workspace_path) {
        Ok(config) => {
            let enabled = config
                .servers
                .iter()
                .filter(|(_, server)| server.enabled)
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            println!(
                "mcp        {}",
                if enabled.is_empty() {
                    "none".to_owned()
                } else {
                    format!("{} (configured; not started)", enabled.join(", "))
                }
            );
        }
        Err(error) => println!("mcp        unavailable ({error})"),
    }
    if let Some(notes) = &provider.notes {
        println!("\nnote: {notes}");
    }
    Ok(())
}

struct ChatArgs {
    target: String,
    resume: Option<String>,
    prompt: Option<String>,
    system: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    workspace: PathBuf,
    yes: bool,
    mcp: bool,
    max_turns: usize,
    max_attempts: u32,
}

impl Default for ChatArgs {
    fn default() -> Self {
        Self {
            target: String::new(),
            resume: None,
            prompt: None,
            system: None,
            max_tokens: None,
            temperature: None,
            workspace: PathBuf::from("."),
            yes: false,
            mcp: false,
            max_turns: 24,
            max_attempts: 3,
        }
    }
}

async fn chat(args: &[String]) -> Result<(), String> {
    let mut chat = ChatArgs::default();
    let mut iter = args.iter();

    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--prompt" | "-p" => {
                chat.prompt = Some(iter.next().ok_or("--prompt needs a value")?.clone());
            }
            "--resume" => {
                chat.resume = Some(iter.next().ok_or("--resume needs a value")?.clone());
            }
            "--system" => {
                chat.system = Some(iter.next().ok_or("--system needs a value")?.clone());
            }
            "--max-tokens" => {
                chat.max_tokens = Some(
                    iter.next()
                        .ok_or("--max-tokens needs a value")?
                        .parse()
                        .map_err(|_| "--max-tokens must be a number".to_owned())?,
                );
            }
            "--temperature" => {
                chat.temperature = Some(
                    iter.next()
                        .ok_or("--temperature needs a value")?
                        .parse()
                        .map_err(|_| "--temperature must be a number".to_owned())?,
                );
            }
            "--workspace" | "-C" => {
                chat.workspace = PathBuf::from(iter.next().ok_or("--workspace needs a value")?);
            }
            "--max-turns" => {
                chat.max_turns = iter
                    .next()
                    .ok_or("--max-turns needs a value")?
                    .parse()
                    .map_err(|_| "--max-turns must be a number".to_owned())?;
            }
            "--max-attempts" => {
                chat.max_attempts = iter
                    .next()
                    .ok_or("--max-attempts needs a value")?
                    .parse()
                    .map_err(|_| "--max-attempts must be a number".to_owned())?;
                if chat.max_attempts == 0 {
                    return Err("--max-attempts must be at least 1".to_owned());
                }
            }
            "--yes" | "-y" => chat.yes = true,
            "--mcp" => chat.mcp = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}`"));
            }
            other => {
                if chat.target.is_empty() {
                    chat.target = other.to_owned();
                } else {
                    return Err(format!("unexpected argument `{other}`"));
                }
            }
        }
    }

    if chat.target.is_empty() && chat.resume.is_none() {
        return Err(
            "usage: keel chat <provider>/<model> | --resume last|<id> [--prompt TEXT] [--workspace DIR] [--mcp] [--yes]"
                .to_owned(),
        );
    }

    let workspace = Workspace::open(&chat.workspace).map_err(|error| error.to_string())?;
    let mut registry = builtin::registry();
    add_mcp_tools(&mut registry, workspace.root(), mcp_requested(chat.mcp)).await;
    let tools: Arc<ToolRegistry> = Arc::new(registry);

    let permissions = if chat.yes {
        PermissionGate::AllowAll
    } else {
        let (gate, receiver) = PermissionGate::ask();
        spawn_permission_prompter(receiver);
        gate
    };

    let data = keel_core::store::data_dir().ok_or("no data directory; set KEEL_DATA_DIR")?;
    let directory = keel_core::store::project_dir(&data, &chat.workspace);

    let resumed = match &chat.resume {
        Some(selector) => Some(find_session(&directory, selector)?),
        None => None,
    };
    if let Some(meta) = &resumed {
        if !chat.target.is_empty() {
            let (provider, model) = resolve(Catalog::builtin(), &chat.target)?;
            let asked = format!("{}/{}", provider.id, model.id);
            let stored = format!("{}/{}", meta.provider, meta.model);
            if asked != stored {
                eprintln!("keel: warning: the session runs {stored}; keeping it over {asked}");
            }
        }
    }
    let target = match &resumed {
        Some(meta) => format!("{}/{}", meta.provider, meta.model),
        None => chat.target.clone(),
    };
    if target.is_empty() {
        return Err("a session needs a provider/model to run against".to_owned());
    }

    let (provider, model) = resolve(Catalog::builtin(), &target)?;
    let route = build_route(provider, model).map_err(|error| error.to_string())?;
    let client = match CredentialStore::discover() {
        Ok(store) => Client::with_resolver(Arc::new(StoreResolver::new(store))),
        Err(error) => {
            eprintln!("keel: warning: {error}; using environment variables only");
            Client::new()
        }
    }
    .map_err(|error| error.to_string())?;

    let (mut session, log) = match resumed {
        Some(meta) => {
            let (log, meta, messages) = keel_core::store::SessionLog::open(&directory, &meta.id)
                .map_err(|error| error.to_string())?;
            let log = Arc::new(log);
            let mut session = Session::new(route, client).with_sink(log.clone());
            session.history = messages;
            // Resume starts from the recorded prompt, not from the default, so
            // the default is never prepended twice. `Session::new` already set
            // the default; the stored header wins, including `None` for a
            // session recorded before the default existed.
            session.system = meta.system.clone();
            // Request-shaping config is recorded in version-2 headers so a
            // resumed compacted session sends the same request. Command-line
            // flags still win and are applied below.
            session.max_output_tokens = meta.max_output_tokens;
            session.temperature = meta.temperature;
            if let Some(system) = &chat.system {
                session = session.with_system(system.clone());
            }
            eprintln!(
                "keel: resumed {} with {} messages ({})",
                meta.id,
                session.history.len(),
                log.path().display()
            );
            (session, log)
        }
        None => {
            let mut session = Session::new(route, client);
            if let Some(system) = &chat.system {
                session = session.with_system(system.clone());
            }
            if let Some(max_tokens) = chat.max_tokens {
                session = session.with_max_output_tokens(max_tokens);
            }
            if let Some(temperature) = chat.temperature {
                session = session.with_temperature(temperature);
            }
            let meta = keel_core::store::SessionMeta::new(
                keel_core::store::new_id(),
                session.route.provider.as_str(),
                session.route.model.as_str(),
                &session.route.protocol_id().to_string(),
            )
            .workspace(
                chat.workspace
                    .canonicalize()
                    .unwrap_or_else(|_| chat.workspace.clone())
                    .display()
                    .to_string(),
            )
            // Header text is the effective prompt exactly as this session
            // sends it, not just the text supplied with `--system`.
            .system(session.system.clone())
            .max_output_tokens(session.max_output_tokens)
            .temperature(session.temperature);
            let log = Arc::new(
                keel_core::store::SessionLog::create(&directory, &meta)
                    .map_err(|error| error.to_string())?,
            );
            eprintln!(
                "keel: session {} (resume with `keel chat --resume {}`)",
                meta.id, meta.id
            );
            (session.with_sink(log.clone()), log)
        }
    };

    if let Some(max_tokens) = chat.max_tokens {
        session = session.with_max_output_tokens(max_tokens);
    }
    if let Some(temperature) = chat.temperature {
        session = session.with_temperature(temperature);
    }
    if session.route.context_window == 0 {
        eprintln!("keel: compaction disabled: this route has no catalog context window");
    }
    session.retry.max_attempts = chat.max_attempts;
    let retry = RetryPolicy {
        max_attempts: chat.max_attempts,
        ..RetryPolicy::default()
    };

    let mut harness = Harness::new(session, tools, workspace)
        .with_permissions(permissions)
        .with_retry(retry)
        .with_max_turns(chat.max_turns);

    if let Some(prompt) = chat.prompt {
        let result = run_turn(&mut harness, &prompt).await;
        report_log_failure(&log);
        return result;
    }

    loop {
        print!("> ");
        io::stdout().flush().map_err(|error| error.to_string())?;
        let Some(line) = read_line() else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "/exit" | "exit" | "quit") {
            break;
        }
        run_turn(&mut harness, line).await?;
    }
    report_log_failure(&log);
    Ok(())
}

fn report_log_failure(log: &keel_core::SessionLog) {
    if let Some(failure) = log.failure() {
        eprintln!("keel: the session log could not be written: {failure}");
    }
    eprintln!("keel: session log {}", log.path().display());
}

fn find_session(
    directory: &std::path::Path,
    selector: &str,
) -> Result<keel_core::SessionMeta, String> {
    let sessions = keel_core::store::list(directory).map_err(|error| error.to_string())?;
    if selector == "last" {
        return sessions
            .into_iter()
            .next_back()
            .map(|summary| summary.meta)
            .ok_or_else(|| format!("no sessions in {}", directory.display()));
    }
    sessions
        .into_iter()
        .find(|summary| summary.meta.id == selector)
        .map(|summary| summary.meta)
        .ok_or_else(|| {
            format!(
                "no session `{selector}` in {}; run `keel sessions`",
                directory.display()
            )
        })
}

fn sessions(args: &[String]) -> Result<(), String> {
    let mut workspace = PathBuf::from(".");
    let mut show: Option<String> = None;
    let mut iter = args.iter();

    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--project" | "-C" => {
                workspace = PathBuf::from(iter.next().ok_or("--project needs a value")?);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}`"));
            }
            other if show.is_none() => show = Some(other.to_owned()),
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }

    let data = keel_core::store::data_dir().ok_or("no data directory; set KEEL_DATA_DIR")?;
    let directory = keel_core::store::project_dir(&data, &workspace);
    match show {
        Some(id) => show_session(&directory, &id),
        None => list_sessions(&directory),
    }
}

fn list_sessions(directory: &std::path::Path) -> Result<(), String> {
    let sessions = keel_core::store::list(directory).map_err(|error| error.to_string())?;
    if sessions.is_empty() {
        println!("no sessions in {}", directory.display());
        return Ok(());
    }
    println!(
        "{:<20} {:<20} {:<26} {:>8}  WORKSPACE",
        "ID", "CREATED", "MODEL", "MESSAGES"
    );
    for summary in sessions {
        println!(
            "{:<20} {:<20} {:<26} {:>8}  {}",
            summary.meta.id,
            keel_core::store::timestamp(summary.meta.created_at),
            format!("{}/{}", summary.meta.provider, summary.meta.model),
            summary.messages,
            summary.meta.workspace.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn show_session(directory: &std::path::Path, id: &str) -> Result<(), String> {
    let (meta, messages) = keel_core::store::read(&keel_core::store::path_for(directory, id))
        .map_err(|error| error.to_string())?;
    println!(
        "session   {}  {}",
        meta.id,
        keel_core::store::timestamp(meta.created_at)
    );
    println!(
        "model     {}/{} via {}",
        meta.provider, meta.model, meta.protocol
    );
    if let Some(workspace) = &meta.workspace {
        println!("workspace {workspace}");
    }
    if let Some(system) = &meta.system {
        println!("system    {}", one_line(system, 120));
    }
    for (index, message) in messages.iter().enumerate() {
        let results = message.tool_results().count();
        let mut marks = String::new();
        if message.has_tool_calls() {
            marks.push_str("[calls] ");
        }
        if results > 0 {
            marks.push_str(&format!("[{results} results] "));
        }
        println!(
            "{:>3} {:<9} {}{}",
            index + 1,
            role_label(&message.role),
            marks,
            one_line(&message.text(), 160)
        );
    }
    Ok(())
}

fn role_label(role: &Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    }
}

fn one_line(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        return flat;
    }
    let mut short: String = flat.chars().take(limit).collect();
    short.push('…');
    short
}

async fn run_turn(harness: &mut Harness, input: &str) -> Result<(), String> {
    let mut wrote_text = false;

    let result = harness
        .run(input, |event| match event {
            RunEvent::Model(LlmEvent::TextDelta { delta, .. }) => {
                print!("{delta}");
                let _ = io::stdout().flush();
                wrote_text = true;
            }
            RunEvent::Model(LlmEvent::Warning(message)) => eprintln!("warning: {message}"),
            RunEvent::Model(LlmEvent::Error(error)) => eprintln!("error: {error}"),
            RunEvent::Retrying {
                attempt,
                after,
                error,
            } => {
                if wrote_text {
                    println!();
                    wrote_text = false;
                }
                eprintln!(
                    "[retry] attempt {attempt} failed ({error}); retrying in {:.1}s",
                    after.as_secs_f64()
                );
            }
            RunEvent::ToolStarted { name, summary, .. } => {
                if wrote_text {
                    println!();
                    wrote_text = false;
                }
                eprintln!("[tool] {name}: {summary}");
            }
            RunEvent::ToolFinished {
                name,
                is_error,
                preview,
                ..
            } => {
                let label = if is_error { "error" } else { "ok" };
                eprintln!("[{name} {label}] {}", preview.replace('\n', " "));
            }
            RunEvent::Compacted {
                reason,
                before_messages,
                after_messages,
                kept_user_messages,
                estimated_tokens_before,
                estimated_tokens_after,
                ..
            } => {
                eprintln!(
                    "[compact] {}: {} -> {} messages ({} kept user), ~{} -> ~{} tokens",
                    reason.as_str(),
                    before_messages,
                    after_messages,
                    kept_user_messages,
                    estimated_tokens_before,
                    estimated_tokens_after
                );
            }
            _ => {}
        })
        .await;
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            // A failed run returns no outcome, but a summary request may
            // already have been paid for. The harness leaves that usage on
            // the session; print it before the error so it is not lost at
            // exit.
            let side_usage = harness.session_mut().take_compaction_usage();
            if !side_usage.is_zero() {
                eprintln!(
                    "[compact] summary usage paid before the run failed: in {} out {}",
                    side_usage.input_tokens, side_usage.output_tokens
                );
            }
            return Err(error.to_string());
        }
    };

    if wrote_text {
        println!();
    }

    let usage = outcome.usage;
    let compaction_usage = outcome.compaction_usage;
    if compaction_usage.is_zero() {
        println!(
            "[{}] turns {} in {} out {} cache read {} write {}",
            stop_label(outcome.stop),
            outcome.turns,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens
        );
    } else {
        println!(
            "[{}] turns {} in {} out {} cache read {} write {} | compact in {} out {}",
            stop_label(outcome.stop),
            outcome.turns,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            compaction_usage.input_tokens,
            compaction_usage.output_tokens
        );
    }

    if let Some(message) = &outcome.last {
        if let Some(error) = &message.error {
            eprintln!("turn failed: {error}");
        }
    }
    Ok(())
}

fn spawn_permission_prompter(mut receiver: tokio::sync::mpsc::Receiver<PermissionRequest>) {
    tokio::spawn(async move {
        while let Some(request) = receiver.recv().await {
            eprintln!();
            eprintln!("[permission] {}: {}", request.tool, request.summary);
            let answer = tokio::task::spawn_blocking(|| {
                eprint!("allow? [y/N] ");
                let _ = io::stderr().flush();
                let mut line = String::new();
                io::stdin()
                    .read_line(&mut line)
                    .map(|_| line)
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();

            let decision = if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                PermissionDecision::Allow
            } else {
                PermissionDecision::deny("denied by user")
            };
            request.respond(decision);
        }
    });
}

fn read_line() -> Option<String> {
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line),
        Err(_) => None,
    }
}

fn repair(args: &[String]) -> Result<(), String> {
    let path = args
        .first()
        .ok_or("usage: keel repair <messages.json> <provider>/<model>")?;
    let target_arg = args
        .get(1)
        .ok_or("usage: keel repair <messages.json> <provider>/<model>")?;

    let (provider, model) = resolve(Catalog::builtin(), target_arg)?;
    let text = std::fs::read_to_string(path).map_err(|error| format!("read {path}: {error}"))?;
    let messages: Vec<Message> =
        serde_json::from_str(&text).map_err(|error| format!("parse {path}: {error}"))?;

    let target = ReplayTarget::new(model.protocol, provider.id.clone(), model.id.clone())
        .with_images(model.capabilities.images);
    let outcome = repair_transcript(messages, &target);

    let json =
        serde_json::to_string_pretty(&outcome.messages).map_err(|error| error.to_string())?;
    println!("{json}");
    for note in &outcome.notes {
        eprintln!("repair: {note}");
    }
    Ok(())
}

fn resolve<'a>(
    catalog: &'a Catalog,
    target: &str,
) -> Result<(&'a Provider, &'a ModelSpec), String> {
    if let Some((provider_id, model_id)) = target.split_once('/') {
        return catalog.find(provider_id, model_id).ok_or_else(|| {
            if catalog.provider(provider_id).is_some() {
                format!("provider `{provider_id}` has no model `{model_id}`")
            } else {
                format!("unknown provider `{provider_id}`")
            }
        });
    }

    let matches = catalog.find_model(target);
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(format!(
            "unknown model `{target}`; run `keel models` to list models"
        )),
        _ => {
            let list = matches
                .iter()
                .map(|(provider, model)| format!("{}/{}", provider.id, model.id))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!("model `{target}` is ambiguous: {list}"))
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn protocol_status(protocol: ProtocolId) -> &'static str {
    match protocol {
        ProtocolId::OpenAiChat | ProtocolId::AnthropicMessages => "ready",
        ProtocolId::OpenAiResponses => "phase 2",
        ProtocolId::GoogleGenerative => "phase 3",
    }
}

fn stop_label(stop: RunStop) -> &'static str {
    match stop {
        RunStop::EndTurn => "end",
        RunStop::MaxTurns => "max_turns",
        RunStop::Cancelled => "cancelled",
        RunStop::Failed => "failed",
    }
}

fn print_help() {
    println!(
        "keel {} — provider seam and agent loop for an LLM harness

usage:
  keel providers
  keel models [provider]
  keel tools [--mcp] [--workspace DIR]
  keel route <provider>/<model> [--workspace DIR]
  keel chat <provider>/<model> [--resume last|ID]
                                  [--prompt TEXT] [--system APPEND]
                                  [--workspace DIR] [--mcp] [--max-turns N]
                                  [--max-attempts N] [--max-tokens N]
                                  [--temperature F] [--yes]
  keel sessions [--project DIR] [ID]
  keel auth set <key>
  keel auth remove <key>
  keel auth list
  keel repair <messages.json> <provider>/<model>
  keel version | help

chat:
  Tools run in the workspace (default: the current directory). Every tool call
  asks for permission unless --yes is passed. `search` needs the `rg` binary.
  Every session starts with keel's default system prompt; --system APPEND adds
  your text after it instead of replacing it. A resumed session keeps the prompt
  recorded in its header, and --system appends to that. A turn that fails after
  output arrives is replayed up to --max-attempts times (default 3) when the
  failure is retryable; the failed attempt stays in the log and repair keeps it
  off the wire.

  Before a request would exceed the model's catalog context window, keel
  compacts the session: it asks the same model, with no tools, for a handoff
  summary, then continues with the recent user messages plus that summary. A
  provider context-overflow error gets one compaction and one retry. One
  [compact] status line is printed on stderr.

sessions:
  Every run is logged as JSONL under the keel data directory, in a directory per
  project. `keel chat --resume last` continues the newest one, and a resumed
  session keeps the provider and model it was recorded with.

mcp:
  MCP is off by default. `--mcp` or KEEL_MCP=1 starts the enabled servers in
  `.keel/mcp.toml`; without it keel lists no MCP tools. A server command runs
  with your full privileges, outside the workspace sandbox, equivalent to
  `bash`, so only opt in for a workspace you trust. Keel registers tools as
  `server__tool` and gates calls like built-ins. A missing or crashing server is
  a warning and contributes no tools. MCP calls are barriers unless the server
  sets parallel_safe = true. The env allowlist is hygiene, not a security
  boundary; args are free-form, so never put secrets in them.

auth:
  A credential is resolved from the environment first, then from the credential
  store. `keel auth set <key>` reads the secret from stdin, or prompts without
  echo on a terminal; never pass it as an argument. `keel auth list` prints keys
  only. The store lives at KEEL_CREDENTIALS when set, otherwise the platform
  config directory; on Unix the file is 0600 inside a 0700 directory. The
  catalog's store key is the provider id (`openai`, `anthropic`, ...).

environment:
  provider keys       OPENAI_API_KEY, ANTHROPIC_API_KEY, OPENCODE_API_KEY,
                      COMMANDCODE_API_KEY, XAI_API_KEY, ...
  base URL override   KEEL_<PROVIDER>_BASE_URL
  custom endpoint     KEEL_CUSTOM_BASE_URL, KEEL_CUSTOM_API_KEY
  credential store    KEEL_CREDENTIALS (default: platform config directory)
  data directory      KEEL_DATA_DIR (default: the platform data directory)
  start MCP servers   KEEL_MCP=1 (same as --mcp)

The catalog is a seed until the generated catalog lands (open topic O-006).",
        env!("CARGO_PKG_VERSION")
    );
}
