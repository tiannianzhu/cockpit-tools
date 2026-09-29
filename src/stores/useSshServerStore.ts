import { create } from 'zustand';
import * as sshServerService from '../services/sshServerService';
import type { SshCodexSyncResult, SshServer, SshServerDraft } from '../types/sshServer';

interface SshServerState {
  servers: SshServer[];
  selectedServerIds: string[];
  selectionLoading: boolean;
  loading: boolean;
  error: string | null;
  syncResultsByServerId: Record<string, SshCodexSyncResult>;
  fetchServers: () => Promise<void>;
  upsertServer: (draft: SshServerDraft) => Promise<void>;
  deleteServer: (serverId: string) => Promise<void>;
  selectServers: (serverIds: string[]) => Promise<void>;
  toggleServerSelection: (serverId: string) => Promise<void>;
  testConnection: (serverId: string) => Promise<string>;
  syncNow: (serverId: string) => Promise<SshCodexSyncResult>;
  applySyncResult: (result: SshCodexSyncResult) => void;
}

function selectedIdsFromList(selectedServerIds: string[], servers: SshServer[]) {
  const serverIds = new Set(servers.map((server) => server.id));
  return [...new Set(selectedServerIds)].filter((serverId) => serverIds.has(serverId));
}

export function shouldAcceptSshSyncResult(
  current: SshCodexSyncResult | undefined,
  incoming: SshCodexSyncResult,
) {
  if (!current?.job_id || !incoming.job_id || current.job_id === incoming.job_id) return true;
  // Every run starts with pending. Do not let an older job's late progress replace it.
  return incoming.stage === 'pending';
}

function syncResultsFromServers(servers: SshServer[]) {
  return Object.fromEntries(
    servers.flatMap((server) =>
      server.last_sync
        ? [{ ...server.last_sync, server_id: server.id, server_name: server.name }]
        : [],
    ).map((result) => [result.server_id, result]),
  ) as Record<string, SshCodexSyncResult>;
}

export const useSshServerStore = create<SshServerState>((set, get) => ({
  servers: [],
  selectedServerIds: [],
  selectionLoading: false,
  loading: false,
  error: null,
  syncResultsByServerId: {},

  fetchServers: async () => {
    set({ loading: true, error: null });
    const previousResults = get().syncResultsByServerId;
    try {
      const list = await sshServerService.listSshServers();
      set((state) => ({
        servers: list.servers,
        selectedServerIds: list.selected_server_ids,
        syncResultsByServerId: {
          ...syncResultsFromServers(list.servers),
          // The persisted result can include a switch completed while the panel
          // was closed. Preserve only events received during this list request.
          ...Object.fromEntries(Object.entries(state.syncResultsByServerId).filter(([id, result]) =>
            result !== previousResults[id] && list.servers.some((server) => server.id === id))),
        },
        loading: false,
      }));
    } catch (error) {
      set({ error: String(error), loading: false });
    }
  },

  upsertServer: async (draft) => {
    const list = await sshServerService.upsertSshServer(draft);
    set({
      servers: list.servers,
      selectedServerIds: list.selected_server_ids,
      error: null,
    });
  },

  deleteServer: async (serverId) => {
    const list = await sshServerService.deleteSshServer(serverId);
    set({
      servers: list.servers,
      selectedServerIds: list.selected_server_ids,
      error: null,
    });
  },

  selectServers: async (serverIds) => {
    const previousIds = get().selectedServerIds;
    const requestedIds = selectedIdsFromList(serverIds, get().servers);
    set({ selectedServerIds: requestedIds, selectionLoading: true, error: null });
    try {
      const list = await sshServerService.selectSshServers(requestedIds);
      set({
        servers: list.servers,
        selectedServerIds: list.selected_server_ids,
        selectionLoading: false,
        error: null,
      });
    } catch (error) {
      set({ selectedServerIds: previousIds, selectionLoading: false, error: String(error) });
      throw error;
    }
  },

  toggleServerSelection: async (serverId) => {
    const selected = new Set(get().selectedServerIds);
    if (selected.has(serverId)) selected.delete(serverId);
    else selected.add(serverId);
    await get().selectServers([...selected]);
  },

  testConnection: async (serverId) => sshServerService.testSshServerConnection(serverId),

  syncNow: async (serverId) => {
    const result = await sshServerService.syncCurrentCodexAccountToSshServer(serverId);
    get().applySyncResult(result);
    void get().fetchServers();
    return result;
  },

  applySyncResult: (result) => {
    set((state) => {
      if (!shouldAcceptSshSyncResult(state.syncResultsByServerId[result.server_id], result)) {
        return state;
      }
      return {
        syncResultsByServerId: { ...state.syncResultsByServerId, [result.server_id]: result },
        servers: state.servers.map((server) =>
        server.id === result.server_id
          ? {
              ...server,
              last_sync: {
                account_id: result.account_id,
                account_email: result.account_email,
                token_generation: result.token_generation,
                bundle_hash: result.bundle_hash,
                synced_at: result.synced_at,
                verified: result.verified,
                error: result.error,
                stage: result.stage,
                job_id: result.job_id,
              },
            }
          : server,
      ),
      };
    });
  },
}));
