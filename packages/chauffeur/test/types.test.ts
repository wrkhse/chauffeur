import { expect, test } from "bun:test"
import type { Asked } from "../src/judge/index.ts"
import {
  connect,
  NavigateError,
  type NavigateRequest,
  type Navigated
} from "../src/client/index.ts"

// @ts-expect-error event and strategy requests are mutually exclusive.
const mixed: NavigateRequest = { on: "event", strategy: [] }

const project = "/Users/me/src/app"
const projectedEvent: NavigateRequest = { on: "event", project }
const projectedStrategy: NavigateRequest = { strategy: [], project }

/** Sourcefed's shape: a socket, a navigation with a signal, and a close. */
type SourcefedClient = (options: { socket: string }) => {
  navigate(
    request: NavigateRequest,
    options: { signal: AbortSignal }
  ): Promise<unknown>
  close(): void
}

// Checked by `tsc`: assigning connect to this shape fails when it drifts.
const sourcefed: SourcefedClient = connect

function navigateThenClose(socket: string, signal: AbortSignal): Promise<unknown> {
  const client = sourcefed({ socket })
  const navigated = client.navigate({ on: "sourcefed.event" }, { signal })
  client.close()

  return navigated
}

function failure(error: unknown): string | undefined {
  if (!(error instanceof NavigateError)) return undefined

  return `${error.reason}: ${error.message}`
}

// Checked by `tsc`: each @ts-expect-error fails the typecheck when the line
// it marks compiles, so these are compile-time tests.
function outcomes(done: Asked<"yes" | "no">): string {
  if (done.ok && done.outcome === "yes") return "yes"

  if (done.ok && done.outcome === "unsure") return "unsure"

  // @ts-expect-error "maybe" is not one of a noul's outcomes.
  if (done.ok && done.outcome === "maybe") return "never"

  // @ts-expect-error an answer that failed has no outcome.
  if (!done.ok) return done.outcome

  return "no"
}

function results(
  event: Navigated<{ on: "x" }>,
  strategy: Navigated<{ strategy: [] }>
) {
  // @ts-expect-error an event's result has intents, not a verdict.
  event.verdict

  // @ts-expect-error a strategy's result has a verdict, not intents.
  strategy.intents

  return [event.intents, strategy.verdict]
}

test("the compile-time checks above are part of the typecheck", () => {
  expect(typeof outcomes).toBe("function")
  expect(typeof results).toBe("function")
  expect(typeof navigateThenClose).toBe("function")
  expect(failure(new NavigateError("busy", "at capacity"))).toBe("busy: at capacity")
  expect(mixed.on).toBe("event")
  expect(projectedEvent.project).toBe(project)
  expect(projectedStrategy.project).toBe(project)
})
