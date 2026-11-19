# sourcefed events

v1 decides which sourcefed events reach the agent when sourcefed's OpenCode
plugin asks it. In v2, sourcefed's daemon asks before it queues each event,
so OpenCode, Pi, MCP and the CLI receive the same filtered stream. The
question is the `sourcefed/triage` rule, a file sourcefed installs.
[sourcefed](../integrations/sourcefed.md) has the setup, a request to try and
the rule itself; the v2 column is what `chauffeur-next` `1.0.0-rc.5` does.

| v1                                                            | v2                                                                                        |
| ------------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| sourcefed's OpenCode plugin calls the `chauffeur.gate` RPC    | sourcefed's daemon navigates `sourcefed.event` before it queues the event, for every host |
| The question in `skills/config/event-gate.json`               | The `screen` step of `~/.config/chauffeur-next/rules/sourcefed/triage.json`               |
| One situation text, told what the monitor watches             | The `event`, `thread` and `monitor` evidence blocks                                       |
| Withholds only on a confident no (P ≤ 0.3)                    | `confidence: 0.7`, and `then` withholds on `no`; `yes`, `unsure` and a failure deliver    |
| `withhold_bodies`: `[message without text]`, withheld unasked | The rule's instructions call it noise, and the judge answers                              |
| `POST /integrations/sourcefed`, for other programs            | [`chauffeur-next navigate`](../cli/navigate.md) or the client, with the same event        |
| sourcefed delivers when Chauffeur fails or takes over 3 s     | The same                                                                                  |

## Moving over

```sh
sourcefed setup chauffeur        # writes rules/sourcefed/triage.json
chauffeur-next daemon status     # "rules" lists "sourcefed/triage"
```

`sourcefed setup chauffeur` never replaces a rule file that is there, so a
`confidence` or `instructions` you change in it stays. A question you
override in `~/.config/chauffeur/event-gate.json` moves into the `screen`
step's `instructions` by hand.

v1 and v2 run side by side. sourcefed marks each event v2 triaged, and its
OpenCode bridge skips v1's `chauffeur.gate` for a marked event and asks it
for an unmarked one, so no event is judged by both. With no v2 daemon
answering, every event bound for OpenCode goes to v1's gate.
[sourcefed's Chauffeur v2 triage](https://github.com/wrkhse/sourcefed/blob/main/packages/daemon/src/triage/README.md)
has the marking, and [slice 2](../plans/slices/slice-2.md) the wire contract.

## Monitors

v1 also has sourcefed follow the pull request, Jira issue or Slack thread a
session works on. `1.0.0-rc.5` has no part that does, so monitors stay with
v1; [#47](https://github.com/wrkhse/chauffeur/issues/47) brings them.
