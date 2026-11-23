#!/usr/bin/env node
'use strict';

// Tiny launcher for the platform binary downloaded by postinstall.

const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');

const exe = path.join(__dirname, process.platform === 'win32' ? 'pi-rs.exe' : 'pi-rs');

if (!fs.existsSync(exe)) {
  // Some npm versions gate postinstall scripts (npm 11 `install-scripts`).
  // Self-heal by running our own vetted installer before giving up.
  console.error('pi-rs: native binary missing; downloading it now...');
  const install = spawnSync(process.execPath, [path.join(__dirname, '..', 'install.js')], {
    stdio: 'inherit',
  });
  if (install.status !== 0 || !fs.existsSync(exe)) {
    console.error(
      'pi-rs: could not download the native binary.\n' +
        'Reinstall with:\n' +
        '  npm install -g @hakergeniusz/pi-rs\n' +
        '  bun install -g @hakergeniusz/pi-rs\n' +
        'If the download was blocked, check your network / proxy settings and retry.'
    );
    process.exit(1);
  }
}

const result = spawnSync(exe, process.argv.slice(2), { stdio: 'inherit' });

if (result.error) {
  console.error(`pi-rs: failed to start the native binary: ${result.error.message}`);
  process.exit(1);
}

// Forward the child's termination: same exit code, or re-raise its signal.
if (result.signal) process.kill(process.pid, result.signal);
process.exit(result.status === null ? 1 : result.status);
