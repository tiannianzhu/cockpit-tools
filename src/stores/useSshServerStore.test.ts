import assert from 'node:assert/strict';
import test from 'node:test';

import { mockIPC } from '@tauri-apps/api/mocks';
import { shouldAcceptSshSyncResult, useSshServerStore } from './useSshServerStore';
import type { SshCodexSyncResult, SshServer, SshServerList } from '../types/sshServer';

function result(jobId: string, stage: SshCodexSyncResult['stage']): SshCodexSyncResult {
  return {
    server_id: 'server-1',
    server_name: 'Server 1',
    job_id: jobId,
    stage,
    account_id: 'account-1',
    account_email: 'person@example.test',
    token_generation: 1,
    bundle_hash: 'bundle',
    synced_at: 1,
    verified: false,
    error: null,
  };
}

test('does not replace a current SSH sync job with a stale event', () => {
  const current = result('new-job', 'transferring');

  assert.equal(shouldAcceptSshSyncResult(current, result('old-job', 'applied')), false);
  assert.equal(shouldAcceptSshSyncResult(current, result('new-job', 'applied')), true);
  assert.equal(shouldAcceptSshSyncResult(current, result('next-job', 'pending')), true);
});

test('registry refresh picks up a switch completed while away and preserves newer live events', async (t) => {
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  const previousStore = useSshServerStore.getState();
  Object.defineProperty(globalThis, 'window', { configurable: true, value: {} });
  t.after(() => {
    useSshServerStore.setState(previousStore, true);
    if (previousWindow) Object.defineProperty(globalThis, 'window', previousWindow);
    else Reflect.deleteProperty(globalThis, 'window');
  });
  const completed = { ...result('new-job', 'applied'), verified: true };
  const server: SshServer = {
    id: completed.server_id, name: 'Test host', host: 'host.example.test', port: 0,
    username: '', codex_home: '', auth: { kind: 'agent' }, sync_on_codex_switch: false,
    created_at: 0, updated_at: 0, last_sync: completed,
  };
  useSshServerStore.setState({ servers: [server], syncResultsByServerId: { [server.id]: result('old-job', 'pending') } });
  mockIPC((command) => {
    assert.equal(command, 'list_ssh_servers');
    return { servers: [server], selected_server_ids: [] };
  });
  await useSshServerStore.getState().fetchServers();
  assert.equal(useSshServerStore.getState().syncResultsByServerId[server.id].job_id, completed.job_id);
  assert.equal(useSshServerStore.getState().syncResultsByServerId[server.id].verified, true);

  let resolveList!: (list: SshServerList) => void;
  mockIPC(() => new Promise<SshServerList>((resolve) => { resolveList = resolve; }));
  const fetch = useSshServerStore.getState().fetchServers();
  const live = result('next-job', 'pending');
  useSshServerStore.getState().applySyncResult(live);
  resolveList({ servers: [server], selected_server_ids: [] });
  await fetch;
  assert.equal(useSshServerStore.getState().syncResultsByServerId[server.id], live);
});
