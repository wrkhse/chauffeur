# Slices

A slice delivers one feature a person uses, through the real path from the
harness or caller to the judge and back. An implemented slice ships in a
release candidate of `chauffeur-next`, which its status names: rc.4 is
`1.0.0-rc.4`. Its page shows the release working end to end.

| #   | Slice                                                | Status            |
| --- | ---------------------------------------------------- | ----------------- |
| 0   | [Setup](#setup)                                      | Implemented       |
| 1   | [Conventional Commits nudge in OpenCode](slice-1.md) | Implemented, rc.4 |
| 2   | [sourcefed triage](slice-2.md)                       | Implemented, rc.5 |
| 3   | [Escalation from Jev to GLiDE](slice-3.md)           | In progress       |

Slice 3's merged parts are steps that wait on earlier ones, in
[`crates/core/src/plan/`](../../../crates/core/src/plan/README.md), and the
rule format that lowers to them, in
[`crates/protocol/src/rule/`](../../../crates/protocol/src/rule/README.md).
No release carries them yet. Slice 3's remaining design is in
[#45](https://github.com/wrkhse/chauffeur/issues/45).

The planned slices are issues under
[Chauffeur v2](https://github.com/wrkhse/chauffeur/issues/43), and build on
the plugin runtime in [#44](https://github.com/wrkhse/chauffeur/issues/44).

| #    | Slice                                                        | Issue                                                |
| ---- | ------------------------------------------------------------ | ---------------------------------------------------- |
| 4    | Recall for opencode-history and the facts tier               | [#46](https://github.com/wrkhse/chauffeur/issues/46) |
| 5    | Harm guard, person tier, permissions and v1's steering rules | [#47](https://github.com/wrkhse/chauffeur/issues/47) |
| 6    | Skills, tools and `ask_chauffeur`                            | [#48](https://github.com/wrkhse/chauffeur/issues/48) |
| 7    | Context renewal, session warming and model failover          | [#49](https://github.com/wrkhse/chauffeur/issues/49) |
| 8-10 | Adapters for Pi, Claude Code and Codex                       | [#50](https://github.com/wrkhse/chauffeur/issues/50) |
| 11   | System Two reasoning models                                  | [#51](https://github.com/wrkhse/chauffeur/issues/51) |
| 12   | Rulebooks, held                                              | [#52](https://github.com/wrkhse/chauffeur/issues/52) |
| 13   | Cutover from v1                                              | [#53](https://github.com/wrkhse/chauffeur/issues/53) |

## Setup

Slice 0 is the workspace every later slice builds in:

- The pinned nightly in `rust-toolchain.toml`, strict workspace lints and the
  LICENSE.
- The [playground](../../../playground/README.md) shell: Vite, TypeScript and
  Bun, with petite-vue and Web Awesome. Rust compiles to WebAssembly with
  wasm-pack, and an edit to Rust rebuilds the module and reloads the page.
- `bacon.toml`, whose jobs build and test through blaze.
