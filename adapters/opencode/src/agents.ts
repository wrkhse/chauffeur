import { Effect } from "effect"
import { Host } from "./host.js"

/** OpenCode's tool that starts a child session. */
export const SUBAGENT_TOOL = "subagent"

/** A session's location; the host lists a project's own agents for the directory it names. */
export type AgentLocation = { readonly directory?: string | undefined }

/** A host slow to list its agents must not hold a tool result or a request; an unread list is unknown. */
const AGENT_LIST_TIMEOUT = "1 second"

/** An agent a subagent call may launch, and whether the host hides it from its agent list. */
export type OwnModelAgent = { readonly id: string; readonly hidden: boolean }

/**
 * The agents a subagent call may launch that run on a model of their own, by
 * ID, sorted. Only agents whose mode is `subagent` or `all` qualify: a
 * `primary` agent runs a session, it is not launched by the subagent tool.
 * `undefined` when the host's agents cannot be read in time, because an unread
 * list is unknown rather than empty: a rule must not steer an agent whose own
 * model it could not see. Read at the session's `location`, where a project's
 * `.opencode/agents` are defined.
 */
export function ownModelAgents(location?: AgentLocation): Effect.Effect<ReadonlyArray<OwnModelAgent> | undefined, never, Host> {
  return Effect.gen(function* () {
    const host = yield* Host
    const listed = location ? host.agent.list({ location }) : host.agent.list()
    const agents = (yield* listed.pipe(Effect.timeout(AGENT_LIST_TIMEOUT))).data
    const own: OwnModelAgent[] = []

    for (const agent of agents) {
      if (agent.mode === "primary" || agent.model === undefined) continue

      own.push({ id: String(agent.id), hidden: agent.hidden })
    }

    return own.toSorted(byId)
  }).pipe(Effect.orElseSucceed((): ReadonlyArray<OwnModelAgent> | undefined => undefined))
}

/** Sorted as plain strings, so every request names the agents in the same order. */
function byId(left: OwnModelAgent, right: OwnModelAgent): number {
  if (left.id === right.id) return 0

  return left.id < right.id ? -1 : 1
}
