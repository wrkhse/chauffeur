import { expect, test } from "bun:test"
import { Effect, Queue, Stream } from "effect"
import type { DaemonClient } from "../src/daemon.js"
import { installExposure } from "../src/exposure.js"
import type { HostEffect, Signal } from "../src/protocol.js"
import { fakeHost, Hooks, install, noRulebooks, noSkills, settle, shippedTexts } from "./support.js"

const sessionID = "ses_prompt_context"

type Synthetic = {
  readonly sessionID: string
  readonly text: string
  readonly description?: string
  readonly delivery?: string
  readonly resume?: boolean
}

type InboxEvent = {
  readonly type: "session.inbox.enqueued"
  readonly data: {
    readonly sessionID: string
    readonly item: { readonly type: "user" | "synthetic" }
  }
}

const enqueued = (item: InboxEvent["data"]["item"]): InboxEvent => ({
  type: "session.inbox.enqueued",
  data: { sessionID, item },
})

test("prompt context waits for the user message, then sends one synthetic message", async () => {
  const hooks = new Hooks()
  const synthetics: Synthetic[] = []
  const stored = new Map<string, ReadonlyArray<string>>()
  const sent: Signal[] = []
  const events = Effect.runSync(Queue.unbounded<InboxEvent>())

  const host = fakeHost({
    session: {
      hook: hooks.register,
      context: () => Effect.succeed([]),
      get: () => Effect.succeed({ location: { directory: "/projects/chauffeur" } }),
      synthetic: (message: Synthetic) => Effect.sync(() => { synthetics.push(message) }),
    },
    storage: {
      get: (key: string) => Effect.succeed(stored.get(key)),
      set: (key: string, value: ReadonlyArray<string>) => Effect.sync(() => { stored.set(key, value) }),
    },
    skill: { list: () => Effect.succeed({ data: [] }) },
    tool: { list: () => Effect.succeed([]) },
    // SAFETY: the test supplies only the inbox event this capability reads.
    event: { subscribe: () => Stream.fromQueue(events) },
  })

  const effects: ReadonlyArray<HostEffect> = [
    { type: "context", agent_id: sessionID, delivery: "prompt", label: "Code Mode patch", skills: [], text: "note one" },
    { type: "context", agent_id: sessionID, delivery: "prompt", label: "Code Mode grep", skills: ["gh"], text: "note two" },
  ]

  const daemon: DaemonClient = {
    skills: noSkills,
    rulebooks: noRulebooks,
    texts: shippedTexts,
    signal: (value) => Effect.sync(() => {
      sent.push(value)

      return effects
    }),
  }

  const plugin = await install(installExposure(), host, daemon)
  const prompt = { text: "Fix it" }

  await hooks.emit("prompt", { sessionID, prompt })

  expect(prompt).not.toHaveProperty("files")
  expect(prompt).toMatchObject({ skills: [{ id: "gh" }] })
  expect(synthetics).toEqual([])

  await Effect.runPromise(Queue.offer(events, enqueued({ type: "synthetic" })))
  await settle()

  expect(synthetics).toEqual([])

  await Effect.runPromise(Queue.offer(events, enqueued({ type: "user" })))
  await settle()

  expect(synthetics).toEqual([{
    sessionID,
    text: "note one\n\nnote two",
    description: "Chauffeur Code Mode patch, Code Mode grep",
    delivery: "steer",
  }])

  await plugin.close()
})
