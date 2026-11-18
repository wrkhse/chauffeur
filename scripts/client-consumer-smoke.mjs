import { execFileSync } from "node:child_process"
import { mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { createServer } from "node:net"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"

const root = mkdtempSync(join(tmpdir(), "chauffeur-consumer-"))
const packageDir = resolve(import.meta.dirname, "../packages/chauffeur")

try {
  const supplied = process.argv[2]
  const archive = supplied
    ? resolve(supplied)
    : join(
        root,
        JSON.parse(
          execFileSync("npm", ["pack", "--json", "--pack-destination", root], {
            cwd: packageDir,
            encoding: "utf8"
          })
        )[0].filename
      )
  const packed = execFileSync("tar", ["-tzf", archive], { encoding: "utf8" })
    .split("\n")
    .filter(Boolean)
    .map((line) => line.replace(/^package\//, ""))

  for (const required of ["LICENSE", "README.md"]) {
    if (!packed.includes(required)) {
      throw new Error(`the tarball has no ${required}: ${packed.join(", ")}`)
    }
  }

  writeFileSync(join(root, "package.json"), JSON.stringify({ private: true, type: "module" }))
  writeFileSync(join(root, "tsconfig.json"), JSON.stringify({
    compilerOptions: {
      module: "NodeNext",
      moduleResolution: "NodeNext",
      noEmit: true,
      strict: true,
      skipLibCheck: false
    }
  }))
  writeFileSync(join(root, "index.ts"), `
import { connect, NavigateError, navigate } from "@fdcn/chauffeur/client"
import { ask, judge, noul } from "@fdcn/chauffeur/judge"

const client = connect()

async function run(): Promise<void> {
  const abort = new AbortController()
  const events: string[] = []

  try {
    const { intents } = await client.navigate(
      { on: "example" },
      { signal: abort.signal, onEvent: (event) => events.push(event.event) }
    )

    void intents
    void events
  } catch (error) {
    if (error instanceof NavigateError) {
      const reason: string = error.reason
      const limit: string | undefined = error.limit

      void reason
      void limit
    }
  } finally {
    client.close()
  }
}

void run()
void navigate({ on: "example" })
void ask(noul("Done?"), { judge: judge({ tier: "system-one" }) })
`)
  writeFileSync(join(root, "run.mjs"), `
import { connect, NavigateError, navigate } from "@fdcn/chauffeur/client"
import { ask, judge, noul } from "@fdcn/chauffeur/judge"
import { createServer } from "node:net"
import { join } from "node:path"

const missing = join(process.cwd(), "missing.sock")

await navigate({ on: "example" }, { socket: missing }).then(
  () => { throw new Error("missing socket answered") },
  (error) => {
    if (!(error instanceof NavigateError)) throw error
    if (error.reason !== "unavailable") throw error
  }
)

const events = []
const daemon = createServer((socket) => {
  socket.setEncoding("utf8")
  let received = ""

  socket.on("data", (chunk) => {
    received += chunk
    let end = received.indexOf("\\n")

    while (end !== -1) {
      const message = JSON.parse(received.slice(0, end))
      received = received.slice(end + 1)
      end = received.indexOf("\\n")
      const event = {
        jsonrpc: "2.0",
        method: "chauffeur.event",
        params: { id: message.id, event: "rule_matched", rule: "example" }
      }

      socket.write(\`\${JSON.stringify(event)}\\n\`)
      socket.write(
        \`\${JSON.stringify({ jsonrpc: "2.0", id: message.id, result: { intents: [] } })}\\n\`
      )
    }
  })
})

const socket = join(process.cwd(), "stand-in.sock")

await new Promise((resolve, reject) => {
  daemon.once("error", reject)
  daemon.listen(socket, resolve)
})

const client = connect({ socket })

try {
  const { intents } = await client.navigate(
    { on: "example" },
    { onEvent: (event) => events.push(event.event) }
  )

  if (intents.length !== 0) throw new Error("unexpected intents")
  if (events.join(",") !== "rule_matched") throw new Error(\`onEvent saw \${events}\`)
} finally {
  client.close()
  await new Promise((resolve) => daemon.close(resolve))
}

const missingClient = connect({ socket: missing })

try {
  const result = await ask(noul("Done?"), {
    judge: judge({ tier: "system-one" }),
    client: missingClient
  })

  if (result.ok || result.reason !== "unavailable") throw new Error("unexpected answer")
} finally {
  missingClient.close()
}
`)
  execFileSync("npm", ["install", "--no-audit", "--no-fund", archive, "typescript@7.0.2"], {
    cwd: root,
    stdio: "inherit"
  })
  execFileSync(process.execPath, [join(root, "node_modules/typescript/bin/tsc"), "--noEmit"], {
    cwd: root,
    stdio: "inherit"
  })
  execFileSync(process.execPath, [join(root, "run.mjs")], { cwd: root, stdio: "inherit" })
  console.log("Packed NodeNext consumer passed")
} finally {
  rmSync(root, { recursive: true, force: true })
}
