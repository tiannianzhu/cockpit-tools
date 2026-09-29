import assert from 'node:assert/strict';
import test from 'node:test';
import { createInitialSessionUsageCheck } from './codexSessionUsageCheck.ts';
import type { CodexSessionUsageSyncResult } from '../types/codex.ts';

const result = { errors: [] } as unknown as CodexSessionUsageSyncResult;

test('list and detail share one pending scan; returning does not scan again', async () => {
  let calls = 0;
  let finish!: (value: CodexSessionUsageSyncResult) => void;
  const check = createInitialSessionUsageCheck(() => {
    calls++;
    return new Promise(resolve => { finish = resolve; });
  });
  const list = check();
  const detail = check();
  assert.equal(list, detail);
  assert.equal(calls, 1);
  finish(result);
  assert.equal(await detail, result);
  assert.equal(await check(), undefined);
  assert.equal(calls, 1);
});

test('a failed initial scan is reported once and does not retry on navigation', async () => {
  let calls = 0;
  const check = createInitialSessionUsageCheck(async () => {
    calls++;
    throw new Error('scan failed');
  });
  await assert.rejects(check(), /scan failed/);
  assert.equal(await check(), undefined);
  assert.equal(calls, 1);
});

test('entering another manager starts its own check', async () => {
  let calls = 0;
  const sync = async () => { calls++; return result; };
  await createInitialSessionUsageCheck(sync)();
  await createInitialSessionUsageCheck(sync)();
  assert.equal(calls, 2);
});
