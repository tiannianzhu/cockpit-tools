import { create } from 'zustand';
import * as sshServerService from '../services/sshServerService';
import type { SshCodexSyncResult, SshServer, SshServerDraft, SshServerList } from '../types/sshServer';

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
  setServerSelected: (serverId: string, selected: boolean) => Promise<void>;
  testConnection: (serverId: string) => Promise<string>;
  syncNow: (serverId: string) => Promise<SshCodexSyncResult>;
  applySyncResult: (result: SshCodexSyncResult) => void;
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

function stateFromList(list: SshServerList) {
  return {
    servers: list.servers,
    syncResultsByServerId: syncResultsFromServers(list.servers),
    selectedServerIds: [...new Set(list.selected_server_ids)]
      .filter(id => list.servers.some(server => server.id === id)),
    error: null,
  };
}

export const useSshServerStore = create<SshServerState>((set, get) => {
  let revision = 0;
  let fetchSequence = 0;
  let mutations = Promise.resolve();
  let selectionsPending = 0;
  const syncRevisions = new Map<string, number>();
  const mutate = (operation: () => Promise<SshServerList>): Promise<void> => {
    revision++;
    const result = mutations.then(async () => {
      const started = revision;
      const list = await operation();
      revision++;
      set(state => {
        const next = stateFromList(list);
        const current = new Map(state.servers.map(server => [server.id, server]));
        next.syncResultsByServerId = { ...next.syncResultsByServerId,
          ...Object.fromEntries(Object.entries(state.syncResultsByServerId).filter(([id]) =>
            (syncRevisions.get(id) ?? 0) > started && list.servers.some(server => server.id === id))),
        };
        return { ...next, servers: next.servers.map(server =>
          (syncRevisions.get(server.id) ?? 0) > started
            ? { ...server, last_sync: current.get(server.id)?.last_sync ?? server.last_sync }
            : server) };
      });
      for (const id of syncRevisions.keys()) {
        if (!list.servers.some(server => server.id === id)) syncRevisions.delete(id);
      }
    });
    mutations = result.catch(() => {});
    return result;
  };
  const select = async (operation: () => Promise<SshServerList>) => {
    selectionsPending++;
    set({ selectionLoading: true, error: null });
    try {
      await mutate(operation);
    } catch (error) {
      set({ error: String(error) });
      throw error;
    } finally {
      selectionsPending--;
      set({ selectionLoading: selectionsPending > 0 });
    }
  };
  return {
  servers: [],
  selectedServerIds: [],
  selectionLoading: false,
  loading: false,
  error: null,
  syncResultsByServerId: {},

  fetchServers: async () => {
    const started = revision;
    const sequence = ++fetchSequence;
    set({ loading: true, error: null });
    try {
      const list = await sshServerService.listSshServers();
      if (sequence === fetchSequence) {
        set({ ...(started === revision ? stateFromList(list) : {}), loading: false });
      }
    } catch (error) {
      if (sequence === fetchSequence) set({ error: String(error), loading: false });
    }
  },

  upsertServer: (draft) => mutate(() => sshServerService.upsertSshServer(draft)),
  deleteServer: (serverId) => mutate(() => sshServerService.deleteSshServer(serverId)),
  selectServers: (serverIds) => select(() => sshServerService.selectSshServers(serverIds)),
  setServerSelected: (serverId, selected) => select(() => sshServerService.setSshServerSelected(serverId, selected)),

  toggleServerSelection: (serverId) => select(() =>
    sshServerService.setSshServerSelected(serverId, !get().selectedServerIds.includes(serverId))),

  testConnection: async (serverId) => sshServerService.testSshServerConnection(serverId),

  syncNow: async (serverId) => {
    const result = await sshServerService.syncCurrentCodexAccountToSshServer(
      serverId,
    );
    get().applySyncResult(result);
    return result;
  },

  applySyncResult: (result) => {
    if (!shouldAcceptSshSyncResult(get().syncResultsByServerId[result.server_id], result)) return;
    revision++;
    syncRevisions.set(result.server_id, revision);
    set((state) => ({
      syncResultsByServerId: { ...state.syncResultsByServerId, [result.server_id]: result },
      servers: state.servers.map((server) => server.id === result.server_id
        ? { ...server, last_sync: result } : server),
    }));
  },
};
});
