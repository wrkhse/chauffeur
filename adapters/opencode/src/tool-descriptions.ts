import type { SessionContext } from "@opencode/plugin/effect/session"
import { Effect, type Scope } from "effect"
import { ownModelAgents, type OwnModelAgent, SUBAGENT_TOOL } from "./agents.js"
import { Host } from "./host.js"
import { fill, type HostTexts, Texts } from "./texts.js"

/**
 * Chauffeur's edits to the tool descriptions each request carries, applied
 * where every tool is final (Code Mode's `execute` and per-request texts
 * included): each `tool_descriptions` text replaces that tool's description,
 * then the user's standing instruction about subagent models is appended to
 * `subagent`, so the agent reads it before launching one. Worded as the
 * user's instruction: the host's own text says to set a subagent's model
 * only when the user asks. The instruction applies only to agents without a
 * model of their own; the agents that have one are named after it, except the
 * host's hidden ones, which are not the user's to choose by name.
 */
export const installToolDescriptions: Effect.Effect<void, never, Host | Texts | Scope.Scope> = Effect.gen(function* () {
  const host = yield* Host
  const texts = yield* Texts

  const rewrite = (event: SessionContext) => event.tools[SUBAGENT_TOOL]
    ? ownModelAgents().pipe(
      Effect.map((agents) => rewriteDescriptions(event.tools, texts.current(), namedAgents(agents))),
      Effect.provideService(Host, host),
    )
    // No subagent tool to name agents in, so the host's agents are not read.
    : Effect.sync(() => rewriteDescriptions(event.tools, texts.current()))

  // On every request that carries the agent's tools, so compaction and
  // warming requests keep the agent loop's prompt prefix and its cache.
  yield* host.session.hook("context", rewrite)
  yield* host.session.hook("compaction", rewrite)
  yield* host.session.hook("generate", rewrite)
})

/** The agents the description may name: the known ones the host does not hide. */
function namedAgents(agents: ReadonlyArray<OwnModelAgent> | undefined): ReadonlyArray<string> {
  const ids: string[] = []

  for (const agent of agents ?? []) {
    if (agent.hidden) continue

    ids.push(agent.id)
  }

  return ids
}

/** Rewrite `tools`' descriptions in place from `texts`, naming the agents that need no model. */
export function rewriteDescriptions(tools: SessionContext["tools"], texts: HostTexts, ownModel: ReadonlyArray<string> = []): void {
  for (const [name, description] of Object.entries(texts.tool_descriptions)) {
    const tool = tools[name]

    if (tool) tool.description = description
  }

  const subagent = tools[SUBAGENT_TOOL]

  if (!subagent) return

  const guidance = texts.subagent.guidance

  if (!subagent.description.includes(guidance)) subagent.description = `${subagent.description}\n\n${guidance}`

  // Named after the guidance, and only once: a later hook sees the filled line.
  const ownModelLine = fill(texts.subagent.own_model, { agents: ownModel.join(", ") })

  if (ownModel.length > 0 && !subagent.description.includes(ownModelLine)) {
    subagent.description = `${subagent.description}\n\n${ownModelLine}`
  }
}
