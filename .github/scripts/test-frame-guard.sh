#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$script_dir/frame-match.sh"

needle='"command":"git status"'

# A narrow pane wraps the JSON in the middle of the needle and indents
# the continuation. The stripped comparison must still see the needle.
wrapped=$(cat <<'FRAME'
$ printf '%s\n' '{"id":"record-1","input":{"com
    mand":"git status"}}}'
FRAME
)

# The same request rendered without a wrap.
single=$(cat <<'FRAME'
$ printf '%s\n' '{"id":"record-1","input":{"command":"git status"}}}'
FRAME
)

# An audit entry: no command key at all.
unrelated=$(cat <<'FRAME'
$ ./chauffeur-core audit 1
{"request_id":"record-1","outcome":"allow"}
FRAME
)

if ! frame_matches "$wrapped" "$needle"; then
  printf 'the wrapped frame did not match %s\n' "$needle" >&2
  exit 1
fi

if ! frame_matches "$single" "$needle"; then
  printf 'the single-line frame did not match %s\n' "$needle" >&2
  exit 1
fi

if frame_matches "$unrelated" "$needle"; then
  printf 'the unrelated frame matched %s\n' "$needle" >&2
  exit 1
fi

printf 'ok: frame guard matches across a wrap\n'
