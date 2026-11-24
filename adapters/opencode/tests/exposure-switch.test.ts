import { expect, test } from "bun:test"
import { Effect } from "effect"
import type { DaemonClient } from "../src/daemon.js"
import { installExposure } from "../src/exposure.js"
import type { SessionID } from "../src/host.js"
import type { HostEffect, ModelRef } from "../src/protocol.js"
import { NO_RENEWAL } from "../src/renewal.js"
import { fakeHost, Hooks, install, noRulebooks, noSkills, shippedTexts } from "./support.js"

const sessionID = "ses_switch"

const onOpus = { providerID: "anthropic", id: "claude-opus-5-5", variant: "high" }

function setup(effects: ReadonlyArray<HostEffect>, manualChange: boolean) {
  const hooks = new Hooks()
  const actions: string[] = []

  const host = fakeHost({
    session: {
      hook: hooks.register,
      context: () => Effect.succeed([]),
      get: () => Effect.succeed({ model: onOpus, agent: "build", location: { directory: "/projects/app" } }),
      switchModel: () => Effect.sync(() => { actions.push("switch") }),
    },
    storage: { get: () => Effect.succeed(undefined), set: () => Effect.void },
    skill: { list: () => Effect.succeed({ data: [] }) },
    tool: { list: () => Effect.succeed([]) },
  })

  const daemon: DaemonClient = {
    skills: noSkills,
    rulebooks: noRulebooks,
    texts: shippedTexts,
    signal: () => Effect.succeed([...effects]),
  }

  const renewal = {
    ...NO_RENEWAL,
    switched: () => manualChange,
    beforeSwitch: (session: SessionID, model: ModelRef) => Effect.sync(() => {
      actions.push(`compact:${String(session)}/${model.provider}/${model.model}`)

      return true
    }),
  }

  return { hooks, actions, host, daemon, renewal }
}

test("a model the user changed by hand is compacted before the next request", async () => {
  const { hooks, actions, host, daemon, renewal } = setup([], true)
  const plugin = await install(installExposure(renewal), host, daemon)

  try {
    await hooks.emit("prompt", { sessionID, prompt: { text: "continue" } })
    expect(actions).toEqual(["compact:ses_switch/anthropic/claude-opus-5-5"])
  } finally {
    await plugin.close()
  }
})

test("an engine model switch is compacted before the switch is applied", async () => {
  const effects: ReadonlyArray<HostEffect> = [{ type: "model", agent_id: sessionID, model: { provider: "openai", model: "gpt-6-sol" } }]
  const { hooks, actions, host, daemon, renewal } = setup(effects, false)
  const plugin = await install(installExposure(renewal), host, daemon)

  try {
    await hooks.emit("prompt", { sessionID, prompt: { text: "continue" } })
    expect(actions).toEqual(["compact:ses_switch/openai/gpt-6-sol", "switch"])
  } finally {
    await plugin.close()
  }
})
