#!/usr/bin/env node
// The shipped 1.0.0-rc.5 daemon and client against sourcefed's triage seam:
// each of its twelve fixture events goes through ChauffeurTriage, and the
// verdict is compared with the fixture. It starts a real daemon and stops
// it, so it runs only inside scripts/e2e/rc5.sh's Linux container, never on
// a developer's machine.

import assert from "node:assert/strict"
import { execFileSync, spawn } from "node:child_process"
import { existsSync, mkdirSync, readFileSync } from "node:fs"
import { dirname, join, resolve } from "node:path"
import { setTimeout } from "node:timers/promises"
import { ChauffeurTriage, chauffeurNavigators } from "@sourcefed/daemon"
import { connect } from "@fdcn/chauffeur/client"

const ROOT = resolve(import.meta.dirname, "../..")
const FIXTURE = join(
  ROOT,
  "packages/daemon/src/triage/fixtures/sourcefed-events.json"
)
const AT = "2026-11-17T00:00:00.000Z"
const RUN_TIMEOUT = 2 * 60_000

// The fixture's twelve cases, by id: a missing or different case fails the
// proof before a daemon starts.
const CASE_IDS = [
  "bot-coverage",
  "approval",
  "status-churn",
  "assignee-churn",
  "thanks",
  "empty-message",
  "ci-failure",
  "requested-changes",
  "direct-question",
  "merge-conflict",
  "merged-followup",
  "new-fact"
]

/** The most recent daemon output, for a failure message. */
let lastOutput = ""

/** The daemon's tail, for a failure message. */
function tail() {
  return `\ndaemon output:\n${lastOutput.trimEnd() || "(none)"}`
}

/** Run the installed binary and return its trimmed output. */
function run(bin, args, env) {
  return execFileSync(bin, args, {
    env,
    encoding: "utf8",
    timeout: 10_000,
    killSignal: "SIGKILL"
  }).trim()
}

/** The fixture's cases, checked against the twelve recorded ids. */
function readCases() {
  const fixture = JSON.parse(readFileSync(FIXTURE, "utf8"))
  const cases = fixture.cases

  assert.equal(cases.length, 12, `the fixture holds ${cases.length} cases, not 12`)

  const ids = cases.map((each) => each.id)

  assert.equal(new Set(ids).size, ids.length, "the fixture repeats a case id")
  assert.deepEqual(
    [...ids].sort(),
    [...CASE_IDS].sort(),
    "the fixture's cases are not the twelve recorded ones"
  )

  for (const each of cases) {
    assert.equal(
      typeof each.expected?.deliver,
      "boolean",
      `${each.id} has no expected.deliver`
    )
  }

  return cases
}

/** The monitor record sourcefed's replay builds for a case. */
function monitorFor(each) {
  return {
    id: "replay",
    name: `replay ${each.id}`,
    source: each.monitor_source,
    delivery: "poll",
    target: { kind: "replay", id: "replay" },
    pollIntervalSec: 60,
    enabled: true,
    createdAt: AT,
    updatedAt: AT,
    cursors: {}
  }
}

/** The event sourcefed's replay builds from a case's v1 params. */
function eventFor(each) {
  const params = each.v1.params
  const event = {
    source: { type: params.source },
    kind: params.kind,
    at: AT,
    summary: params.summary,
    body: params.body,
    actionable: params.actionable
  }

  if (each.subject) {
    event.subject = { type: "replay", key: each.id, ...each.subject }
  }

  return event
}

/**
 * The client's connect, recording the intents of the case in flight. A late
 * reply from a call ChauffeurTriage already dropped sets nothing.
 */
function recordingConnect(where, activeCase) {
  const client = connect(where)

  return {
    async navigate(request, options) {
      const call = activeCase()
      const result = await client.navigate(request, options)

      if (call && activeCase() === call) call.intents = result.intents

      return result
    },
    close() {
      client.close()
    }
  }
}

/** Start the installed daemon, keeping its output for a failure message. */
function spawnDaemon(bin, env, failure) {
  let output = ""
  const daemon = spawn(bin, ["daemon", "run"], {
    env,
    stdio: ["ignore", "pipe", "pipe"]
  })
  const exited = new Promise((done) => daemon.once("exit", done))

  for (const stream of [daemon.stdout, daemon.stderr]) {
    stream.on("data", (data) => {
      output = (output + data).slice(-8192)
      lastOutput = output
    })
  }

  daemon.once("error", (error) => {
    failure.error = error
  })

  return { daemon, exited }
}

/** Wait, bounded, until the daemon's socket exists. */
async function waitForSocket(daemon, socket, failure) {
  for (let attempt = 0; attempt < 400; attempt++) {
    if (failure.error) throw failure.error
    if (existsSync(socket)) return

    if (daemon.exitCode !== null || daemon.signalCode !== null) {
      throw new Error(`the daemon exited before its socket${tail()}`)
    }

    await setTimeout(50)
  }

  throw new Error(`no socket at ${socket} after 20 s${tail()}`)
}

/** Stop the daemon, with a bound and then a kill. */
async function stopDaemon(daemon, exited) {
  if (daemon.exitCode !== null || daemon.signalCode !== null) return

  daemon.kill("SIGTERM")
  await Promise.race([exited, setTimeout(5000)])

  if (daemon.exitCode === null && daemon.signalCode === null) {
    daemon.kill("SIGKILL")
  }
}

/** Ask every case, print one line each, and return how many failed. */
async function askAll(cases) {
  let active
  const triage = new ChauffeurTriage({
    connect: chauffeurNavigators((where) => recordingConnect(where, () => active))
  })
  const seen = new Set()
  let withheld = 0
  let delivered = 0
  let failed = 0

  console.log("case              expected  actual    intents")

  try {
    for (const each of cases) {
      const expected = each.expected.deliver ? "deliver" : "withhold"
      const call = { id: each.id, intents: [] }

      active = call
      const decision = await triage.decide(monitorFor(each), eventFor(each))
      active = undefined

      const actual = decision.deliver ? "deliver" : "withhold"
      const untriaged = decision.deliver && !decision.triaged
      const mismatch = actual !== expected
      const note = mismatch ? "  MISMATCH" : untriaged ? "  UNTRIAGED" : ""

      seen.add(each.id)
      if (decision.deliver) delivered += 1
      else withheld += 1
      if (mismatch || untriaged) failed += 1

      console.log(
        `${each.id.padEnd(18)}${expected.padEnd(10)}${actual.padEnd(10)}` +
          `${JSON.stringify(call.intents)}${note}`
      )
    }
  } finally {
    triage.close()
  }

  const missing = CASE_IDS.filter((id) => !seen.has(id))

  if (missing.length > 0) {
    console.error(`missing cases: ${missing.join(", ")}`)
    failed += missing.length
  }

  const verdict =
    failed === 0 ? "every verdict matches the fixture" : `${failed} failed`

  console.log(
    `${cases.length} cases: ${withheld} withheld, ${delivered} delivered; ${verdict}`
  )

  return failed
}

async function main() {
  const bin = process.env.CHAUFFEUR_BIN
  const socket = process.env.CHAUFFEUR_SOCKET

  if (!bin) throw new Error("CHAUFFEUR_BIN is required")
  if (!socket) throw new Error("CHAUFFEUR_SOCKET is required")

  const cases = readCases()
  const env = { ...process.env }

  // The installed binary must find its shipped content by itself.
  delete env.CHAUFFEUR_DISTRIBUTION
  delete env.TYPESAFE_API_KEY

  const version = run(bin, ["version"], env)

  assert.equal(
    version,
    "chauffeur-next 1.0.0-rc.5",
    "the binary is not the pinned release"
  )
  console.log(`${version}; ${cases.length} cases; ${socket}`)

  const failure = {}
  const { daemon, exited } = spawnDaemon(bin, env, failure)

  try {
    await waitForSocket(daemon, socket, failure)

    const status = JSON.parse(run(bin, ["daemon", "status"], env))

    assert.ok(
      status.rules?.includes("sourcefed/triage"),
      `the daemon did not load sourcefed/triage: ${JSON.stringify(status)}`
    )

    const failed = await askAll(cases)

    if (failed > 0) process.exitCode = 1
  } finally {
    await stopDaemon(daemon, exited)
  }
}

const watchdog = globalThis.setTimeout(() => {
  console.error(`rc5-triage.mjs did not finish within 2 minutes${tail()}`)
  process.exit(1)
}, RUN_TIMEOUT)

try {
  await main()
} catch (error) {
  const message = error instanceof Error ? error.message : String(error)

  console.error(message.includes("daemon output:") ? message : `${message}${tail()}`)
  process.exitCode = 1
} finally {
  globalThis.clearTimeout(watchdog)
}
