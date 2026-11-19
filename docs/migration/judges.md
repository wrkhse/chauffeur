# Judges

v1 chooses its judge with environment variables, for every question at once.
In v2 every model is an entry in `models.json`, each tier lists its models in
fallback order, and a rule names the judge for each step. The v2 column is
what `chauffeur-next` `1.0.0-rc.5` does: it ships one model, Jev, as the
System One tier.

| v1                                             | v2                                                                                    |
| ---------------------------------------------- | ------------------------------------------------------------------------------------- |
| `CHAUFFEUR_JUDGE=jev`                          | Nothing to set: the shipped `tiers["system-one"]` is `["jev"]`                        |
| `TYPESAFE_API_KEY`, required at startup        | The same variable. With no key the daemon starts, and Jev's steps fail as unavailable |
| `TYPESAFE_BASE_URL`, `TYPESAFE_DEFAULT_MODEL`  | `endpoint` and `model` of a `systemone` entry in your `models.json`                   |
| `CHAUFFEUR_JUDGE_FALLBACK`                     | A second model in the tier's list                                                     |
| A 3-second timeout for each provider           | `timeout_ms` for each model, `3000` for Jev                                           |
| After a failure, the fallback answers for 60 s | Every question starts at the tier's first model                                       |
| A failure or an invalid answer falls back      | Unavailable, rate-limited or timed out falls back; an invalid answer fails the step   |
| One judge for every rule                       | A rule's `judges`: each names a tier and its `confidence`, and may name a `model`     |

Your `models.json` loads after the shipped one: an entry replaces the shipped
entry of the same name, and a tier's list replaces the shipped list. This
one moves `TYPESAFE_BASE_URL` for a proxy, and falls back to the shipped
`jev`:

```json
{
  "models": {
    "jev-proxy": {
      "format": "systemone",
      "endpoint": "https://typesafe.proxy.example",
      "model": "jev-latest",
      "key": { "env": "TYPESAFE_API_KEY" },
      "timeout_ms": 2000
    }
  },
  "tiers": { "system-one": ["jev-proxy", "jev"] }
}
```

A rule then picks its judge by tier:

```json
"judges": { "quick": { "tier": "system-one", "confidence": 0.7 } }
```

`1.0.0-rc.5` reads two formats: `systemone`, the API Jev speaks, and
`placeholder`, fixed or recorded answers for tests. These parts of v1's
judge configuration have no counterpart in it and stay with v1, each with the
issue that brings it:

- `CHAUFFEUR_JUDGE=subq` or `glide`, and `CHAUFFEUR_JUDGE_ESCALATE`:
  [#45](https://github.com/wrkhse/chauffeur/issues/45);
- `judge.json`'s `routes` to the facts judge:
  [#46](https://github.com/wrkhse/chauffeur/issues/46);
- `judge.json`'s `system_two.profiles`:
  [#51](https://github.com/wrkhse/chauffeur/issues/51).

How a tier's models take turns is in
[`tiers`](../../crates/judges/src/tiers/README.md), the file's format in
[`config`](../../crates/judges/src/config/README.md), and what `confidence`
decides in [`confidence`](../../crates/judges/src/confidence/README.md).
