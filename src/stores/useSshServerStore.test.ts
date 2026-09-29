import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { runInNewContext } from 'node:vm';
import ts from 'typescript';
import { settlePromises } from '../../tests/helpers/reactHookHarness';

function setup() {
  let selected = ['a'];
  let pendingRead: ((value: any) => void) | null = null;
  let delayRead = false;
  let delaySelection = false;
  let pendingSelection: (() => void) | null = null;
  const servers = ['a', 'b'].map(id => ({ id, name: id, sync_on_codex_switch: true, last_sync: null }));
  const snapshot = () => ({ selected_server_ids: [...selected], servers: structuredClone(servers) });
  const service = {
    async listSshServers() {
      const value = snapshot();
      return delayRead ? new Promise(resolve => { pendingRead = () => resolve(value); }) : value;
    },
    async setSshServerSelected(id: string, enabled: boolean) {
      selected = selected.filter(item => item !== id);
      if (enabled) selected.push(id);
      const value = snapshot();
      return delaySelection ? new Promise(resolve => { pendingSelection = () => resolve(value); }) : value;
    },
    async selectSshServers(ids: string[]) { selected = [...ids]; return snapshot(); },
    async syncCurrentCodexAccountToSshServer(id: string) {
      return { server_id: id, verified: true, account_id: 'new', account_email: 'fixture@example.invalid',
        token_generation: 2, bundle_hash: 'fixture', synced_at: 42, error: null };
    },
  };
  const source = readFileSync(new URL('./useSshServerStore.ts', import.meta.url), 'utf8');
  const compiled = ts.transpileModule(source, {compilerOptions: {module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022}}).outputText;
  const exports: any = {};
  const require = createRequire(import.meta.url);
  runInNewContext(compiled, {exports, require:(name:string)=> name === '../services/sshServerService' ? service : require(name)});
  return {store:exports.useSshServerStore, delay(){delayRead=true;}, release(){pendingRead?.(null);},
    delaySelection(){delaySelection=true;}, releaseSelection(){pendingSelection?.();}};
}

test('selecting another SSH host and deselecting one preserve the other target', async () => {
  const {store} = setup();
  await store.getState().fetchServers();
  assert.equal(JSON.stringify(store.getState().selectedServerIds), JSON.stringify(['a']));
  await store.getState().setServerSelected('b',true);
  assert.equal(JSON.stringify(store.getState().selectedServerIds), JSON.stringify(['a','b']));
  await store.getState().setServerSelected('a',false);
  assert.equal(JSON.stringify(store.getState().selectedServerIds), JSON.stringify(['b']));
  await store.getState().selectServers([]);
  assert.equal(store.getState().selectedServerIds.length,0);
});

test('an older list fetch cannot replace selections made while it was pending', async () => {
  const s = setup();
  await s.store.getState().fetchServers();
  s.delay();
  const oldRead = s.store.getState().fetchServers();
  await s.store.getState().setServerSelected('b',true);
  s.release();
  await oldRead;
  assert.equal(JSON.stringify(s.store.getState().selectedServerIds), JSON.stringify(['a','b']));
});

test('each host sync result updates only its own status and keeps the full selection', async () => {
  const {store} = setup();
  await store.getState().fetchServers();
  await store.getState().setServerSelected('b',true);
  await store.getState().syncNow('a');
  store.getState().applySyncResult({server_id:'b',verified:false,error:'fixture offline'});
  assert.equal(store.getState().servers[0].last_sync.verified,true);
  assert.equal(store.getState().servers[1].last_sync.verified,false);
  assert.equal(JSON.stringify(store.getState().selectedServerIds), JSON.stringify(['a','b']));
});

test('selection responses cannot erase a sync result received during the request', async () => {
  const s=setup();
  await s.store.getState().fetchServers();
  s.delaySelection();
  const selecting=s.store.getState().setServerSelected('b',true);
  await settlePromises();
  s.store.getState().applySyncResult({server_id:'b',verified:true,synced_at:42});
  s.releaseSelection();
  await selecting;
  assert.equal(s.store.getState().servers[1].last_sync?.verified,true);
  assert.equal(JSON.stringify(s.store.getState().selectedServerIds),JSON.stringify(['a','b']));
});


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
