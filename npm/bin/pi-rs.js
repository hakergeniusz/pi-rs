#!/usr/bin/env node
'use strict';

// Tiny launcher for the platform binary downloaded by postinstall.

const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');

const exe = path.join(__dirname, process.platform === 'win32' ? 'pi-rs.exe' : 'pi-rs');

if (!fs.existsSync(exe)) {
  console.error(
    'pi-rs: the native binary is missing.\n' +
      'Reinstall to download it again:\n' +
      '  npm install -g pi-rs\n' +
      '  bun install -g pi-rs\n' +
      'If the download was blocked, check your network / proxy settings and retry.'
  );
  process.exit(1);
}

const result = spawnSync(exe, process.argv.slice(2), { stdio: 'inherit' });

if (result.error) {
  console.error(`pi-rs: failed to start the native binary: ${result.error.message}`);
  process.exit(1);
}

// Forward the child's termination: same exit code, or re-raise its signal.
if (result.signal) process.kill(process.pid, result.signal);
process.exit(result.status === null ? 1 : result.status);
