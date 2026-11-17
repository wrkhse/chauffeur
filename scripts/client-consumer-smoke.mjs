import { execFileSync } from "node:child_process"
import { mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"

const root = mkdtempSync(join(tmpdir(), "chauffeur-consumer-"))
const packageDir = resolve(import.meta.dirname, "../packages/chauffeur")

try {
  const packed = execFileSync("npm", ["pack", "--json", "--pack-destination", root], {
    cwd: packageDir,
    encoding: "utf8"
  })
  const archive = join(root, JSON.parse(packed)[0].filename)

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
import { navigate } from "@fdcn/chauffeur/client"
import { ask, judge, noul } from "@fdcn/chauffeur/judge"
void navigate({ on: "example" })
void ask(noul("Done?"), { judge: judge({ tier: "system-one" }) })
`)
  writeFileSync(join(root, "run.mjs"), `
import { navigate } from "@fdcn/chauffeur/client"
import { ask, connect, judge, noul } from "@fdcn/chauffeur/judge"
import { join } from "node:path"
const socket = join(process.cwd(), "missing.sock")
await navigate({ on: "example" }, { socket }).then(
  () => { throw new Error("missing socket answered") },
  error => { if (error.reason !== "unavailable") throw error }
)
const client = connect({ socket })
try {
  const result = await ask(noul("Done?"), { judge: judge({ tier: "system-one" }), client })
  if (result.ok || result.reason !== "unavailable") throw new Error("unexpected answer")
} finally {
  client.close()
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
