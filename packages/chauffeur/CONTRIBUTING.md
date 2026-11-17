# Contributing to `@fdcn/chauffeur`

The package is written in TypeScript and built with Bun.

| File                       | Holds                                                                    |
| -------------------------- | ------------------------------------------------------------------------ |
| `src/client/wire.ts`       | Requests, intents, verdicts, events and `NavigateError`, as the schema   |
| `src/client/socket.ts`     | `socketPath`: the option, name, `$CHAUFFEUR_SOCKET`, then runtime folder |
| `src/client/connection.ts` | `Connection`: calls by ID on one socket, events routed, cancelling       |
| `src/client/navigate.ts`   | `navigate` and `connect`                                                 |
| `src/judge/index.ts`       | `judge`, `noul`, `ask`                                                   |

```sh
bun install
bun run typecheck   # tsc --noEmit
bun test            # the tests, against a stand-in daemon
bun run build       # bun build and the declarations
bun run check       # typecheck, tests and the build
```

The tests against the real daemon run when `CHAUFFEUR_BIN` names a built
`chauffeur`, as CI does; they start and stop it, so they stay off a
developer's machine.
