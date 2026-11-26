//! The judgment audit log: one bounded JSON record per line.
//!
//! Append and recent adapt wrkhse/chauffeur origin/v2
//! crates/daemon/src/audit.rs at 2273807, introduced at 0eb45c0.
//!
//! A log path is refused when it is a symlink, and when the opened file's device
//! or inode differs from the lstat result.
//!
//! Appends keep the folder 0700 and its files 0600. Reads change nothing and
//! refuse a folder or file another user owns or the group or others can write.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{
    DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
const LOCK: &str = "lock";
const FOLDER: &str = "audit folder";
const TAIL_CHUNK_BYTES: usize = 4096;
const LOCK_ATTEMPTS: u32 = 50;
const LOCK_RETRY: Duration = Duration::from_millis(10);
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
/// Appends hold an exclusive lock on the folder's `lock` file, and reads take
/// a shared one when it exists, so readers run together while writers take
/// turns with them. Both try the lock 50 times, 10 ms apart, then fail
/// instead of waiting longer.
#[derive(Debug)]
pub struct AuditLog {
    folder: PathBuf,
    max_bytes: u64,
    torn: AtomicUsize,
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
            torn: AtomicUsize::new(0),
        }
    }

    /// The final lines without a newline that reads through this handle
    /// skip, as a crash mid-write leaves them.
    #[must_use]
    pub fn torn_lines(&self) -> usize {
        self.torn.load(Ordering::Relaxed)
    }

    /// Appends a record, replacing the archive when the current file rotates.
    ///
    /// A final line without a newline is cut before the record is written.
    ///
    /// # Errors
    /// Returns an error if the line is too large, a log file is a symlink or
    /// changes while opening, a torn tail is longer than a record, the lock
    /// stays held by another handle past the wait, or the log cannot be
    /// locked, created, rotated, or written.
    pub fn append(&self, record: &AuditRecord) -> io::Result<()> {
        let line = record
            .line()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let adding = u64::try_from(line.len()).map_err(io::Error::other)?;
        let _lock = self.lock()?;

        let path = self.folder.join(CURRENT);
        let mut options = OpenOptions::new();
        options.read(true).append(true);

        let mut file = open_or_create(&path, CURRENT, &options)?;
        let size = cut_torn_tail(&mut file)?;

        if size > 0 && size.saturating_add(adding) > self.max_bytes {
            fs::rename(&path, self.folder.join(ROTATED))?;
            file = open_or_create(&path, CURRENT, &options)?;
        }

        file.write_all(&line)
    }

    /// Reads the last records, oldest first, up to [`MAX_RECENT`].
    ///
    /// A read changes no permissions and creates nothing: a missing folder
    /// holds no records, and a folder with no `lock` file has had no append,
    /// so it is read without the lock. A final line without a newline is
    /// skipped and counted in [`AuditLog::torn_lines`].
    ///
    /// # Errors
    /// Returns an error if the folder, lock or a log file is owned by another
    /// user or writable by the group or others, a file is a symlink or
    /// changes while opening, the lock stays held by another handle past the
    /// wait or cannot be taken, or a file cannot be read or has a malformed
    /// or oversized line. Missing logs contribute no records.
    pub fn recent(&self, count: usize) -> io::Result<Vec<AuditRecord>> {
        let count = count.min(MAX_RECENT);
        let mut records = VecDeque::with_capacity(count);

        let folder = match fs::metadata(&self.folder) {
            Ok(folder) => folder,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let owner = own_uid()?;
        trusted(FOLDER, &folder, owner)?;

        let lock = open_trusted(&self.folder.join(LOCK), LOCK, owner)?;

        if let Some(lock) = &lock {
            wait_for_shared_lock(lock)?;
        }

        for name in [ROTATED, CURRENT] {
            let path = self.folder.join(name);

            if let Some(file) = open_trusted(&path, name, owner)? {
                read_records(file, &mut records, count, &self.torn)?;
            }
        }

        Ok(records.into())
    }

    /// Makes the folder private and holds its lock until the file drops,
    /// waiting a bounded time for another handle to release it.
    fn lock(&self) -> io::Result<File> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.folder)?;
        fs::set_permissions(&self.folder, fs::Permissions::from_mode(0o700))?;

        let mut options = OpenOptions::new();
        options.read(true).write(true);

        let file = open_or_create(&self.folder.join(LOCK), LOCK, &options)?;
        wait_for_lock(&file)?;

        Ok(file)
    }
}

/// Opens an existing regular file with `options`, or creates it alone, and
/// makes it private.
fn open_or_create(
    path: &Path,
    name: &'static str,
    options: &OpenOptions,
) -> io::Result<File> {
    if let Some(file) = open_existing(path, name, options)? {
        private(&file)?;
        return Ok(file);
    }

    let mut creating = options.clone();

    // A create that fails opens what another process made first, and returns
    // the create's error when nothing is there.
    let file = match creating.create_new(true).mode(0o600).open(path) {
        Ok(file) => file,
        Err(error) => open_existing(path, name, options)?.ok_or(error)?,
    };

    private(&file)?;
    Ok(file)
}

/// Opens a regular file that exists to read, never through a symlink, and
/// refuses one that is not [`trusted`]. Nothing at `path` gives `None`.
fn open_trusted(
    path: &Path,
    name: &'static str,
    owner: u32,
) -> io::Result<Option<File>> {
    let mut options = OpenOptions::new();
    options.read(true);

    let Some(file) = open_existing(path, name, &options)? else {
        return Ok(None);
    };

    trusted(name, &file.metadata()?, owner)?;
    Ok(Some(file))
}

/// Refuses what another user owns or the group or others can write: either
/// could have planted records.
fn trusted(name: &str, metadata: &fs::Metadata, owner: u32) -> io::Result<()> {
    if metadata.uid() != owner {
        return Err(refused(name, "is owned by another user"));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(refused(name, "is writable by the group or others"));
    }

    Ok(())
}

/// The effective user ID, without a new dependency: a pipe is owned by the
/// user who makes it. Linux sets a pipe inode's owner to the caller's fsuid
/// (`fs/pipe.c`), and macOS reports `kauth_getuid()` (`bsd/kern/sys_pipe.c`).
fn own_uid() -> io::Result<u32> {
    let (reader, _writer) = io::pipe()?;
    let pipe = File::from(OwnedFd::from(reader));

    Ok(pipe.metadata()?.uid())
}

/// Opens a regular file that exists, never through a symlink. Nothing at
/// `path` gives `None`.
fn open_existing(
    path: &Path,
    name: &'static str,
    options: &OpenOptions,
) -> io::Result<Option<File>> {
    let Some(before) = entry(path, name)? else {
        return Ok(None);
    };

    let file = options.open(path)?;
    let opened = file.metadata()?;

    if (opened.dev(), opened.ino()) != (before.dev(), before.ino()) {
        return Err(refused(name, "changed while opening"));
    }

    Ok(Some(file))
}

/// What is at `path`, without following a symlink: a regular file or
/// nothing.
fn entry(path: &Path, name: &'static str) -> io::Result<Option<fs::Metadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    if metadata.file_type().is_symlink() {
        return Err(refused(name, "is a symlink"));
    }
    if !metadata.is_file() {
        return Err(refused(name, "is not a regular file"));
    }

    Ok(Some(metadata))
}

fn private(file: &File) -> io::Result<()> {
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

fn refused(name: &str, why: &str) -> io::Error {
    io::Error::other(format!("{name} {why}"))
}

/// Takes the exclusive lock on `file`, trying [`LOCK_ATTEMPTS`] times
/// [`LOCK_RETRY`] apart, so a writer that never releases it fails the caller
/// closed instead of hanging it.
fn wait_for_lock(file: &File) -> io::Result<()> {
    for _ in 0..LOCK_ATTEMPTS {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) => thread::sleep(LOCK_RETRY),
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "audit lock is held by another handle",
    ))
}

/// Takes the shared lock on `file`, trying [`LOCK_ATTEMPTS`] times
/// [`LOCK_RETRY`] apart, so a writer that never releases it fails the caller
/// closed instead of hanging it. Other readers never block the wait; only a
/// writer's exclusive lock does.
fn wait_for_shared_lock(file: &File) -> io::Result<()> {
    for _ in 0..LOCK_ATTEMPTS {
        match file.try_lock_shared() {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) => thread::sleep(LOCK_RETRY),
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "audit lock is held by another handle",
    ))
}

/// Cuts a final line without a newline and returns the length that remains.
///
/// The scan reads backwards in chunks over at most the last
/// [`MAX_RECORD_BYTES`]. A whole line is never longer, so a span with no
/// newline before the end means the tail is not a torn record.
fn cut_torn_tail(file: &mut File) -> io::Result<u64> {
    let size = file.metadata()?.len();
    let span = u64::try_from(MAX_RECORD_BYTES).map_err(io::Error::other)?;
    let floor = size.saturating_sub(span);
    let mut chunk = [0_u8; TAIL_CHUNK_BYTES];
    let step = u64::try_from(chunk.len()).map_err(io::Error::other)?;
    // A counted loop: the scan ends after these chunks whatever they hold.
    let chunks = size.saturating_sub(floor).div_ceil(step);

    for index in 0..chunks {
        let end = size.saturating_sub(index.saturating_mul(step));
        let start = end.saturating_sub(step).max(floor);
        let len =
            usize::try_from(end.saturating_sub(start)).map_err(io::Error::other)?;
        let bytes = chunk
            .get_mut(..len)
            .ok_or_else(|| io::Error::other("tail chunk is too small"))?;

        file.seek(SeekFrom::Start(start))?;
        file.read_exact(bytes)?;

        if let Some(newline) = bytes.iter().rposition(|byte| *byte == b'\n') {
            let newline = u64::try_from(newline).map_err(io::Error::other)?;
            let whole = start.saturating_add(newline).saturating_add(1);
            file.set_len(whole)?;

            return Ok(whole);
        }
    }

    if floor > 0 {
        let error = limit("torn line bytes", MAX_RECORD_BYTES);
        return Err(io::Error::new(io::ErrorKind::InvalidData, error));
    }

    file.set_len(0)?;
    Ok(0)
}

fn read_records(
    file: File,
    records: &mut VecDeque<AuditRecord>,
    count: usize,
    torn: &AtomicUsize,
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
        if line.last() != Some(&b'\n') {
            // Only the end of the file stops a line short of its newline.
            // The count is atomic, so concurrent readers cannot race it.
            let seen = torn.load(Ordering::Relaxed);
            torn.store(seen.saturating_add(1), Ordering::Relaxed);

            return Ok(());
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
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::{
        AuditError, AuditLog, AuditRecord, CURRENT, FOLDER, LOCK, LOCK_ATTEMPTS,
        LOCK_RETRY, MAX_RECENT, MAX_RECORD_BYTES, ROTATED, TAIL_CHUNK_BYTES,
        check_text, entry, format_at, open_or_create, own_uid, trusted,
    };
    use crate::{
        Engine, Judgment, MAX_EVENTS, MAX_TEXT_BYTES, Request, RuleSet, Trace,
        TraceEvent,
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
    fn default_log_keeps_two_large_records_in_one_file() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        let first = padded("call-1");
        let second = padded("call-2");
        let both = [first.line().unwrap(), second.line().unwrap()].concat();

        log.append(&first).unwrap();
        log.append(&second).unwrap();

        // Past every one-operator change to MAX_AUDIT_BYTES, the largest
        // being (8 + 1024) * 1024, yet well under the default bound.
        assert!(both.len() > 1_056_768);
        assert_eq!(fs::read(folder.join(CURRENT)).unwrap(), both);
        assert_eq!(
            fs::metadata(folder.join(ROTATED)).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(log.recent(2).unwrap(), [first, second]);
    }

    #[test]
    fn rotates_past_the_bound_but_not_at_it() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let records = [record("call-1"), record("call-2"), record("call-3")];
        let lines = records.each_ref().map(|item| item.line().unwrap());
        let full = [lines[0].as_slice(), lines[1].as_slice()].concat();
        let max = u64::try_from(full.len()).unwrap();
        let log = AuditLog::with_max(&folder, max);

        log.append(&records[0]).unwrap();
        log.append(&records[1]).unwrap();

        assert_eq!(fs::read(folder.join(CURRENT)).unwrap(), full);
        assert_eq!(
            fs::metadata(folder.join(ROTATED)).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        log.append(&records[2]).unwrap();

        assert_eq!(fs::read(folder.join(ROTATED)).unwrap(), full);
        assert_eq!(fs::read(folder.join(CURRENT)).unwrap(), lines[2]);
        assert_eq!(log.recent(3).unwrap(), records);
    }

    #[test]
    fn keeps_a_first_record_larger_than_the_bound() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let item = record("call-1");
        let log = AuditLog::with_max(&folder, 1);

        log.append(&item).unwrap();

        assert_eq!(
            fs::read(folder.join(CURRENT)).unwrap(),
            item.line().unwrap()
        );
        assert_eq!(
            fs::metadata(folder.join(ROTATED)).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn accepts_text_at_the_bound_and_refuses_one_byte_more() {
        let at_bound = "x".repeat(MAX_TEXT_BYTES);
        let over = "x".repeat(MAX_TEXT_BYTES + 1);

        assert_eq!(check_text("session", &at_bound), Ok(()));
        assert_eq!(
            check_text("session", &over),
            Err(AuditError::Limit {
                name: "session",
                max: 16_384
            })
        );
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
    fn refuses_a_symlinked_log_in_append_and_recent() {
        let line = record("call-1").line().unwrap();

        // A link to itself and a link to a real file: neither is followed.
        for target in [CURRENT, "elsewhere.jsonl"] {
            let scratch = Scratch::new();
            let folder = scratch.folder();
            let log = AuditLog::new(&folder);
            fs::create_dir(&folder).unwrap();
            fs::write(folder.join("elsewhere.jsonl"), &line).unwrap();
            symlink(target, folder.join(CURRENT)).unwrap();

            let appended = log.append(&record("call-2")).unwrap_err();
            let read = log.recent(1).unwrap_err();

            for error in [appended, read] {
                assert_eq!(error.kind(), io::ErrorKind::Other);
                assert_eq!(error.to_string(), "audit.jsonl is a symlink");
            }
            assert_eq!(fs::read(folder.join("elsewhere.jsonl")).unwrap(), line);
        }
    }

    #[test]
    fn reports_no_torn_lines_for_a_fresh_or_clean_log() {
        let scratch = Scratch::new();
        let log = AuditLog::new(&scratch.folder());
        let item = record("call-1");

        assert_eq!(log.torn_lines(), 0);

        log.append(&item).unwrap();

        assert_eq!(log.recent(1).unwrap(), [item]);
        assert_eq!(log.torn_lines(), 0);
    }

    #[test]
    fn both_racers_open_the_file_one_of_them_creates() {
        let scratch = Scratch::new();
        let mut options = OpenOptions::new();
        options.read(true).write(true);

        // Each round races two creates of a fresh path: the loser sees
        // AlreadyExists and opens the winner's file instead of failing.
        for round in 0..1000 {
            let path = scratch.0.join(format!("race-{round}"));
            let barrier = Barrier::new(2);
            let opened = thread::scope(|scope| {
                let racers = [(); 2].map(|()| {
                    scope.spawn(|| {
                        barrier.wait();
                        open_or_create(&path, "race", &options)
                            .map(drop)
                            .map_err(|error| error.kind())
                    })
                });

                racers.map(|racer| racer.join().unwrap())
            });

            assert_eq!(opened, [Ok(()), Ok(())]);
        }
    }

    #[test]
    fn reports_a_lookup_failure_other_than_not_found() {
        let scratch = Scratch::new();
        let file = scratch.0.join("plain");
        fs::write(&file, b"").unwrap();

        // A regular file as the parent makes symlink_metadata fail with
        // ENOTDIR, which is an error and not a missing log.
        let error = entry(&file.join(CURRENT), CURRENT).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
    }

    #[test]
    fn restores_private_modes_on_the_next_append() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);

        log.append(&record("call-1")).unwrap();

        for name in [CURRENT, LOCK] {
            let open = fs::Permissions::from_mode(0o644);
            fs::set_permissions(folder.join(name), open).unwrap();
        }
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();

        log.append(&record("call-2")).unwrap();

        assert_eq!(mode(&folder), 0o700);
        assert_eq!(mode(&folder.join(CURRENT)), 0o600);
        assert_eq!(mode(&folder.join(LOCK)), 0o600);
    }

    #[test]
    fn two_handles_append_concurrently_without_losing_records() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let writers = ["a", "b"];
        let ids = writers.map(|writer| (0..4).map(move |n| format!("{writer}-{n}")));
        let mut expected: Vec<String> = ids.into_iter().flatten().collect();
        let line = record("a-0").line().unwrap().len();
        let max = u64::try_from(line * 4).unwrap();
        let barrier = Barrier::new(writers.len());

        // Eight lines at a four-line bound rotate exactly once.
        thread::scope(|scope| {
            for writer in writers {
                let (folder, barrier) = (&folder, &barrier);

                scope.spawn(move || {
                    let log = AuditLog::with_max(folder, max);
                    barrier.wait();

                    for index in 0..4 {
                        log.append(&record(&format!("{writer}-{index}"))).unwrap();
                    }
                });
            }
        });

        let records = AuditLog::new(&folder).recent(8).unwrap();
        let mut seen: Vec<String> = records
            .iter()
            .map(|item| item.request_id().to_owned())
            .collect();
        seen.sort();
        expected.sort();

        assert_eq!(seen, expected);
        assert_eq!(fs::read(folder.join(ROTATED)).unwrap().len(), line * 4);
        assert_eq!(fs::read(folder.join(CURRENT)).unwrap().len(), line * 4);
    }

    #[test]
    fn fails_an_append_whose_lock_stays_held_past_the_bound() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        let (first, blocked, last) = (record("a"), record("b"), record("c"));
        log.append(&first).unwrap();

        let holder = File::open(folder.join(LOCK)).unwrap();
        holder.lock().unwrap();
        let started = Instant::now();
        let error = log.append(&blocked).unwrap_err();
        let waited = started.elapsed();
        drop(holder);

        // Every attempt sleeps once, and a loaded runner stays far under 5 s.
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "audit lock is held by another handle");
        assert!(waited >= LOCK_RETRY * LOCK_ATTEMPTS, "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");

        log.append(&last).unwrap();

        assert_eq!(log.recent(3).unwrap(), [first, last]);
    }

    #[test]
    fn fails_a_read_whose_lock_stays_held_past_the_bound() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        let item = record("call-1");
        log.append(&item).unwrap();

        let modes = [(folder.clone(), 0o750), (folder.join(LOCK), 0o400)];

        for (path, bits) in &modes {
            fs::set_permissions(path, fs::Permissions::from_mode(*bits)).unwrap();
        }

        let holder = File::open(folder.join(LOCK)).unwrap();
        holder.lock().unwrap();
        let started = Instant::now();
        let error = log.recent(1).unwrap_err();
        let waited = started.elapsed();
        drop(holder);

        // Every attempt sleeps once, and a loaded runner stays far under 5 s.
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "audit lock is held by another handle");
        assert!(waited >= LOCK_RETRY * LOCK_ATTEMPTS, "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
        assert_eq!(log.recent(1).unwrap(), [item]);

        for (path, bits) in modes {
            assert_eq!(mode(&path), bits);
        }
    }

    #[test]
    fn two_readers_hold_shared_locks_and_both_succeed() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        let items = [record("a"), record("b")];

        for item in &items {
            log.append(item).unwrap();
        }

        // Two separate handles hold a shared lock on the same lock file at
        // once. An exclusive read lock would wait and fail both reads.
        let path = folder.join(LOCK);
        let holders = [File::open(&path).unwrap(), File::open(&path).unwrap()];
        for holder in &holders {
            holder.lock_shared().unwrap();
        }

        let started = Instant::now();
        let first = AuditLog::new(&folder).recent(2).unwrap();
        let second = AuditLog::new(&folder).recent(2).unwrap();
        let waited = started.elapsed();

        assert_eq!(first, items);
        assert_eq!(second, items);
        assert!(waited < Duration::from_secs(5), "{waited:?}");
    }

    #[test]
    fn skips_counts_and_cuts_a_torn_final_line() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        let [first, second, third] =
            [record("call-1"), record("call-2"), record("call-3")];
        let whole = [first.line().unwrap(), second.line().unwrap()].concat();
        // Longer than one tail chunk, so the cut scans several.
        let torn = padded("call-x").line().unwrap()[..300_000].to_vec();
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join(CURRENT), [whole.clone(), torn].concat()).unwrap();

        assert_eq!(log.recent(5).unwrap(), [first.clone(), second.clone()]);
        assert_eq!(log.torn_lines(), 1);

        log.append(&third).unwrap();

        assert_eq!(
            fs::read(folder.join(CURRENT)).unwrap(),
            [whole, third.line().unwrap()].concat()
        );
        assert_eq!(log.recent(5).unwrap(), [first, second, third]);
        assert_eq!(log.torn_lines(), 1);
    }

    #[test]
    fn cuts_torn_tails_that_end_on_a_chunk_boundary() {
        let whole = record("call-1").line().unwrap();
        let item = record("call-2");

        // A newline as the first byte of the last chunk, as the last byte
        // of the chunk before it, and one byte further back; then a log of
        // exactly two chunks that is all torn.
        let cases = [
            (whole.clone(), TAIL_CHUNK_BYTES - 1),
            (whole.clone(), TAIL_CHUNK_BYTES),
            (whole, TAIL_CHUNK_BYTES + 1),
            (Vec::new(), 2 * TAIL_CHUNK_BYTES),
        ];

        for (kept, torn) in cases {
            let scratch = Scratch::new();
            let folder = scratch.folder();
            let log = AuditLog::new(&folder);
            let bytes = [kept.clone(), vec![b'x'; torn]].concat();
            fs::create_dir(&folder).unwrap();
            fs::write(folder.join(CURRENT), bytes).unwrap();

            log.append(&item).unwrap();

            assert_eq!(
                fs::read(folder.join(CURRENT)).unwrap(),
                [kept, item.line().unwrap()].concat()
            );
        }
    }

    #[test]
    fn cuts_a_log_that_holds_only_a_torn_line() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        let item = record("call-1");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join(CURRENT), b"{\"at\"").unwrap();

        assert_eq!(log.recent(1).unwrap(), []);
        assert_eq!(log.torn_lines(), 1);

        log.append(&item).unwrap();

        assert_eq!(
            fs::read(folder.join(CURRENT)).unwrap(),
            item.line().unwrap()
        );
        assert_eq!(log.recent(1).unwrap(), [item]);
    }

    #[test]
    fn refuses_to_cut_a_tail_longer_than_a_record() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        let line = record("call-1").line().unwrap();
        let bytes = [line, vec![b'x'; MAX_RECORD_BYTES]].concat();
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join(CURRENT), &bytes).unwrap();

        let error = log.append(&record("call-2")).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "torn line bytes exceeds 1048576");
        assert_eq!(fs::read(folder.join(CURRENT)).unwrap(), bytes);
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
    fn reads_a_missing_folder_as_empty_without_creating_it() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let plain = scratch.0.join("plain");
        fs::write(&plain, b"").unwrap();

        assert_eq!(AuditLog::new(&folder).recent(1).unwrap(), []);
        assert_eq!(
            fs::metadata(&folder).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        // A regular file in the folder's path is an error, not a missing log.
        let error = AuditLog::new(&plain.join("log")).recent(1).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
    }

    #[test]
    fn reads_change_no_permissions() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        let item = record("call-1");
        log.append(&item).unwrap();

        let reads = [
            (folder.clone(), 0o750),
            (folder.join(CURRENT), 0o640),
            (folder.join(LOCK), 0o400),
        ];

        for (path, bits) in &reads {
            fs::set_permissions(path, fs::Permissions::from_mode(*bits)).unwrap();
        }

        assert_eq!(log.recent(1).unwrap(), [item]);

        for (path, bits) in reads {
            assert_eq!(mode(&path), bits);
        }
    }

    #[test]
    fn refuses_to_read_what_the_group_or_others_can_write() {
        let scratch = Scratch::new();
        let folder = scratch.folder();
        let log = AuditLog::new(&folder);
        log.append(&record("call-1")).unwrap();

        let targets = [
            (FOLDER, folder.clone(), 0o700),
            (LOCK, folder.join(LOCK), 0o600),
            (CURRENT, folder.join(CURRENT), 0o600),
        ];

        for (name, path, private) in targets {
            for writable in [0o020, 0o002] {
                let bits = private | writable;
                fs::set_permissions(&path, fs::Permissions::from_mode(bits))
                    .unwrap();

                let error = log.recent(1).unwrap_err();

                assert_eq!(error.kind(), io::ErrorKind::Other);
                assert_eq!(
                    error.to_string(),
                    format!("{name} is writable by the group or others")
                );
                assert_eq!(mode(&path), bits);

                let restore = fs::Permissions::from_mode(private);
                fs::set_permissions(&path, restore).unwrap();
            }
        }
    }

    #[test]
    fn refuses_a_log_owned_by_another_user() {
        let scratch = Scratch::new();
        let path = scratch.0.join(CURRENT);
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let own = own_uid().unwrap();

        // Giving a file another owner needs root, so the test passes the
        // reader a different user ID instead.
        let error = trusted(CURRENT, &metadata, own.wrapping_add(1)).unwrap_err();

        assert_eq!(own, metadata.uid());
        trusted(CURRENT, &metadata, own).unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "audit.jsonl is owned by another user");
    }

    #[test]
    fn round_trips_the_exact_judgment_including_a_dropped_event() {
        let request = request("call-1");
        let mut trace = Trace::new();

        for _ in 0..MAX_EVENTS {
            trace.push(TraceEvent::rule_started("overflow").unwrap());
        }

        trace.finish();

        let started = json!({ "kind": "rule_started", "rule": "overflow" });
        let mut events = vec![started; MAX_EVENTS - 1];
        events.push(json!({ "kind": "finished" }));

        assert_eq!(trace.events().len(), MAX_EVENTS);
        assert_eq!(trace.dropped(), 1);

        let judgment = Judgment::new(request.id(), None, trace).unwrap();
        let record = AuditRecord::new(&request, &judgment).unwrap();
        let expected = json!({
            "request_id": "call-1",
            "verdict": null,
            "trace": { "dropped": 1, "events": events },
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

    /// A record whose judgment carries 600 KiB of padding.
    fn padded(id: &str) -> AuditRecord {
        let mut item = record(id);
        item.judgment["padding"] = json!("x".repeat(600 * 1024));

        item
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
