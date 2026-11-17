# `route`

Sends an event, or a navigation, to every rule that hooks it, runs them on the
router's pool of workers, and appends what each did to the audit log. An
inline strategy runs on its own and returns its verdict.

| File           | Holds                                                                                          |
| -------------- | ---------------------------------------------------------------------------------------------- |
| `mod.rs`       | `Router`: the loaded configuration, admission, `emit`, `replace` on reload, and one rule's run |
| `navigate.rs`  | `Router::navigate`: intents from every hooked rule, or an inline strategy's verdict            |
| `cooldown.rs`  | `Cooldowns`: when each rule last returned something in each session, bounded to 4,096 pairs    |
| `log_entry.rs` | How a run ended, and each step's answer or failure, as JSON for the audit log                  |

```mermaid
sequenceDiagram
    participant S as socket
    participant R as Router
    participant C as Cooldowns
    participant W as runtime run_strategy
    participant A as Audit
    S->>R: admit, then emit or navigate
    R->>R: rules whose on and when match
    R-->>S: rule_matched, as each run starts
    R->>C: cooling for this rule and session?
    C-->>R: skip, logged as cooldown
    R->>W: run the rest on the pool, until the deadline or a cancel
    W-->>R: verdict, refused, busy, stopped, deadline or cancelled
    R-->>S: round_ready and step_settled, for a navigation
    R->>C: fire when a rule returned something
    R->>A: one line per run or skip, in hook order
    R-->>S: actions, or intents
```

```rust
use std::time::Duration;

use chauffeur_daemon::audit::{Audit, MAX_BYTES};

let router = Router::new(loaded, Audit::new(&paths.state, MAX_BYTES));

let permit = router.admit(Source::Harness)?;
let actions = router.emit(
    &event,
    Duration::from_millis(1500),
    &Cancel::new(Source::Harness),
);

let permit = router.admit(Source::Integration)?;
let intents = router.navigate(request, &cancel, &|event| send(event))?;
```

- Rules run on the router's own pool: at most 32 at once, with up to 8
  kept waiting between calls, so a project with hundreds of rules starts no
  thread of its own. A rule that finds every worker busy waits for one to
  finish; one still waiting when the call's deadline passes does not run and
  is logged as skipped, and a run already going reports just after it.
- A rule's `rule_matched` is sent as its run starts, not when it matches:
  a rule in cooldown, and one no worker starts before the deadline, is not
  announced.
- A rule no worker starts before the deadline earns a line of its own,
  `{"rule": ..., "event": ..., "session": ..., "skipped": "deadline"}`, in
  hook order among the runs, and returns nothing; the wire has no event for
  it.
- The audit log keeps hook order, one line per run or skipped rule, while a
  navigation's `chauffeur.event` progress streams as each run settles.
- A run whose line cannot be written still returns its action, and the
  daemon says so on stderr.
- A rule that misses the deadline, or fails, returns nothing; the other
  rules' intents still return.
- A navigation streams at most 4,096 events; its intents always return.
- Past 4,096 rule and session pairs, the oldest cooldown is forgotten.
  Cooldowns start afresh with each daemon.
- A reload swaps the configuration whole; a run in progress keeps the one
  it started with.
