use std::time::Duration;

use chauffeur_protocol::{Event, Navigate, Rejection};
use chauffeur_runtime::{Busy, Cancel, Capacity, Permit, Source};
use serde_json::{Value, json};

use crate::route::Router;

/// The longest a caller may give rules to answer.
const MAX_DEADLINE: Duration = Duration::from_secs(30);
const DEFAULT_DEADLINE: Duration = Duration::from_secs(3);

const PARSE_ERROR: i64 = -32_700;
const INVALID_REQUEST: i64 = -32_600;
const METHOD_NOT_FOUND: i64 = -32_601;
const INVALID_PARAMS: i64 = -32_602;

/// The notification that cancels a request on the same connection.
pub const CANCEL: &str = "$/cancelRequest";

/// One JSON-RPC 2.0 request or notification, read from a line.
pub struct Request {
    /// Its ID; a notification has none and gets no reply.
    pub id: Option<Value>,
    /// The method it calls.
    pub method: String,
    /// Its params, or null.
    pub params: Value,
}

/// A request read from `line`, or the error reply for a line that is not one.
pub fn parse(line: &str) -> Result<Request, Value> {
    let request: Value =
        serde_json::from_str(line).map_err(|_| error(&Value::Null, PARSE_ERROR, "not JSON"))?;
    let id = request.get("id").cloned();
    if id
        .as_ref()
        .is_some_and(|id| !id.is_string() && id.as_i64().is_none() && id.as_u64().is_none())
    {
        return Err(error(
            &Value::Null,
            INVALID_REQUEST,
            "id must be a string or an integer; null is refused",
        ));
    }
    let text = |field: &str| request.get(field).and_then(Value::as_str);
    let (Some("2.0"), Some(method)) = (text("jsonrpc"), text("method")) else {
        return Err(error(
            &id.unwrap_or(Value::Null),
            INVALID_REQUEST,
            "not a JSON-RPC 2.0 request",
        ));
    };
    Ok(Request {
        method: method.to_owned(),
        params: request.get("params").cloned().unwrap_or(Value::Null),
        id,
    })
}

/// A call checked and admitted, ready to run on a thread of its own.
pub struct Call {
    id: Option<Value>,
    work: Work,
    /// The call's place for its source; none for a call that needs no place.
    permit: Option<Permit>,
}

enum Work {
    Emit(Event, Duration),
    Navigate(Navigate),
    Status,
}

/// Checks `request`'s params, then admits it for its source, so a call that
/// is malformed, over a limit or busy is answered at once, before any thread
/// starts.
///
/// # Errors
///
/// The error reply to send.
pub fn prepare(router: &Router, request: Request) -> Result<Call, Value> {
    let id = request.id;
    let refuse = |error: Value| json!({ "jsonrpc": "2.0", "id": id, "error": error });
    let (work, source) = match request.method.as_str() {
        "chauffeur.emit" => (
            Work::Emit(
                event(&request.params).map_err(refuse)?,
                deadline(&request.params),
            ),
            Some(Source::Harness),
        ),
        "chauffeur.navigate" => {
            let navigate = Navigate::parse(&request.params)
                .map_err(|rejection| refuse(rejection.to_error()))?;
            (Work::Navigate(navigate), Some(Source::Integration))
        }
        "chauffeur.status" => (Work::Status, None),
        other => {
            return Err(refuse(error_object(
                METHOD_NOT_FOUND,
                &format!("no method `{other}`"),
            )));
        }
    };
    let permit = source
        .map(|source| router.admit(source))
        .transpose()
        .map_err(|busy| refuse(busy_error(busy)))?;
    Ok(Call { id, work, permit })
}

impl Call {
    /// Runs the call, with `notify` given each notification it streams, and
    /// returns its reply and the permit for its place.
    ///
    /// The caller drops the permit once the call is done, before the reply
    /// is written: a worker still running keeps its own copy, so the place
    /// may outlive that drop. A call cancelled while it ran, by
    /// `$/cancelRequest` or its connection's end, is answered with
    /// [`Rejection::Cancelled`] whatever its method.
    pub fn answer(
        self,
        router: &Router,
        cancel: &Cancel,
        notify: &(dyn Fn(Value) + Sync),
    ) -> (Option<Value>, Option<Permit>) {
        let id = self.id;
        let cancel = self
            .permit
            .as_ref()
            .map_or_else(|| cancel.clone(), |permit| cancel.admitted(permit.clone()));
        let result = match self.work {
            Work::Emit(event, deadline) => {
                Ok(json!({ "actions": router.emit(&event, deadline, &cancel) }))
            }
            Work::Navigate(request) => {
                let progress = |mut event: Value| {
                    if let Some(fields) = event.as_object_mut() {
                        fields.insert("id".into(), id.clone().unwrap_or(Value::Null));
                    }
                    notify(
                        json!({ "jsonrpc": "2.0", "method": "chauffeur.event", "params": event }),
                    );
                };
                router
                    .navigate(request, &cancel, &progress)
                    .map_err(|rejection| rejection.to_error())
            }
            Work::Status => Ok(status(router)),
        };
        let reply = if cancel.is_cancelled() {
            json!({ "jsonrpc": "2.0", "id": id, "error": Rejection::Cancelled.to_error() })
        } else {
            match result {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
            }
        };
        drop(cancel);
        (id.is_some().then_some(reply), self.permit)
    }
}

fn event(params: &Value) -> Result<Event, Value> {
    let event = params
        .get("event")
        .ok_or_else(|| error_object(INVALID_PARAMS, "no `event`"))?;
    Event::parse(&event.to_string())
        .map_err(|error| error_object(INVALID_PARAMS, &error.to_string()))
}

fn deadline(params: &Value) -> Duration {
    params
        .get("deadline_ms")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_DEADLINE, Duration::from_millis)
        .min(MAX_DEADLINE)
}

fn status(router: &Router) -> Value {
    let loaded = router.loaded();
    let rules: Vec<&str> = loaded
        .iter()
        .flat_map(|loaded| loaded.rules.iter().map(|rule| rule.id()))
        .collect();
    let refused = loaded
        .as_ref()
        .map_or_default(|loaded| loaded.refused.clone());
    json!({ "version": env!("CARGO_PKG_VERSION"), "protocol": 1, "rules": rules, "refused": refused })
}

fn error_object(code: i64, message: &str) -> Value {
    json!({ "code": code, "message": message })
}

/// `Rejection::Busy`'s error, with a message naming the limit that was
/// reached and its number: the caller's share of calls, or the daemon's
/// capacity. `Router` admits against `Capacity::DAEMON`, so these are its
/// numbers.
fn busy_error(busy: Busy) -> Value {
    let capacity = Capacity::DAEMON;
    let message = if busy.share {
        format!(
            "the daemon is busy: this caller's share of {} calls is in use",
            capacity.per_source
        )
    } else {
        format!(
            "the daemon is busy: the daemon's capacity of {} calls is in use",
            capacity.global
        )
    };

    let mut error = Rejection::Busy.to_error();
    if let Some(fields) = error.as_object_mut() {
        fields.insert("message".into(), json!(message));
    }

    error
}

/// An error reply to `id`.
pub fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": error_object(code, message) })
}

/// The error reply to a call whose ID is already open on its connection.
pub fn duplicate(id: &Value) -> Value {
    error(
        id,
        INVALID_REQUEST,
        "a call with this id is already open on this connection",
    )
}

#[cfg(test)]
mod tests {
    use chauffeur_runtime::{Busy, Source};

    use super::busy_error;

    /// A busy error carries `Rejection::Busy`'s code and reason, and a
    /// message naming the limit that was reached and its number.
    #[test]
    fn a_busy_error_names_the_limit_that_was_reached() {
        let share = busy_error(Busy {
            from: Source::Integration,
            share: true,
        });
        let capacity = busy_error(Busy {
            from: Source::Harness,
            share: false,
        });

        assert_eq!(share["code"], -32_000);
        assert_eq!(share["data"]["reason"], "busy");
        assert_eq!(
            share["message"],
            "the daemon is busy: this caller's share of 8 calls is in use"
        );
        assert_eq!(capacity["code"], -32_000);
        assert_eq!(capacity["data"]["reason"], "busy");
        assert_eq!(
            capacity["message"],
            "the daemon is busy: the daemon's capacity of 16 calls is in use"
        );
    }
}
