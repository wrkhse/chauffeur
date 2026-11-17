import cases from "./navigate-examples.json"
import type { Feature } from "../feature.ts"
import { navigate as run } from "../wasm/chauffeur_playground.js"

/** The browser and native contract test use the same examples. */
const examples: Readonly<Record<string, { input: unknown }>> = cases

type Navigated =
  | { events: ReadonlyArray<Record<string, unknown>>; result: Record<string, unknown>; refused?: ReadonlyArray<string> }
  | { rejected: { code: number; message: string; data: Record<string, unknown> } }
  | { error: string }

type NavigateView = {
  readonly $template: string
  readonly names: ReadonlyArray<string>
  example: string
  source: string
  result: Navigated | undefined
  load(name: string): void
  update(source: string): void
  pretty(value: unknown): string
}

const first = Object.keys(examples)[0] ?? ""
const show = (value: unknown): string => JSON.stringify(value, null, 2)

/** A request, rule files and stand-in replies in; the events and intents, the verdict, or the rejection out. */
export const navigate: Feature = {
  id: "navigate",
  title: "Navigate",
  view: (): NavigateView => {
    const view: NavigateView = {
      $template: "#navigate",
      names: Object.keys(examples),
      example: first,
      source: show(examples[first]?.input),
      result: undefined,
      load(name) {
        this.example = name
        this.update(show(examples[name]?.input))
      },
      update(source) {
        this.source = source
        try {
          this.result = JSON.parse(run(source)) as Navigated
        } catch (error) {
          this.result = { error: error instanceof Error ? error.message : String(error) }
        }
      },
      pretty: show,
    }

    view.update(view.source)
    return view
  },
}
