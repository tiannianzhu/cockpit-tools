import { create } from 'zustand';
import { inspectSshServerAccount } from '../services/sshServerService';
import type { SshServer, SshServerAccountInspection } from '../types/sshServer';

interface InspectionEntry {
  config: string;
  sync: string;
  reading: boolean;
  account?: SshServerAccountInspection;
  error?: string;
  checkedAt?: number;
}

interface InspectionState {
  entries: Record<string, InspectionEntry>;
  inspect: (server: SshServer, force?: boolean) => Promise<void>;
  retain: (serverIds: string[]) => void;
}

// App-session cache: survives tab unmounts, but never writes account data to disk.
export function createSshAccountInspectionStore(read = inspectSshServerAccount) {
  const pending = new Map<string, { entry: InspectionEntry; promise: Promise<void> }>();
  return create<InspectionState>((set, get) => ({
    entries: {},
    inspect: (server, force = false) => {
      const config = JSON.stringify([server.host, server.port, server.username, server.codex_home, server.auth]);
      const sync = JSON.stringify(server.last_sync
        ? [server.last_sync.job_id, server.last_sync.synced_at, server.last_sync.account_id, server.last_sync.verified]
        : null);
      const previous = get().entries[server.id];
      const sameSource = previous?.config === config;
      if (sameSource && previous.sync === sync) {
        const request = pending.get(server.id);
        if (request?.entry === previous) return request.promise;
        // A failed read is also cached; retry only on an explicit refresh.
        if (!force) return Promise.resolve();
      }
      const entry: InspectionEntry = {
        config, sync, reading: true,
        account: sameSource ? previous.account : undefined,
        checkedAt: sameSource ? previous.checkedAt : undefined,
      };
      set((state) => ({ entries: { ...state.entries, [server.id]: entry } }));
      const finish = (result: Partial<InspectionEntry>) => {
        if (get().entries[server.id] !== entry) return;
        set((state) => ({ entries: { ...state.entries, [server.id]: { ...entry, ...result, reading: false } } }));
      };
      const promise = Promise.resolve().then(() => read(server.id)).then(
        (account) => finish({ account, checkedAt: Date.now() }),
        (error: unknown) => finish({ error: String(error) }),
      ).finally(() => {
        if (pending.get(server.id)?.entry === entry) pending.delete(server.id);
      });
      pending.set(server.id, { entry, promise });
      return promise;
    },
    retain: (serverIds) => {
      const ids = new Set(serverIds);
      if (Object.keys(get().entries).every((id) => ids.has(id))) return;
      set((state) => ({ entries: Object.fromEntries(Object.entries(state.entries).filter(([id]) => ids.has(id))) }));
      for (const id of pending.keys()) if (!ids.has(id)) pending.delete(id);
    },
  }));
}

export const useSshAccountInspectionStore = createSshAccountInspectionStore();
