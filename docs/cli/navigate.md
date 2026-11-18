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

| Exit | Means                                                                                               |
| ---- | --------------------------------------------------------------------------------------------------- |
| 0    | Intents or a verdict came back, or the stdout pipe closed                                           |
| 1    | The request was rejected or stdin failed                                                            |
| 2    | The daemon is unavailable, does not answer in time, speaks another protocol, or stdout write failed |

Rejections such as `busy` and `judge_unavailable` retain their reason in
the JSON reply's `error.data.reason` and exit 1.

A script tests the reply with `jq`:

```sh
chauffeur navigate < request.json > reply.jsonl
status=$?
if [ "$status" -eq 0 ]; then
  tail -1 reply.jsonl \
    | jq -e '.result.intents | any(.withhold == "event")' > /dev/null \
    && echo "withheld"
fi
```

A rejection or an unavailable daemon exits 1 or 2 and prints nothing, so it
must not be read as "not withheld", which is why `jq` runs only after
`chauffeur navigate` exits 0.

The request line it writes to the daemon is limited to 1 MiB, including its
trailing newline, so the JSON read from stdin may be a little smaller: the
JSON-RPC envelope counts toward the limit too. It waits for the
request's `deadline_ms`, 3 seconds by default and capped at 30 seconds, and 2
seconds more, in all, however many events arrive. A daemon message is limited
to 4 MiB. The socket is `$CHAUFFEUR_SOCKET`, or the installation's own.
The wire contract is in [Slice 2](../plans/slices/slice-2.md#wire-contract).

The tests in `crates/cli/tests/navigate.rs` that run by default start their
daemons in-process, on private sockets under the temp folder, so they do not
touch a live daemon. These checks need no daemon at all:

```sh
cargo fmt --all -- --check
cargo clippy -p chauffeur-cli --all-targets --locked -- -D warnings
cargo build -p chauffeur-cli --locked
```

Three tests spawn and kill subprocesses, so they are `#[ignore]`d; run them in
CI or a disposable container: one starts and kills a daemon, one reads one line
and closes the pipe, and one kills a live `chauffeur navigate` that waits on a
slow daemon. CI runs these three, and only these, with:

```sh
cargo nextest run -p chauffeur-cli --test navigate --run-ignored only --locked
```
