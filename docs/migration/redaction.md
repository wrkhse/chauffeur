# Redaction

v2 redacts evidence before any model call, as v1 does, with the same shipped
prefixes and the same shapes. Only the files move. What is matched, and what
is sent in its place, is in
[`redact`](../../crates/runtime/src/redact/README.md).

| v1                                                            | v2                                                             |
| ------------------------------------------------------------- | -------------------------------------------------------------- |
| Shipped prefixes in `skills/safety/redaction.json`            | `safety/redaction.json` in the shipped content, the same list  |
| `$CHAUFFEUR_CONFIG_DIR/redaction.json`: `prefixes`, `replace` | `~/.config/chauffeur-next/redaction.json`, the same two fields |
| `$CHAUFFEUR_STATE_DIR/learned-redaction.json`, learned shapes | Not read: `1.0.0-rc.5` masks by prefix and shape alone         |

Your own prefixes move as they are:

```sh
cp ~/.config/chauffeur/redaction.json ~/.config/chauffeur-next/redaction.json
```

The daemon uses the file from the next event. An edit that does not load,
such as a file with a field other than `prefixes` and `replace`, keeps the
prefixes loaded before. Learned shapes stay with v1;
[#47](https://github.com/wrkhse/chauffeur/issues/47) brings them.
