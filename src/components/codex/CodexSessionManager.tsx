import { CodexSessionUsageBadge } from './CodexSessionUsageBadge';
import { CodexSessionRowActions } from './CodexSessionRowActions';
import { CodexSessionList } from './CodexSessionList';
import { CodexSessionSearch } from './CodexSessionSearch';
import { CodexSessionTrashModal, type CodexSessionTrashSource } from './CodexSessionTrashModal';
import { CodexSessionExportModal, type CodexSessionExportSource } from './CodexSessionExportModal';
import { CodexSessionImportModal, type CodexSessionImportSource } from './CodexSessionImportModal';
import { CodexSessionTransfer, useCodexSessionTransfer } from './CodexSessionTransfer';
import { CodexSessionToolbar } from './CodexSessionToolbar';
import { buildGroups, buildSessionTrees, type SessionGroup } from '../../utils/codexSessionPresentation';
import { type MouseEvent, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { confirm as confirmDialog, open as openFileDialog } from '@tauri-apps/plugin-dialog';
import { Copy, Eye, Folder, RefreshCw, Upload, X } from 'lucide-react';
import { ModalErrorMessage, useModalErrorState } from '../ModalErrorMessage';
import { SingleSelectDropdown, type SingleSelectOption } from '../SingleSelectDropdown';
import { useEscClose } from '../../hooks/useEscClose';
import { useCodexSessionList } from '../../hooks/useCodexSessionList';
import type {
  CodexSessionRecord,
  CodexSessionTokenStats,
  CodexSessionUsageReport,
  CodexSessionTrashSummary,
} from '../../types/codex';
import type { InstanceProfile } from '../../types/instance';
import { useCodexInstanceStore } from '../../stores/useCodexInstanceStore';
import { validateSessionImportPaths } from '../../services/codexInstanceService';
import {
  type CodexArchiveFilter,
  type CodexSessionKindFilter,
} from '../../utils/codexSessionFilters';
import { CodexSessionVisibilityRepairModal } from './CodexSessionVisibilityRepairModal';
import {
  CodexSessionUsagePanel,
  CodexSessionUsageSummary,
  useInitialSessionUsageCheck,
} from './CodexSessionUsagePanel';

import { useSshServerStore } from '../../stores/useSshServerStore';
import { RemoteCodexSessionManager } from './RemoteCodexSessionManager';
import '../../styles/pages/codex-remote-sessions.css';

type SessionManagerView = 'list' | 'usage';

type MessageState = { text: string; tone?: 'error' };
type SessionTokenStatsMap = Record<string, CodexSessionTokenStats>;

type InstanceSortField = 'createdAt' | 'lastLaunchedAt';
type InstanceSortDirection = 'asc' | 'desc';
function readCodexInstanceSortPreference(): {
  field: InstanceSortField;
  direction: InstanceSortDirection;
} {
  const sortField = localStorage.getItem('agtools.codex.instances.sort_field');
  const sortDirection = localStorage.getItem('agtools.codex.instances.sort_direction');
  return {
    field: sortField === 'lastLaunchedAt' ? 'lastLaunchedAt' : 'createdAt',
    direction: sortDirection === 'desc' ? 'desc' : 'asc',
  };
}

function sortInstancesForDisplay(instances: InstanceProfile[]): InstanceProfile[] {
  const sortPreference = readCodexInstanceSortPreference();
  return [...instances].sort((left, right) => {
    if (left.isDefault && !right.isDefault) return -1;
    if (!left.isDefault && right.isDefault) return 1;
    const leftValue =
      sortPreference.field === 'createdAt'
        ? left.createdAt || 0
        : left.lastLaunchedAt || 0;
    const rightValue =
      sortPreference.field === 'createdAt'
        ? right.createdAt || 0
        : right.lastLaunchedAt || 0;
    return sortPreference.direction === 'asc'
      ? leftValue - rightValue
      : rightValue - leftValue;
  });
}

function buildDefaultExpandedGroups(_groups: SessionGroup[]): string[] {
  return [];
}


interface CodexSessionManagerProps {
  host: string;
  onHostChange: (host: string) => void;
  onManageHosts: () => void;
}

export function CodexSessionManager({ host, onHostChange, onManageHosts }: CodexSessionManagerProps) {
  const { t } = useTranslation();
  const servers = useSshServerStore((state) => state.servers);
  const fetchServers = useSshServerStore((state) => state.fetchServers);
  const error = useSshServerStore((state) => state.error);
  useEffect(() => { void fetchServers(); }, [fetchServers]);
  const remoteHost = servers.find((server) => server.id === host);
  return (
    <div className="codex-session-host-workspace">
      <div className="codex-session-host-selector">
        <span>{t('codex.remoteSessions.host', '会话所在主机')}</span>
        <div className="codex-session-host-select">
          <SingleSelectDropdown value={host} onChange={onHostChange}
            ariaLabel={t('codex.remoteSessions.host', '会话所在主机')}
            options={[
              { value: 'local', label: t('codex.remoteSessions.local', '本地') },
              ...servers.map((server) => ({ value: server.id, label: server.name || server.host })),
              ...(host !== 'local' && !remoteHost ? [{ value: host, label: t('codex.remoteSessions.unavailable', '主机已移除或不可用') }] : []),
            ]} />
        </div>
        <button type="button" className="btn btn-secondary" onClick={onManageHosts}>
          {t('codex.remoteSessions.manageHosts', '管理主机')}
        </button>
      </div>
      {error && <p className="codex-remote-session-error" role="alert">{error}</p>}
      {remoteHost ? (
        <RemoteCodexSessionManager
          key={JSON.stringify([remoteHost.id, remoteHost.host, remoteHost.port, remoteHost.username, remoteHost.codex_home])}
          serverId={remoteHost.id} serverName={remoteHost.name || remoteHost.host} />
      ) : host === 'local' ? <LocalCodexSessionManager /> : (
        <p role="status">{t('codex.remoteSessions.chooseHost', '所选主机不可用，请选择其他主机。')}</p>
      )}
    </div>
  );
}

function LocalCodexSessionManager() {
  const initialUsageCheck = useInitialSessionUsageCheck();
  const { t } = useTranslation();
  const instances = useCodexInstanceStore((state) => state.instances);
  const refreshInstances = useCodexInstanceStore((state) => state.refreshInstances);
  const syncThreadsAcrossInstances = useCodexInstanceStore((state) => state.syncThreadsAcrossInstances);
  const syncSessionsToInstance = useCodexInstanceStore((state) => state.syncSessionsToInstance);
  const listSessionsAcrossInstances = useCodexInstanceStore((state) => state.listSessionsAcrossInstances);
  const moveSessionsToTrashAcrossInstances = useCodexInstanceStore(
    (state) => state.moveSessionsToTrashAcrossInstances,
  );
  const listTrashedSessionsAcrossInstances = useCodexInstanceStore(
    (state) => state.listTrashedSessionsAcrossInstances,
  );
  const restoreSessionsFromTrashAcrossInstances = useCodexInstanceStore(
    (state) => state.restoreSessionsFromTrashAcrossInstances,
  );
  const deleteTrashedSessionsAcrossInstances = useCodexInstanceStore(
    (state) => state.deleteTrashedSessionsAcrossInstances,
  );
  const emptySessionTrashAcrossInstances = useCodexInstanceStore(
    (state) => state.emptySessionTrashAcrossInstances,
  );
  const previewSessionExport = useCodexInstanceStore((state) => state.previewSessionExport);
  const exportSessions = useCodexInstanceStore((state) => state.exportSessions);
  const previewSessionImport = useCodexInstanceStore((state) => state.previewSessionImport);
  const importSessions = useCodexInstanceStore((state) => state.importSessions);
  const openSessionLocation = useCodexInstanceStore((state) => state.openSessionLocation);
  const openSessionRollout = useCodexInstanceStore((state) => state.openSessionRollout);
  const [sessions, setSessions] = useState<CodexSessionRecord[]>([]);
  const [expandedGroups, setExpandedGroups] = useState<string[]>([]);
  const [showSyncTargetModal, setShowSyncTargetModal] = useState(false);
  const [syncTargetInstanceId, setSyncTargetInstanceId] = useState('');
  const [showRestoreModal, setShowRestoreModal] = useState(false);
  const [showExportModal, setShowExportModal] = useState(false);
  const [showImportModal, setShowImportModal] = useState(false);
  const [showRepairVisibilityModal, setShowRepairVisibilityModal] = useState(false);
  const [importFilePath, setImportFilePath] = useState('');
  const [importTargetInstanceId, setImportTargetInstanceId] = useState('');
  const [loading, setLoading] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [syncingToInstance, setSyncingToInstance] = useState(false);
  const [repairingVisibility, setRepairingVisibility] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [importing, setImporting] = useState(false);
  const transfer = useCodexSessionTransfer();
  const { begin: beginTransfer, getCurrentId: getTransferId,
    succeed: succeedTransfer, fail: failTransfer } = transfer;
  const [message, setMessage] = useState<MessageState | null>(null);
  const [tokenStatsBySessionId, setTokenStatsBySessionId] = useState<SessionTokenStatsMap>({});
  const [titleSearchInput, setTitleSearchInput] = useState('');
  const [appliedTitleSearch, setAppliedTitleSearch] = useState('');
  const [archiveFilter, setArchiveFilter] = useState<CodexArchiveFilter>('active');
  const [sessionKindFilter, setSessionKindFilter] = useState<CodexSessionKindFilter>('conversation');
  const [sessionView, setSessionView] = useState<SessionManagerView>('list');
  const [sessionLocationTarget, setSessionLocationTarget] = useState<{
    session: CodexSessionRecord;
    action: 'location' | 'rollout';
  } | null>(null);
  const [openingSessionLocation, setOpeningSessionLocation] = useState(false);
  const [openingSessionId, setOpeningSessionId] = useState<string | null>(null);
  const {
    message: syncTargetModalError,
    scrollKey: syncTargetModalErrorScrollKey,
    set: setSyncTargetModalError,
  } = useModalErrorState();
  const {
    message: sessionLocationError,
    scrollKey: sessionLocationErrorScrollKey,
    set: setSessionLocationError,
  } = useModalErrorState();
  const hasInitializedExpandedGroupsRef = useRef(false);
  const loadSessionsPromiseRef = useRef<Promise<void> | null>(null);

  const {
    groups: groupedSessions, allIds: allSessionIds,
    selectedIds, setSelectedIds, selectedSet: selectedIdSet, selectedSessions,
    allSelected: allSessionsSelected, toggleSelection,
  } = useCodexSessionList(sessions, sessionKindFilter, archiveFilter, appliedTitleSearch);
  const orderedInstances = useMemo(() => sortInstancesForDisplay(instances), [instances]);
  const targetInstanceOptions = useMemo<SingleSelectOption[]>(
    () => [
      {
        value: '',
        label: t('codex.sessionManager.targetModal.pickTarget', '请选择目标实例'),
      },
      ...orderedInstances.map((instance) => ({
        value: instance.id,
        label: instance.isDefault
          ? t('instances.defaultName', '默认实例')
          : instance.name || t('instances.defaultName', '默认实例'),
      })),
    ],
    [orderedInstances, t],
  );
  const importTargetOptions = useMemo<SingleSelectOption[]>(
    () =>
      orderedInstances.map((instance) => ({
        value: instance.id,
        label: instance.isDefault
          ? t('instances.defaultName', '默认实例')
          : instance.name || t('instances.defaultName', '默认实例'),
      })),
    [orderedInstances, t],
  );

  const syncTargetInstance = useMemo(
    () => orderedInstances.find((instance) => instance.id === syncTargetInstanceId) ?? null,
    [orderedInstances, syncTargetInstanceId],
  );
  const syncTargetExistingCount = useMemo(() => {
    if (!syncTargetInstance) return 0;
    return selectedSessions.filter((session) =>
      session.locations.some((location) => location.instanceId === syncTargetInstance.id),
    ).length;
  }, [selectedSessions, syncTargetInstance]);
  const instanceCount = instances.length;
  const hasAppliedSearch = Boolean(appliedTitleSearch);
  const hasSearchInput = Boolean(titleSearchInput.trim());
  const loadSessions = useCallback(async () => {
    if (loadSessionsPromiseRef.current) {
      return await loadSessionsPromiseRef.current;
    }

    const task = (async () => {
      setLoading(true);
      try {
        const nextSessions = await listSessionsAcrossInstances();
        const nextGroups = buildGroups(buildSessionTrees(nextSessions));
        const hasInitializedExpandedGroups = hasInitializedExpandedGroupsRef.current;
        setSessions(nextSessions);
        setSelectedIds((prev) => prev.filter((id) => nextSessions.some((item) => item.sessionId === id)));
        setExpandedGroups((prev) => {
          const valid = prev.filter((cwd) => nextGroups.some((group) => group.cwd === cwd));

          if (prev.length === 0) {
            return hasInitializedExpandedGroups ? [] : buildDefaultExpandedGroups(nextGroups);
          }

          return valid.length > 0 ? valid : buildDefaultExpandedGroups(nextGroups);
        });
        hasInitializedExpandedGroupsRef.current = true;
      } catch (error) {
        setMessage({ text: String(error), tone: 'error' });
      } finally {
        setLoading(false);
      }
    })();

    loadSessionsPromiseRef.current = task;
    try {
      await task;
    } finally {
      if (loadSessionsPromiseRef.current === task) {
        loadSessionsPromiseRef.current = null;
      }
    }
  }, [listSessionsAcrossInstances]);

  const handleUsageReport = useCallback((report: CodexSessionUsageReport) => {
    setTokenStatsBySessionId(Object.fromEntries(
      (report.sessionTokens ?? []).map(stats => [stats.sessionId, stats]),
    ));
  }, []);

  const trashSource = useMemo<CodexSessionTrashSource>(() => ({
    load: async () => (await listTrashedSessionsAcrossInstances()).map(session => ({
      id: session.sessionId,
      sessionId: session.sessionId,
      parentThreadId: session.parentThreadId,
      sessionKind: session.sessionKind,
      cwd: session.cwd,
      title: session.title,
      deletedAt: session.deletedAt,
      sizeBytes: session.sizeBytes,
      locations: session.locations,
    })),
    restore: restoreSessionsFromTrashAcrossInstances,
    purge: deleteTrashedSessionsAcrossInstances,
    empty: emptySessionTrashAcrossInstances,
  }), [listTrashedSessionsAcrossInstances, restoreSessionsFromTrashAcrossInstances,
    deleteTrashedSessionsAcrossInstances, emptySessionTrashAcrossInstances]);

  useEffect(() => {
    void loadSessions();
  }, [loadSessions]);

  useEffect(() => {
    const nextTitleQuery = titleSearchInput.trim();
    const timer = window.setTimeout(() => {
      setMessage(null);
      setAppliedTitleSearch((current) => (current === nextTitleQuery ? current : nextTitleQuery));
    }, 300);

    return () => {
      window.clearTimeout(timer);
    };
  }, [titleSearchInput]);

  const toggleSession = (sessionId: string) => toggleSelection([sessionId]);
  const toggleGroupSelection = toggleSelection;
  const toggleAllSessions = () => toggleSelection(allSessionIds);

  const toggleGroupExpanded = (cwd: string) => {
    setExpandedGroups((prev) => (prev.includes(cwd) ? prev.filter((item) => item !== cwd) : [...prev, cwd]));
  };

  const handleOpenRestoreModal = () => {
    setShowRestoreModal(true);
  };

  const handleOpenSyncTargetModal = async () => {
    if (selectedIds.length === 0) {
      setMessage({ text: t('codex.sessionManager.messages.pickOne', '请至少选择一条会话'), tone: 'error' });
      return;
    }

    setMessage(null);
    setSyncTargetModalError(null);
    try {
      const latestInstances = await refreshInstances();
      const targetCandidates = sortInstancesForDisplay(
        latestInstances.length > 0 ? latestInstances : instances,
      );
      const firstMissingTarget = targetCandidates.find((instance) =>
        selectedSessions.some((session) =>
          !session.locations.some((location) => location.instanceId === instance.id),
        ),
      );
      setSyncTargetInstanceId((firstMissingTarget ?? targetCandidates[0])?.id ?? '');
      setShowSyncTargetModal(true);
    } catch (error) {
      setMessage({ text: String(error), tone: 'error' });
    }
  };

  const handleCloseSyncTargetModal = () => {
    setShowSyncTargetModal(false);
  };

  useEscClose(showSyncTargetModal, handleCloseSyncTargetModal);

  const handleSyncSessions = async () => {
    setMessage(null);
    try {
      const latestInstances = await refreshInstances();
      if (latestInstances.length < 2) {
        setMessage({
          text: t('codex.sessionManager.messages.syncNeedTwo', '至少需要两个实例才能同步会话'),
          tone: 'error',
        });
        return;
      }

      const confirmed = await confirmDialog(
        t(
          'codex.sessionManager.confirm.syncMessage',
          '会将缺失会话的 rollout、session_index 条目和会话文件时间同步到所有实例，并对同 ID 会话做事件级合并，随后触发官方 Codex 重建会话索引；写入前会备份目标文件。确认继续？',
        ),
        {
          title: t('codex.sessionManager.actions.syncSessions', '同步会话'),
          okLabel: t('common.confirm', '确认'),
          cancelLabel: t('common.cancel', '取消'),
        },
      );
      if (!confirmed) return;

      setSyncing(true);
      const summary = await syncThreadsAcrossInstances();
      setMessage({ text: summary.message });
      await loadSessions();
    } catch (error) {
      setMessage({ text: String(error), tone: 'error' });
    } finally {
      setSyncing(false);
    }
  };

  const handleSyncSelectedToInstance = async () => {
    if (selectedIds.length === 0) {
      setSyncTargetModalError(t('codex.sessionManager.messages.pickOne', '请至少选择一条会话'));
      return;
    }
    if (!syncTargetInstanceId) {
      setSyncTargetModalError(t('codex.sessionManager.targetModal.pickTarget', '请选择目标实例'));
      return;
    }

    setSyncingToInstance(true);
    setSyncTargetModalError(null);
    try {
      const summary = await syncSessionsToInstance(selectedIds, syncTargetInstanceId);
      setMessage({ text: summary.message });
      setShowSyncTargetModal(false);
      setSyncTargetInstanceId('');
      setSelectedIds([]);
      await loadSessions();
    } catch (error) {
      setSyncTargetModalError(String(error));
    } finally {
      setSyncingToInstance(false);
    }
  };

  const handleRefresh = async () => {
    setMessage(null);
    try {
      await refreshInstances();
      await loadSessions();
    } catch (error) {
      setMessage({ text: String(error), tone: 'error' });
    }
  };

  const handleClearSearch = () => {
    setTitleSearchInput('');
    setMessage(null);

    if (!appliedTitleSearch) {
      return;
    }

    setAppliedTitleSearch('');
  };

  const handleRepairVisibility = async () => {
    setMessage(null);
    setShowRepairVisibilityModal(true);
  };

  /** 用翻译键组装“移到废纸篓”结果提示；后端未返回结构化计数时回退到后端文案。 */
  const buildTrashSummaryText = (summary: CodexSessionTrashSummary): string => {
    if (summary.failures?.length) {
      return t('codex.sessionManager.messages.trashPartial', '已删除 {{count}} 条会话（含子代理）；以下项目未完成或需要检查：', { count: summary.trashedSessionCount }) + '\n' + summary.failures.join('\n');
    }
    if (summary.trashedSessionCount === 0) {
      return t('codex.sessionManager.messages.trashSummaryEmpty', '所选会话在当前实例集合中不存在，无需处理');
    }
    return t('codex.sessionManager.messages.trashSummary',
      '已将 {{count}} 条会话移到废纸篓，并已在官方 Codex 中删除',
      { count: summary.trashedSessionCount });
  };

  const handleMoveToTrash = async () => {
    if (selectedIds.length === 0) {
      setMessage({ text: t('codex.sessionManager.messages.pickOne', '请至少选择一条会话'), tone: 'error' });
      return;
    }

    const confirmed = await confirmDialog(
      t(
        'codex.sessionManager.confirm.message',
        '会将所选会话及其全部子代理会话从对应实例中移到废纸篓，便于后续恢复。确认继续？',
      ),
      {
        title: t('codex.sessionManager.confirm.title', '移到废纸篓'),
        okLabel: t('common.confirm', '确认'),
        cancelLabel: t('common.cancel', '取消'),
        kind: 'warning',
      },
    );
    if (!confirmed) return;

    setDeleting(true);
    setMessage(null);
    try {
      const summary = await moveSessionsToTrashAcrossInstances(selectedIds);
      setMessage({ text: buildTrashSummaryText(summary), tone: summary.failures?.length ? 'error' : undefined });
      setSelectedIds([]);
      await loadSessions();
    } catch (error) {
      await loadSessions();
      setMessage({ text: String(error), tone: 'error' });
    } finally {
      setDeleting(false);
    }
  };

  const exportSource = useMemo<CodexSessionExportSource>(() => ({
    preview: previewSessionExport,
    export: async (sessionIds, path) => {
      const transferId = getTransferId() ?? beginTransfer('export', sessionIds.length);
      try {
        const summary = await exportSessions(sessionIds, path, transferId);
        succeedTransfer(transferId, summary.message);
        return summary;
      } catch (error) {
        const errorText = String(error);
        failTransfer(transferId, errorText);
        setMessage({ text: errorText, tone: 'error' });
        throw error;
      } finally {
        setExporting(false);
      }
    },
  }), [previewSessionExport, exportSessions, getTransferId, beginTransfer, succeedTransfer, failTransfer]);

  const handleExportSessions = () => {
    if (selectedIds.length === 0) {
      setMessage({ text: t('codex.sessionManager.messages.pickOne', '请至少选择一条会话'), tone: 'error' });
      return;
    }
    setMessage(null);
    setShowExportModal(true);
  };

  const handleExportStart = (sessionIds: string[]) => {
    beginTransfer('export', sessionIds.length);
    setExporting(true);
    setMessage(null);
    setShowExportModal(false);
  };

  const importSource = useMemo<CodexSessionImportSource>(() => ({
    preview: (path, targetId) => {
      if (!targetId) throw new Error(t('codex.sessionManager.targetModal.pickTarget', '请选择目标实例'));
      return previewSessionImport(path, targetId);
    },
    validatePaths: validateSessionImportPaths,
    import: async (path, ids, targetId, cwdMappings = {}) => {
      if (!targetId) throw new Error(t('codex.sessionManager.targetModal.pickTarget', '请选择目标实例'));
      const transferId = getTransferId() ?? beginTransfer('import', ids.length);
      try {
        const summary = await importSessions(path, targetId, ids, cwdMappings, transferId);
        succeedTransfer(transferId, summary.message);
        return summary;
      } catch (error) {
        const errorText = String(error);
        failTransfer(transferId, errorText);
        setMessage({ text: errorText, tone: 'error' });
        throw error;
      } finally {
        setImporting(false);
      }
    },
  }), [previewSessionImport, importSessions, t, getTransferId, beginTransfer, succeedTransfer, failTransfer]);

  const handleOpenImportModal = async () => {
    const selected = await openFileDialog({ multiple: false,
      filters: [{ name: 'ZIP', extensions: ['zip'] }] });
    const filePath = Array.isArray(selected) ? selected[0] : selected;
    if (!filePath) return;
    setMessage(null);
    try {
      const latestInstances = await refreshInstances();
      const targetCandidates = sortInstancesForDisplay(latestInstances.length > 0 ? latestInstances : instances);
      const defaultTarget = targetCandidates.find(instance => instance.isDefault) ?? targetCandidates[0];
      setImportTargetInstanceId(importTargetInstanceId || defaultTarget?.id || '');
      setImportFilePath(filePath);
      setShowImportModal(true);
    } catch (error) {
      setMessage({ text: String(error), tone: 'error' });
    }
  };

  const handleImportStart = (ids: string[]) => {
    beginTransfer('import', ids.length);
    setImporting(true);
    setMessage(null);
    setShowImportModal(false);
  };

  const pickSessionInstanceId = (session: CodexSessionRecord): string | null => {
    if (session.locations.length === 1) {
      return session.locations[0]?.instanceId ?? null;
    }
    if (session.locations.length > 1) {
      // Prefer default instance when present; otherwise first location.
      const preferred =
        session.locations.find((loc) => loc.instanceId === '__default__') ??
        session.locations[0];
      return preferred?.instanceId ?? null;
    }
    return null;
  };

  const openSessionAtInstance = async (
    session: CodexSessionRecord,
    action: 'location' | 'rollout',
    instanceId: string | null,
    insidePicker: boolean,
  ) => {
    setOpeningSessionLocation(true);
    setOpeningSessionId(session.sessionId);
    if (insidePicker) {
      setSessionLocationError(null);
    }
    try {
      if (action === 'location') {
        await openSessionLocation(session.sessionId, instanceId);
      } else {
        await openSessionRollout(session.sessionId, instanceId);
      }
      setSessionLocationTarget(null);
      setSessionLocationError(null);
    } catch (error) {
      const errorText = String(error);
      if (insidePicker) {
        setSessionLocationError(errorText);
        return;
      }
      setMessage({ text: errorText, tone: 'error' });
    } finally {
      setOpeningSessionLocation(false);
      setOpeningSessionId(null);
    }
  };

  const handleOpenSessionTarget = (
    event: MouseEvent<HTMLButtonElement>,
    session: CodexSessionRecord,
    action: 'location' | 'rollout',
  ) => {
    event.preventDefault();
    event.stopPropagation();
    setMessage(null);
    if (session.locations.length > 1) {
      // Require explicit choice when ambiguous (#1510); use an in-app picker so the
      // instance name shown in the location column is directly selectable (#2129).
      setSessionLocationError(null);
      setSessionLocationTarget({ session, action });
      return;
    }
    void openSessionAtInstance(session, action, pickSessionInstanceId(session), false);
  };

  const handleOpenSessionLocation = (
    event: MouseEvent<HTMLButtonElement>,
    session: CodexSessionRecord,
  ) => {
    handleOpenSessionTarget(event, session, 'location');
  };

  const handleOpenSessionRollout = (
    event: MouseEvent<HTMLButtonElement>,
    session: CodexSessionRecord,
  ) => {
    handleOpenSessionTarget(event, session, 'rollout');
  };

  const handleCloseSessionLocationPicker = () => {
    if (openingSessionLocation) return;
    setSessionLocationTarget(null);
    setSessionLocationError(null);
  };

  useEscClose(
    Boolean(sessionLocationTarget) && !openingSessionLocation,
    handleCloseSessionLocationPicker,
  );

  return (
    <section className="codex-session-manager">
      {sessionView === 'usage' ? (
        <CodexSessionUsagePanel initialCheck={initialUsageCheck} onBack={() => setSessionView('list')} />
      ) : null}
      {sessionView === 'list' ? (
      <>
      <CodexSessionUsageSummary onReport={handleUsageReport} initialCheck={initialUsageCheck} onOpenDetail={() => setSessionView('usage')} />
      <div className="codex-session-manager__header">
        <CodexSessionSearch
          query={titleSearchInput} onQueryChange={setTitleSearchInput}
          onClear={handleClearSearch} canClear={hasSearchInput || hasAppliedSearch}
          disabled={loading} archive={archiveFilter} onArchiveChange={setArchiveFilter}
          kind={sessionKindFilter} onKindChange={setSessionKindFilter}
        />
        <CodexSessionToolbar selectedCount={selectedIds.length} allSelected={allSessionsSelected}
          hasSessions={allSessionIds.length > 0} loading={loading}
          busy={syncing || syncingToInstance || repairingVisibility || deleting || exporting || importing}
          onToggleAll={toggleAllSessions} onExport={() => void handleExportSessions()}
          onTrash={() => void handleMoveToTrash()} onOpenTrash={handleOpenRestoreModal}
          onRefresh={() => void handleRefresh()}
          selectionExtras={<button className="btn btn-secondary codex-session-manager__action-button" type="button"
            onClick={() => void handleOpenSyncTargetModal()}
            disabled={syncing || syncingToInstance || repairingVisibility || deleting || loading || exporting || selectedIds.length === 0}>
            <Copy size={14} className={syncingToInstance ? 'icon-spin' : undefined} />
            {t('codex.sessionManager.actions.copyToInstance', '复制到实例')} ({selectedIds.length})
          </button>}
          maintenanceExtras={<>
            <button className="btn btn-secondary codex-session-manager__action-button" type="button"
              onClick={() => void handleSyncSessions()}
              disabled={syncing || syncingToInstance || repairingVisibility || deleting || loading || instanceCount < 2}
              title={instanceCount < 2
                ? t('codex.sessionManager.messages.syncNeedTwo', '至少需要两个实例才能同步会话')
                : t('codex.sessionManager.actions.syncSessions', '同步会话')}>
              <RefreshCw size={14} className={syncing ? 'icon-spin' : undefined} />
              {t('codex.sessionManager.actions.syncSessions', '同步会话')}
            </button>
            <button className="btn btn-secondary codex-session-manager__action-button" type="button"
              onClick={() => void handleOpenImportModal()}
              disabled={loading || syncing || syncingToInstance || repairingVisibility || deleting || exporting || importing}>
              <Upload size={14} />
              {t('codex.sessionManager.actions.importSessions', '导入会话')}
            </button>
            <button className="btn btn-secondary codex-session-manager__action-button" type="button"
              onClick={() => void handleRepairVisibility()}
              disabled={repairingVisibility || loading || deleting || syncing || syncingToInstance || exporting}>
              <Eye size={14} />
              {t('codex.sessionManager.actions.repairVisibility', '修复可见性')}
            </button>
          </>}
        />
      </div>

      {message ? (
        <div className={`message-bar ${message.tone === 'error' ? 'error' : 'success'}`}>{message.text}</div>
      ) : null}

      <CodexSessionTransfer transfer={transfer} />

      {loading && sessions.length === 0 ? (
        <div className="empty-state">
          <h3>{t('common.loading', '加载中...')}</h3>
        </div>
      ) : null}

      {!loading && groupedSessions.length === 0 ? (
        <div className="empty-state codex-session-manager__empty">
          <Folder size={42} className="empty-icon" />
          <h3>
            {hasAppliedSearch
              ? t('codex.sessionManager.empty.searchTitle', '未找到匹配会话')
              : t('codex.sessionManager.empty.title', '还没有可管理的会话')}
          </h3>
          <p>
            {hasAppliedSearch
              ? t('codex.sessionManager.empty.searchDesc', '请调整标题关键词后再试。')
              : t('codex.sessionManager.empty.desc', '当前实例集合中还没有发现会话记录。')}
          </p>
        </div>
      ) : null}

      <CodexSessionList
        groups={groupedSessions}
        disabled={deleting}
        selectedIds={selectedIdSet}
        expandedGroups={expandedGroups}
        onToggleGroup={toggleGroupExpanded}
        onToggleGroupSelection={toggleGroupSelection}
        onToggleSession={toggleSession}
        renderActions={(session) => {
          return <>
            <CodexSessionRowActions sessionId={session.sessionId} disabled={openingSessionLocation}
              opening={openingSessionId === session.sessionId}
              onOpenLocation={event => handleOpenSessionLocation(event, session)}
              onOpenFile={event => handleOpenSessionRollout(event, session)}
              onError={text => setMessage({ text, tone: 'error' })} />
            <CodexSessionUsageBadge stats={tokenStatsBySessionId[session.sessionId]} />
          </>;
        }}
      />
      </>
      ) : null}

      {showSyncTargetModal ? (
        <div className="modal-overlay">
          <div className="modal codex-session-target-modal" onClick={(event) => event.stopPropagation()}>
            <div className="modal-header">
              <h2>{t('codex.sessionManager.targetModal.title', '复制到实例')}</h2>
              <button
                className="modal-close"
                type="button"
                onClick={handleCloseSyncTargetModal}
                disabled={syncingToInstance}
                aria-label={t('common.close', '关闭')}
              >
                <X size={18} />
              </button>
            </div>
            <div className="modal-body">
              <ModalErrorMessage message={syncTargetModalError} scrollKey={syncTargetModalErrorScrollKey} />
              <p className="codex-session-target-modal__hint">
                {t(
                  'codex.sessionManager.targetModal.hint',
                  '会把所选会话的 rollout、session_index 条目和会话文件时间补到目标实例，并触发官方 Codex 重建会话索引；已有同 ID 会话会自动跳过。',
                )}
              </p>
              <label className="codex-session-target-modal__field">
                <span>{t('codex.sessionManager.targetModal.targetInstance', '目标实例')}</span>
                <SingleSelectDropdown
                  className="codex-session-target-modal__select"
                  value={syncTargetInstanceId}
                  options={targetInstanceOptions}
                  onChange={(value) => {
                    setSyncTargetInstanceId(value);
                    setSyncTargetModalError(null);
                  }}
                  disabled={syncingToInstance}
                  ariaLabel={t('codex.sessionManager.targetModal.targetInstance', '目标实例')}
                  menuMaxHeight={240}
                />
              </label>
              <div className="codex-session-target-modal__summary">
                <span>
                  {t('codex.sessionManager.targetModal.selectedCount', {
                    defaultValue: '已选择 {{count}} 条会话',
                    count: selectedIds.length,
                  })}
                </span>
                {syncTargetInstance ? (
                  <span>
                    {t('codex.sessionManager.targetModal.existingCount', {
                      defaultValue: '目标已存在 {{count}} 条',
                      count: syncTargetExistingCount,
                    })}
                  </span>
                ) : null}
              </div>
            </div>
            <div className="modal-footer">
              <button
                className="btn btn-secondary"
                type="button"
                onClick={handleCloseSyncTargetModal}
                disabled={syncingToInstance}
              >
                {t('common.cancel', '取消')}
              </button>
              <button
                className="btn btn-primary"
                type="button"
                onClick={() => void handleSyncSelectedToInstance()}
                disabled={syncingToInstance || !syncTargetInstanceId || selectedIds.length === 0}
              >
                <Copy size={14} className={syncingToInstance ? 'icon-spin' : undefined} />
                {t('codex.sessionManager.targetModal.confirm', '复制会话')}
              </button>
            </div>
          </div>
        </div>
      ) : null}

      <CodexSessionExportModal open={showExportModal} sessionIds={selectedIds}
        source={exportSource} onClose={() => setShowExportModal(false)}
        onExportStart={handleExportStart} onMessage={text => setMessage({ text })} />

      <CodexSessionImportModal open={showImportModal} filePath={importFilePath}
        source={importSource} onClose={() => setShowImportModal(false)}
        targetOptions={importTargetOptions} defaultTargetId={importTargetInstanceId} requireTarget
        onTargetChange={setImportTargetInstanceId} onImportStart={handleImportStart}
        onChanged={() => loadSessions()} onMessage={text => setMessage({ text })} />

      <CodexSessionVisibilityRepairModal
        open={showRepairVisibilityModal}
        selectedSessionIds={selectedIds}
        totalSessionCount={allSessionIds.length}
        onClose={() => setShowRepairVisibilityModal(false)}
        onRunningChange={setRepairingVisibility}
        onRepaired={() => loadSessions()}
      />

      <CodexSessionTrashModal open={showRestoreModal} onClose={() => setShowRestoreModal(false)}
        source={trashSource} onChanged={() => loadSessions()}
        onMessage={text => setMessage({ text })} />
      {sessionLocationTarget ? (
        <div className="modal-overlay">
          <div
            className="modal codex-session-location-modal"
            onClick={(event) => event.stopPropagation()}
          >
            <div className="modal-header">
              <h2>{t('codex.sessionManager.locationPicker.title', '选择实例')}</h2>
              <button
                className="modal-close"
                type="button"
                onClick={handleCloseSessionLocationPicker}
                disabled={openingSessionLocation}
                aria-label={t('common.close', '关闭')}
              >
                <X size={18} />
              </button>
            </div>
            <div className="modal-body">
              <ModalErrorMessage
                message={sessionLocationError}
                scrollKey={sessionLocationErrorScrollKey}
              />
              <p className="codex-session-location-modal__hint">
                {sessionLocationTarget.action === 'rollout'
                  ? t(
                      'codex.sessionManager.locationPicker.rolloutHint',
                      '该会话存在于多个实例，请选择要打开会话文件的实例：',
                    )
                  : t(
                      'codex.sessionManager.locationPicker.hint',
                      '该会话存在于多个实例，请选择要打开文件所在位置的实例：',
                    )}
              </p>
              <div className="codex-session-location-modal__list">
                {sessionLocationTarget.session.locations.map((location) => (
                  <button
                    key={location.instanceId}
                    className="codex-session-location-modal__option"
                    type="button"
                    onClick={() =>
                      void openSessionAtInstance(
                        sessionLocationTarget.session,
                        sessionLocationTarget.action,
                        location.instanceId,
                        true,
                      )
                    }
                    disabled={openingSessionLocation}
                  >
                    <span className="codex-session-location-modal__option-name">
                      {location.instanceName}
                    </span>
                    {location.running ? (
                      <span className="codex-session-location-modal__option-badge">
                        {t('codex.sessionManager.locationPicker.running', '运行中')}
                      </span>
                    ) : null}
                  </button>
                ))}
              </div>
            </div>
            <div className="modal-footer">
              <button
                className="btn btn-secondary"
                type="button"
                onClick={handleCloseSessionLocationPicker}
                disabled={openingSessionLocation}
              >
                {t('common.cancel', '取消')}
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </section>
  );
}
