# `chauffeur daemon`

Runs the daemon, and asks the running one about itself.

## `chauffeur daemon run`

Runs the daemon in the foreground until it is stopped.

```sh
chauffeur daemon run
# Shipped content from a checkout.
chauffeur daemon run --distribution ./distribution
```

| Option                | Default                                                                  |
| --------------------- | ------------------------------------------------------------------------ |
| `--distribution PATH` | `$CHAUFFEUR_DISTRIBUTION`, or `share/<name>/` beside the binary's folder |
| `--models FILE`       | The shipped models and your `models.json`; this file replaces both       |

`--models` is for tests and the playground: a file of placeholders answers
every tier, so no key is needed and no request leaves the machine.

```sh
chauffeur daemon run --models fixtures/placeholders.json
```

- **One per installation.** If a daemon already answers on the socket, it
  says so and exits 0, so starting it twice is harmless.
- **A private socket.** The socket's folder is created with mode `0700`; one
  that is open to others, or owned by someone else, is refused.
- **Reloads by itself.** Edits to models, rules, redaction or projects apply
  to the next event; a change that does not load keeps what was loaded. A rule
  file that does not load is listed by `daemon status`, and the rule it held
  before, if any, still runs. Where the platform cannot report file changes,
  such as in a sandbox, it checks every 2 seconds.
- **Your own projects.** `projects.json` in the configuration folder names the
  projects whose inline System Two strategies may run, as a JSON array of
  absolute paths, such as `["/Users/me/src/app"]`. A project it does not list
  is refused with `judge_unavailable`; with no file, no project may run one.
  The file may be at most 64 KiB and list at most 256 projects, and one that
  is larger or names a relative path is refused: the daemon does not start
  with it, and a reload keeps the list loaded before.
- **Keys** come from its own environment, so start it where the keys are set,
  or put them in the service's environment file when Homebrew runs it.
- **Several calls per connection.** Each call runs on its own thread: at
  most 16 calls open on one connection, and at most 4 of them may be
  `chauffeur.navigate`; a fifth is refused as `busy` until one finishes. An
  ID already open is refused too. `$/cancelRequest` cancels a call by its ID,
  and a connection that closes cancels everything it carried. A write of one
  message that takes over 5 seconds does the same: nothing more is written
  and the connection is shut. A cancelled call answers `-32800` with
  `data.reason` `cancelled`, even if it panicked; any other panic answers
  `-32603` `internal error`. A connection with no call open that sends
  nothing for 30 seconds is closed.
- **Notifications run nothing.** A request with no `id` is ignored, except
  `$/cancelRequest`: no method runs and no reply is written.
- **Admission.** Two limits count calls, not connections: the daemon's
  capacity of 16 calls, and each source's share of 8: 8 for harnesses
  (`chauffeur.emit`), 8 for callers (`chauffeur.navigate`). Past either, a
  call is refused as `busy` at once, never queued, with a message naming the
  limit reached:
  `the daemon is busy: this caller's share of 8 calls is in use` or
  `the daemon is busy: the daemon's capacity of 16 calls is in use`.
  The capacity fills only when both shares are full, and the share is
  checked first, so with 16 = 8 + 8 a refusal names its share; the capacity
  message appears only for a capacity below the shares' sum.
  A connection's own caps are separate: 16 open calls, 4 of them navigations.
- **Every run** goes to the audit log, with a line of its own for a rule no
  worker started before the deadline,
  `{"rule": …, "event": …, "session": …, "skipped": "deadline"}`. See
  [`audit`](audit.md).

As a service:

```sh
brew services start chauffeur-next   # reads ~/.homebrew/services/chauffeur-next.env
```

## `chauffeur daemon status`

Prints the running daemon's version, the rules it loaded, and the rule files
it refused, each with its path and why.

```sh
chauffeur daemon status
```

```json
{
  "version": "1.0.0-rc.1",
  "protocol": 1,
  "rules": ["conventional-commits", "team/release"],
  "refused": [
    "~/.config/chauffeur/rules/team/broken.json: … names judge `nobody`, …"
  ]
}
```

A rule file is checked when it loads, so `refused` is where a mistake in a
rule shows, with no separate validation step.

A rule's ID is its path under `rules/` without `.json`, such as
`team/release`. Folders below `rules/` may be links; a folder reached twice is
read once, by the first path in name order, and at most 256 folders and 512
rule files are read from each of the shipped and your own `rules/`.

From slice 5, `chauffeur daemon status --project WORKTREE` lists the rules a
session in that worktree would load, its project rules included, and the
ones refused.
