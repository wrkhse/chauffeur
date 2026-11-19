# CLI

One binary runs the daemon and talks to it. Its name is the installation's:
`chauffeur`, or `chauffeur-next` for a release candidate, which keeps its own
socket, configuration and audit log beside an installed `chauffeur`.

| Command                                    | Does                                                                      |
| ------------------------------------------ | ------------------------------------------------------------------------- |
| [`daemon run`, `daemon status`](daemon.md) | Runs the daemon; reports its version, loaded rules and refused rule files |
| [`emit`](emit.md)                          | Sends one harness event and prints what the rules returned                |
| [`audit`](audit.md)                        | Prints the runs in the audit log, and follows it                          |
| [`navigate`](navigate.md)                  | Sends one request as a caller does and prints the messages back           |
| [`version`](#version)                      | Prints the installation's name and version                                |

## `version`

Prints the installation's name and version, from the binary itself, with no
daemon needed. The name is the binary's file name.

```sh
chauffeur-next version   # chauffeur-next 1.0.0-rc.5
```

## Where things are

| What               | Where, for the name `chauffeur`                                                    | Set with                                                 |
| ------------------ | ---------------------------------------------------------------------------------- | -------------------------------------------------------- |
| The socket         | `$XDG_RUNTIME_DIR/chauffeur/daemon.sock`, or under `$TMPDIR`                       | `CHAUFFEUR_SOCKET`                                       |
| Your configuration | `~/.config/chauffeur/`: `models.json`, `projects.json`, `rules/`, `redaction.json` | `XDG_CONFIG_HOME`                                        |
| Shipped content    | `share/chauffeur/` beside the binary's folder: `models/`, `rules/`, `safety/`      | `CHAUFFEUR_DISTRIBUTION`, or `daemon run --distribution` |
| OpenCode adapter   | `share/chauffeur/opencode/`, the folder OpenCode's `plugins` names                 | Where the release is unpacked                            |
| The audit log      | `~/.local/state/chauffeur/audit.jsonl`                                             | `XDG_STATE_HOME`                                         |
| Model keys         | The environment the daemon runs in, such as `TYPESAFE_API_KEY` for Jev             | Each model's `key` in `models.json`                      |

Commands exit 0 on success and normally exit 1 on failure. The
[`navigate`](navigate.md) command also uses exit 2 for daemon and stdout
errors and treats a closed pipe as success.
