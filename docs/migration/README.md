# Moving from v1

Chauffeur v2 installs beside v1. The release candidate is the binary
`chauffeur-next`, with its own socket, `~/.config/chauffeur-next/` and audit
log, so v1's `chauffeur` keeps running while you move one piece at a time.
v2 reads no v1 file, so nothing is converted behind your back.

`chauffeur-next` `1.0.0-rc.5` runs the parts of v1 below. Each page shows
v1's file, variable or command beside the v2 one that replaces it, and its v2
column is what `1.0.0-rc.5` does.

| v1 feature                              | v2                                                        | Page                                   |
| --------------------------------------- | --------------------------------------------------------- | -------------------------------------- |
| Steering after a tool call, in OpenCode | Rule files on `tool.after`; `conventional-commits` ships  | [Rule files: v1 to v2](rule-format.md) |
| Judge choice and fallback               | `models.json`, and the judge each step names              | [Judges](judges.md)                    |
| Redaction                               | The same prefixes and shapes, before any model call       | [Redaction](redaction.md)              |
| sourcefed's event gate                  | The `sourcefed/triage` rule, asked by sourcefed's daemon  | [sourcefed events](sourcefed.md)       |
| Audit feed, rule validation             | `chauffeur-next audit` and `chauffeur-next daemon status` | [Seeing what it decided](audit.md)     |

Everything else stays with v1. Each part has the issue that brings it:

- steering at the end of a turn, permissions and shell guards:
  [#47](https://github.com/wrkhse/chauffeur/issues/47);
- skills, tools and `ask_chauffeur`:
  [#48](https://github.com/wrkhse/chauffeur/issues/48);
- model failover, context renewal and warming:
  [#49](https://github.com/wrkhse/chauffeur/issues/49);
- `judge.json`'s System Two profiles:
  [#51](https://github.com/wrkhse/chauffeur/issues/51);
- rulebooks: [#52](https://github.com/wrkhse/chauffeur/issues/52);
- sourcefed monitors: [#47](https://github.com/wrkhse/chauffeur/issues/47).

v1 goes away at cutover, [#53](https://github.com/wrkhse/chauffeur/issues/53),
which holds the cutover prompt.
[The v2 plan](https://github.com/wrkhse/chauffeur/issues/43) holds them all,
each with its status.

## Where configuration goes

Shipped content is `share/chauffeur-next/` beside the binary's `bin/` folder.
[Where things are](../cli/README.md#where-things-are) lists every v2 path and
the variable that moves it.

| v1                                            | v2                                                                         |
| --------------------------------------------- | -------------------------------------------------------------------------- |
| `skills/rules/conventional-commits.json`      | `rules/conventional-commits.json` in the shipped content                   |
| `$CHAUFFEUR_CONFIG_DIR/rules/`                | `~/.config/chauffeur-next/rules/`                                          |
| `.chauffeur/rules/` in a project              | The same folder, read only for a `navigate` request that names the project |
| `skills/safety/redaction.json`                | `safety/redaction.json` in the shipped content                             |
| `$CHAUFFEUR_CONFIG_DIR/redaction.json`        | `~/.config/chauffeur-next/redaction.json`                                  |
| `CHAUFFEUR_JUDGE`, `CHAUFFEUR_JUDGE_FALLBACK` | `~/.config/chauffeur-next/models.json`                                     |
| `TYPESAFE_BASE_URL`, `TYPESAFE_DEFAULT_MODEL` | The `jev` entry in `models.json`                                           |
| `TYPESAFE_API_KEY`                            | The same variable, in the environment the daemon starts in                 |
| `$CHAUFFEUR_STATE_DIR/audit.jsonl`            | `~/.local/state/chauffeur-next/audit.jsonl`                                |
| `CHAUFFEUR_CONFIG_DIR`, `CHAUFFEUR_STATE_DIR` | `XDG_CONFIG_HOME`, `XDG_STATE_HOME`                                        |
| `CHAUFFEUR_IDLE_STEERING=true`                | No setting: a message with `"delivery": "wake"` starts a turn when idle    |

A harness event, such as a tool call in OpenCode, runs the shipped rules and
the ones in `~/.config/chauffeur-next/rules/`, so a project's own tool-call
rule stays with v1.

## Moving your own rules

1. Install `chauffeur-next`, start its daemon and add its adapter to
   OpenCode, as in the [README](../../README.md#install).
2. Rewrite each v1 rule as a v2 file under `~/.config/chauffeur-next/rules/`,
   following [Rule files: v1 to v2](rule-format.md). Its path there is its
   ID, so `rules/team/release.json` is `team/release`.
3. Check that the daemon loaded it, then send it an event by hand:

   ```sh
   chauffeur-next daemon status   # "rules" lists it, or "refused" says why

   echo '{ "event": "tool.after", "session": "s1", "tool": "shell",
           "input": { "command": "git commit -m \"fixed stuff\"" } }' \
     | chauffeur-next emit

   chauffeur-next audit 1 --brief
   ```

4. Remove the v1 file from `~/.config/chauffeur/rules/` once the v2 rule
   answers as you want, so the agent hears each correction once.

A rule that uses a v1 field with no v2 counterpart, listed under
[Field by field](rule-format.md#field-by-field), stays in v1.
