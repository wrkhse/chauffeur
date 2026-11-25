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

    use chauffeur_judgment::RequestError;

    use super::CliError;

    fn source_text(error: &CliError) -> Option<String> {
        error.source().map(ToString::to_string)
    }

    #[test]
    fn keeps_the_request_error_as_its_source() {
        let error = CliError::from(RequestError::Invalid("bad".to_owned()));

        assert_eq!(source_text(&error), Some("invalid request: bad".to_owned()));
    }

    #[test]
    fn keeps_the_io_error_as_its_source() {
        let error = CliError::from(io::Error::other("disk full"));

        assert_eq!(source_text(&error), Some("disk full".to_owned()));
    }

    #[test]
    fn usage_has_no_source() {
        assert_eq!(source_text(&CliError::Usage), None);
    }
}
