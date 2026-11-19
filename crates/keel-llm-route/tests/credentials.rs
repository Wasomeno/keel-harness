//! The credential store, its resolver and the existing auth path.

use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use futures_util::StreamExt;
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{
    Auth, CREDENTIALS_VERSION, Client, CredentialError, CredentialRef, CredentialSource,
    CredentialStore, Framing, Route, SecretResolver, StoreResolver,
};
use keel_llm_types::{Compat, ErrorKind, LlmEvent, Message, ProtocolId, Request};
use keel_testkit::{Chunk, FauxProvider, Turn};

fn store_in(directory: &tempfile::TempDir) -> CredentialStore {
    CredentialStore::at(directory.path().join("credentials.json"))
}

fn read_store(store: &CredentialStore) -> serde_json::Value {
    let text = fs::read_to_string(store.path()).expect("read the credential file");
    serde_json::from_str(&text).expect("the credential file is JSON")
}

#[test]
fn a_store_round_trips_keys_and_values() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store_in(&directory);

    store.set("openai", "sk-test-openai").expect("set");
    store
        .set("anthropic.work", "sk-test-anthropic")
        .expect("set");
    assert_eq!(
        store.list().expect("list"),
        vec!["anthropic.work".to_owned(), "openai".to_owned()]
    );
    assert_eq!(
        store.get("openai").expect("get").as_deref(),
        Some("sk-test-openai")
    );
    assert!(store.remove("openai").expect("remove"));
    assert!(!store.remove("openai").expect("remove again"));
    assert_eq!(
        store.list().expect("list"),
        vec!["anthropic.work".to_owned()]
    );

    let reloaded = store_in(&directory);
    assert_eq!(
        reloaded.get("anthropic.work").expect("get").as_deref(),
        Some("sk-test-anthropic")
    );
}

#[test]
fn the_file_is_versioned_and_a_newer_version_is_refused() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store_in(&directory);
    store.set("openai", "sk-test-versioned").expect("set");

    let file = read_store(&store);
    assert_eq!(file["version"], CREDENTIALS_VERSION);
    assert_eq!(file["credentials"]["openai"], "sk-test-versioned");

    fs::write(
        store.path(),
        r#"{"version":99,"credentials":{"openai":"sk-test-versioned"}}"#,
    )
    .expect("write a future version");
    let error = store.list().expect_err("a newer version is refused");
    assert!(error.to_string().contains("version 99"), "{error}");
    assert!(!error.to_string().contains("sk-test-versioned"), "{error}");
}

#[test]
fn an_absent_file_is_an_empty_store_not_an_error() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store_in(&directory);
    assert!(store.list().expect("list").is_empty());
    assert_eq!(store.get("openai").expect("get"), None);

    let resolver = StoreResolver::new(store);
    assert_eq!(resolver.resolve(&CredentialRef::from_store("openai")), None);
    assert_eq!(
        resolver
            .resolve_result(&CredentialRef::from_store("openai"))
            .expect("an absent file is not a failure"),
        None
    );
}

#[test]
fn invalid_keys_and_empty_secrets_are_refused_before_writing() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store_in(&directory);

    let error = store.set("bad key", "sk-test").expect_err("invalid key");
    assert!(error.to_string().contains("bad key"), "{error}");
    let error = store.set("openai", "   ").expect_err("empty secret");
    assert!(error.to_string().contains("openai"), "{error}");
    assert!(!store.path().exists(), "a rejected write creates no file");
}

#[cfg(unix)]
#[test]
fn the_file_is_0600_and_the_created_directory_is_0700() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("temp dir");
    let path = root.path().join("private/credentials.json");
    let store = CredentialStore::at(&path);
    store.set("openai", "sk-test-permissions").expect("set");

    let file = fs::metadata(&path).expect("file metadata");
    assert_eq!(file.permissions().mode() & 0o777, 0o600);
    let parent = fs::metadata(path.parent().unwrap()).expect("directory metadata");
    assert_eq!(parent.permissions().mode() & 0o777, 0o700);
}

#[test]
fn replacing_the_file_is_atomic_for_readers() {
    use std::sync::atomic::AtomicBool;

    let directory = tempfile::tempdir().expect("temp dir");
    let store = store_in(&directory);
    store.set("openai", "0").expect("set");

    let stop = Arc::new(AtomicBool::new(false));
    let failures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let reader_store = store.clone();
    let reader_stop = stop.clone();
    let reader_failures = failures.clone();
    let reader = std::thread::spawn(move || {
        while !reader_stop.load(Ordering::Relaxed) {
            match reader_store.get("openai") {
                Ok(Some(value)) if value.parse::<u32>().is_ok() => {}
                Ok(other) => reader_failures
                    .lock()
                    .expect("failures")
                    .push(format!("unexpected value: {other:?}")),
                Err(error) => reader_failures
                    .lock()
                    .expect("failures")
                    .push(error.to_string()),
            }
        }
    });

    for value in 1..=200u32 {
        store
            .set("openai", &value.to_string())
            .expect("replace the file");
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().expect("reader");
    let failures = failures.lock().expect("failures");
    assert!(failures.is_empty(), "{failures:?}");

    let leftovers: Vec<_> = fs::read_dir(directory.path())
        .expect("read the directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(leftovers, vec!["credentials.json"]);
}

#[test]
fn a_corrupt_file_is_an_error_without_its_contents() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store_in(&directory);
    fs::write(
        store.path(),
        r#"{"version":1,"credentials":{"openai":"sk-test-corrupt"#,
    )
    .expect("write a truncated file");
    let resolver = StoreResolver::new(store);

    let error = resolver
        .resolve_result(&CredentialRef::from_store("openai"))
        .expect_err("a corrupt file is not a missing credential");
    assert_eq!(error.kind, ErrorKind::Auth);
    assert!(error.message.contains("not valid JSON"), "{error}");
    assert!(!error.message.contains("sk-test-corrupt"), "{error}");
}

struct CountingSource {
    key: String,
    value: String,
    loads: AtomicUsize,
}

impl CredentialSource for CountingSource {
    fn load(&self) -> Result<BTreeMap<String, String>, CredentialError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        Ok(BTreeMap::from([(self.key.clone(), self.value.clone())]))
    }
}

#[test]
fn concurrent_resolves_share_one_store_read() {
    const THREADS: usize = 16;

    let source = Arc::new(CountingSource {
        key: "openai".to_owned(),
        value: "sk-test-shared".to_owned(),
        loads: AtomicUsize::new(0),
    });
    let resolver = Arc::new(StoreResolver::with_source(source.clone()));
    let start = Arc::new(Barrier::new(THREADS + 1));

    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let resolver = resolver.clone();
        let start = start.clone();
        handles.push(std::thread::spawn(move || {
            start.wait();
            resolver.resolve(&CredentialRef::from_store("openai"))
        }));
    }
    start.wait();
    let values: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread"))
        .collect();

    assert_eq!(
        source.loads.load(Ordering::SeqCst),
        1,
        "concurrent misses must coalesce into one read"
    );
    assert!(
        values
            .iter()
            .all(|value| value.as_deref() == Some("sk-test-shared")),
        "{values:?}"
    );
}

#[test]
fn a_cached_key_stays_cached_until_a_reload() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store_in(&directory);
    store.set("openai", "sk-test-first").expect("set");
    let resolver = StoreResolver::new(store.clone());

    assert_eq!(
        resolver.resolve(&CredentialRef::from_store("openai")),
        Some("sk-test-first".to_owned())
    );
    store.set("openai", "sk-test-second").expect("set");
    assert_eq!(
        resolver.resolve(&CredentialRef::from_store("openai")),
        Some("sk-test-first".to_owned()),
        "a hit is served from the snapshot"
    );
    resolver.reload().expect("reload");
    assert_eq!(
        resolver.resolve(&CredentialRef::from_store("openai")),
        Some("sk-test-second".to_owned())
    );

    store.set("anthropic", "sk-test-new-key").expect("set");
    assert_eq!(
        resolver.resolve(&CredentialRef::from_store("anthropic")),
        Some("sk-test-new-key".to_owned()),
        "a miss reloads and sees a newly added key"
    );
}

#[test]
fn environment_wins_and_the_store_is_the_fallback() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store_in(&directory);
    store.set("openai", "sk-test-store").expect("set");
    let resolver = StoreResolver::new(store);

    let from_path = CredentialRef::from_env("PATH").with_store_key("openai");
    assert_eq!(
        resolver.resolve(&from_path),
        std::env::var("PATH").ok(),
        "an environment value wins over the store"
    );

    let missing_env = "KEEL_TEST_CREDENTIAL_NOT_SET_7A3F";
    assert!(std::env::var_os(missing_env).is_none());
    let fallback = CredentialRef::from_env(missing_env).with_store_key("openai");
    assert_eq!(
        resolver.resolve(&fallback),
        Some("sk-test-store".to_owned()),
        "a missing environment variable falls back to the store"
    );
}

#[test]
fn a_missing_credential_names_both_sources_and_no_value() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store_in(&directory);
    store.set("other", "sk-test-unrelated").expect("set");
    let resolver = StoreResolver::new(store);

    let credential =
        CredentialRef::from_env("KEEL_TEST_MISSING_OPENAI_KEY").with_store_key("openai");
    let auth = Auth::bearer(credential);
    let error = auth.apply(&resolver).expect_err("no credential is set");

    assert_eq!(error.kind, ErrorKind::Auth);
    assert!(
        error.message.contains("KEEL_TEST_MISSING_OPENAI_KEY"),
        "{error}"
    );
    assert!(error.message.contains("store key `openai`"), "{error}");
    assert!(!error.message.contains("sk-test-unrelated"), "{error}");
}

#[test]
fn debug_output_never_contains_a_secret() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = store_in(&directory);
    store.set("openai", "sk-test-debug-secret").expect("set");
    let resolver = StoreResolver::new(store.clone());
    assert_eq!(
        resolver.resolve(&CredentialRef::from_store("openai")),
        Some("sk-test-debug-secret".to_owned())
    );

    let text = format!("{store:?} {resolver:?}");
    assert!(!text.contains("sk-test-debug-secret"), "{text}");
}

fn answer_turn(text: &str) -> Turn {
    Turn::sse(vec![
        Chunk::text(format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\n",
            serde_json::json!(text)
        )),
        Chunk::text(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ),
        Chunk::text("data: [DONE]\n\n"),
    ])
}

fn route(endpoint: impl Into<String>) -> Route {
    Route {
        provider: "test".into(),
        model: "gpt-4.1".into(),
        endpoint: endpoint.into(),
        protocol: Arc::new(OpenAiChat::new(Compat::for_protocol(
            ProtocolId::OpenAiChat,
        ))),
        auth: Auth::bearer(CredentialRef::from_store("test")),
        framing: Framing::Sse,
        headers: Vec::new(),
        context_window: 0,
        max_output_tokens: 0,
    }
}

fn request() -> Request {
    Request::new("gpt-4.1").messages(vec![Message::user("hi")])
}

#[tokio::test]
async fn a_client_request_resolves_the_stored_secret() {
    let provider = FauxProvider::start(vec![answer_turn("ok")]).await;

    let directory = tempfile::tempdir().expect("temp dir");
    let store = store_in(&directory);
    store
        .set("test", "sk-test-through-the-client")
        .expect("store the secret");
    let client = Client::with_resolver(Arc::new(StoreResolver::new(store))).expect("client");

    let mut stream = client
        .stream(&route(provider.url()), &request())
        .await
        .expect("the request starts");
    while let Some(event) = stream.next().await {
        if matches!(event.expect("event"), LlmEvent::Done { .. }) {
            break;
        }
    }

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-test-through-the-client")
    );
    assert_eq!(
        requests[0].json()["messages"][0]["content"],
        "hi",
        "the normal request path is unchanged"
    );
    assert!(!format!("{client:?}").contains("sk-test-through-the-client"));
}

#[tokio::test]
async fn concurrent_client_requests_share_one_store_read() {
    let provider = FauxProvider::start(vec![answer_turn("one"), answer_turn("two")]).await;
    let source = Arc::new(CountingSource {
        key: "test".to_owned(),
        value: "sk-test-concurrent-client".to_owned(),
        loads: AtomicUsize::new(0),
    });
    let client = Arc::new(
        Client::with_resolver(Arc::new(StoreResolver::with_source(source.clone())))
            .expect("client"),
    );
    let route = Arc::new(route(provider.url()));
    let start = Arc::new(tokio::sync::Barrier::new(2));

    let mut handles = Vec::new();
    for _ in 0..2 {
        let client = client.clone();
        let route = route.clone();
        let start = start.clone();
        handles.push(tokio::spawn(async move {
            start.wait().await;
            let mut stream = client
                .stream(&route, &request())
                .await
                .expect("the request starts");
            while let Some(event) = stream.next().await {
                if matches!(event.expect("event"), LlmEvent::Done { .. }) {
                    break;
                }
            }
        }));
    }
    for handle in handles {
        handle.await.expect("request task");
    }

    assert_eq!(
        source.loads.load(Ordering::SeqCst),
        1,
        "two concurrent client requests read the store once"
    );
    let requests = provider.requests().await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.header("authorization")
                == Some("Bearer sk-test-concurrent-client")),
        "both requests share the resolved value"
    );
}
