import { Effect } from "effect"
import type { SessionID } from "./host.js"
import type { ModelRef } from "./protocol.js"

/** What a capability asks the renewal to do before a context is used. */
export type RenewalControl = {
  /**
   * Before a prompt is admitted: when the engine says the session's context
   * should be renewed, queue a compaction ahead of the prompt, to be written
   * by the engine's cheaper model. True when one was queued.
   */
  readonly renew: (sessionID: SessionID) => Effect.Effect<boolean>
  /**
   * Before a model switch: when the engine says the context is too long for
   * the new model to re-read, queue its cheap summary and wait for it. True
   * when the context was renewed.
   */
  readonly beforeSwitch: (sessionID: SessionID, model: ModelRef) => Effect.Effect<boolean>
  /** True when the session's model differs from the last one that served it. */
  readonly switched: (sessionID: SessionID, model: ModelRef) => boolean
}

export const NO_RENEWAL: RenewalControl = {
  renew: () => Effect.succeed(false),
  beforeSwitch: () => Effect.succeed(false),
  switched: () => false,
}
