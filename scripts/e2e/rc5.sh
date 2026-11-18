#!/usr/bin/env bash
# The 1.0.0-rc.5 end-to-end proof, inside a Linux arm64 container. It
# verifies the release artifacts, builds sourcefed at its pinned head, writes
# the daemon's rule and models, and runs scripts/e2e/rc5-triage.mjs.
#
# It installs a Linux binary, runs npm ci and starts a daemon, so it never
# runs on the developer's machine:
#
#   docker run --rm --platform linux/arm64 \
#     -v "$PWD:/src:ro" -v rc5-work:/work -v "$PWD/dist:/art:ro" \
#     node:22-bookworm bash /src/scripts/e2e/rc5.sh

set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  echo "rc5.sh runs in the Linux container only;" \
    "it must not run on the developer's machine" >&2
  exit 1
fi

# Linux alone does not prove a container, and this script deletes /work.
if [ ! -e /.dockerenv ] && [ ! -e /run/.containerenv ]; then
  echo "rc5.sh must run in a container, not a Linux host: neither" \
    "/.dockerenv nor /run/.containerenv exists, and it deletes /work" >&2
  exit 1
fi

REPO=/src
ART=/art
WORK=/work
CONFIG=$WORK/config
STATE=$WORK/state
RUN=$WORK/run
SRC=$WORK/sourcefed
PREFIX=$WORK/prefix
BIN=$PREFIX/bin/chauffeur-next
SOCKET=$WORK/run/daemon.sock

SOURCEFED=https://github.com/wrkhse/sourcefed.git
# PR #29's squash merge on main, so the run proves the merged commit.
SOURCEFED_HEAD=0a1a5e73f5c53147fbd2ba5a1100fed89a2a1a8d
DAEMON=chauffeur-1.0.0-rc.5-linux-arm64.tar.gz
CLIENT=fdcn-chauffeur-1.0.0-rc.5.tgz

echo "== checksums"
(cd "$ART" && sha256sum -c - <<'SUMS'
983a4339b62f642fa9f03f9b5c16d0c95f6ac98e8aef78f3ef2744166c2e9a41  chauffeur-1.0.0-rc.5-linux-arm64.tar.gz
8f24e8ed8f4a6c36ab2238722e90dddf026e8bbbc17f7522aab96798baa26d68  fdcn-chauffeur-1.0.0-rc.5.tgz
SUMS
)

# Both artifacts are verified before the work dir is cleared, so a wrong
# tarball never destroys a previous run's work.
rm -rf "$CONFIG" "$STATE" "$RUN" "$SRC" "$PREFIX" "$WORK/unpacked"
mkdir -p "$CONFIG" "$STATE" "$PREFIX/bin" "$PREFIX/share" "$WORK/unpacked"
# The daemon refuses a socket folder another mode or owner could reach.
install -d -m 0700 "$RUN"

echo "== chauffeur-next"
tar -xzf "$ART/$DAEMON" -C "$WORK/unpacked"
# Homebrew's layout: bin/chauffeur becomes bin/chauffeur-next, and
# share/chauffeur/ becomes share/chauffeur-next/ beside it.
install -m 0755 "$WORK/unpacked/bin/chauffeur" "$BIN"
cp -R "$WORK/unpacked/share/chauffeur" "$PREFIX/share/chauffeur-next"
"$BIN" version

echo "== sourcefed ${SOURCEFED_HEAD:0:7}"
git clone --quiet --branch main --single-branch "$SOURCEFED" "$SRC"
git -C "$SRC" checkout --quiet --detach "$SOURCEFED_HEAD"

head=$(git -C "$SRC" rev-parse HEAD)

if [ "$head" != "$SOURCEFED_HEAD" ]; then
  echo "sourcefed is at $head, not $SOURCEFED_HEAD" >&2
  exit 1
fi

echo "sourcefed ${head:0:7}"

(
  cd "$SRC"
  npm ci --no-audit --no-fund --loglevel=error
  node scripts/build-tests.mjs
)

echo "== client @fdcn/chauffeur"
# The tarball is the verified release asset, not a registry package; its
# exports resolve from the checkout's node_modules.
mkdir -p "$SRC/node_modules/@fdcn/chauffeur" "$SRC/scripts/e2e"
tar -xzf "$ART/$CLIENT" -C "$SRC/node_modules/@fdcn/chauffeur" --strip-components=1
cp "$REPO/scripts/e2e/rc5-triage.mjs" "$SRC/scripts/e2e/rc5-triage.mjs"

echo "== config"
mkdir -p "$CONFIG/chauffeur-next/rules/sourcefed"

# One recorded answer per fixture case, matched on the event block sourcefed
# builds: yes 0.9 for a deliver case, no 0.9 for a withhold case.
# shellcheck disable=SC2016 # the JS uses template literals, not shell expansion
FIXTURE="$SRC/packages/daemon/src/triage/fixtures/sourcefed-events.json" \
ANSWERS="$CONFIG/chauffeur-next/triage-answers.json" \
node --input-type=module -e '
import { readFileSync, writeFileSync } from "node:fs"

const fixture = JSON.parse(readFileSync(process.env.FIXTURE, "utf8"))
const answers = fixture.cases.map((each) => {
  if (typeof each.evidence?.event !== "string") {
    throw new Error(`${each.id} has no evidence.event`)
  }

  return {
    evidence: { event: each.evidence.event },
    p: each.expected.deliver ? { yes: 0.9, no: 0.1 } : { yes: 0.1, no: 0.9 }
  }
})

writeFileSync(process.env.ANSWERS, `${JSON.stringify({ answers }, null, 2)}\n`)
console.log(`${answers.length} recorded answers`)
'

cat > "$CONFIG/chauffeur-next/models.json" <<'JSON'
{
  "models": {
    "triage": {
      "format": "placeholder",
      "answers": "triage-answers.json"
    }
  },
  "tiers": { "system-one": ["triage"] }
}
JSON

# The rule comes from sourcefed's own TRIAGE_RULE, transpiled from its
# source and written by its own installer, so this harness never restates it.
(
  cd "$SRC"
  ./node_modules/.bin/esbuild packages/cli/src/utils/chauffeur-rule.ts \
    --platform=node --format=esm --target=node22 \
    --outfile=triage-rule.mjs --log-level=warning

  # shellcheck disable=SC2016 # the JS uses template literals, not shell expansion
  XDG_CONFIG_HOME="$CONFIG" node --input-type=module -e '
import { installTriageRule } from "./triage-rule.mjs"

const installed = await installTriageRule({ name: "chauffeur-next" })

if (installed.status !== "written") {
  console.error(
    `the rule was not written: ${installed.status} ${installed.path} ` +
      `${installed.reason ?? ""}`
  )
  process.exit(1)
}

console.log(`wrote ${installed.path}`)
'
)

echo "== triage"
(
  cd "$SRC"
  CHAUFFEUR_BIN="$BIN" \
  CHAUFFEUR_SOCKET="$SOCKET" \
  XDG_CONFIG_HOME="$CONFIG" \
  XDG_STATE_HOME="$STATE" \
  node scripts/e2e/rc5-triage.mjs
)
