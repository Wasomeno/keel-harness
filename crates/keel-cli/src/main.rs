use std::io::{self, BufRead, Write};

use keel_core::{Session, build_route};
use keel_llm_catalog::{Catalog, ModelSpec, Provider};
use keel_llm_route::Client;
use keel_llm_types::{
    LlmEvent, MaxTokensField, Message, ProtocolId, ReplayTarget, StopReason, repair_transcript,
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
        "route" => {
            let target = args.get(1).ok_or("usage: keel route <provider>/<model>")?;
            show_route(target)
        }
        "chat" => chat(&args[1..]).await,
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

fn show_route(target: &str) -> Result<(), String> {
    let (provider, model) = resolve(Catalog::builtin(), target)?;
    let route = build_route(provider, model).map_err(|error| error.to_string())?;
    let compat = route.protocol.compat();

    println!("provider   {}", route.provider);
    println!("model      {}", route.model);
    println!("protocol   {}", route.protocol_id());
    println!("endpoint   {}", route.endpoint);
    println!("auth       {}", route.auth.describe());
    println!("framing    {:?}", route.framing);
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
    if let Some(notes) = &provider.notes {
        println!("\nnote: {notes}");
    }
    Ok(())
}

#[derive(Default)]
struct ChatArgs {
    target: String,
    prompt: Option<String>,
    system: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
}

async fn chat(args: &[String]) -> Result<(), String> {
    let mut chat = ChatArgs::default();
    let mut iter = args.iter();

    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--prompt" | "-p" => {
                chat.prompt = Some(iter.next().ok_or("--prompt needs a value")?.clone());
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

    if chat.target.is_empty() {
        return Err("usage: keel chat <provider>/<model> [--prompt TEXT]".to_owned());
    }

    let (provider, model) = resolve(Catalog::builtin(), &chat.target)?;
    let route = build_route(provider, model).map_err(|error| error.to_string())?;
    let client = Client::new().map_err(|error| error.to_string())?;
    let mut session = Session::new(route, client);
    if let Some(system) = chat.system {
        session = session.with_system(system);
    }
    if let Some(max_tokens) = chat.max_tokens {
        session = session.with_max_output_tokens(max_tokens);
    }
    if let Some(temperature) = chat.temperature {
        session = session.with_temperature(temperature);
    }

    if let Some(prompt) = chat.prompt {
        return run_turn(&mut session, &prompt).await;
    }

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("> ");
        io::stdout().flush().map_err(|error| error.to_string())?;
        let Some(line) = lines.next() else {
            break;
        };
        let line = line.map_err(|error| error.to_string())?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "/exit" | "exit" | "quit") {
            break;
        }
        run_turn(&mut session, line).await?;
    }
    Ok(())
}

async fn run_turn(session: &mut Session, input: &str) -> Result<(), String> {
    let mut wrote_text = false;

    let outcome = session
        .step_streaming(Some(input), |event| match event {
            LlmEvent::TextDelta { delta, .. } => {
                print!("{delta}");
                let _ = io::stdout().flush();
                wrote_text = true;
            }
            LlmEvent::Warning(message) => eprintln!("warning: {message}"),
            LlmEvent::Error(error) => eprintln!("error: {error}"),
            _ => {}
        })
        .await
        .map_err(|error| error.to_string())?;

    if wrote_text {
        println!();
    }

    let usage = outcome.message.usage;
    println!(
        "[{}] in {} out {} cache read {} write {}",
        stop_label(outcome.message.stop),
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens
    );
    for note in &outcome.notes {
        eprintln!("repair: {note}");
    }
    if let Some(error) = &outcome.message.error {
        eprintln!("turn failed: {error}");
    }
    Ok(())
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

fn stop_label(stop: Option<StopReason>) -> &'static str {
    match stop {
        Some(StopReason::EndTurn) => "end",
        Some(StopReason::MaxTokens) => "max_tokens",
        Some(StopReason::ToolUse) => "tool_use",
        Some(StopReason::StopSequence) => "stop_sequence",
        Some(StopReason::Refusal) => "refusal",
        Some(StopReason::ContentFilter) => "content_filter",
        Some(StopReason::Aborted) => "aborted",
        Some(StopReason::Error) => "error",
        None => "unknown",
    }
}

fn print_help() {
    println!(
        "keel {} — provider seam for an LLM agent harness

usage:
  keel providers
  keel models [provider]
  keel route <provider>/<model>
  keel chat <provider>/<model> [--prompt TEXT] [--system TEXT]
                                  [--max-tokens N] [--temperature F]
  keel repair <messages.json> <provider>/<model>
  keel version | help

environment:
  provider keys       OPENAI_API_KEY, ANTHROPIC_API_KEY, OPENCODE_API_KEY,
                      COMMANDCODE_API_KEY, XAI_API_KEY, ...
  base URL override   KEEL_<PROVIDER>_BASE_URL
  custom endpoint     KEEL_CUSTOM_BASE_URL, KEEL_CUSTOM_API_KEY

The catalog is a seed until the generated catalog lands (open topic O-006).",
        env!("CARGO_PKG_VERSION")
    );
}
