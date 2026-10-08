import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import test from 'node:test';

const runner = fileURLToPath(new URL('../verify-viewer-readability.mjs', import.meta.url));
async function failedBrowser(mode, expected) {
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'deppy-verifier-failure-'));
  const browser = path.join(temporary, 'fake-chrome');
  fs.writeFileSync(browser, '#!/bin/sh\nexit 7\n', {mode});
  try {
    const result = await new Promise(resolve => execFile(process.execPath, [runner], {
      env: {...process.env, CHROME_PATH: browser, TMPDIR: temporary}, timeout: 2000,
    }, (error, stdout, stderr) => resolve({error, stdout, stderr})));
    assert.equal(result.error?.killed, false, 'runner must fail promptly without test timeout killing it');
    assert.equal(result.error?.code, 1, 'browser startup failure must exit with failure');
    assert.match(result.stderr, expected);
    assert.deepEqual(fs.readdirSync(temporary), ['fake-chrome'], 'failed runner must remove isolated profile');
  } finally {
    fs.rmSync(temporary, {recursive: true, force: true});
  }
}

test('verifier handles browser exiting before reporting within 2 seconds', async () => {
  await failedBrowser(0o755, /Chromium exited before reporting.*7/);
});
test('verifier handles browser spawn failure within 2 seconds', async () => {
  await failedBrowser(0o644, /EACCES/);
});
