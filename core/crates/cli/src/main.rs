//! Entry point for judging requests and inspecting the local audit log.

use std::error::Error;
use std::io::{self, Read, Write};
use std::num::ParseIntError;
use std::path::PathBuf;
use std::str::{self, Utf8Error};
use std::{env, fmt};

use chauffeur_judgment::{
    AuditError, AuditLog, AuditRecord, Engine, MAX_RECENT, MAX_REQUEST_BYTES,
    Request, RequestError, RuleSet,
};
use serde::Serialize;

const USAGE: &str = "usage: chauffeur-core judge | rules | audit [N] | version";
const DEFAULT_RECENT: usize = 20;

enum CliError {
    Usage,
    Count(ParseIntError),
    MissingHome,
    Io(io::Error),
    Utf8(Utf8Error),
    Request(RequestError),
    Audit(AuditError),
    Json(serde_json::Error),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => formatter.write_str(USAGE),
            Self::Count(error) => {
                write!(formatter, "invalid audit count: {error}\n{USAGE}")
            }
            Self::MissingHome => {
                formatter.write_str("set CHAUFFEUR_CORE_STATE_DIR or HOME")
            }
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Utf8(error) => write!(formatter, "request is not UTF-8: {error}"),
            Self::Request(error) => write!(formatter, "{error}"),
            Self::Audit(error) => write!(formatter, "{error}"),
            Self::Json(error) => write!(formatter, "{error}"),
        }
    }
}

impl fmt::Debug for CliError {
    // Result-based main prints this diagnostic through Debug.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Usage | Self::MissingHome => None,
            Self::Count(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Utf8(error) => Some(error),
            Self::Request(error) => Some(error),
            Self::Audit(error) => Some(error),
            Self::Json(error) => Some(error),
        }
    }
}

impl From<ParseIntError> for CliError {
    fn from(error: ParseIntError) -> Self {
        Self::Count(error)
    }
}

impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<Utf8Error> for CliError {
    fn from(error: Utf8Error) -> Self {
        Self::Utf8(error)
    }
}

impl From<RequestError> for CliError {
    fn from(error: RequestError) -> Self {
        Self::Request(error)
    }
}

impl From<AuditError> for CliError {
    fn from(error: AuditError) -> Self {
        Self::Audit(error)
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

fn main() -> Result<(), CliError> {
    let mut args = env::args().skip(1);
    let command = args.next();
    let argument = args.next();

    if args.next().is_some() {
        return Err(CliError::Usage);
    }

    let mut out = io::stdout().lock();

    match (command.as_deref(), argument.as_deref()) {
        (Some("judge"), None) => judge(&mut out)?,
        (Some("rules"), None) => {
            for name in RuleSet::builtin().names() {
                writeln!(out, "{name}")?;
            }
        }
        (Some("audit"), count) => {
            let count = count.map_or(Ok(DEFAULT_RECENT), str::parse::<usize>)?;
            audit(count.min(MAX_RECENT), &mut out)?;
        }
        (Some("version"), None) => writeln!(out, "{}", env!("CARGO_PKG_VERSION"))?,
        _ => return Err(CliError::Usage),
    }

    out.flush()?;
    Ok(())
}

fn judge(out: &mut impl Write) -> Result<(), CliError> {
    let request = read_request()?;
    let judgment = Engine::new(RuleSet::builtin()).judge(&request);
    let record = AuditRecord::new(&request, &judgment)?;

    AuditLog::new(&state_dir()?).append(&record)?;
    serde_json::to_writer_pretty(&mut *out, &judgment)?;
    writeln!(out)?;

    Ok(())
}

fn read_request() -> Result<Request, CliError> {
    let limit = u64::try_from(MAX_REQUEST_BYTES).map_err(io::Error::other)?;
    let mut bytes = Vec::new();

    io::stdin()
        .lock()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;

    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(RequestError::Limit {
            name: "request bytes",
            max: limit,
        }
        .into());
    }

    Ok(Request::parse(str::from_utf8(&bytes)?)?)
}

fn audit(count: usize, out: &mut impl Write) -> Result<(), CliError> {
    let records = AuditLog::new(&state_dir()?).recent(count)?;

    for record in records {
        write_record(out, &record)?;
    }

    Ok(())
}

fn write_record(
    out: &mut impl Write,
    record: &impl Serialize,
) -> Result<(), CliError> {
    serde_json::to_writer(&mut *out, record)?;
    writeln!(out)?;

    Ok(())
}

fn state_dir() -> Result<PathBuf, CliError> {
    if let Some(path) = env::var_os("CHAUFFEUR_CORE_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }

    let home = env::var_os("HOME").ok_or(CliError::MissingHome)?;
    Ok(PathBuf::from(home).join(".local/state/chauffeur-core"))
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::io;
    use std::str;

    use chauffeur_judgment::{AuditError, RequestError};
    use serde::de::Error as _;

    use super::CliError;

    /// The error's Display, its Debug, and its source's Display.
    fn texts(error: &CliError) -> (String, String, Option<String>) {
        (
            error.to_string(),
            format!("{error:?}"),
            error.source().map(ToString::to_string),
        )
    }

    #[test]
    fn usage_prints_the_usage_without_a_source() {
        let usage = "usage: chauffeur-core judge | rules | audit [N] | version";

        assert_eq!(
            texts(&CliError::Usage),
            (usage.to_owned(), usage.to_owned(), None),
        );
    }

    #[test]
    fn count_prints_the_parse_error_and_the_usage() {
        let parsed = "x".parse::<usize>();
        let error = CliError::from(parsed.expect_err("x is not a count"));
        let text = "invalid audit count: invalid digit found in string\n\
                    usage: chauffeur-core judge | rules | audit [N] | version";

        assert_eq!(
            texts(&error),
            (
                text.to_owned(),
                text.to_owned(),
                Some("invalid digit found in string".to_owned()),
            ),
        );
    }

    #[test]
    fn missing_home_names_both_variables_without_a_source() {
        let text = "set CHAUFFEUR_CORE_STATE_DIR or HOME";

        assert_eq!(
            texts(&CliError::MissingHome),
            (text.to_owned(), text.to_owned(), None),
        );
    }

    #[test]
    fn io_prints_and_keeps_the_io_error() {
        let error = CliError::from(io::Error::other("disk full"));

        assert_eq!(
            texts(&error),
            (
                "disk full".to_owned(),
                "disk full".to_owned(),
                Some("disk full".to_owned()),
            ),
        );
    }

    #[test]
    fn utf8_prints_and_keeps_the_utf8_error() {
        let bytes = vec![b'a', 0xff];
        let error =
            CliError::from(str::from_utf8(&bytes).expect_err("0xff is not UTF-8"));
        let inner = "invalid utf-8 sequence of 1 bytes from index 1";
        let text = format!("request is not UTF-8: {inner}");

        assert_eq!(texts(&error), (text.clone(), text, Some(inner.to_owned())));
    }

    #[test]
    fn request_prints_and_keeps_the_request_error() {
        let error = CliError::from(RequestError::Invalid("bad".to_owned()));
        let text = "invalid request: bad";

        assert_eq!(
            texts(&error),
            (text.to_owned(), text.to_owned(), Some(text.to_owned())),
        );
    }

    #[test]
    fn audit_prints_and_keeps_the_audit_error() {
        let error = CliError::from(AuditError::Limit {
            name: "record bytes",
            max: 65_536,
        });
        let text = "record bytes exceeds 65536";

        assert_eq!(
            texts(&error),
            (text.to_owned(), text.to_owned(), Some(text.to_owned())),
        );
    }

    #[test]
    fn json_prints_and_keeps_the_json_error() {
        let error = CliError::from(serde_json::Error::custom("bad json"));
        let text = "bad json";

        assert_eq!(
            texts(&error),
            (text.to_owned(), text.to_owned(), Some(text.to_owned())),
        );
    }
}
