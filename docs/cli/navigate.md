# `chauffeur navigate`

Sends one `chauffeur.navigate` request, read as JSON on stdin, as a caller
does, and prints each JSON-RPC message that comes back, one per line: the
run's events as they happen, then the intents or, for an inline strategy,
the verdict. For hook scripts, Git hooks and CI on the same machine.

A `chauffeur.event` carries, in `params`, the `id` of the call that produced
it, added by the daemon, so events from two calls on one connection stay
apart.

A rule that starts and is then cut short settles each of its steps with
`failed` naming why: `deadline`, `cancelled`, `busy`, `stopped` or
`refused`.

```sh
chauffeur navigate <<'EOF'
{ "protocol": 1, "deadline_ms": 3000, "on": "sourcefed.event",
  "evidence": { "event": "thanks, looks good 👍", "monitor": "PR #42" } }
EOF
```

```text
{"jsonrpc":"2.0","method":"chauffeur.event","params":{"event":"rule_matched","id":1,"rule":"sourcefed/triage"}}
{"jsonrpc":"2.0","method":"chauffeur.event","params":{"event":"round_ready","id":1,"round":1,"rule":"sourcefed/triage","steps":["screen"]}}
{"jsonrpc":"2.0","method":"chauffeur.event","params":{"answer":{"level":0.9,"model":"jev","outcome":"no",…},"event":"step_settled","id":1,"rule":"sourcefed/triage","step":"screen"}}
{"id":1,"jsonrpc":"2.0","result":{"intents":[{"rule":"sourcefed/triage","withhold":"event"}]}}
```

| Exit | Means                                                                      |
| ---- | -------------------------------------------------------------------------- |
| 0    | Intents or a verdict came back, or the stdout pipe closed                  |
| 1    | The request was rejected or stdin failed                                   |
| 2    | The daemon is unavailable, speaks another protocol, or stdout write failed |

Rejections such as `busy` and `judge_unavailable` retain their reason in
the JSON reply's `error.data.reason` and exit 1.

A script tests the reply with `jq`:

```sh
if chauffeur navigate < request.json | tail -1 \
  | jq -e '.result.intents | any(.withhold == "event")' > /dev/null; then
  echo "withheld"
fi
```

It reads at most 1 MiB from stdin, including any newline. It waits for the
request's `deadline_ms`, 3 seconds by default and capped at 30 seconds, and 2
seconds more, in all, however many events arrive. A daemon message is limited
to 4 MiB. The socket is `$CHAUFFEUR_SOCKET`, or the installation's own.
The wire contract is in [Slice 2](../plans/slices/slice-2.md#wire-contract).

The CLI integration tests start daemons, including on test threads. On a host
with a live daemon, use the build-only checks; run tests in CI or a disposable
container:

```sh
cargo fmt --all -- --check
cargo clippy -p chauffeur-cli --all-targets --locked -- -D warnings
cargo build -p chauffeur-cli --locked
```

Two of them start processes, so they are `#[ignore]`d: one starts and kills a
daemon, the other reads one line and closes the pipe. CI runs these two, and
only these, with:

```sh
cargo nextest run -p chauffeur-cli --test navigate --run-ignored only --locked
```
