# Seeing what it decided

v2 keeps v1's audit command, with a log of its own, and has no validation
command: a rule is checked when it loads. The v2 column is what
`chauffeur-next` `1.0.0-rc.5` does, and each command has its page under
[CLI](../cli/README.md).

| v1                                           | v2                                                                                           |
| -------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `chauffeur audit [N]`, `--follow`, `--brief` | [`chauffeur-next audit`](../cli/audit.md), with the same arguments                           |
| `$CHAUFFEUR_STATE_DIR/audit.jsonl`           | `~/.local/state/chauffeur-next/audit.jsonl`, moved aside past 8 MiB as v1's is               |
| One line for each signal, with its effects   | One line for each rule's run: its `rule`, `event`, `session`, `steps` and what it `returned` |
| A line's `judges`: who answered each round   | Each step's `answer`: its `model`, whether it was a `placeholder`, its `p` and its `level`   |
| `chauffeur skill validate PATH`              | [`chauffeur-next daemon status`](../cli/daemon.md): `refused` lists each file and why        |
| `chauffeur signal --file probe.json`         | [`chauffeur-next emit`](../cli/emit.md), with the event on stdin                             |
| `chauffeur health`                           | `chauffeur-next daemon status`, which prints the daemon's version                            |

The two logs are separate files, so `chauffeur audit` shows what v1 decided
and `chauffeur-next audit` what v2 did:

```sh
chauffeur-next audit --follow --brief
```

```text
13:54:43  conventional-commits  s1  nonconventional yes 0.93 jev  312 ms  → message.append
```

v1's `chauffeur skill validate-project` and `chauffeur skills` have no v2
command in `1.0.0-rc.5`. Checking a project's rules is in
[#47](https://github.com/wrkhse/chauffeur/issues/47), and the skill catalog in
[#48](https://github.com/wrkhse/chauffeur/issues/48).
