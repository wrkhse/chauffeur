#!/usr/bin/env bash
set -euo pipefail

# Offline integration test for carry-forward.sh. A stub gh returns the comment
# fixture, so the real script runs without network access. Each case checks
# GITHUB_OUTPUT, the same way the workflow reads the result.
script="$(dirname -- "${BASH_SOURCE[0]}")/carry-forward.sh"
workflow="$(dirname -- "${BASH_SOURCE[0]}")/../workflows/opencode-review.yml"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/test-carry-forward.XXXXXX")
trap 'rm -rf -- "$tmp"' EXIT

marker='<!-- opencode-review -->'
head=1111111111111111111111111111111111111111
base=2222222222222222222222222222222222222222
other=3333333333333333333333333333333333333333

cat > "$tmp/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[ "${1:-}" = api ] || { echo "stub gh: unexpected command: $*" >&2; exit 2; }
cat -- "$(dirname -- "$0")/comments.json"
STUB
chmod +x "$tmp/gh"

# One marker comment by $id whose meta records $meta_head, $meta_base and
# $verdict. Lower ids are older, so the newest matching comment wins.
comment() {
  local id=$1 meta_head=$2 meta_base=$3 verdict=$4 meta
  meta=$(jq -nc --arg head "$meta_head" --arg base "$meta_base" \
    --arg verdict "$verdict" \
    '{head: $head, base: $base, verdict: $verdict}')
  jq -n --argjson id "$id" --arg marker "$marker" --arg meta "$meta" \
    --arg verdict "$verdict" \
    '{id: $id, user: {id: $id},
      body: ($marker + "\n**Verdict: " + $verdict + "**\n"
        + "<!-- opencode-review-meta " + $meta + " -->")}'
}

# Runs the real script with a JSON array of comments and checks its output.
expect_carried() {
  local name=$1 expected=$2 comments=$3 got
  printf '%s\n' "$comments" > "$tmp/comments.json"
  rm -f -- "$tmp/out"
  PATH="$tmp:$PATH" GITHUB_OUTPUT="$tmp/out" REPO=owner/repo PR=7 \
    HEAD_SHA="$head" BASE_SHA="$base" bash "$script" > "$tmp/stdout" 2> "$tmp/stderr"
  got=$(sed -n 's/^carried=//p' "$tmp/out")
  if [ "$got" != "$expected" ]; then
    cat -- "$tmp/stdout" "$tmp/stderr" >&2
    printf 'expected carried=%s, got %s\n' "$expected" "$got" >&2
    return 1
  fi
  printf 'ok: %s\n' "$name"
}

good=$(comment 200 "$head" "$base" 'MERGE AS IS')
stale=$(comment 100 "$other" "$base" 'MERGE AS IS')

expect_carried 'newest matching marker comment carries the verdict' true \
  "$(jq -cn --argjson stale "$stale" --argjson good "$good" '[$stale, $good]')"

wrong_head=$(comment 200 "$other" "$base" 'MERGE AS IS')
expect_carried 'a different head never carries (shared head SHA)' false \
  "$(jq -cn --argjson c "$wrong_head" '[$c]')"

fix_first=$(comment 200 "$head" "$base" 'FIX FIRST')
expect_carried 'FIX FIRST never carries' false \
  "$(jq -cn --argjson c "$fix_first" '[$c]')"

wrong_base=$(comment 200 "$head" "$other" 'MERGE AS IS')
expect_carried 'a different base never carries' false \
  "$(jq -cn --argjson c "$wrong_base" '[$c]')"

wrong_author=$(comment 12345 "$head" "$base" 'MERGE AS IS')
expect_carried 'a comment from another author never carries' false \
  "$(jq -cn --argjson c "$wrong_author" '[$c]')"

grep -Fq -- '!github.event.changes.body' "$workflow"
printf 'ok: workflow never carries a body edit\n'
