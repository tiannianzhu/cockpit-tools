import type { CodexSessionKindFilter } from '../utils/codexSessionFilters';
import { invoke } from '@tauri-apps/api/core';

export interface RemoteCodexSession {
  parentThreadId?: string | null;
  id: string;
  title: string;
  cwd: string | null;
  updatedAt: number;
  sessionKind: Exclude<CodexSessionKindFilter, 'all'>;
  archived: boolean;
  sizeBytes: number;
}
export interface RemoteCodexSessionTrashEntry {
  sessionIds?: string[];
  trashRootId: string;
  parentThreadId?: string | null;
  cwd: string | null;
  sessionKind: string;
  id: string;
  title: string;
  deletedAt: number;
  archived: boolean;
  sizeBytes: number;
}
export const listRemoteSessions = (serverId: string) =>
  invoke<{ sessions: RemoteCodexSession[]; total: number }>('list_remote_codex_sessions', { serverId });
export const listRemoteTrash = (serverId: string) =>
  invoke<RemoteCodexSessionTrashEntry[]>('list_remote_codex_session_trash', { serverId });
export const trashRemoteSession = (serverId: string, sessionId: string) =>
  invoke<RemoteCodexSessionTrashEntry>('trash_remote_codex_session', { serverId, sessionId });
export const restoreRemoteSession = (serverId: string, sessionId: string) =>
  invoke('restore_remote_codex_session', { serverId, sessionId });
export const purgeRemoteSession = (serverId: string, sessionId: string) =>
  invoke('purge_remote_codex_session', { serverId, sessionId });
export const clearRemoteTrash = (serverId: string) =>
  invoke('clear_remote_codex_session_trash', { serverId });
export const querySessionUsage = (serverId: string, query: import('../types/codex').CodexSessionUsageQuery = {}) =>
  invoke<import('../types/codex').CodexSessionUsageReport>('query_remote_codex_session_usage', { serverId, query });
// Summary and details can overlap while navigating; share the host scan.
const usageScans = new Map<string, Promise<import('../types/codex').CodexSessionUsageSyncResult>>();
export async function syncSessionUsage(serverId: string, query: import('../types/codex').CodexSessionUsageQuery = {}, rebuild = false) {
  // Rebuild must not silently reuse an incremental request already in flight.
  if (rebuild) await usageScans.get(serverId)?.catch(() => undefined);
  let scan = usageScans.get(serverId);
  if (!scan) {
    scan = invoke<import('../types/codex').CodexSessionUsageSyncResult>('sync_remote_codex_session_usage', { serverId, query: {}, rebuild });
    usageScans.set(serverId, scan);
    void scan.finally(() => { if (usageScans.get(serverId) === scan) usageScans.delete(serverId); }).catch(() => {});
  }
  const result = await scan;
  return { ...result, report: await querySessionUsage(serverId, query) };
}

export const openSessionTarget = (serverId: string, sessionId: string, folder: boolean) =>
  invoke<void>('open_remote_codex_session_target', { serverId, sessionId, folder });

export const previewSessionExport = (serverId: string, sessionIds: string[]) =>
  invoke<import('../types/codex').CodexSessionExportPreview>('preview_remote_codex_session_export', { serverId, sessionIds });
export const exportSessionPackage = (serverId: string, sessionIds: string[], exportPath: string, transferId?: string) =>
  invoke<import('../types/codex').CodexSessionExportSummary>('export_remote_codex_sessions', { serverId, sessionIds, exportPath, transferId });
export const previewSessionImport = (serverId: string, importFilePath: string) =>
  invoke<import('../types/codex').CodexSessionImportPreview>('preview_remote_codex_session_import', { serverId, importFilePath });
export const importSessionPackage = (serverId: string, importFilePath: string, sessionIds: string[], transferId?: string) =>
  invoke<import('../types/codex').CodexSessionImportSummary>('import_remote_codex_sessions', { serverId, importFilePath, sessionIds, transferId });
