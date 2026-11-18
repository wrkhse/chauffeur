# `@fdcn/chauffeur`

Asks Chauffeur from any TypeScript program, over the daemon's Unix socket.

| Entry     | Holds                                                                       |
| --------- | --------------------------------------------------------------------------- |
| `/client` | `navigate` on a connection of its own, `connect` for many, and its types    |
| `/judge`  | `judge`, `noul` and `ask`: one question, its result typed from its outcomes |

## Install from the release tarball

```sh
npm install https://github.com/wrkhse/chauffeur/releases/download/v1.0.0-rc.5/fdcn-chauffeur-1.0.0-rc.5.tgz
```

From v1.0.0-rc.5 on, each prerelease attaches this tarball. The package has no
runtime dependencies.

## Connect

`connect` keeps one connection for many calls, and `close()` ends it so the
process can exit. `navigate` on the client sends one request and reports each
event as it arrives.

```ts
import { connect, NavigateError, socketPath } from "@fdcn/chauffeur/client"

const socket = socketPath()
const client = connect({ socket })
const abort = new AbortController()

try {
  const { intents } = await client.navigate(
    { on: "sourcefed.event", evidence: { event, thread } },
    {
      signal: abort.signal,
      onEvent: (event) => console.log(event)
    }
  )

  if (intents.some((intent) => intent.withhold === "event")) {
    /* ... */
  }
} catch (error) {
  if (error instanceof NavigateError) {
    console.error(error.reason, error.limit, error.max)
  }
} finally {
  client.close()
}
```

## Limits

One socket carries at most 16 open calls, 4 of them navigations. A fifth
navigation is refused as `busy`. A reply line may be 4 MiB, and a request line
may be 1 MiB.

- `navigate` rejects with a `NavigateError` whose `reason` is the daemon's,
  or `unavailable` when no daemon answers, it speaks another protocol, or it
  has not answered two seconds past the deadline. An aborted signal rejects
  with the signal's reason, and the daemon cancels the call.
- A request may name `project`, the absolute path of the project it works in.
  The daemon adds that project's rules, and runs an inline System Two judge
  only for a project its `projects.json` lists.
- `ask` never rejects; a rejected request includes its message and any limit
  and maximum alongside `{ ok: false, reason }`.
- `connect()` keeps one connection, opened again on the next call after the
  daemon restarts; call `close()` to end it and allow the process to exit.
- Each event reaches `onEvent` as it arrives; a listener that throws ends only
  its own call.
- `navigate` without a client opens a connection of its own and closes it when
  the result arrives, so a script exits as soon as it is done.

To build, typecheck and test the package, see [CONTRIBUTING.md](CONTRIBUTING.md).
