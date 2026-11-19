# `tiers`

The four kinds of judge a step can name, and how a tier's models answer as
one core `Judge`. A tier's API formats live in its folder, such as `system_one/`.

| Path          | Holds                                                                                                                                                                                                               |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `mod.rs`      | `Tier`: `facts`, `system-one`, `system-two`, `person`                                                                                                                                                               |
| `fallback.rs` | `Fallback`: a tier's models in order, built from `models.json` and the keys found. The next model answers only what the one before could not, and every model's work ends at the round's deadline                   |
| `judge.rs`    | `NamedJudge`: a `Fallback`, a `Confidence`, the run's evidence and its deadline, as a core `Judge`                                                                                                                  |
| `system_one/` | Fast classifiers. The `systemone` format asks a yes/no question as `noul` and any other as `choice` or `score`, as the model's entry says: [system_one](system_one/README.md) has its files, its entry and its wire |

`system-one` is the one tier with a format of its own. A step on
`system-two`, `facts` or `person` is answered only by a placeholder listed
under that tier in `models.json`, and with none it fails as unavailable. Their
formats are planned in
[#51](https://github.com/wrkhse/chauffeur/issues/51) (`system-two`),
[#46](https://github.com/wrkhse/chauffeur/issues/46) (`facts`) and
[#47](https://github.com/wrkhse/chauffeur/issues/47) (`person`).

A step reaches a model through four layers, each knowing only the one below:
the rule's named judge, such as `"quick": { "tier": "system-one" }`; the
tier's models in `models.json`'s `tiers`; each model's entry in its `models`;
and the entry's format, which turns questions into probabilities.

```mermaid
flowchart LR
    Steps["A judge's questions"] --> M1["First model"]
    M1 -->|"answered or invalid"| Done["Result"]
    M1 -->|"unavailable, rate-limited, timed out"| Left{"Time left before the deadline?"}
    M1 -->|"cut off by the deadline"| Deadline["Failure::Deadline"]
    Left -->|"yes"| M2["Next model"]
    Left -->|"no"| Deadline
    M2 --> Done
    M2 -->|"none left"| Unavailable["Failure::Unavailable"]
    Done --> Settle["NamedJudge settles with its Confidence"]
```

```rust
use std::time::{Duration, Instant};

use chauffeur_core::Panel;
use chauffeur_judges::{Fallback, NamedJudge, Tier};

let keys = |env: &str| std::env::var(env).ok();
let models = Fallback::build(&loaded, Tier::SystemOne, None, keys)?;

let deadline = Instant::now().checked_add(Duration::from_secs(5));
let judge = NamedJudge::new(&models, &confidence, &evidence)
    .with_deadline(deadline);

let run = plan.run(&Panel::new().with("quick", &judge))?;
```

- A judge that names a `model` asks it first, then the tier's models in
  order, each once. A model that is not registered is passed over, so a rule
  shared with someone who lacks it runs on their tier.
- A model whose key is missing is left out, so a shipped model starts
  answering once its key is set.
- An invalid reply does not fall back: the model answered badly, and asking
  another would hide it.
- Without the `systemone` feature, as in the browser build, `systemone`
  models are left out as if their key were missing.
- A judge's deadline is one moment that all its models share. Each model is
  asked only while time is left, and one call takes at most the shorter of
  the model's `timeout_ms` and the time left, from connecting to reading the
  whole reply.
- A model that runs out its own timeout with time left fails as timed out,
  and the next model answers. Once the deadline passes, the questions still
  open fail as `Failure::Deadline` and no further model is asked; answers
  and failures already given stand.
- `Fallback::answer_all_until` and `Format::answer_all_until` take the
  deadline; `answer_all` is the same call with none. The format's default
  refuses a round whose deadline has passed and otherwise calls
  `answer_all`, which it cannot interrupt, so a format that blocks, such as
  `systemone`, overrides it to end its work at the deadline.
- With no deadline, as `NamedJudge::new` gives, nothing reads the clock, which
  the browser build does not have.
