import { invoke } from '@tauri-apps/api/core';
import type { SshCodexSyncResult, SshServer, SshServerAccountInspection, SshServerDraft, SshServerList } from '../types/sshServer';

function toServer(draft: SshServerDraft): SshServer {
  return {
    id: draft.id ?? '',
    name: draft.name,
    host: draft.host,
    // 0 tells the backend to resolve the SSH config/default port.
    port: draft.port ?? 0,
    username: draft.username,
    // Each entry targets one CODEX_HOME; the backend creates it when applying an account.
    codex_home: draft.codex_home?.trim() ?? '',
    auth: draft.auth,
    sync_on_codex_switch: draft.sync_on_codex_switch ?? false,
    created_at: 0,
    updated_at: 0,
    last_sync: null,
  };
}

export async function listSshServers(): Promise<SshServerList> {
  return invoke<SshServerList>('list_ssh_servers');
}

export async function upsertSshServer(draft: SshServerDraft): Promise<SshServerList> {
  return invoke<SshServerList>('upsert_ssh_server', { server: toServer(draft) });
}

export async function deleteSshServer(serverId: string): Promise<SshServerList> {
  return invoke<SshServerList>('delete_ssh_server', { serverId });
}

export async function selectSshServers(serverIds: string[]): Promise<SshServerList> {
  return invoke<SshServerList>('select_ssh_servers', { serverIds });
}

export async function testSshServerConnection(serverId: string): Promise<string> {
  return await invoke('test_ssh_server_connection', { serverId });
}

export async function syncCurrentCodexAccountToSshServer(
  serverId: string,
): Promise<SshCodexSyncResult> {
  return await invoke('sync_current_codex_account_to_ssh_server', {
    serverId,
  });
}

export async function inspectSshServerAccount(serverId: string): Promise<SshServerAccountInspection> {
  return await invoke('inspect_ssh_server_account', { serverId });
}

export async function switchSshServerAccount(serverId: string, accountId: string): Promise<SshCodexSyncResult> {
  return await invoke('switch_ssh_server_account', { serverId, accountId });
}
