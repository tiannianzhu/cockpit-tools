import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { confirm, open as chooseFile } from '@tauri-apps/plugin-dialog';
import { Folder, Upload } from 'lucide-react';
import { CodexSessionRowActions } from './CodexSessionRowActions';
import { CodexSessionUsageSummary, CodexSessionUsagePanel, useInitialSessionUsageCheck, type SessionUsageSource } from './CodexSessionUsagePanel';
import { CodexSessionList } from './CodexSessionList';
import { CodexSessionSearch } from './CodexSessionSearch';
import { CodexSessionExportModal, type CodexSessionExportSource } from './CodexSessionExportModal';
import { CodexSessionImportModal, type CodexSessionImportSource } from './CodexSessionImportModal';
import { CodexSessionTransfer, useCodexSessionTransfer } from './CodexSessionTransfer';
import { CodexSessionToolbar } from './CodexSessionToolbar';
import { CodexSessionTrashModal, type CodexSessionTrashSource } from './CodexSessionTrashModal';
import { buildGroups, buildSessionTrees, formatTokenStats } from '../../utils/codexSessionPresentation';
import { type CodexArchiveFilter, type CodexSessionKindFilter } from '../../utils/codexSessionFilters';
import type { CodexSessionRecord, CodexSessionUsageReport } from '../../types/codex';
import * as remote from '../../services/remoteCodexSessionService';
import { useCodexSessionList } from '../../hooks/useCodexSessionList';

export function RemoteCodexSessionManager({ serverId, serverName }: { serverId: string; serverName: string }) {
  const { t } = useTranslation();
  const [usageView, setUsageView] = useState(false);
  const [usageReport, setUsageReport] = useState<CodexSessionUsageReport | null>(null);
  const usageSource = useMemo<SessionUsageSource>(() => ({
    querySessionUsage: query => remote.querySessionUsage(serverId, query),
    syncSessionUsage: options => remote.syncSessionUsage(serverId, options.query, options.rebuild),
  }), [serverId]);
  const initialUsageCheck = useInitialSessionUsageCheck(usageSource);
  const sessionTokens = useMemo(() => new Map(usageReport?.sessionTokens?.map(value => [value.sessionId, value]) ?? []), [usageReport]);
  const [query, setQuery] = useState('');
  const [archiveFilter, setArchiveFilter] = useState<CodexArchiveFilter>('active');
  const [kind, setKind] = useState<CodexSessionKindFilter>('conversation');
  const [expanded, setExpanded] = useState<string[]>([]);
  const [trashOpen, setTrashOpen] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);
  const [importPath, setImportPath] = useState('');
  const [sessions, setSessions] = useState<CodexSessionRecord[]>([]);
  const [loading, setLoading] = useState(false);
  const [mutating, setBusy] = useState(false);
  const transfer = useCodexSessionTransfer();
  const { begin, getCurrentId, succeed, fail } = transfer;
  const busy = mutating || transfer.task?.status === 'running';
  const [openingId, setOpeningId] = useState<string | null>(null);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const request = useRef(0);
  const mounted = useRef(true);

  const refresh = useCallback(async () => {
    const current = ++request.current;
    setLoading(true);
    setError('');
    try {
      // Both hosts filter roots locally after loading the complete hierarchy.
      const result = await remote.listRemoteSessions(serverId);
      if (current !== request.current) return;
      const records: CodexSessionRecord[] = result.sessions.map(row => ({
        sessionId: row.id, title: row.title, archived: row.archived,
        parentThreadId: row.parentThreadId, cwd: row.cwd || '', updatedAt: row.updatedAt,
        sessionKind: row.sessionKind, locationCount: 1, locations: [],
      }));
      setSessions(records);
      const groups = buildGroups(buildSessionTrees(records));
      setExpanded(previous => previous.filter(cwd => groups.some(group => group.cwd === cwd)));
    } catch (cause) {
      if (current === request.current) setError(String(cause));
    } finally {
      if (current === request.current) setLoading(false);
    }
  }, [serverId]);

  useEffect(() => {
    mounted.current = true;
    void refresh();
    return () => { mounted.current = false; request.current++; };
  }, [refresh]);

  const { visible, groups, selectedIds: selected, selectedSet, allIds, allSelected,
    toggleSelection } = useCodexSessionList(sessions, kind, archiveFilter, query);

  const trashSource = useMemo<CodexSessionTrashSource>(() => ({
    load: async () => (await remote.listRemoteTrash(serverId)).map(row => ({
      id: row.id, sessionId: row.id, parentThreadId: row.parentThreadId,
      familyId: row.trashRootId, cwd: row.cwd || '', title: row.title,
      deletedAt: row.deletedAt, sizeBytes: row.sizeBytes, sessionKind: row.sessionKind,
      locations: [{ instanceName: serverName }],
    })),
    restore: async ids => {
      for (const id of ids) await remote.restoreRemoteSession(serverId, id);
      return { message: t('codex.sessionManager.shared.restored', '已恢复 {{count}} 条主会话及其子代理', { count: ids.length }) };
    },
    purge: async ids => {
      for (const id of ids) await remote.purgeRemoteSession(serverId, id);
      return { message: t('codex.sessionManager.shared.purged', '已永久删除 {{count}} 条主会话及其子代理备份', { count: ids.length }) };
    },
    empty: async () => {
      await remote.clearRemoteTrash(serverId);
      return { message: t('codex.sessionManager.shared.emptied', '废纸篓已清空') };
    },
  }), [serverId, serverName, t]);

  async function trashSelected() {
    if (!await confirm(t('codex.sessionManager.confirm.message', '会将所选会话及其全部子代理会话从对应实例中移到废纸篓，便于后续恢复。确认继续？') + ` (${selected.length})`, { kind: 'warning' }) || !mounted.current) return;
    setBusy(true); setError(''); setNotice('');
    const failures: string[] = [];
    let deleted = 0;
    try {
      for (const id of selected) {
        if (!mounted.current) return;
        try {
          await remote.trashRemoteSession(serverId, id);
          deleted += 1;
        } catch (cause) {
          failures.push(`${id}: ${String(cause)}`);
        }
      }
    } finally {
      if (mounted.current) {
        await refresh();
        setNotice(t('codex.sessionManager.shared.trashed', '已将 {{count}} 条主会话及其子代理移到废纸篓', { count: deleted }));
        if (failures.length) setError(t('codex.sessionManager.messages.trashSkipped', '以下 {{count}} 条主会话未删除，已继续处理其他项：', { count: failures.length }) + '\n' + failures.join('\n'));
        setBusy(false);
      }
    }
  }

  const exportSource = useMemo<CodexSessionExportSource>(() => ({
    preview: ids => remote.previewSessionExport(serverId, ids),
    export: async (ids, path) => {
      const id = getCurrentId() ?? begin('export', ids.length);
      try {
        const result = await remote.exportSessionPackage(serverId, ids, path, id);
        succeed(id, result.message);
        return result;
      } catch (error) { fail(id, String(error)); throw error; }
    },
  }), [serverId, begin, getCurrentId, succeed, fail]);
  const importSource = useMemo<CodexSessionImportSource>(() => ({
    preview: path => remote.previewSessionImport(serverId, path),
    import: async (path, ids) => {
      const id = getCurrentId() ?? begin('import', ids.length);
      try {
        const result = await remote.importSessionPackage(serverId, path, ids, id);
        succeed(id, result.message);
        return result;
      } catch (error) { fail(id, String(error)); throw error; }
    },
  }), [serverId, begin, getCurrentId, succeed, fail]);
  async function openImport() {
    try {
      const path = await chooseFile({ multiple: false, filters: [{ name: 'ZIP', extensions: ['zip'] }] });
      if (typeof path === 'string' && mounted.current) setImportPath(path);
    } catch (cause) { if (mounted.current) setError(String(cause)); }
  }

  async function openTarget(id: string, folder: boolean) {
    setOpeningId(id); setError('');
    try { await remote.openSessionTarget(serverId, id, folder); }
    catch (cause) { if (mounted.current) setError(String(cause)); }
    finally { if (mounted.current) setOpeningId(null); }
  }

  return <section className="codex-session-manager" aria-label={serverName}>
    {usageView ? <CodexSessionUsagePanel initialCheck={initialUsageCheck} source={usageSource} onBack={() => setUsageView(false)} /> : <>
      <CodexSessionUsageSummary initialCheck={initialUsageCheck} source={usageSource} onReport={setUsageReport} onOpenDetail={() => setUsageView(true)} />
      <div className="codex-session-manager__header">
        <CodexSessionSearch query={query} onQueryChange={setQuery} onClear={() => setQuery('')} canClear={Boolean(query)}
          disabled={loading || busy} archive={archiveFilter} onArchiveChange={setArchiveFilter} kind={kind} onKindChange={setKind} />
        <CodexSessionToolbar selectedCount={selected.length} allSelected={allSelected}
          hasSessions={visible.length > 0} loading={loading} busy={busy} onToggleAll={() => toggleSelection(allIds)}
          onExport={() => setExportOpen(true)} onTrash={() => void trashSelected()} onOpenTrash={() => setTrashOpen(true)} onRefresh={() => void refresh()}
          maintenanceExtras={<button type="button" className="btn btn-secondary codex-session-manager__action-button" disabled={loading || busy}
            onClick={() => void openImport()}><Upload size={14} />{t('codex.sessionManager.actions.importSessions', '导入会话')}</button>} />
      </div>
      {error && <div role="alert" className="message-bar error">{error}</div>}
      {notice && <div role="status" className="message-bar success">{notice}</div>}
      {loading ? <div className="empty-state" role="status"><h3>{t('common.loading', '加载中...')}</h3></div>
        : visible.length === 0 ? !error && <div className="empty-state codex-session-manager__empty">
          <Folder size={42} className="empty-icon" /><h3>{t('codex.sessionManager.empty.searchTitle', '未找到匹配会话')}</h3>
        </div> : <CodexSessionList disabled={busy} groups={groups} selectedIds={selectedSet} expandedGroups={expanded}
          onToggleGroup={cwd => setExpanded(previous => previous.includes(cwd) ? previous.filter(value => value !== cwd) : [...previous, cwd])}
          onToggleGroupSelection={toggleSelection} onToggleSession={id => toggleSelection([id])}
          renderActions={session => <>
            <CodexSessionRowActions sessionId={session.sessionId} disabled={busy} opening={openingId === session.sessionId}
              onOpenLocation={() => void openTarget(session.sessionId, true)} onOpenFile={() => void openTarget(session.sessionId, false)} onError={setError} />
            {sessionTokens.has(session.sessionId) && <span className="codex-session-row__tokens">{formatTokenStats(sessionTokens.get(session.sessionId))}</span>}
          </>} />}
    </>}
    <CodexSessionExportModal open={exportOpen} sessionIds={selected} source={exportSource}
      onClose={() => setExportOpen(false)} onMessage={setNotice} onError={setError}
      onExportStart={ids => { begin('export', ids.length); setExportOpen(false); }} />
    <CodexSessionImportModal open={Boolean(importPath)} filePath={importPath} source={importSource}
      onClose={() => setImportPath('')} onMessage={setNotice} onError={setError} onChanged={refresh}
      onImportStart={ids => { begin('import', ids.length); setImportPath(''); }} />
    <CodexSessionTrashModal open={trashOpen} onClose={() => setTrashOpen(false)} source={trashSource}
      onChanged={refresh} onMessage={message => setNotice(message)} />
    <CodexSessionTransfer transfer={transfer} />
  </section>;
}
