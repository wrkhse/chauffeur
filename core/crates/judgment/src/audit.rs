//! The judgment audit log: one bounded JSON record per line.
//!
//! Append and recent adapt wrkhse/chauffeur origin/v2
//! crates/daemon/src/audit.rs at 2273807, introduced at 0eb45c0.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Judgment, MAX_TEXT_BYTES, Request};

/// The default size at which appending rotates the current log.
pub const MAX_AUDIT_BYTES: u64 = 8 * 1024 * 1024;
/// The most bytes in a JSON record, including its line feed.
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;
/// The most records a recent read returns.
pub const MAX_RECENT: usize = 1024;

const CURRENT: &str = "audit.jsonl";
const ROTATED: &str = "audit.1.jsonl";
const SECOND_DIGITS: usize = 20;
const MICROSECOND_DIGITS: usize = 6;

/// Why an audit record is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditError {
    /// The record is malformed.
    Invalid(String),
    /// A part of the record exceeds its limit.
    Limit {
        /// The part that exceeds the limit.
        name: &'static str,
        /// The limit in bytes.
        max: u64,
    },
}

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid audit record: {message}"),
            Self::Limit { name, max } => write!(f, "{name} exceeds {max}"),
        }
    }
}

impl std::error::Error for AuditError {}

/// The request context and the exact judgment as serialized for the caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "AuditRecordFile")]
pub struct AuditRecord {
    at: String,
    request_id: String,
    session: String,
    hook: String,
    judgment: Value,
}

impl AuditRecord {
    /// Records a request and its judgment at the current time.
    ///
    /// # Errors
    /// Returns an error if the clock precedes the Unix epoch, the judgment
    /// cannot be serialized, or the record exceeds its byte limit.
    pub fn new(request: &Request, judgment: &Judgment) -> Result<Self, AuditError> {
        AuditRecordFile {
            at: format_at(SystemTime::now())?,
            request_id: request.id().to_owned(),
            session: request.session().to_owned(),
            hook: request.hook().to_owned(),
            judgment: serde_json::to_value(judgment)
                .map_err(|error| invalid_json(&error))?,
        }
        .try_into()
    }

    /// The time as 20 Unix-second digits, a dot, and six microsecond digits.
    #[must_use]
    pub const fn at(&self) -> &str {
        self.at.as_str()
    }

    /// The id of the request.
    #[must_use]
    pub const fn request_id(&self) -> &str {
        self.request_id.as_str()
    }

    /// The session the request belongs to.
    #[must_use]
    pub const fn session(&self) -> &str {
        self.session.as_str()
    }

    /// The hook that raises the request.
    #[must_use]
    pub const fn hook(&self) -> &str {
        self.hook.as_str()
    }

    /// The judgment object, including the trace's dropped-event count.
    #[must_use]
    pub const fn judgment(&self) -> &Value {
        &self.judgment
    }

    fn line(&self) -> Result<Vec<u8>, AuditError> {
        let mut line =
            serde_json::to_vec(self).map_err(|error| invalid_json(&error))?;

        if line.len() >= MAX_RECORD_BYTES {
            return Err(limit("record bytes", MAX_RECORD_BYTES));
        }

        line.push(b'\n');
        Ok(line)
    }
}

#[derive(Deserialize)]
#[serde(rename = "AuditRecord", deny_unknown_fields)]
struct AuditRecordFile {
    at: String,
    request_id: String,
    session: String,
    hook: String,
    judgment: Value,
}

impl TryFrom<AuditRecordFile> for AuditRecord {
    type Error = AuditError;

    fn try_from(file: AuditRecordFile) -> Result<Self, Self::Error> {
        check_at(&file.at)?;
        check_text("request id", &file.request_id)?;
        check_text("session", &file.session)?;
        check_text("hook", &file.hook)?;

        if !file.judgment.is_object() {
            return Err(AuditError::Invalid("judgment is not an object".to_owned()));
        }

        let record = Self {
            at: file.at,
            request_id: file.request_id,
            session: file.session,
            hook: file.hook,
            judgment: file.judgment,
        };
        record.line()?;

        Ok(record)
    }
}

fn format_at(time: SystemTime) -> Result<String, AuditError> {
    let elapsed = time
        .duration_since(UNIX_EPOCH)
        .map_err(|error| AuditError::Invalid(error.to_string()))?;

    Ok(format!(
        "{:0SECOND_DIGITS$}.{:0MICROSECOND_DIGITS$}",
        elapsed.as_secs(),
        elapsed.subsec_micros()
    ))
}

fn check_at(at: &str) -> Result<(), AuditError> {
    let valid = at.split_once('.').is_some_and(|(seconds, micros)| {
        seconds.len() == SECOND_DIGITS
            && seconds.bytes().all(|byte| byte.is_ascii_digit())
            && seconds.parse::<u64>().is_ok()
            && micros.len() == MICROSECOND_DIGITS
            && micros.bytes().all(|byte| byte.is_ascii_digit())
    });

    if !valid {
        return Err(AuditError::Invalid("at is not a timestamp".to_owned()));
    }

    Ok(())
}

fn check_text(name: &'static str, value: &str) -> Result<(), AuditError> {
    if value.is_empty() {
        return Err(AuditError::Invalid(format!("{name} is empty")));
    }
    if value.len() > MAX_TEXT_BYTES {
        return Err(limit(name, MAX_TEXT_BYTES));
    }

    Ok(())
}

fn invalid_json(error: &serde_json::Error) -> AuditError {
    AuditError::Invalid(error.to_string())
}

fn limit(name: &'static str, max: usize) -> AuditError {
    AuditError::Limit {
        name,
        max: u64::try_from(max).unwrap_or(u64::MAX),
    }
}

/// Appends judgment records in a private folder.
///
/// Reads and writes through the same instance do not overlap.
#[derive(Debug)]
pub struct AuditLog {
    folder: PathBuf,
    max_bytes: u64,
    files: Mutex<()>,
}

impl AuditLog {
    /// Uses the default rotation threshold in this folder.
    #[must_use]
    pub fn new(folder: &Path) -> Self {
        Self::with_max(folder, MAX_AUDIT_BYTES)
    }

    /// Sets the threshold for rotating a non-empty current file.
    #[must_use]
    pub fn with_max(folder: &Path, max_bytes: u64) -> Self {
        Self {
            folder: folder.to_owned(),
            max_bytes,
            files: Mutex::new(()),
        }
    }

    /// Appends a record, replacing the archive when the current file rotates.
    ///
    /// # Errors
    /// Returns an error if the line is too large or the log cannot be created,
    /// rotated, or written.
    pub fn append(&self, record: &AuditRecord) -> io::Result<()> {
        let line = record
            .line()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let adding = u64::try_from(line.len()).map_err(io::Error::other)?;
        let _files = self
            .files
            .lock()
            .map_err(|_| io::Error::other("log lock poisoned"))?;

        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.folder)?;

        let path = self.folder.join(CURRENT);
        let size = match fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };

        if size > 0 && size.saturating_add(adding) > self.max_bytes {
            fs::rename(&path, self.folder.join(ROTATED))?;
        }

        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?
            .write_all(&line)
    }

    /// Reads the last records, oldest first, up to [`MAX_RECENT`].
    ///
    /// # Errors
    /// Returns an error if either log cannot be read or has a malformed or
    /// oversized line. Missing logs contribute no records.
    pub fn recent(&self, count: usize) -> io::Result<Vec<AuditRecord>> {
        let _files = self
            .files
            .lock()
            .map_err(|_| io::Error::other("log lock poisoned"))?;
        let count = count.min(MAX_RECENT);
        let mut records = VecDeque::with_capacity(count);

        for name in [ROTATED, CURRENT] {
            let file = match File::open(self.folder.join(name)) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };

            read_records(file, &mut records, count)?;
        }

        Ok(records.into())
    }
}

fn read_records(
    file: File,
    records: &mut VecDeque<AuditRecord>,
    count: usize,
) -> io::Result<()> {
    let size = file.metadata()?.len();
    let max_read = u64::try_from(MAX_RECORD_BYTES.saturating_add(1))
        .map_err(io::Error::other)?;
    let mut reader = BufReader::new(file).take(size);
    let mut line = Vec::new();

    loop {
        line.clear();

        if reader
            .by_ref()
            .take(max_read)
            .read_until(b'\n', &mut line)?
            == 0
        {
            return Ok(());
        }
        if line.len() > MAX_RECORD_BYTES {
            let error = limit("record bytes", MAX_RECORD_BYTES);
            return Err(io::Error::new(io::ErrorKind::InvalidData, error));
        }

        let record = serde_json::from_slice(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        records.push_back(record);

        if records.len() > count {
            records.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::{
        AuditError, AuditLog, AuditRecord, CURRENT, MAX_RECENT, MAX_RECORD_BYTES,
        ROTATED, format_at,
    };
    use crate::{
        Engine, Judgment, MAX_TEXT_BYTES, Request, RuleSet, Trace, TraceEvent,
    };

    #[test]
    fn appends_and_reads_exact_records_in_order() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        let first = record("call-1");
        let second = record("call-2");

        log.append(&first).unwrap();
        log.append(&second).unwrap();

        assert_eq!(AuditLog::new(&folder).recent(2).unwrap(), [first, second]);
        assert_eq!(mode(&folder), 0o700);
        assert_eq!(mode(&folder.join(CURRENT)), 0o600);
    }

    #[test]
    fn returns_only_the_last_two_records() {
        let scratch = Scratch::new();
        let log = AuditLog::new(&scratch.folder());
        let first = record("call-1");
        let second = record("call-2");
        let third = record("call-3");

        for item in [&first, &second, &third] {
            log.append(item).unwrap();
        }

        assert_eq!(log.recent(2).unwrap(), [second, third]);
    }

    #[test]
    fn caps_the_number_of_records_returned() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let first = record("call-1");
        let second = record("call-2");
        let log = AuditLog::new(&folder);
        let mut bytes = first.line().unwrap();
        bytes.extend(second.line().unwrap().repeat(MAX_RECENT));
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join(CURRENT), bytes).unwrap();

        assert_eq!(log.recent(usize::MAX).unwrap(), vec![second; MAX_RECENT]);
    }

    #[test]
    fn replaces_the_archive_on_the_second_rotation_and_reads_both_files() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let first = record("call-1");
        let second = record("call-2");
        let third = record("call-3");
        let first_line = first.line().unwrap();
        let second_line = second.line().unwrap();
        let third_line = third.line().unwrap();
        let log =
            AuditLog::with_max(&folder, u64::try_from(first_line.len()).unwrap());

        log.append(&first).unwrap();
        log.append(&second).unwrap();

        assert_eq!(fs::read(folder.join(ROTATED)).unwrap(), first_line);
        assert_eq!(fs::read(folder.join(CURRENT)).unwrap(), second_line);
        assert_eq!(log.recent(3).unwrap(), [first, second.clone()]);

        log.append(&third).unwrap();

        assert_eq!(fs::read(folder.join(ROTATED)).unwrap(), second_line);
        assert_eq!(fs::read(folder.join(CURRENT)).unwrap(), third_line);
        assert_eq!(log.recent(3).unwrap(), [second, third]);
    }

    #[test]
    fn refuses_an_oversized_record_without_creating_a_log() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        let mut large = record("call-1");
        large.judgment = json!({ "text": "x".repeat(MAX_RECORD_BYTES) });

        let error = log.append(&large).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "record bytes exceeds 1048576");
        assert_eq!(
            fs::metadata(folder).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        let json = serde_json::to_value(large).unwrap();
        let error = serde_json::from_value::<AuditRecord>(json).unwrap_err();

        assert_eq!(error.to_string(), "record bytes exceeds 1048576");
    }

    #[test]
    fn refuses_an_oversized_raw_line_even_if_its_json_is_small() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        let item = record("call-1");
        let mut bytes = serde_json::to_vec(&item).unwrap();
        bytes.resize(MAX_RECORD_BYTES, b' ');
        bytes.push(b'\n');
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join(CURRENT), &bytes).unwrap();

        let error = log.recent(1).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "record bytes exceeds 1048576");

        bytes.pop();
        bytes.pop();
        bytes.push(b'\n');
        fs::write(folder.join(CURRENT), bytes).unwrap();

        assert_eq!(log.recent(1).unwrap(), [item]);
    }

    #[test]
    fn refuses_a_malformed_line_in_either_log() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        fs::create_dir(&folder).unwrap();

        for name in [ROTATED, CURRENT] {
            let path = folder.join(name);
            fs::write(&path, b"!\n").unwrap();

            let error = log.recent(2).unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(error.to_string(), "expected value at line 1 column 1");
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn missing_and_empty_files_return_no_records() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);

        assert_eq!(log.recent(1).unwrap(), []);

        fs::create_dir(&folder).unwrap();
        File::create(folder.join(CURRENT)).unwrap();

        assert_eq!(log.recent(1).unwrap(), []);
    }

    #[test]
    fn round_trips_the_exact_judgment_including_a_dropped_event() {
        let request = request("call-1");
        let mut trace = Trace::new();
        trace.push(TraceEvent::finished().unwrap());
        let judgment = Judgment::new(request.id(), None, trace).unwrap();
        let record = AuditRecord::new(&request, &judgment).unwrap();
        let expected = json!({
            "request_id": "call-1",
            "verdict": null,
            "trace": { "dropped": 1, "events": [] },
        });
        let json = serde_json::to_string(&record).unwrap();

        assert_eq!(record.at().len(), 27);
        assert_eq!(record.request_id(), "call-1");
        assert_eq!(record.session(), "s1");
        assert_eq!(record.hook(), "tool_before");
        assert_eq!(record.judgment(), &expected);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            json!({
                "at": record.at(),
                "request_id": "call-1",
                "session": "s1",
                "hook": "tool_before",
                "judgment": expected,
            })
        );
        assert_eq!(serde_json::from_str::<AuditRecord>(&json).unwrap(), record);
    }

    #[test]
    fn refuses_unknown_and_invalid_fields_on_read() {
        let value = serde_json::to_value(record("call-1")).unwrap();
        let fields = [
            (
                "extra",
                json!(1),
                "unknown field `extra`, expected one of `at`, `request_id`, \
                 `session`, `hook`, `judgment`",
            ),
            (
                "at",
                json!("123.000000"),
                "invalid audit record: at is not a timestamp",
            ),
            (
                "request_id",
                json!(""),
                "invalid audit record: request id is empty",
            ),
            (
                "session",
                json!("x".repeat(MAX_TEXT_BYTES + 1)),
                "session exceeds 16384",
            ),
            ("hook", json!(""), "invalid audit record: hook is empty"),
            (
                "judgment",
                json!([]),
                "invalid audit record: judgment is not an object",
            ),
        ];

        for (field, replacement, message) in fields {
            let mut file = value.clone();
            file[field] = replacement;
            let error = serde_json::from_value::<AuditRecord>(file).unwrap_err();

            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn formats_seconds_and_microseconds_at_fixed_width() {
        let sample = UNIX_EPOCH
            .checked_add(Duration::new(12, 345_678_999))
            .unwrap();
        let before_epoch = UNIX_EPOCH.checked_sub(Duration::from_secs(1)).unwrap();

        assert_eq!(format_at(sample).unwrap(), "00000000000000000012.345678");
        assert_eq!(
            format_at(UNIX_EPOCH).unwrap(),
            "00000000000000000000.000000"
        );
        assert_eq!(
            format_at(before_epoch).unwrap_err(),
            AuditError::Invalid(
                "second time provided was later than self".to_owned()
            )
        );
    }

    fn record(id: &str) -> AuditRecord {
        let request = request(id);
        let judgment = Engine::new(RuleSet::builtin()).judge(&request);

        AuditRecord::new(&request, &judgment).unwrap()
    }

    fn request(id: &str) -> Request {
        let json = json!({
            "id": id,
            "session": "s1",
            "hook": "tool_before",
            "subject": { "tool": "shell", "input": { "command": "git status" } },
        });

        Request::parse(&json.to_string()).unwrap()
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let name = format!(
                "audit-test-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
            let path = target.join(name);

            fs::create_dir_all(target).unwrap();
            fs::create_dir(&path).unwrap();

            Self(path)
        }

        fn folder(&self) -> PathBuf {
            self.0.join("log")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
