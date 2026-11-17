import { expect, test } from "bun:test"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { connect } from "../src/client/index.ts"
import { ask, judge, noul } from "../src/judge/index.ts"
import { standIn } from "./stand-in.ts"

const ANSWER = {
  outcome: "yes" as const,
  p: { yes: 0.9, no: 0.1 },
  level: 0.1,
  model: "jev",
  placeholder: false
}

test("ask sends a one-step inline strategy naming the evidence it sends", async () => {
  const daemon = await standIn(() => ({
    result: { verdict: { ask: { answer: ANSWER } } }
  }))
  const chauffeur = connect({ socket: daemon.socket })
  const quick = judge({ tier: "system-one", confidence: 0.7 })

  try {
    const done = await ask(noul("Is the migration finished?"), {
      judge: quick,
      evidence: { log: "42 of 42" },
      deadlineMs: 1500,
      client: chauffeur
    })

    expect(done).toEqual({ ok: true, ...ANSWER })
    expect(daemon.messages[0]?.params).toEqual({
      protocol: 1,
      judges: { asked: { tier: "system-one", confidence: 0.7 } },
      strategy: [
        {
          id: "ask",
          judge: "asked",
          ask: {
            instructions: "Is the migration finished?",
            outcomes: ["yes", "no"],
            evidence: ["log"]
          }
        }
      ],
      evidence: { log: "42 of 42" },
      deadline_ms: 1500
    })
  } finally {
    chauffeur.close()
    await daemon.close()
  }
})

test("a failure, a rejection, no daemon and an abort are each ok: false with why", async () => {
  const failed = await standIn(() => ({
    result: { verdict: { ask: { failed: "timeout" } } }
  }))
  const busy = await standIn(() => ({
    error: { code: -32000, message: "busy", data: { reason: "busy" } }
  }))
  const quick = judge({ tier: "system-one" })
  const missing = join(tmpdir(), "chauffeur-missing", "daemon.sock")
  const aborted = AbortSignal.abort()

  try {
    const at = (socket: string) => ({ judge: quick, client: connect({ socket }) })

    expect(await ask(noul("?"), at(failed.socket))).toEqual({
      ok: false,
      reason: "timeout"
    })
    expect(await ask(noul("?"), at(busy.socket))).toEqual({
      ok: false,
      reason: "busy",
      message: "busy"
    })
    expect(await ask(noul("?"), at(missing))).toMatchObject({
      ok: false,
      reason: "unavailable"
    })
    expect(await ask(noul("?"), { ...at(failed.socket), signal: aborted })).toEqual({
      ok: false,
      reason: "cancelled"
    })
  } finally {
    await failed.close()
    await busy.close()
  }
})

test("ask preserves rejected limit details and refuses an unknown outcome", async () => {
  const rejected = await standIn(() => ({
    error: {
      code: -32602,
      message: "evidence is too large",
      data: { reason: "limit_exceeded", limit: "evidence_bytes", max: 262144 }
    }
  }))
  const invalid = await standIn(() => ({
    result: { verdict: { ask: { answer: { ...ANSWER, outcome: "maybe" } } } }
  }))
  const limitedClient = connect({ socket: rejected.socket })
  const invalidClient = connect({ socket: invalid.socket })
  const quick = judge({ tier: "system-one" })

  try {
    expect(await ask(noul("?"), { judge: quick, client: limitedClient })).toEqual({
      ok: false,
      reason: "limit_exceeded",
      message: "evidence is too large",
      limit: "evidence_bytes",
      max: 262144
    })
    expect(await ask(noul("?"), { judge: quick, client: invalidClient })).toEqual({
      ok: false,
      reason: "invalid"
    })
  } finally {
    limitedClient.close()
    invalidClient.close()
    await rejected.close()
    await invalid.close()
  }
})

test("a confidence outside 0 to 1 is refused before anything is sent", () => {
  expect(() => judge({ tier: "system-one", confidence: 1.5 })).toThrow(RangeError)
  expect(() =>
    judge({ tier: "system-one", confidence: { yes: 0.9, no: -1 } })
  ).toThrow(RangeError)
})

test("ask forwards the project it works in, and omits an absent one", async () => {
  const daemon = await standIn(() => ({
    result: { verdict: { ask: { answer: ANSWER } } }
  }))
  const chauffeur = connect({ socket: daemon.socket })
  const deep = judge({ tier: "system-two" })

  try {
    await ask(noul("?"), {
      judge: deep,
      project: "/Users/me/src/app",
      client: chauffeur
    })
    await ask(noul("?"), { judge: deep, client: chauffeur })

    expect(daemon.messages[0]?.params).toMatchObject({
      project: "/Users/me/src/app"
    })
    expect(daemon.messages[1]?.params).not.toHaveProperty("project")
  } finally {
    chauffeur.close()
    await daemon.close()
  }
})
