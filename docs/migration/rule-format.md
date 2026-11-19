# Rule files: v1 to v2

A v1 rule and a v2 rule say the same things: which event, which cheap
filter, which yes/no questions, and what to deliver. A v2 rule also names the
judge and its confidence, makes every question a step in a strategy, and
holds every word the judge is told. The v2 column is what `chauffeur-next`
`1.0.0-rc.5` loads; the whole grammar is in
[`rule`](../../crates/protocol/src/rule/README.md).

## Field by field

| v1                                         | v2                                                                                               |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------ |
| `schema_version`, `id`, `name`             | None: the ID is the file's path under `rules/` without `.json`, such as `team/release`           |
| `"on": "tool_result"`                      | `"on": "tool.after"`                                                                             |
| `when.tools`                               | `when.tools`                                                                                     |
| None                                       | `when.input.contains`: one string that some text in the call's input holds, such as `git commit` |
| A step's `question`                        | A step's `ask`: `"outcomes": ["yes", "no"]`, its `instructions`, and the `evidence` it sees      |
| `yes_at_or_above`                          | The `confidence` of the judge the step names; `then` acts on `yes`, so `0.7` reads as v1's 0.7   |
| `minimum_confidence`                       | None: with `yes` at 0.7 or more, its margin over `no` is 0.4 or more                             |
| A second step, asked once the first holds  | Every step is asked in one round, and a `then` entry's `if` names each answer it needs           |
| `then.delivery`: `steer`, `resume`, `wait` | `message.append` with `"delivery"`: `"now"`, `"wake"`, `"next"`                                  |
| `then.text`                                | `"parts": [{ "text": "…" }]`                                                                     |
| None                                       | `"label"`: one line of at most 100 characters, shown as `Chauffeur · <rule>: <label>`            |
| `cooldown_seconds`                         | `cooldown_seconds`, for each rule and session                                                    |
| `rules.json` `max_deliveries: 2`           | None: every rule whose `then` holds returns its message                                          |

A rule file with a field v2 does not know is refused, and
`chauffeur-next daemon status` says which. A rule that needs one of these v1
fields stays in v1, each with the issue that brings it:

- `when.input_excludes`, `when.result_includes` and `when.session_kind`:
  [#47](https://github.com/wrkhse/chauffeur/issues/47);
- `when.history` with `tools_called`, `tools_called_any` and
  `tools_not_called`, and `when.status`, `when.source` and `when.hooks`:
  [#47](https://github.com/wrkhse/chauffeur/issues/47);
- `priority` and `once`: [#47](https://github.com/wrkhse/chauffeur/issues/47);
- `then.skill`: [#48](https://github.com/wrkhse/chauffeur/issues/48);
- `"on": "turn_end"`, since the OpenCode adapter sends `tool.after` alone:
  [#47](https://github.com/wrkhse/chauffeur/issues/47).

### A step that waits on another

A v2 step waits on an earlier answer with `after` and `after_any`, such as a
second model asked only when the first is near its bar. `1.0.0-rc.5` refuses
a step that has either; the next release candidate accepts them. Its shipped
`conventional-commits` rule uses them, asking GLiDE only when Jev is near its bar
or fails, and keeping Jev's `yes` when GLiDE fails.
[`rule`](../../crates/protocol/src/rule/README.md) has their grammar and
[slice 3](../plans/slices/slice-3.md) their design.

## Evidence

v1 renders one situation text for every question. v2 hands a step only the
blocks its `ask` names, redacted, and leaves out a name the event lacks.

| Event                                       | Blocks                                                                                                                                  |
| ------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `tool.after`, from the OpenCode adapter     | `tool_call` (the tool and its input), `tool_output` (the result's text, at most 64 KiB), and each other string field, such as `session` |
| A caller's event, such as `sourcefed.event` | Each key of the request's `evidence`, such as `event`, `thread` and `monitor`                                                           |

v1 adds "A call the user explicitly asked for, exactly as asked, does not
count" to every tool-result question out of sight, and shows the judge the
user's latest request. v2 adds nothing to a rule's `instructions`, and
`1.0.0-rc.5` has no block that holds the user's request. A rule that should
spare what the user asked for says so in its message, as the shipped rule
does: "Unless the user chose the message, amend it …".

## Worked example: `conventional-commits`

v1, `skills/rules/conventional-commits.json`:

```json
{
  "schema_version": 2,
  "id": "conventional-commits",
  "name": "Conventional commit messages",
  "on": "tool_result",
  "when": {
    "tools": ["shell", "execute"],
    "input_excludes": ["feat:", "feat(", "fix:", "fix(", "…"]
  },
  "steps": [
    {
      "id": "nonconventional",
      "question": "Does this call create a git commit whose message does not follow the Conventional Commits format (…)? …",
      "yes_at_or_above": 0.7,
      "minimum_confidence": 0.4
    }
  ],
  "then": {
    "delivery": "steer",
    "text": "Chauffeur: Use Conventional Commits for commit messages, …"
  },
  "cooldown_seconds": 120
}
```

v2, `rules/conventional-commits.json` in the shipped content:

```json
{
  "on": "tool.after",
  "when": { "tools": ["shell"], "input": { "contains": "git commit" } },
  "judges": { "quick": { "tier": "system-one", "confidence": 0.7 } },
  "strategy": [
    {
      "id": "nonconventional",
      "judge": "quick",
      "ask": {
        "outcomes": ["yes", "no"],
        "evidence": ["tool_call"],
        "instructions": "Does this call create a git commit whose message does not follow Conventional Commits?"
      }
    }
  ],
  "then": [
    {
      "if": { "nonconventional": "yes" },
      "message.append": {
        "label": "commit message isn't conventional; asking the agent to amend it",
        "parts": [
          {
            "text": "Chauffeur, which the user set up to check commits: this message does not follow Conventional Commits. Unless the user chose the message, amend it to `type(scope): description`, such as `fix(parser): handle empty input`."
          }
        ],
        "delivery": "now"
      }
    }
  ],
  "cooldown_seconds": 120
}
```

- The filter admits every `shell` call whose input holds `git commit`. v1's
  list of conventional prefixes has no v2 field, so Jev reads each commit.
- `yes_at_or_above` is the `quick` judge's `confidence`, and `then` names
  the answer it acts on.
- The message has a `label`, which OpenCode shows on the message's row, and
  carries the exception for a message the user chose.

With the daemon running where `TYPESAFE_API_KEY` is set, send the rule a
commit by hand:

```sh
echo '{ "event": "tool.after", "session": "s1", "tool": "shell",
        "input": { "command": "git commit -m \"fixed stuff\"" } }' \
  | chauffeur-next emit
```

```json
{
  "actions": [
    {
      "rule": "conventional-commits",
      "action": "message.append",
      "payload": {
        "label": "commit message isn't conventional; asking the agent to amend it",
        "parts": [
          { "text": "Chauffeur, which the user set up to check commits: …" }
        ],
        "delivery": "now"
      }
    }
  ]
}
```

`actions` is empty when Jev answers `no` or `unsure`, fails, or the rule is
cooling down; [`chauffeur-next audit`](../cli/audit.md) says which. Your own rule
goes in `~/.config/chauffeur-next/rules/`, and the daemon loads it for the
next event.
