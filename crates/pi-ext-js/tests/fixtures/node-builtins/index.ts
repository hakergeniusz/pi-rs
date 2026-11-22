/**
 * E4 fixture: one extension importing every `node:*` built-in and host
 * package the port serves, then calling a representative API from each.
 * The default factory stores a JSON report on `globalThis`, which the Rust
 * tests assert against. All filesystem work is under the OS temp dir, and
 * every environment value used here is synthetic.
 */

import { Buffer } from "node:buffer";
import { execFileSync, spawnSync } from "node:child_process";
import { createHash, createHmac, getHashes, randomUUID } from "node:crypto";
import {
  existsSync,
  lstatSync,
  readFileSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { readFile } from "node:fs/promises";
import { homedir, platform, tmpdir } from "node:os";
import { basename, extname, join, relative, resolve } from "node:path";
import process from "node:process";
import { fileURLToPath, pathToFileURL, URL, URLSearchParams } from "node:url";
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
  // QuickJS seeds Math.random() identically in each fresh runtime, so two
  // tests can otherwise pick the same temp path and collide on symlink().
  // The exists-check alone does not save parallel tests: both can pass it
  // before either creates the pair, so creation retries on EEXIST too.
  let file = "";
  let link = "";
  let attempt = 0;
  for (;;) {
    file = join(tmpdir(), `pi-ext-js-node-fixture-${Date.now()}-${Math.random()}-${attempt++}.txt`);
    link = `${file}.link`;
    if (existsSync(file) || existsSync(link)) continue;
    try {
      writeFileSync(file, "fixture", "utf8");
      symlinkSync(file, link);
      break;
    } catch (error) {
      if ((error as { code?: string } | null)?.code !== "EEXIST") throw error;
    }
  }

  const absolute = new URL("https://User:pass@Example.COM:8080/a/b?x=1#f");
  const searchUrl = new URL("http://x/?a=1&b=2");
  searchUrl.searchParams.set("a", "9");
  const relativeUrl = new URL("../img.png", "https://example.com/a/b/page.html");

  const envKeys = Object.keys(process.env);
  const fileStat = statSync(file);
  const directoryStat = statSync(tmpdir());

  const result = {
    buffer: Buffer.from("hi").toString("hex"),
    bufferAlloc: Buffer.alloc(3, "a").toString("utf8"),
    hash: createHash("sha256").update("abc").digest("hex"),
    hmac: createHmac("sha256", "key").update("abc").digest("hex"),
    hmacChunked: createHmac("sha256", "key")
      .update("The quick brown fox ")
      .update("jumps over the lazy dog")
      .digest("hex"),
    hmacBufferCopy: createHmac("sha256", "key")
      .update("abc")
      .copy()
      .digest("hex"),
    hmacAlgorithms: getHashes().indexOf("sha256") >= 0,
    uuidShape: /^[0-9a-f-]{36}$/.test(randomUUID()),
    fsRead: readFileSync(file, "utf8"),
    fsExists: existsSync(file),
    fsBasename: basename(file),
    fsExt: extname(file),
    fsAsync: "",
    fsStatIsFile: fileStat.isFile(),
    fsStatIsDirectory: directoryStat.isDirectory(),
    fsStatSize: fileStat.size,
    fsStatModeType: typeof fileStat.mode,
    fsStatMtimeType: typeof fileStat.mtimeMs,
    fsLstatLinkIsLink: lstatSync(link).isSymbolicLink(),
    fsStatLinkIsLink: statSync(link).isSymbolicLink(),
    osPlatform: platform(),
    osHome: typeof homedir(),
    pathJoin: join("a", "b", "c"),
    pathResolve: resolve("/work", "x", "..", "y"),
    pathRelative: relative("/work/a", "/work/b"),
    processCwd: process.cwd(),
    processCwdType: typeof process.cwd(),
    processPlatformType: typeof process.platform,
    processArchType: typeof process.arch,
    processVersionType: typeof process.version,
    processEnvIsObject: typeof process.env === "object" && process.env !== null,
    processEnvKeysValid: envKeys.every(
      (key) =>
        key.length > 0 &&
        /^[A-Za-z_][A-Za-z0-9_]*$/.test(key) &&
        !/key|token|secret|password|credential|auth/i.test(key),
    ),
    urlFromPath: fileURLToPath("file:///tmp/a%20b.txt"),
    urlToPath: pathToFileURL("/tmp/a b.txt").href,
    urlHref: absolute.href,
    urlProtocol: absolute.protocol,
    urlHost: absolute.host,
    urlHostname: absolute.hostname,
    urlPort: absolute.port,
    urlOrigin: absolute.origin,
    urlPathname: new URL("http://x/a b/cé").pathname,
    urlDefaultPort: new URL("http://example.com:80/x").port,
    urlFileOrigin: new URL("file:///tmp/x").origin,
    urlSearchParams: [searchUrl.searchParams.get("a"), searchUrl.search, searchUrl.href],
    urlSearchParamsEncode: new URLSearchParams({ a: "hello world" }).toString(),
    urlRelative: relativeUrl.href,
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
    rmSync(link, { force: true });
    (globalThis as Record<string, unknown>).__nodeResult = JSON.stringify(result);
  });
}
