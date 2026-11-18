# End-to-end proofs

Container-only proofs that a shipped release works with the caller it was
built for. They install real packages and start real daemons, so they must
not run on the developer's machine.

| File             | Holds                                                     |
| ---------------- | --------------------------------------------------------- |
| `rc5.sh`         | Checksums, Homebrew layout, sourcefed, config and the run |
| `rc5-triage.mjs` | The twelve fixture events through the client and triage   |

## Triage on 1.0.0-rc.5

This proof asks the shipped `chauffeur-next` daemon, through the shipped
`@fdcn/chauffeur` client, whether sourcefed should deliver each of twelve
recorded events. It drives the seam sourcefed's worker uses:

```js
const triage = new ChauffeurTriage({ connect: chauffeurNavigators(connect) })

const decision = await triage.decide(monitor, event)
```

`decide` builds the evidence, asks the daemon, and returns `deliver`. The
driver compares that with `expected.deliver` in
`packages/daemon/src/triage/fixtures/sourcefed-events.json`: six events
withhold and six deliver.

### Pinned inputs

- Daemon: `chauffeur-1.0.0-rc.5-linux-arm64.tar.gz`
  - sha256: `983a4339b62f642fa9f03f9b5c16d0c95f6ac98e8aef78f3ef2744166c2e9a41`
- Client: `fdcn-chauffeur-1.0.0-rc.5.tgz`
  - sha256: `8f24e8ed8f4a6c36ab2238722e90dddf026e8bbbc17f7522aab96798baa26d68`
- Sourcefed: `wrkhse/sourcefed`, branch `main`, commit `0a1a5e7`
  (PR #29's merge commit)

`rc5.sh` checks both checksums with `sha256sum -c` before anything runs.
It pins the sourcefed checkout to `0a1a5e73f5c53147fbd2ba5a1100fed89a2a1a8d`,
PR #29's merge commit on `main`, and fails if `HEAD` is anything else.

### Run it

It is meant for an arm64 Linux container with Node 22. On an Apple silicon
Mac:

```sh
mkdir -p dist
curl -fL -o dist/chauffeur-1.0.0-rc.5-linux-arm64.tar.gz \
  https://github.com/wrkhse/chauffeur/releases/download/v1.0.0-rc.5/chauffeur-1.0.0-rc.5-linux-arm64.tar.gz
curl -fL -o dist/fdcn-chauffeur-1.0.0-rc.5.tgz \
  https://github.com/wrkhse/chauffeur/releases/download/v1.0.0-rc.5/fdcn-chauffeur-1.0.0-rc.5.tgz

docker run --rm --platform linux/arm64 \
  -v "$PWD:/src:ro" \
  -v rc5-work:/work \
  -v "$PWD/dist:/art:ro" \
  node:22-bookworm \
  bash /src/scripts/e2e/rc5.sh
```

The repository is read-only at `/src`; the run keeps everything it writes
in the `rc5-work` volume at `/work` and reads the two tarballs from `/art`.
The volume holds the daemon's Unix socket, which a host bind mount may not
support. It clones sourcefed from GitHub into `/work`, so the container needs
network access.

**Never run `rc5.sh` on the developer's machine.** It installs a Linux
binary, runs `npm ci` and starts a daemon. The script refuses anything but
Linux and requires a container marker (`/.dockerenv` or
`/run/.containerenv`); the arm64 tarball does not run on macOS.

### What it does

1. Verifies the two checksums in `/art`.
2. Unpacks the daemon tarball and installs it as Homebrew does:
   `bin/chauffeur` becomes `bin/chauffeur-next`, and `share/chauffeur/`
   becomes `share/chauffeur-next/`.
3. Clones sourcefed's `main`, checks out the pinned merge commit, runs
   `npm ci` and `node scripts/build-tests.mjs`, and unpacks the client
   tarball as `@fdcn/chauffeur` in the checkout's `node_modules`.
4. Writes `$XDG_CONFIG_HOME/chauffeur-next/models.json` and a placeholder
   answers file generated from the fixture, one answer per case.
5. Writes `$XDG_CONFIG_HOME/chauffeur-next/rules/sourcefed/triage.json`
   from sourcefed's own `TRIAGE_RULE`, transpiled from
   `packages/cli/src/utils/chauffeur-rule.ts` and written by its
   `installTriageRule`, so the harness never restates the rule.
6. Runs `rc5-triage.mjs`, which starts the installed daemon and asks it each
   of the twelve cases.

### What it prints

```text
chauffeur-next 1.0.0-rc.5; 12 cases; /work/run/daemon.sock
case              expected  actual    intents
bot-coverage      withhold  withhold  [{"rule":"sourcefed/triage","withhold":"event"}]
approval          withhold  withhold  [{"rule":"sourcefed/triage","withhold":"event"}]
...
ci-failure        deliver   deliver   []
...
12 cases: 6 withheld, 6 delivered; every verdict matches the fixture
```

The intents column is the daemon's own reply, captured through the client
`connect` the driver hands to `chauffeurNavigators`. Sourcefed's triage
also logs one connection line to stderr when the first case starts.

### Record it

The recording is one `termctrl` session running the same container, marked
at four moments and cut by `scripts/e2e/rc5.plan.json`. Start it with
`--record`, then mark `verify` once both checksums pass and sourcefed is at
its pinned head, `triage` as the case table starts, `verdict` once the
summary line is on screen, and `end` once the summary has settled:

```sh
termctrl start rc5 --record "$TMPDIR/rc5.termctrl" --cols 120 --rows 40 -- \
  docker run --rm --platform linux/arm64 \
  -v "$PWD:/src:ro" -v rc5-work:/work -v "$PWD/dist:/art:ro" \
  node:22-bookworm bash /src/scripts/e2e/rc5.sh

termctrl mark rc5 verify
termctrl mark rc5 triage
termctrl mark rc5 verdict
termctrl mark rc5 end
termctrl stop rc5
```

Render the recording with the plan's captions and the footer into the
scratch folder. At `--cols 120`, a 7 px cell and a 2x pixel ratio, the video
is about 1700 px wide, so the GIF scales it to 1200 px at 10 frames a second:

```sh
termctrl video "$TMPDIR/rc5.termctrl" --out "$TMPDIR/slice-2.mp4" \
  --edit scripts/e2e/rc5.plan.json --cell-width 7 --pixel-ratio 2 --footer

ffmpeg -y -i "$TMPDIR/slice-2.mp4" -vf \
  "fps=10,scale=1200:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128:stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle" \
  -loop 0 docs/media/slice-2.gif
```

### How it fails

`rc5.sh` exits non-zero on a checksum mismatch, a sourcefed checkout at
another head, a failed build, or a rule that is not written. The driver exits
non-zero if the installed binary is not `chauffeur-next 1.0.0-rc.5`, the
daemon does not create its socket, `daemon status` does not list
`sourcefed/triage`, the fixture does not hold the twelve cases as
labelled, or any `deliver` differs from the fixture. A deliver case the daemon
answered without triaging is marked `UNTRIAGED` and fails too, so a missing
answer cannot pass as a correct delivery. A failure prints the case's line and a
summary, and the last 8 KiB of daemon output follows an error.

### What it does not prove

The judge is a placeholder: the rule's `quick` judge at system-one reads a
recorded answer per case, so this proves that the shipped client, daemon, rule
loading and verdict path agree with sourcefed's `ChauffeurTriage`, not that a
real model would give those verdicts. It also does not exercise sourcefed's
watcher, polling or HTTP delivery, which sourcefed's own tests cover, and it
mimics Homebrew's layout rather than running `brew`.
