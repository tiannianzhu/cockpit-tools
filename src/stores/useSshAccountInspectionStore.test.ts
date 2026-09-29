import assert from 'node:assert/strict';
import test from 'node:test';
import { createSshAccountInspectionStore } from './useSshAccountInspectionStore';
import type { SshServer, SshServerAccountInspection } from '../types/sshServer';

const host: SshServer = {
  id: 'host-a', name: 'Test host', host: 'host.example.test', port: 0,
  username: '', codex_home: '', auth: { kind: 'agent' },
  sync_on_codex_switch: false, created_at: 0, updated_at: 0, last_sync: null,
};
const account: SshServerAccountInspection = { server_id: host.id, auth_mode: 'oauth', account_id: 'account-a' };

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

test('returning to a host reuses its result; explicit refresh reads again', async () => {
  let calls = 0;
  const store = createSshAccountInspectionStore(async () => { calls++; return account; });
  await store.getState().inspect(host);
  const first = store.getState().entries[host.id];
  assert.ok(first.checkedAt);
  await store.getState().inspect({ ...host });
  await store.getState().inspect({ ...host, name: 'Renamed', sync_on_codex_switch: true, updated_at: 1 });
  assert.equal(calls, 1);
  assert.equal(store.getState().entries[host.id], first);
  await store.getState().inspect(host, true);
  assert.equal(calls, 2);
});

test('leaving during a read does not lose it or issue a duplicate on return', async () => {
  const response = deferred<SshServerAccountInspection>();
  let calls = 0;
  const store = createSshAccountInspectionStore(() => { calls++; return response.promise; });
  const first = store.getState().inspect(host);
  const second = store.getState().inspect({ ...host });
  const forced = store.getState().inspect(host, true);
  assert.equal(first, second);
  assert.equal(first, forced);
  response.resolve(account);
  await first;
  assert.equal(calls, 1);
  assert.equal(store.getState().entries[host.id].account, account);
  assert.equal(store.getState().entries[host.id].reading, false);
});

test('connection changes discard old identity and ignore late old responses', async () => {
  const oldResponse = deferred<SshServerAccountInspection>();
  const newResponse = deferred<SshServerAccountInspection>();
  let calls = 0;
  const store = createSshAccountInspectionStore(() => (++calls === 1 ? oldResponse : newResponse).promise);
  const oldRead = store.getState().inspect(host);
  const newRead = store.getState().inspect({ ...host, host: 'other.example.test' });
  assert.equal(store.getState().entries[host.id].account, undefined);
  const nextAccount = { ...account, account_id: 'account-b' };
  newResponse.resolve(nextAccount);
  await newRead;
  oldResponse.resolve(account);
  await oldRead;
  assert.equal(store.getState().entries[host.id].account, nextAccount);
});

test('a completed account switch refreshes only that host once', async () => {
  const calls: string[] = [];
  const store = createSshAccountInspectionStore(async (id) => { calls.push(id); return { ...account, server_id: id }; });
  const other = { ...host, id: 'host-b' };
  await Promise.all([store.getState().inspect(host), store.getState().inspect(other)]);
  const switched: SshServer = { ...host, last_sync: {
    account_id: 'account-b', account_email: 'person@example.test', token_generation: 1,
    bundle_hash: 'test-bundle', synced_at: 1, verified: true, error: null, stage: 'applied', job_id: 'job-a',
  } };
  await store.getState().inspect(switched);
  await store.getState().inspect(switched);
  await store.getState().inspect(other);
  assert.deepEqual(calls, ['host-a', 'host-b', 'host-a']);
});

test('failed refresh retains the last successful result and waits for explicit retry', async () => {
  let calls = 0;
  const store = createSshAccountInspectionStore(async () => {
    if (++calls === 2) throw new Error('offline');
    return account;
  });
  await store.getState().inspect(host);
  const checkedAt = store.getState().entries[host.id].checkedAt;
  await store.getState().inspect(host, true);
  await store.getState().inspect(host);
  assert.equal(calls, 2);
  assert.equal(store.getState().entries[host.id].account, account);
  assert.equal(store.getState().entries[host.id].checkedAt, checkedAt);
  assert.match(store.getState().entries[host.id].error!, /offline/);
  await store.getState().inspect(host, true);
  assert.equal(calls, 3);
  assert.equal(store.getState().entries[host.id].error, undefined);
});

test('removing a host evicts its result and late responses cannot restore it', async () => {
  const response = deferred<SshServerAccountInspection>();
  const store = createSshAccountInspectionStore(() => response.promise);
  const read = store.getState().inspect(host);
  store.getState().retain([]);
  response.resolve(account);
  await read;
  assert.deepEqual(store.getState().entries, {});
});
