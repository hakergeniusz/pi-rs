#!/usr/bin/env node
'use strict';

// postinstall: fetch the pi-rs release binary that matches this host and
// verify its SHA-256 checksum BEFORE writing anything into ./bin.
//
// Deliberately small and auditable: HTTPS only, no shell invocations, no
// eval, fixed URLs derived from a hardcoded version, and a hard timeout.

const crypto = require('crypto');
const fs = require('fs');
const path = require('path');

const VERSION = '0.1.1';
const RELEASE_BASE = `https://github.com/hakergeniusz/pi-rs/releases/download/v${VERSION}`;
const DOWNLOAD_TIMEOUT_MS = 5 * 60 * 1000; // 5 min for a ~22 MB binary

// Node's process.platform / process.arch -> published release asset name.
function assetFor(platform, arch) {
  const table = {
    'linux-x64': 'pi-linux-x86_64',
    'linux-arm64': 'pi-linux-aarch64',
    'darwin-x64': 'pi-darwin-x86_64',
    'darwin-arm64': 'pi-darwin-aarch64',
    'win32-x64': 'pi-windows-x86_64.exe',
  };
  const asset = table[`${platform}-${arch}`];
  if (!asset) {
    throw new Error(
      `unsupported platform ${platform}/${arch}; ` +
        'build pi-rs from source at https://github.com/hakergeniusz/pi-rs'
    );
  }
  return asset;
}

async function get(url) {
  let response;
  try {
    response = await fetch(url, {
      redirect: 'follow',
      signal: AbortSignal.timeout(DOWNLOAD_TIMEOUT_MS),
    });
  } catch (err) {
    throw new Error(`download failed for ${url}: ${err.message}`);
  }
  if (!response.ok) {
    throw new Error(`download failed for ${url}: HTTP ${response.status} ${response.statusText}`);
  }
  return response;
}

// Accept "<sha256>  <filename>" (sha256sum / shasum), "<sha256> *<filename>",
// or a bare 64-hex digest. Only a digest for our asset (or an unnamed one)
// counts; a SHA256SUMS entry for a different file is rejected.
function parseChecksum(text, asset) {
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    const m = /^([0-9a-fA-F]{64})(?:\s+\*?\s*(\S.*))?$/.exec(line);
    if (!m) continue;
    const name = m[2] ? m[2].trim() : '';
    if (name === '' || name === asset) return m[1].toLowerCase();
  }
  return null;
}

async function main() {
  if (process.env.PI_RS_SKIP_DOWNLOAD === '1') {
    console.log('pi-rs install: PI_RS_SKIP_DOWNLOAD=1, skipping binary download');
    return;
  }

  const asset = assetFor(process.platform, process.arch);
  console.log(`pi-rs install: fetching ${asset} from release v${VERSION}`);

  const binaryResponse = await get(`${RELEASE_BASE}/${asset}`);
  const binary = Buffer.from(await binaryResponse.arrayBuffer());

  const checksumResponse = await get(`${RELEASE_BASE}/${asset}.sha256`);
  const checksumText = await checksumResponse.text();

  const expected = parseChecksum(checksumText, asset);
  if (!expected) {
    throw new Error(`no valid SHA-256 entry for '${asset}' in checksum file`);
  }
  const actual = crypto.createHash('sha256').update(binary).digest('hex');
  if (actual !== expected) {
    throw new Error(
      `SHA-256 mismatch for '${asset}': expected ${expected}, got ${actual} ` +
        '(nothing was installed)'
    );
  }
  console.log(`pi-rs install: checksum OK (${actual})`);

  const binDir = path.join(__dirname, 'bin');
  const targetName = process.platform === 'win32' ? 'pi-rs.exe' : 'pi-rs';
  const target = path.join(binDir, targetName);
  const temp = path.join(binDir, `${targetName}.download-${process.pid}`);

  fs.mkdirSync(binDir, { recursive: true });
  fs.writeFileSync(temp, binary);
  if (process.platform !== 'win32') fs.chmodSync(temp, 0o755);
  fs.renameSync(temp, target);

  console.log(`pi-rs install: installed ${asset} -> ${target}`);
}

main().catch((err) => {
  console.error(`pi-rs install: ${err && err.message ? err.message : String(err)}`);
  process.exit(1);
});
