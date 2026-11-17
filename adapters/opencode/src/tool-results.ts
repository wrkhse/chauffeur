import type { Plugin } from "@opencode/plugin/effect"
import type { ToolHooks } from "@opencode/plugin/effect/tool"
import { Effect, Option, Schema, type Scope } from "effect"
import { ownModelAgents, SUBAGENT_TOOL } from "./agents.js"
import { ASK_TOOL } from "./ask-tool.js"
import { Daemon } from "./daemon.js"
import type { ExposureControl } from "./exposure.js"
import { Host, toSessionID } from "./host.js"
import { signal, TEXT_CODE_POINTS, type CatalogEntry, type HostEffect, type SignalKind } from "./protocol.js"
import { deliverContext } from "./skills.js"
import { clip, clipEnds, userText } from "./text.js"
import { type HostTexts, Texts } from "./texts.js"

const ERROR_CODE_POINTS = 240

const TOOL_RESULT_TIMEOUT = "3 seconds"

const RECOVERY_TIMEOUT = "8 seconds"

const MAX_CANDIDATES = 4

const MISSING_TOOL = /(?:unknown|missing|unavailable|not found|no matching|not registered|cannot call|not available).{0,50}(?:tool|function)|(?:tool|function).{0,50}(?:unknown|missing|unavailable|not found|not registered|not available)/i

/** A subagent call's arguments: the agent it launches, or the child session it continues. */
const SubagentCall = Schema.Struct({ agent: Schema.optional(Schema.String), sessionID: Schema.optional(Schema.String) })

const decodeSubagentCall = Schema.decodeUnknownOption(SubagentCall)

type ToolResult = ToolHooks["execute.after"]

/**
 * Report tool results, deliver mid-turn context into the running turn, and
 * apply verified hidden-tool reveals.
 */
export function installToolResults(exposure: ExposureControl): Effect.Effect<void, never, Host | Daemon | Texts | Scope.Scope> {
  return Effect.gen(function* () {
    const host = yield* Host
    const daemon = yield* Daemon
    const texts = yield* Texts

    // Chauffeur's own tool already told the engine what the agent needs.
    yield* host.tool.hook("execute.after", (event) => event.tool === ASK_TOOL ? Effect.void :
      report(event, exposure, texts.current()).pipe(
        Effect.provideService(Host, host),
        Effect.provideService(Daemon, daemon),
        Effect.catch((error) => Effect.logError("chauffeur: tool result not reported", error)),
      ))
  })
}

function report(event: ToolResult, exposure: ExposureControl, texts: HostTexts): Effect.Effect<void, unknown, Host | Daemon> {
  return Effect.gen(function* () {
    const host = yield* Host
    const daemon = yield* Daemon

    // Read once, first: this session names both its location's agents and the result's workspace.
    const session = yield* host.session.get({ sessionID: event.sessionID }).pipe(
      // An unreadable session still reports the result, as a top-level session without a workspace.
      Effect.map((info) => ({ location: info.location, workspace: clip(String(info.location.directory), TEXT_CODE_POINTS), subagent: info.parentID !== undefined })),
      Effect.orElseSucceed(() => ({ location: undefined, workspace: "", subagent: false })),
    )

    const { location, ...reported } = session
    const observed = observe(event)
    const recovery = MISSING_TOOL.test(observed.evidence) ? yield* recover(host, exposure, event, observed) : NO_RECOVERY

    // Only the subagent tool launches an agent, so only it reads the host's agents.
    const agents = event.tool === SUBAGENT_TOOL ? yield* ownModelAgents(location) : []
    const launched = event.tool === SUBAGENT_TOOL ? yield* launchedAgent(host, event.input) : undefined

    // Unknown is treated as having its own model, so no rule suggests
    // overriding it: missing a cheaper-model hint is harmless, overriding a
    // configured model is not. A hidden agent still counts.
    const agentHasModel = event.tool === SUBAGENT_TOOL && (agents === undefined || launched === undefined || agents.some((agent) => agent.id === launched))

    const kind: Extract<SignalKind, { type: "tool_result" }> = {
      type: "tool_result",
      tool: clip(event.tool, TEXT_CODE_POINTS),
      ok: event.status === "completed",
      ...reported,
      ...observed,
      user_request: recovery.userRequest,
      candidates: recovery.candidates,
    }

    // An older daemon's structs refuse unknown fields, so the false case is no field at all.
    if (agentHasModel) kind.agent_has_model = true

    const effects = yield* daemon.signal(signal(String(event.sessionID), kind), recovery.candidates.length > 0 ? RECOVERY_TIMEOUT : TOOL_RESULT_TIMEOUT)

    yield* applyEffects(event, effects, exposure, texts)
  })
}

type Observed = { readonly input: string; readonly error: string; readonly evidence: string }

type Recovery = { readonly userRequest: string; readonly candidates: CatalogEntry[] }

const NO_RECOVERY: Recovery = { userRequest: "", candidates: [] }

/**
 * The agent a subagent call launches: the one it names, or the agent the child
 * session it continues runs on. `undefined` when neither resolves, because an
 * unknown agent counts as having a model of its own.
 */
function launchedAgent(host: Plugin.Context, input: ToolResult["input"]): Effect.Effect<string | undefined, never> {
  return Effect.gen(function* () {
    const call = Option.getOrUndefined(decodeSubagentCall(input))

    if (call?.agent) return call.agent

    if (!call?.sessionID) return undefined

    return yield* host.session.get({ sessionID: toSessionID(call.sessionID) }).pipe(
      Effect.map((info) => (info.agent ? String(info.agent) : undefined)),
      Effect.orElseSucceed(() => undefined),
    )
  })
}

/** The call's input, its error, and the bounded evidence both leave, from each end. */
function observe(event: ToolResult): Observed {
  const input = summarize("input", () => JSON.stringify(event.input), clip)
  const error = event.status === "error" ? clip(event.error.message, ERROR_CODE_POINTS) : ""
  const output = event.status === "completed" ? summarize("output", () => JSON.stringify(event.result.content ?? event.result.output), clipEnds) : ""

  return { input, error, evidence: clipEnds(`${error} ${output}`.trim(), TEXT_CODE_POINTS) }
}

/** For a missing tool: the user's latest request and the hidden tools it could mean. */
function recover(host: Plugin.Context, exposure: ExposureControl, event: ToolResult, observed: Observed): Effect.Effect<Recovery, unknown> {
  return Effect.gen(function* () {
    const lastUser = (yield* host.session.context({ sessionID: event.sessionID })).findLast((message) => message.type === "user")
    const userRequest = lastUser?.type === "user" ? userText(lastUser.text) : ""
    const catalog = yield* exposure.candidates(event.sessionID)

    return { userRequest, candidates: matching(catalog, `${observed.input} ${observed.evidence} ${userRequest}`) }
  })
}

/** Reveal confirmed tools; deliver context into this turn, each skill once. */
function applyEffects(event: ToolResult, effects: ReadonlyArray<HostEffect>, exposure: ExposureControl, texts: HostTexts): Effect.Effect<void, unknown, Host> {
  const delivered = new Set<string>()

  return Effect.forEach(effects, (effect) => {
    if (effect.agent_id !== String(event.sessionID)) return Effect.void

    if (effect.type === "tools") return effect.reveal.length > 0 ? exposure.reveal(event.sessionID, effect.reveal) : Effect.void

    // Prompt context joins a user message in the prompt hook; the rest reaches this turn.
    if (effect.type !== "context" || effect.delivery === "prompt") return Effect.void

    const skills = effect.skills.filter((skill) => !delivered.has(skill))

    for (const skill of skills) delivered.add(skill)

    return deliverContext(event.sessionID, { ...effect, skills }, texts)
  }, { discard: true })
}

/** Lexical matches narrow the registered hidden catalog; Jev judges need. */
function matching(catalog: ReadonlyArray<CatalogEntry>, text: string): CatalogEntry[] {
  const words = new Set(text.toLowerCase().match(/[a-z][a-z0-9]{3,}/g) ?? [])

  return catalog.filter((tool) => {
    const names = tool.id.toLowerCase().split(/[_-]/)

    return names.some((name) => name.length >= 4 && words.has(name))
      || tool.description.toLowerCase().split(/\W+/).filter((word) => word.length >= 5 && words.has(word)).length >= 2
  }).slice(0, MAX_CANDIDATES)
}

function summarize(part: "input" | "output", encode: () => string | undefined, bound: (value: string, codePoints: number) => string): string {
  try {
    return bound(encode() ?? "", TEXT_CODE_POINTS)
  } catch {
    return `unserializable ${part}`
  }
}
