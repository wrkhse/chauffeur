import { Effect } from "effect"
import { Host } from "./host.js"

/** OpenCode's tool that starts a child session. */
export const SUBAGENT_TOOL = "subagent"

/** An agent a subagent call may launch, and whether the host hides it from its agent list. */
export type OwnModelAgent = { readonly id: string; readonly hidden: boolean }

/**
 * The agents a subagent call may launch that run on a model of their own, by
 * ID, sorted; `undefined` when the host's agents cannot be read, because an
 * unread list is unknown rather than empty: a rule must not steer an agent
 * whose own model it could not see.
 */
export function ownModelAgents(): Effect.Effect<ReadonlyArray<OwnModelAgent> | undefined, never, Host> {
  return Effect.gen(function* () {
    const host = yield* Host
    const agents = (yield* host.agent.list()).data
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
