import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { confirm as confirmDialog } from '@tauri-apps/plugin-dialog';
import { ChevronDown, ChevronRight, Folder, RotateCcw, Trash2, X } from 'lucide-react';
import { ModalErrorMessage, useModalErrorState } from '../ModalErrorMessage';
import { useEscClose } from '../../hooks/useEscClose';
import { formatRelativeTime, formatSessionId, resolveGroupLabel } from '../../utils/codexSessionPresentation';

export interface CodexSessionTrashRow {
  /** Unique selection and operation key. A remote trash batch can use its batch ID. */
  id: string;
  sessionId: string;
  parentThreadId?: string | null;
  /** Distinguishes independent families whose session IDs happen to match. */
  familyId?: string;
  sessionKind?: string;
  cwd: string;
  title: string;
  deletedAt?: number | null;
  sizeBytes: number;
  locations?: { instanceName: string }[];
}

export interface CodexSessionTrashSource {
  load: () => Promise<CodexSessionTrashRow[]>;
  restore: (ids: string[]) => Promise<{ message: string }>;
  purge: (ids: string[]) => Promise<{ message: string }>;
  empty: () => Promise<{ message: string }>;
}

interface Props {
  open: boolean;
  onClose: () => void;
  source: CodexSessionTrashSource;
  onChanged?: () => void | Promise<void>;
  onMessage?: (message: string) => void;
}

interface TrashTree extends CodexSessionTrashRow {
  children: TrashTree[];
}

function formatBytes(value: number): string {
  if (value >= 1024 ** 3) return `${(value / 1024 ** 3).toFixed(1)} GB`;
  if (value >= 1024 ** 2) return `${(value / 1024 ** 2).toFixed(1)} MB`;
  if (value >= 1024) return `${(value / 1024).toFixed(1)} KB`;
  return `${value} B`;
}

export function buildTrashTrees(rows: CodexSessionTrashRow[]): TrashTree[] {
  const nodes = rows.map(row => ({ ...row, children: [] as TrashTree[] }));
  const bySessionId = new Map<string, TrashTree[]>();
  nodes.forEach(node => bySessionId.set(node.sessionId, [...(bySessionId.get(node.sessionId) ?? []), node]));
  const roots: TrashTree[] = [];
  nodes.forEach(node => {
    if (node.parentThreadId) {
      const parent = bySessionId.get(node.parentThreadId)?.find(candidate =>
        candidate.id !== node.id && (node.familyId === undefined || candidate.familyId === node.familyId));
      if (parent) parent.children.push(node);
    } else if (node.sessionKind !== 'subagent') {
      roots.push(node);
    }
  });
  return roots;
}

export function familyRows(tree: TrashTree): TrashTree[] {
  return [tree, ...tree.children.flatMap(familyRows)];
}

function TrashBranch({ row, selected, disabled, child = false, onToggle, onPurge }: {
  row: TrashTree;
  selected: boolean;
  disabled: boolean;
  child?: boolean;
  onToggle: (id: string) => void;
  onPurge: (id: string) => void;
}) {
  const { t, i18n } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  return <div className={`codex-session-branch${child ? ' codex-session-branch--child' : ''}`}>
    <div className="codex-session-row">
      <div className="codex-session-row__left">
        {!child && <input className="codex-session-row__checkbox" type="checkbox"
          aria-label={t('codex.sessionManager.selectConversation', '选择对话：{{title}}', { title: row.title })}
          disabled={disabled} checked={selected} onChange={() => onToggle(row.id)} />}
        <div className="codex-session-row__content">
          <span className="codex-session-row__title" title={row.title}>{row.title || row.sessionId}</span>
          {row.locations && row.locations.length > 0 && <span className="codex-session-row__meta">
            {row.locations.map(location => location.instanceName).join(' / ')}
          </span>}
          <span className="codex-session-row__meta codex-session-row__session-id" title={row.sessionId}>
            {t('codex.sessionManager.labels.sessionId', '会话 ID')}: {formatSessionId(row.sessionId)}
          </span>
          {row.children.length > 0 && <button type="button" className="codex-session-branch__toggle"
            aria-expanded={expanded} onClick={() => setExpanded(value => !value)}>
            {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
            {t('codex.sessionManager.childAgents', '子代理（{{count}}）', { count: row.children.length })}
          </button>}
        </div>
      </div>
      <div className="codex-session-row__right">
        {!child && <button className="btn btn-danger codex-session-restore-row__delete" type="button"
          onClick={() => onPurge(row.id)} disabled={disabled}>
          <Trash2 size={13} />{t('codex.sessionManager.restoreModal.deleteOne', '永久删除')}
        </button>}
        <span className="codex-session-row__time">{formatRelativeTime(row.deletedAt, i18n.language.startsWith('zh'))}</span>
      </div>
    </div>
    {expanded && row.children.length > 0 && <div className="codex-session-branch__children">
      {row.children.map(item => <TrashBranch key={item.id} row={item} selected={false}
        disabled={disabled} child onToggle={onToggle} onPurge={onPurge} />)}
    </div>}
  </div>;
}

export function CodexSessionTrashModal({ open, onClose, source, onChanged, onMessage }: Props) {
  const { t, i18n } = useTranslation();
  const [rows, setRows] = useState<CodexSessionTrashRow[]>([]);
  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const [expandedGroups, setExpandedGroups] = useState<string[]>([]);
  const [loading, setLoading] = useState(false);
  const [restoring, setRestoring] = useState(false);
  const [purging, setPurging] = useState(false);
  const { message: error, scrollKey: errorScrollKey, set: setError } = useModalErrorState();
  const loadVersion = useRef(0);
  const busy = loading || restoring || purging;
  const trees = useMemo(() => buildTrashTrees(rows), [rows]);
  const selectedSet = useMemo(() => new Set(selectedIds), [selectedIds]);
  const groups = useMemo(() => {
    const byCwd = new Map<string, TrashTree[]>();
    trees.forEach(tree => byCwd.set(tree.cwd, [...(byCwd.get(tree.cwd) ?? []), tree]));
    return [...byCwd].map(([cwd, sessions]) => ({
      cwd,
      sessions: sessions.sort((a, b) => (b.deletedAt ?? 0) - (a.deletedAt ?? 0) || a.title.localeCompare(b.title)),
      latestDeletedAt: Math.max(...sessions.map(row => row.deletedAt ?? 0)),
    })).sort((a, b) => b.latestDeletedAt - a.latestDeletedAt || a.cwd.localeCompare(b.cwd, 'zh-CN'));
  }, [trees]);
  const selectedTrees = useMemo(() => trees.filter(tree => selectedSet.has(tree.id)), [trees, selectedSet]);
  const totalSize = rows.reduce((sum, row) => sum + (row.sizeBytes ?? 0), 0);
  const selectedSize = selectedTrees.flatMap(familyRows).reduce((sum, row) => sum + (row.sizeBytes ?? 0), 0);
  const allSelected = trees.length > 0 && trees.every(tree => selectedSet.has(tree.id));

  const load = useCallback(async () => {
    const version = ++loadVersion.current;
    setLoading(true);
    setError(null);
    setRows([]);
    try {
      const next = await source.load();
      if (version !== loadVersion.current) return null;
      setRows(next);
      setExpandedGroups([...new Set(next.map(row => row.cwd))]);
      setSelectedIds(previous => previous.filter(id => next.some(row => row.id === id)));
      return next;
    } catch (cause) {
      if (version === loadVersion.current) {
        setSelectedIds([]);
        setError(String(cause));
      }
      return null;
    } finally {
      if (version === loadVersion.current) setLoading(false);
    }
  }, [source, setError]);

  useEffect(() => {
    if (open) {
      setSelectedIds([]);
      void load();
    } else {
      loadVersion.current += 1;
    }
    return () => { loadVersion.current += 1; };
  }, [open, load]);

  const refreshAfterFailure = useCallback(async (message: string) => {
    await Promise.allSettled([load(), Promise.resolve().then(() => onChanged?.())]);
    setError(message);
  }, [load, onChanged, setError]);

  const close = useCallback(() => {
    if (busy) return;
    setSelectedIds([]);
    setError(null);
    onClose();
  }, [busy, onClose, setError]);
  useEscClose(open, close);

  const toggle = (id: string) => setSelectedIds(previous => previous.includes(id)
    ? previous.filter(value => value !== id) : [...previous, id]);
  const toggleGroup = (ids: string[]) => setSelectedIds(previous => ids.every(id => previous.includes(id))
    ? previous.filter(id => !ids.includes(id)) : [...new Set([...previous, ...ids])]);

  const restore = async () => {
    if (selectedIds.length === 0) {
      setError(t('codex.sessionManager.messages.pickRestoreOne', '请至少选择一条待恢复会话'));
      return;
    }
    setRestoring(true);
    setError(null);
    try {
      const result = await source.restore(selectedIds);
      onMessage?.(result.message);
      setSelectedIds([]);
      const [next] = await Promise.all([load(), onChanged?.()]);
      if (next && next.length === 0) onClose();
    } catch (cause) {
      await refreshAfterFailure(String(cause));
    } finally {
      setRestoring(false);
    }
  };

  const purge = async (ids: string[]) => {
    const uniqueIds = [...new Set(ids.filter(Boolean))];
    if (uniqueIds.length === 0) {
      setError(t('codex.sessionManager.restoreModal.pickDeleteOne', '请至少选择一条要永久删除的会话'));
      return;
    }
    const deletingSize = trees.filter(tree => uniqueIds.includes(tree.id)).flatMap(familyRows)
      .reduce((sum, row) => sum + (row.sizeBytes ?? 0), 0);
    const confirmed = await confirmDialog(uniqueIds.length === 1
      ? t('codex.sessionManager.restoreModal.deleteOneConfirm',
          '确定要永久删除这个会话吗？删除后无法恢复，预计释放 {{size}}。', { size: formatBytes(deletingSize) })
      : t('codex.sessionManager.restoreModal.deleteSelectedConfirm',
          '确定要永久删除选中的 {{count}} 条会话吗？删除后无法恢复，预计释放 {{size}}。',
          { count: uniqueIds.length, size: formatBytes(deletingSize) }), {
      title: t('codex.sessionManager.restoreModal.permanentDeleteTitle', '永久删除'),
      okLabel: t('codex.sessionManager.restoreModal.permanentDeleteAction', '永久删除'),
      cancelLabel: t('common.cancel', '取消'),
      kind: 'warning',
    });
    if (!confirmed) return;
    setPurging(true);
    setError(null);
    try {
      const result = await source.purge(uniqueIds);
      onMessage?.(result.message);
      setSelectedIds(previous => previous.filter(id => !uniqueIds.includes(id)));
      const [next] = await Promise.all([load(), onChanged?.()]);
      if (next && next.length === 0) onClose();
    } catch (cause) {
      await refreshAfterFailure(String(cause));
    } finally {
      setPurging(false);
    }
  };

  const empty = async () => {
    if (rows.length === 0) return;
    const confirmed = await confirmDialog(t('codex.sessionManager.restoreModal.emptyTrashConfirm',
      '确定要清空废纸篓吗？其中 {{count}} 条会话将被永久删除且无法恢复，预计释放 {{size}}。',
      { count: trees.length, size: formatBytes(totalSize) }), {
      title: t('codex.sessionManager.restoreModal.emptyTrash', '清空废纸篓'),
      okLabel: t('codex.sessionManager.restoreModal.emptyTrash', '清空废纸篓'),
      cancelLabel: t('common.cancel', '取消'),
      kind: 'warning',
    });
    if (!confirmed) return;
    setPurging(true);
    setError(null);
    try {
      const result = await source.empty();
      onMessage?.(result.message);
      setSelectedIds([]);
      await Promise.all([load(), onChanged?.()]);
      onClose();
    } catch (cause) {
      await refreshAfterFailure(String(cause));
    } finally {
      setPurging(false);
    }
  };

  if (!open) return null;
  return <div className="modal-overlay">
    <div className="modal codex-session-restore-modal" onClick={event => event.stopPropagation()}>
      <div className="modal-header">
        <h2>{t('codex.sessionManager.restoreModal.title', '废纸篓')}</h2>
        <button className="modal-close" type="button" onClick={close} disabled={busy}
          aria-label={t('common.close', '关闭')}><X size={18} /></button>
      </div>
      <div className="modal-body">
        <ModalErrorMessage message={error} scrollKey={errorScrollKey} />
        {loading && <div className="codex-session-restore-modal__empty"><h3>{t('common.loading', '加载中...')}</h3></div>}
        {!loading && trees.length === 0 && <div className="codex-session-restore-modal__empty">
          <Folder size={36} className="empty-icon" />
          <h3>{t('codex.sessionManager.restoreModal.emptyTitle', '废纸篓里还没有会话')}</h3>
          <p>{t('codex.sessionManager.restoreModal.emptyDesc', '已移到废纸篓的会话会显示在这里。')}</p>
        </div>}
        {!loading && trees.length > 0 && <>
          <div className="codex-session-restore-modal__summary">
            <span>{t('codex.sessionManager.restoreModal.summary', '共 {{count}} 条，{{size}}',
              { count: trees.length, size: formatBytes(totalSize) })}</span>
            <span>{t('codex.sessionManager.restoreModal.selectedSummary', '已选 {{count}} 条，{{size}}',
              { count: selectedIds.length, size: formatBytes(selectedSize) })}</span>
          </div>
          <div className="codex-session-restore-actions">
            <button className="btn btn-secondary" type="button" disabled={busy}
              onClick={() => setSelectedIds(allSelected ? [] : trees.map(tree => tree.id))}>
              {allSelected ? t('codex.sessionManager.restoreModal.clearSelected', '取消选择')
                : t('codex.sessionManager.restoreModal.selectAll', '全选')}
            </button>
            <button className="btn btn-danger" type="button" disabled={busy || rows.length === 0}
              onClick={() => void empty()}>
              <Trash2 size={14} className={purging && selectedIds.length === 0 ? 'icon-spin' : undefined} />
              {t('codex.sessionManager.restoreModal.emptyTrash', '清空废纸篓')}
            </button>
          </div>
          <p className="codex-session-restore-modal__hint">
            {t('codex.sessionManager.restoreModal.hint',
              '废纸篓中的会话可以恢复到原实例，也可以永久删除以释放磁盘空间。')}
          </p>
          <div className="codex-session-restore-list"><div className="codex-session-manager__list">
            {groups.map(group => {
              const ids = group.sessions.map(row => row.id);
              const groupSelected = ids.length > 0 && ids.every(id => selectedSet.has(id));
              const expanded = expandedGroups.includes(group.cwd);
              return <section className="codex-session-folder" key={group.cwd}>
                <div className="codex-session-folder__row">
                  <div className="codex-session-folder__left">
                    <button className="codex-session-folder__expand" type="button"
                      aria-label={expanded ? t('codex.sessionManager.actions.collapse', '收起') : t('codex.sessionManager.actions.expand', '展开')}
                      onClick={() => setExpandedGroups(previous => expanded ? previous.filter(cwd => cwd !== group.cwd)
                        : [...previous, group.cwd])}>
                      {expanded ? <ChevronDown size={16} /> : <ChevronRight size={16} />}
                    </button>
                    <input className="codex-session-folder__checkbox" type="checkbox" disabled={busy}
                      checked={groupSelected} onChange={() => toggleGroup(ids)} />
                    <Folder size={16} className="codex-session-folder__icon" />
                    <button className="codex-session-folder__label" type="button" title={group.cwd}
                      onClick={() => setExpandedGroups(previous => expanded ? previous.filter(cwd => cwd !== group.cwd)
                        : [...previous, group.cwd])}>
                      {resolveGroupLabel(group.cwd) || t('common.unknown', '未知')}
                    </button>
                  </div>
                  <span className="codex-session-folder__time">{formatRelativeTime(group.latestDeletedAt, i18n.language.startsWith('zh'))}</span>
                </div>
                {expanded && <div className="codex-session-folder__children">
                  {group.sessions.map(row => <TrashBranch key={row.id} row={row} selected={selectedSet.has(row.id)}
                    disabled={busy} onToggle={toggle} onPurge={id => void purge([id])} />)}
                </div>}
              </section>;
            })}
          </div></div>
        </>}
      </div>
      <div className="modal-footer">
        <button className="btn btn-secondary" type="button" onClick={close} disabled={busy}>
          {t('common.cancel', '取消')}
        </button>
        <button className="btn btn-danger" type="button" disabled={busy || selectedIds.length === 0}
          onClick={() => void purge(selectedIds)}>
          <Trash2 size={14} className={purging && selectedIds.length > 0 ? 'icon-spin' : undefined} />
          {t('codex.sessionManager.restoreModal.deleteSelected', '永久删除选中')} ({selectedIds.length})
        </button>
        <button className="btn btn-primary" type="button" disabled={busy || selectedIds.length === 0}
          onClick={() => void restore()}>
          <RotateCcw size={14} className={restoring ? 'icon-spin' : undefined} />
          {t('codex.sessionManager.restoreModal.restoreAction', '恢复选中会话')} ({selectedIds.length})
        </button>
      </div>
    </div>
  </div>;
}
