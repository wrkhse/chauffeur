import { Effect } from "effect"
import { Host } from "./host.js"

/** OpenCode's tool that starts a child session. */
export const SUBAGENT_TOOL = "subagent"

/**
 * The agents a subagent call may launch that run on a model of their own, by
 * ID, sorted; none when the host's agents cannot be read.
 */
export function ownModelAgents(): Effect.Effect<ReadonlyArray<string>, never, Host> {
  return Effect.gen(function* () {
    const host = yield* Host
    const agents = (yield* host.agent.list()).data
    const ids: string[] = []

    for (const agent of agents) {
      if (agent.mode === "primary" || agent.model === undefined) continue

      ids.push(String(agent.id))
    }

    return ids.toSorted()
  }).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))
}
