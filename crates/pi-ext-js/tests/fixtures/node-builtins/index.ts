/**
 * E4 fixture: one extension importing every `node:*` built-in and host
 * package the port serves, then calling a representative API from each.
 * The default factory stores a JSON report on `globalThis`, which the Rust
 * tests assert against. All filesystem work is under the OS temp dir.
 */

import { Buffer } from "node:buffer";
import { execFileSync, spawnSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { homedir, platform, tmpdir } from "node:os";
import { basename, extname, join, relative, resolve } from "node:path";
import process from "node:process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { format, inspect, types } from "node:util";
import {
  getAllTools,
  getActiveTools,
  getSessionMetadata,
  getSettings,
  setActiveTools,
} from "@earendil-works/pi-coding-agent";
import { createMcpToolName, mcpNamespace } from "@earendil-works/pi-mcp";

export default function (): void {
  const file = join(tmpdir(), `pi-ext-js-node-fixture-${Date.now()}-${Math.random()}.txt`);
  writeFileSync(file, "fixture", "utf8");

  const result = {
    buffer: Buffer.from("hi").toString("hex"),
    bufferAlloc: Buffer.alloc(3, "a").toString("utf8"),
    hash: createHash("sha256").update("abc").digest("hex"),
    uuidShape: /^[0-9a-f-]{36}$/.test(randomUUID()),
    fsRead: readFileSync(file, "utf8"),
    fsExists: existsSync(file),
    fsBasename: basename(file),
    fsExt: extname(file),
    fsAsync: "",
    osPlatform: platform(),
    osHome: typeof homedir(),
    pathJoin: join("a", "b", "c"),
    pathResolve: resolve("/work", "x", "..", "y"),
    pathRelative: relative("/work/a", "/work/b"),
    processCwd: process.cwd(),
    urlFromPath: fileURLToPath("file:///tmp/a%20b.txt"),
    urlToPath: pathToFileURL("/tmp/a b.txt").href,
    exec: execFileSync("printf", ["hello"], { encoding: "utf8" }),
    spawnStatus: spawnSync("true", []).status,
    utilFormat: format("%s=%d", "x", 3),
    utilInspect: inspect({ a: 1 }),
    utilTypes: types.isArray([]) && types.isBuffer(Buffer.from("x")),
    settings: getSettings().model,
    sessionCwd: getSessionMetadata().cwd,
    tools: getAllTools().length,
    activeTools: getActiveTools().length,
    mcpNamespace: mcpNamespace("my-server"),
    mcpToolName: createMcpToolName("srv", "do.thing"),
  };
  setActiveTools(["existing-tool"]);

  readFile(file, "utf8").then((text) => {
    result.fsAsync = text;
    rmSync(file, { force: true });
    (globalThis as Record<string, unknown>).__nodeResult = JSON.stringify(result);
  });
}
