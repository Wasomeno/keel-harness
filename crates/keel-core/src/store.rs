//! Session logs.
//!
//! A session is one append-only JSONL file: a header naming the route and the
//! workspace, then one record per canonical message. The log is the transcript;
//! the in-memory [`Session`](crate::Session) is a view rebuilt from it, so a
//! crash loses at most the turn in flight.
//!
//! Files live outside the repository, under the platform data directory, in a
//! directory derived from the project path. Nothing is written unless the caller
//! asks for a log.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use keel_llm_types::{CachePolicy, Message};
use serde::{Deserialize, Serialize};

use crate::compact::CompactionReason;
use crate::test_loop::TestStatus;

/// Bumped when a record changes shape. A file from a newer version is refused
/// rather than half-read.
///
/// Version 2 adds the `compaction` record and optional request-shaping fields
/// on the header. Version-1 files still load: they simply have no compaction
/// records, and the new header fields default to `None`.
pub const LOG_VERSION: u32 = 2;

const HEADER_EXTENSION: &str = "jsonl";

/// Who the session talks to and where it runs. Written as the first line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: u32,
    pub id: String,
    pub created_at: u64,
    pub provider: String,
    pub model: String,
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Request-shaping config, recorded so resume sends the same request the
    /// live session did. Absent in version-1 files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Cache policy recorded so a resumed session keeps an opt-out instead of
    /// silently returning to `Auto`. Absent in older files and treated as
    /// `Auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CachePolicy>,
}

impl SessionMeta {
    pub fn new(id: impl Into<String>, provider: &str, model: &str, protocol: &str) -> Self {
        Self {
            version: LOG_VERSION,
            id: id.into(),
            created_at: now(),
            provider: provider.to_owned(),
            model: model.to_owned(),
            protocol: protocol.to_owned(),
            workspace: None,
            system: None,
            max_output_tokens: None,
            temperature: None,
            cache: None,
        }
    }

    pub fn workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    pub fn system(mut self, system: Option<String>) -> Self {
        self.system = system;
        self
    }

    pub fn max_output_tokens(mut self, tokens: Option<u32>) -> Self {
        self.max_output_tokens = tokens;
        self
    }

    pub fn temperature(mut self, temperature: Option<f32>) -> Self {
        self.temperature = temperature;
        self
    }

    pub fn cache(mut self, cache: Option<CachePolicy>) -> Self {
        self.cache = cache;
        self
    }
}

/// The replacement view produced by one compaction.
///
/// The log stores `summary` and the full `messages` view. The summary message
/// is also the last element of `messages`; keeping it separately makes the
/// record readable without assuming that shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub reason: CompactionReason,
    pub summary: Message,
    pub messages: Vec<Message>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record {
    Meta(SessionMeta),
    Message {
        at: u64,
        message: Message,
    },
    Compaction {
        at: u64,
        reason: CompactionReason,
        summary: Message,
        messages: Vec<Message>,
    },
    /// One visible-test update. It is an auxiliary record: resume ignores it
    /// for history, and readers that only need messages skip it.
    TestStatus {
        at: u64,
        status: TestStatus,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: line {line}: {source}")]
    Json {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path}: the first line is not a session header")]
    MissingHeader { path: PathBuf },
    #[error("{path}: session version {version} is newer than {expected}")]
    Version {
        path: PathBuf,
        version: u32,
        expected: u32,
    },
    #[error("no sessions in {0}")]
    Empty(PathBuf),
    #[error("the data directory is unknown; set KEEL_DATA_DIR")]
    NoDataDir,
}

fn io(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// The file that holds one session.
pub fn path_for(directory: &Path, id: &str) -> PathBuf {
    directory.join(format!("{id}.{HEADER_EXTENSION}"))
}

/// An open log for appending. Failures are remembered rather than raised: a
/// turn should not fail because the disk is full, but the caller can report it.
#[derive(Debug)]
pub struct SessionLog {
    path: PathBuf,
    file: Mutex<File>,
    failure: Mutex<Option<String>>,
}

impl SessionLog {
    /// Creates a new log in `directory`, writing the header first.
    pub fn create(directory: &Path, meta: &SessionMeta) -> Result<Self, StoreError> {
        fs::create_dir_all(directory).map_err(|error| io(directory, error))?;
        let path = directory.join(format!("{}.{HEADER_EXTENSION}", meta.id));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| io(&path, error))?;
        let log = Self {
            path: path.clone(),
            file: Mutex::new(file),
            failure: Mutex::new(None),
        };
        log.write(&Record::Meta(meta.clone()))?;
        Ok(log)
    }

    /// Opens an existing log for appending and returns what it holds.
    pub fn open(
        directory: &Path,
        id: &str,
    ) -> Result<(Self, SessionMeta, Vec<Message>), StoreError> {
        let path = path_for(directory, id);
        let (meta, messages) = read(&path)?;
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|error| io(&path, error))?;
        let log = Self {
            path,
            file: Mutex::new(file),
            failure: Mutex::new(None),
        };
        Ok((log, meta, messages))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The first write failure, if one happened. Appending keeps the session
    /// usable; the caller decides whether a failed log is fatal.
    pub fn failure(&self) -> Option<String> {
        self.lock_failure().clone()
    }

    pub fn append(&self, message: &Message) -> Result<(), StoreError> {
        self.write(&Record::Message {
            at: now(),
            message: message.clone(),
        })
    }

    /// Appends one compaction record. Earlier bytes are never rewritten.
    pub fn append_compaction(&self, record: &CompactionRecord) -> Result<(), StoreError> {
        self.write(&Record::Compaction {
            at: now(),
            reason: record.reason,
            summary: record.summary.clone(),
            messages: record.messages.clone(),
        })
    }

    /// Appends the latest visible-test status. Earlier bytes are unchanged.
    pub fn append_test_status(&self, status: &TestStatus) -> Result<(), StoreError> {
        self.write(&Record::TestStatus {
            at: now(),
            status: status.clone(),
        })
    }

    fn write(&self, record: &Record) -> Result<(), StoreError> {
        let mut line = serde_json::to_string(record).map_err(|source| StoreError::Json {
            path: self.path.clone(),
            line: 0,
            source,
        })?;
        line.push('\n');
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Flush per record: a crash then loses only the turn in flight.
        file.write_all(line.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|error| io(&self.path, error))
    }

    fn lock_failure(&self) -> MutexGuard<'_, Option<String>> {
        self.failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn note_failure(&self, error: StoreError) {
        let mut failure = self.lock_failure();
        if failure.is_none() {
            *failure = Some(error.to_string());
        }
    }

    fn record(&self, message: &Message) {
        if let Err(error) = self.append(message) {
            self.note_failure(error);
        }
    }
}

impl crate::session::SessionSink for SessionLog {
    fn message(&self, message: &Message) {
        self.record(message);
    }

    fn compaction(&self, record: &CompactionRecord) {
        if let Err(error) = self.append_compaction(record) {
            self.note_failure(error);
        }
    }

    fn test_status(&self, status: &TestStatus) {
        if let Err(error) = self.append_test_status(status) {
            self.note_failure(error);
        }
    }
}

/// Reads a log without opening it for writing.
pub fn read(path: &Path) -> Result<(SessionMeta, Vec<Message>), StoreError> {
    let file = File::open(path).map_err(|error| io(path, error))?;
    let lines: Vec<String> = BufReader::new(file)
        .lines()
        .collect::<Result<_, _>>()
        .map_err(|error| io(path, error))?;
    let numbered: Vec<(usize, &String)> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| (index + 1, line))
        .filter(|(_, line)| !line.trim().is_empty())
        .collect();

    let Some((_, header)) = numbered.first() else {
        return Err(StoreError::MissingHeader {
            path: path.to_path_buf(),
        });
    };
    let Record::Meta(meta) =
        serde_json::from_str::<Record>(header).map_err(|source| StoreError::Json {
            path: path.to_path_buf(),
            line: 1,
            source,
        })?
    else {
        return Err(StoreError::MissingHeader {
            path: path.to_path_buf(),
        });
    };
    if meta.version > LOG_VERSION {
        return Err(StoreError::Version {
            path: path.to_path_buf(),
            version: meta.version,
            expected: LOG_VERSION,
        });
    }

    let last = numbered.last().map(|(line, _)| *line);
    let mut messages = Vec::new();
    for (line, text) in numbered.iter().skip(1) {
        match serde_json::from_str::<Record>(text) {
            Ok(Record::Message { message, .. }) => messages.push(message),
            // A compaction record replaces everything before it. Messages
            // after it append to that view, so the result is the latest
            // compaction plus the messages that followed.
            Ok(Record::Compaction {
                summary,
                messages: view,
                ..
            }) => {
                messages = if view.is_empty() { vec![summary] } else { view };
            }
            // Auxiliary state, not history.
            Ok(Record::TestStatus { .. }) => {}
            Ok(Record::Meta(_)) => {
                return Err(StoreError::Json {
                    path: path.to_path_buf(),
                    line: *line,
                    source: serde_json::Error::io(std::io::Error::other("a second session header")),
                });
            }
            // A crash can leave a half-written final line. Anything earlier is
            // real corruption and is reported.
            Err(error) => {
                if Some(*line) == last {
                    break;
                }
                return Err(StoreError::Json {
                    path: path.to_path_buf(),
                    line: *line,
                    source: error,
                });
            }
        }
    }

    Ok((meta, messages))
}

/// One row of `keel sessions`.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    pub meta: SessionMeta,
    pub messages: usize,
}

impl SessionSummary {
    pub fn path(&self, directory: &Path) -> PathBuf {
        directory.join(format!("{}.{HEADER_EXTENSION}", self.meta.id))
    }
}

/// Every session in `directory`, oldest first, skipping files that do not parse
/// so one damaged log does not hide the rest.
pub fn list(directory: &Path) -> Result<Vec<SessionSummary>, StoreError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io(directory, error)),
    };

    let mut summaries = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| io(directory, error))?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some(HEADER_EXTENSION) {
            continue;
        }
        let Ok((meta, messages)) = read(&path) else {
            continue;
        };
        summaries.push(SessionSummary {
            meta,
            messages: messages.len(),
        });
    }
    summaries.sort_by(|left, right| {
        (left.meta.created_at, &left.meta.id).cmp(&(right.meta.created_at, &right.meta.id))
    });
    Ok(summaries)
}

/// The most recent session in `directory`.
pub fn latest(directory: &Path) -> Result<SessionSummary, StoreError> {
    list(directory)?
        .pop()
        .ok_or_else(|| StoreError::Empty(directory.to_path_buf()))
}

/// A fresh, sortable id: seconds since the epoch plus four hex digits.
pub fn new_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:010}-{:04x}",
        now.as_secs(),
        (now.subsec_nanos() >> 8) & 0xffff
    )
}

/// `YYYY-MM-DD HH:MM:SSZ` for an epoch second. Session ids are opaque, so the
/// listing formats the seconds it already stores rather than pulling in a date
/// crate for one line of output.
pub fn timestamp(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let remainder = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        remainder / 3600,
        (remainder % 3600) / 60,
        remainder % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since the epoch to a calendar date.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 { year + 1 } else { year },
        month as u64,
        day as u64,
    )
}

/// The platform data directory, overridable with `KEEL_DATA_DIR`.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os("KEEL_DATA_DIR") {
        return Some(PathBuf::from(directory));
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/keel"))
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|appdata| PathBuf::from(appdata).join("keel"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .map(|base| base.join("keel"))
    }
}

/// The session directory for one project. The path is canonicalized when it can
/// be, so a relative `-C` and an absolute one land in the same place.
pub fn project_dir(data_dir: &Path, project: &Path) -> PathBuf {
    let canonical = project
        .canonicalize()
        .unwrap_or_else(|_| project.to_path_buf());
    let text = canonical.to_string_lossy();
    let slug: String = text
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = slug.trim_matches('-');
    let tail = &trimmed[trimmed.len().saturating_sub(48)..];
    data_dir
        .join("sessions")
        .join(format!("{tail}-{:05x}", hash(text.as_bytes())))
}

fn hash(bytes: &[u8]) -> u32 {
    // FNV-1a: stable across runs and platforms, which the directory name needs.
    let mut value: u32 = 0x811c_9dc5;
    for byte in bytes {
        value ^= u32::from(*byte);
        value = value.wrapping_mul(0x0100_0193);
    }
    value & 0x000f_ffff
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_llm_types::{CachePolicy, Message, Part};

    fn meta(id: &str) -> SessionMeta {
        SessionMeta::new(id, "neuralwatt", "nw-small", "openai-chat")
    }

    #[test]
    fn a_log_round_trips_its_header_and_messages() {
        let directory = tempfile::tempdir().expect("temp dir");
        let log = SessionLog::create(directory.path(), &meta("0000000001-aa00")).expect("create");
        log.append(&Message::user("hello")).expect("append");
        log.append(&Message::assistant(vec![Part::text("hi")]))
            .expect("append");

        let (loaded, messages) = read(log.path()).expect("read");
        assert_eq!(loaded.id, "0000000001-aa00");
        assert_eq!(loaded.workspace, None);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text(), "hello");
        assert_eq!(messages[1].text(), "hi");
        assert_eq!(log.failure(), None);
    }

    #[test]
    fn a_cache_policy_round_trips_in_the_header() {
        let directory = tempfile::tempdir().expect("temp dir");
        let header = meta("0000000009-cc00").cache(Some(CachePolicy::None));
        let log = SessionLog::create(directory.path(), &header).expect("create");
        log.append(&Message::user("hello")).expect("append");

        let (loaded, _) = read(log.path()).expect("read");
        assert_eq!(loaded.cache, Some(CachePolicy::None));
    }

    #[test]
    fn a_test_status_record_is_auxiliary_and_leaves_history_unchanged() {
        let directory = tempfile::tempdir().expect("temp dir");
        let log = SessionLog::create(directory.path(), &meta("0000000008-dd00")).expect("create");
        log.append(&Message::user("go")).expect("append");
        log.append_test_status(&TestStatus {
            runs: 1,
            last_command: Some("python -m pytest".to_owned()),
            last_outcome: Some(crate::test_loop::TestOutcome::Failed),
            failing_tests: vec!["tests/test_app.py::test_value".to_owned()],
            excerpt: Some("assert 1 == 2".to_owned()),
        })
        .expect("append test status");
        log.append(&Message::user("after")).expect("append");

        let (_, messages) = read(log.path()).expect("read");
        assert_eq!(
            messages.iter().map(Message::text).collect::<Vec<_>>(),
            vec!["go", "after"]
        );
        let text = std::fs::read_to_string(log.path()).expect("read file");
        assert!(text.contains(r#""type":"test_status""#), "{text}");
        assert!(text.contains("tests/test_app.py::test_value"), "{text}");
    }

    #[test]
    fn a_half_written_last_line_is_dropped() {
        let directory = tempfile::tempdir().expect("temp dir");
        let log = SessionLog::create(directory.path(), &meta("0000000002-bb00")).expect("create");
        log.append(&Message::user("kept")).expect("append");

        let mut file = OpenOptions::new()
            .append(true)
            .open(log.path())
            .expect("open");
        file.write_all(br#"{"type":"message","at":1,"message":{"rol"#)
            .expect("write a torn line");
        drop(file);

        let (_, messages) = read(log.path()).expect("a torn tail is not an error");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text(), "kept");
    }

    #[test]
    fn corruption_before_the_last_line_is_reported() {
        let directory = tempfile::tempdir().expect("temp dir");
        let log = SessionLog::create(directory.path(), &meta("0000000003-cc00")).expect("create");
        log.append(&Message::user("first")).expect("append");
        log.append(&Message::user("second")).expect("append");

        let path = log.path();
        let text = fs::read_to_string(path).expect("read");
        let mut lines: Vec<&str> = text.lines().collect();
        lines[1] = "{ not json";
        fs::write(path, lines.join("\n") + "\n").expect("write");

        let error = read(path).expect_err("corruption is reported");
        assert!(error.to_string().contains("line 2"), "{error}");
    }

    #[test]
    fn list_orders_sessions_and_latest_picks_the_newest() {
        let directory = tempfile::tempdir().expect("temp dir");
        for (id, created_at) in [("0000000005-ee00", 5), ("0000000004-dd00", 4)] {
            let mut header = meta(id);
            header.created_at = created_at;
            let log = SessionLog::create(directory.path(), &header).expect("create");
            log.append(&Message::user("hi")).expect("append");
        }

        let sessions = list(directory.path()).expect("list");
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.meta.id.as_str())
                .collect::<Vec<_>>(),
            ["0000000004-dd00", "0000000005-ee00"]
        );
        assert_eq!(sessions[0].messages, 1);
        assert_eq!(
            latest(directory.path()).expect("latest").meta.id,
            "0000000005-ee00"
        );
    }

    #[test]
    fn an_empty_directory_has_no_latest_session() {
        let directory = tempfile::tempdir().expect("temp dir");
        let error = latest(directory.path()).expect_err("nothing to resume");
        assert!(matches!(error, StoreError::Empty(_)), "{error}");
    }

    #[test]
    fn a_newer_log_version_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut header = meta("0000000006-ff00");
        header.version = LOG_VERSION + 1;
        let log = SessionLog::create(directory.path(), &header).expect("create");
        log.append(&Message::user("hi")).expect("append");

        let error = read(log.path()).expect_err("a newer version is refused");
        assert!(error.to_string().contains("newer"), "{error}");
    }

    #[test]
    fn a_version_1_log_still_loads() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("version1.jsonl");
        let header = "{\"type\":\"meta\",\"version\":1,\"id\":\"v1\",\"created_at\":1,\
                      \"provider\":\"test\",\"model\":\"gpt-4.1\",\"protocol\":\"openai-chat\"}\n";
        let message = serde_json::to_string(&Message::user("from the old log")).expect("message");
        let line = format!("{{\"type\":\"message\",\"at\":2,\"message\":{message}}}\n");
        fs::write(&path, format!("{header}{line}")).expect("write");

        let (loaded, messages) = read(&path).expect("a version-1 log loads");
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.max_output_tokens, None);
        assert_eq!(loaded.temperature, None);
        assert_eq!(loaded.cache, None);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text(), "from the old log");
    }

    #[test]
    fn a_compaction_record_replaces_the_view_and_later_messages_append() {
        let directory = tempfile::tempdir().expect("temp dir");
        let log = SessionLog::create(directory.path(), &meta("0000000007-aa00")).expect("create");
        log.append(&Message::user("old one")).expect("append");
        log.append(&Message::user("old two")).expect("append");

        let bytes_before = fs::read(log.path()).expect("read before");
        let summary = crate::compact::summary_message("handoff");
        let record = CompactionRecord {
            reason: CompactionReason::Threshold,
            summary: summary.clone(),
            messages: vec![Message::user("old two"), summary.clone()],
        };
        log.append_compaction(&record).expect("append compaction");
        log.append(&Message::user("after")).expect("append");

        let bytes_after = fs::read(log.path()).expect("read after");
        assert!(
            bytes_after.starts_with(&bytes_before),
            "the file only grew; earlier bytes are unchanged"
        );

        let (_, messages) = read(log.path()).expect("read");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].text(), "old two");
        assert!(
            messages[1]
                .text()
                .starts_with(crate::compact::SUMMARY_PREFIX)
        );
        assert_eq!(messages[2].text(), "after");
        assert_eq!(log.failure(), None);
    }

    #[test]
    fn the_latest_compaction_record_wins() {
        let directory = tempfile::tempdir().expect("temp dir");
        let log = SessionLog::create(directory.path(), &meta("0000000008-bb00")).expect("create");
        log.append(&Message::user("first")).expect("append");
        let first_summary = crate::compact::summary_message("first handoff");
        log.append_compaction(&CompactionRecord {
            reason: CompactionReason::Threshold,
            summary: first_summary.clone(),
            messages: vec![first_summary],
        })
        .expect("append");
        log.append(&Message::user("middle")).expect("append");
        let second_summary = crate::compact::summary_message("second handoff");
        log.append_compaction(&CompactionRecord {
            reason: CompactionReason::Overflow,
            summary: second_summary.clone(),
            messages: vec![Message::user("middle"), second_summary],
        })
        .expect("append");
        log.append(&Message::user("last")).expect("append");

        let (_, messages) = read(log.path()).expect("read");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].text(), "middle");
        assert!(messages[1].text().contains("second handoff"));
        assert_eq!(messages[2].text(), "last");
    }

    #[test]
    fn timestamps_render_utc() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00Z");
        assert_eq!(timestamp(1_700_000_000), "2023-11-14 22:13:20Z");
        // Matches the `date` header NeuralWatt sent with that `created` value.
        assert_eq!(timestamp(1_795_022_254), "2026-11-18 17:17:34Z");
    }

    #[test]
    fn the_project_directory_is_stable_and_separates_projects() {
        let data = Path::new("/tmp/keel-data");
        let first = project_dir(data, Path::new("/tmp/one"));
        assert_eq!(first, project_dir(data, Path::new("/tmp/one")));
        assert_ne!(first, project_dir(data, Path::new("/tmp/two")));
        assert!(first.starts_with(data.join("sessions")));
        assert!(
            first
                .file_name()
                .and_then(|name| name.to_str())
                .expect("a name")
                .len()
                <= 48 + 6
        );
    }
}
