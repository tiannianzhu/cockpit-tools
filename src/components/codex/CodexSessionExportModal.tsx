import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { save as saveFileDialog } from '@tauri-apps/plugin-dialog';
import { Check, Download, Folder, FolderOpen, RefreshCw, RotateCcw, Search, Trash2, X } from 'lucide-react';
import { ModalErrorMessage, useModalErrorState } from '../ModalErrorMessage';
import { SingleSelectDropdown, type SingleSelectOption } from '../SingleSelectDropdown';
import { useEscClose } from '../../hooks/useEscClose';
import { formatRelativeTime } from '../../utils/codexSessionPresentation';
import type { CodexSessionExportPreview, CodexSessionExportPreviewItem } from '../../types/codex';

type ExportSelectionFilter = 'all' | 'selected' | 'unselected';

export interface CodexSessionExportSource {
  preview: (ids: string[]) => Promise<CodexSessionExportPreview>;
  export: (ids: string[], path: string) => Promise<{ message: string }>;
}

interface Props {
  open: boolean;
  sessionIds: string[];
  source: CodexSessionExportSource;
  onClose: () => void;
  onMessage?: (message: string) => void;
  onError?: (message: string) => void;
  onExportStart?: (ids: string[], path: string) => void;
}

function formatBytes(value: number): string {
  if (value >= 1024 ** 3) return `${(value / 1024 ** 3).toFixed(1)} GB`;
  if (value >= 1024 ** 2) return `${(value / 1024 ** 2).toFixed(1)} MB`;
  if (value >= 1024) return `${(value / 1024).toFixed(1)} KB`;
  return `${value} B`;
}

function buildDefaultSessionExportName(): string {
  const now = new Date();
  const pad = (value: number) => String(value).padStart(2, '0');
  return `codex-sessions-${[now.getFullYear(), pad(now.getMonth() + 1), pad(now.getDate()), '-',
    pad(now.getHours()), pad(now.getMinutes()), pad(now.getSeconds())].join('')}.zip`;
}

export function CodexSessionExportModal({ open, sessionIds, source, onClose, onMessage, onError, onExportStart }: Props) {
  const { t, i18n } = useTranslation();
  const isZh = i18n.resolvedLanguage?.toLowerCase().startsWith('zh') ?? true;
  const [exportPreview, setExportPreview] = useState<CodexSessionExportPreview | null>(null);
  const [exportPath, setExportPath] = useState('');
  const [selectedExportIds, setSelectedExportIds] = useState<string[]>([]);
  const [removedExportIds, setRemovedExportIds] = useState<string[]>([]);
  const [exportSourceFilter, setExportSourceFilter] = useState('all');
  const [exportSelectionFilter, setExportSelectionFilter] = useState<ExportSelectionFilter>('all');
  const [loadingExportPreview, setLoadingExportPreview] = useState(false);
  const [exporting, setExporting] = useState(false);
  const { message: exportModalError, scrollKey: exportModalErrorScrollKey, set: setExportModalError } = useModalErrorState();
  const selectedExportIdSet = useMemo(() => new Set(selectedExportIds), [selectedExportIds]);
  const removedExportIdSet = useMemo(() => new Set(removedExportIds), [removedExportIds]);
  const exportPreviewItems = useMemo(() => exportPreview?.items ?? [], [exportPreview]);
  const exportAvailableItems = useMemo(() => exportPreviewItems.filter(item => !removedExportIdSet.has(item.sessionId)),
    [exportPreviewItems, removedExportIdSet]);
  const exportSelectedItems = useMemo(() => exportAvailableItems.filter(item => selectedExportIdSet.has(item.sessionId)),
    [exportAvailableItems, selectedExportIdSet]);
  const exportableSessionIds = useMemo(() => exportSelectedItems.map(item => item.sessionId), [exportSelectedItems]);
  const exportSelectedSizeBytes = useMemo(() => exportSelectedItems.reduce((sum, item) => sum + item.sizeBytes, 0),
    [exportSelectedItems]);
  const exportAvailableSizeBytes = useMemo(() => exportAvailableItems.reduce((sum, item) => sum + item.sizeBytes, 0),
    [exportAvailableItems]);
  const exportSourceOptions = useMemo<SingleSelectOption[]>(() => {
    const sources = new Map<string, { label: string; count: number }>();
    exportAvailableItems.forEach(item => {
      const current = sources.get(item.sourceInstanceId);
      sources.set(item.sourceInstanceId, { label: item.sourceInstanceName, count: (current?.count ?? 0) + 1 });
    });
    return [{ value: 'all', label: t('codex.sessionManager.exportModal.allSources', '全部来源') },
      ...[...sources].map(([value, option]) => ({ value, label: `${option.label} (${option.count})` }))];
  }, [exportAvailableItems, t]);
  const exportSelectionFilterOptions = useMemo<SingleSelectOption[]>(() => [
    { value: 'all', label: t('codex.sessionManager.exportModal.filterAll', '全部') },
    { value: 'selected', label: t('codex.sessionManager.exportModal.filterSelected', '已选') },
    { value: 'unselected', label: t('codex.sessionManager.exportModal.filterUnselected', '未选') },
  ], [t]);
  const exportFilteredItems = useMemo(() => exportAvailableItems.filter(item => {
    if (exportSourceFilter !== 'all' && item.sourceInstanceId !== exportSourceFilter) return false;
    const selected = selectedExportIdSet.has(item.sessionId);
    if (exportSelectionFilter === 'selected' && !selected) return false;
    if (exportSelectionFilter === 'unselected' && selected) return false;
    return true;
  }), [exportAvailableItems, exportSourceFilter, exportSelectionFilter, selectedExportIdSet]);

  useEffect(() => {
    if (!open) return;
    let active = true;
    setExportPreview(null);
    setExportPath('');
    setSelectedExportIds([]);
    setRemovedExportIds([]);
    setExportSourceFilter('all');
    setExportSelectionFilter('all');
    setExportModalError(null);
    setLoadingExportPreview(true);
    void source.preview(sessionIds).then(preview => {
      if (!active) return;
      setExportPreview(preview);
      setSelectedExportIds(preview.items.map(item => item.sessionId));
    }).catch(error => {
      if (active) setExportModalError(String(error));
    }).finally(() => {
      if (active) setLoadingExportPreview(false);
    });
    return () => { active = false; };
  }, [open, sessionIds, source, setExportModalError]);

  useEffect(() => {
    if (exportSourceFilter !== 'all' && !exportSourceOptions.some(option => option.value === exportSourceFilter)) {
      setExportSourceFilter('all');
    }
  }, [exportSourceFilter, exportSourceOptions]);

  const handleCloseExportModal = useCallback(() => {
    if (loadingExportPreview || exporting) return;
    onClose();
  }, [loadingExportPreview, exporting, onClose]);
  useEscClose(open, handleCloseExportModal);

  const addExportSelection = useCallback((items: CodexSessionExportPreviewItem[]) => {
    const ids = items.map(item => item.sessionId);
    setSelectedExportIds(previous => [...new Set([...previous, ...ids])]);
  }, []);
  const removeExportSelection = useCallback((items: CodexSessionExportPreviewItem[]) => {
    const ids = new Set(items.map(item => item.sessionId));
    setSelectedExportIds(previous => previous.filter(id => !ids.has(id)));
  }, []);
  const removeExportItems = useCallback((items: CodexSessionExportPreviewItem[]) => {
    const ids = items.map(item => item.sessionId);
    const removed = new Set(ids);
    setRemovedExportIds(previous => [...new Set([...previous, ...ids])]);
    setSelectedExportIds(previous => previous.filter(id => !removed.has(id)));
  }, []);
  const toggleExportSession = useCallback((id: string) => setSelectedExportIds(previous => previous.includes(id)
    ? previous.filter(value => value !== id) : [...previous, id]), []);
  const keepFilteredExportItems = useCallback(() => {
    const visible = new Set(exportFilteredItems.map(item => item.sessionId));
    setRemovedExportIds(previous => [...new Set([...previous, ...exportAvailableItems
      .filter(item => !visible.has(item.sessionId)).map(item => item.sessionId)])]);
    setSelectedExportIds(exportFilteredItems.map(item => item.sessionId));
  }, [exportAvailableItems, exportFilteredItems]);
  const removeUncheckedExportItems = useCallback(() => {
    removeExportItems(exportAvailableItems.filter(item => !selectedExportIdSet.has(item.sessionId)));
  }, [exportAvailableItems, removeExportItems, selectedExportIdSet]);
  const restoreRemovedExportItems = useCallback(() => setRemovedExportIds([]), []);
  const handleChooseExportPath = async () => {
    setExportModalError(null);
    try {
      const selected = await saveFileDialog({ defaultPath: buildDefaultSessionExportName(),
        filters: [{ name: 'ZIP', extensions: ['zip'] }] });
      if (selected) setExportPath(selected);
    } catch (error) {
      setExportModalError(String(error));
    }
  };
  const handleConfirmExportSessions = async () => {
    if (!exportPreview) {
      setExportModalError(t('codex.sessionManager.exportModal.noPreview', '请先完成导出预览'));
      return;
    }
    if (exportableSessionIds.length === 0) {
      setExportModalError(t('codex.sessionManager.exportModal.noExportable', '没有可导出的会话'));
      return;
    }
    if (!exportPath) {
      setExportModalError(t('codex.sessionManager.exportModal.pickPath', '请选择导出位置'));
      return;
    }
    setExporting(true);
    setExportModalError(null);
    try {
      const ids = [...exportableSessionIds];
      onExportStart?.(ids, exportPath);
      const summary = await source.export(ids, exportPath);
      onMessage?.(summary.message);
      onClose();
    } catch (error) {
      setExportModalError(String(error));
      onError?.(String(error));
    } finally {
      setExporting(false);
    }
  };

  if (!open) return null;
  return (
        <div className="modal-overlay">
          <div className="modal codex-session-export-modal" onClick={(event) => event.stopPropagation()}>
            <div className="modal-header">
              <h2>{t('codex.sessionManager.exportModal.title', '导出会话')}</h2>
              <button
                className="modal-close"
                type="button"
                onClick={handleCloseExportModal}
                disabled={loadingExportPreview || exporting}
                aria-label={t('common.close', '关闭')}
              >
                <X size={18} />
              </button>
            </div>
            <div className="modal-body">
              <ModalErrorMessage message={exportModalError} scrollKey={exportModalErrorScrollKey} />
              <p className="codex-session-export-modal__hint">
                {t(
                  'codex.sessionManager.exportModal.hint',
                  '导出前可先确认会话列表、来源实例和文件大小；会话包只包含 rollout 文件和 session_index 条目，不包含账号、Token、API Key 或应用配置。',
                )}
              </p>
              {loadingExportPreview ? (
                <div className="codex-session-restore-modal__empty">
                  <RefreshCw size={28} className="icon-spin empty-icon" />
                  <h3>{t('codex.sessionManager.exportModal.previewing', '正在预览会话...')}</h3>
                </div>
              ) : null}
              {!loadingExportPreview && exportPreview ? (
                <>
                  <div className="codex-session-export-modal__summary">
                    <span>
                      {t('codex.sessionManager.exportModal.selectedExportCount', {
                        defaultValue: '已选 {{selected}} / 可导出 {{total}}',
                        selected: exportableSessionIds.length,
                        total: exportAvailableItems.length,
                      })}
                    </span>
                    <span>
                      {t('codex.sessionManager.exportModal.selectedSize', {
                        defaultValue: '已选大小 {{selected}} / 总计 {{total}}',
                        selected: formatBytes(exportSelectedSizeBytes),
                        total: formatBytes(exportAvailableSizeBytes),
                      })}
                    </span>
                    <span>
                      {t('codex.sessionManager.exportModal.missingCount', {
                        defaultValue: '缺失 {{count}} 条',
                        count: exportPreview.missingSessionCount,
                      })}
                    </span>
                  </div>
                  {removedExportIds.length > 0 ? (
                    <div className="codex-session-export-modal__notice is-neutral">
                      <span>
                        {t('codex.sessionManager.exportModal.removedNotice', {
                          defaultValue: '已从本次导出列表移出 {{count}} 条',
                          count: removedExportIds.length,
                        })}
                      </span>
                      <button
                        className="btn btn-secondary codex-session-export-modal__notice-action"
                        type="button"
                        onClick={restoreRemovedExportItems}
                        disabled={exporting}
                      >
                        <RotateCcw size={13} />
                        {t('codex.sessionManager.exportModal.restoreRemoved', '恢复列表')}
                      </button>
                    </div>
                  ) : null}
                  <div className="codex-session-export-modal__path">
                    <div className="codex-session-export-modal__path-copy">
                      <strong>{t('codex.sessionManager.exportModal.exportPath', '导出位置')}</strong>
                      <span title={exportPath}>
                        {exportPath || t('codex.sessionManager.exportModal.pathUnset', '尚未选择导出位置')}
                      </span>
                    </div>
                    <button
                      className="btn btn-secondary"
                      type="button"
                      onClick={() => void handleChooseExportPath()}
                      disabled={exporting}
                    >
                      <FolderOpen size={14} />
                      {t('codex.sessionManager.exportModal.choosePath', '选择位置')}
                    </button>
                  </div>
                  {exportPreview.missingSessionCount > 0 ? (
                    <div className="codex-session-export-modal__notice">
                      {t('codex.sessionManager.exportModal.missingNotice', {
                        defaultValue: '{{count}} 条会话已不在当前实例集合中，导出时会跳过。',
                        count: exportPreview.missingSessionCount,
                      })}
                    </div>
                  ) : null}
                  {exportAvailableItems.length > 0 ? (
                    <>
                      <div className="codex-session-export-filter">
                        <label className="codex-session-export-filter__field">
                          <span>{t('codex.sessionManager.exportModal.sourceLabel', '来源')}</span>
                          <SingleSelectDropdown
                            className="codex-session-export-filter__select"
                            value={exportSourceFilter}
                            options={exportSourceOptions}
                            onChange={setExportSourceFilter}
                            disabled={exporting}
                            ariaLabel={t('codex.sessionManager.exportModal.sourceLabel', '来源')}
                            menuMaxHeight={240}
                          />
                        </label>
                        <label className="codex-session-export-filter__field">
                          <span>{t('codex.sessionManager.exportModal.selectionLabel', '选择')}</span>
                          <SingleSelectDropdown
                            className="codex-session-export-filter__select"
                            value={exportSelectionFilter}
                            options={exportSelectionFilterOptions}
                            onChange={(value) => setExportSelectionFilter(value as ExportSelectionFilter)}
                            disabled={exporting}
                            ariaLabel={t('codex.sessionManager.exportModal.selectionLabel', '选择')}
                            menuMaxHeight={220}
                          />
                        </label>
                      </div>
                      <div className="codex-session-export-actions">
                        <span>
                          {t('codex.sessionManager.exportModal.visibleCount', {
                            defaultValue: '当前显示 {{visible}} / {{total}}',
                            visible: exportFilteredItems.length,
                            total: exportAvailableItems.length,
                          })}
                        </span>
                        <button
                          className="btn btn-secondary"
                          type="button"
                          onClick={() => addExportSelection(exportFilteredItems)}
                          disabled={exporting || exportFilteredItems.length === 0}
                        >
                          <Check size={13} />
                          {t('codex.sessionManager.exportModal.selectVisible', '选中筛选')}
                        </button>
                        <button
                          className="btn btn-secondary"
                          type="button"
                          onClick={() => removeExportSelection(exportFilteredItems)}
                          disabled={exporting || exportFilteredItems.length === 0}
                        >
                          <X size={13} />
                          {t('codex.sessionManager.exportModal.clearVisible', '取消筛选')}
                        </button>
                        <button
                          className="btn btn-secondary"
                          type="button"
                          onClick={keepFilteredExportItems}
                          disabled={exporting || exportFilteredItems.length === 0}
                        >
                          {t('codex.sessionManager.exportModal.keepVisibleOnly', '仅保留筛选')}
                        </button>
                        <button
                          className="btn btn-secondary"
                          type="button"
                          onClick={() => removeExportItems(exportFilteredItems)}
                          disabled={exporting || exportFilteredItems.length === 0}
                        >
                          <Trash2 size={13} />
                          {t('codex.sessionManager.exportModal.removeVisible', '移出筛选')}
                        </button>
                        <button
                          className="btn btn-secondary"
                          type="button"
                          onClick={removeUncheckedExportItems}
                          disabled={exporting || exportAvailableItems.length === exportableSessionIds.length}
                        >
                          {t('codex.sessionManager.exportModal.removeUnchecked', '移出未选')}
                        </button>
                      </div>
                      {exportFilteredItems.length > 0 ? (
                        <div className="codex-session-export-list">
                          {exportFilteredItems.map((item: CodexSessionExportPreviewItem) => {
                            const selected = selectedExportIdSet.has(item.sessionId);
                            return (
                              <div
                                className={`codex-session-export-row${selected ? ' is-selected' : ''}`}
                                key={item.sessionId}
                              >
                                <label className="codex-session-export-row__check">
                                  <input
                                    className="codex-session-row__checkbox"
                                    type="checkbox"
                                    checked={selected}
                                    disabled={exporting}
                                    onChange={() => toggleExportSession(item.sessionId)}
                                  />
                                  <span className="sr-only">
                                    {t('codex.sessionManager.exportModal.selectItem', '选择导出会话')}
                                  </span>
                                </label>
                                <div className="codex-session-export-row__content">
                                  <span className="codex-session-export-row__title" title={item.title}>
                                    {item.title || t('codex.sessionManager.untitled', '未命名会话')}
                                  </span>
                                  <span className="codex-session-export-row__meta" title={item.cwd}>
                                    {item.cwd}
                                  </span>
                                  <span className="codex-session-export-row__meta">
                                    {t('codex.sessionManager.exportModal.sourceInstance', {
                                      defaultValue: '来源：{{name}}',
                                      name: item.sourceInstanceName,
                                    })}
                                  </span>
                                </div>
                                <div className="codex-session-export-row__right">
                                  <span className="codex-session-import-row__size">{formatBytes(item.sizeBytes)}</span>
                                  <span className="codex-session-row__time">
                                    {formatRelativeTime(item.updatedAt, isZh)}
                                  </span>
                                  <button
                                    className="btn btn-secondary codex-session-export-row__remove"
                                    type="button"
                                    onClick={() => removeExportItems([item])}
                                    disabled={exporting}
                                  >
                                    <Trash2 size={13} />
                                    {t('codex.sessionManager.exportModal.removeItem', '移出')}
                                  </button>
                                </div>
                              </div>
                            );
                          })}
                        </div>
                      ) : (
                        <div className="codex-session-restore-modal__empty">
                          <Search size={32} className="empty-icon" />
                          <h3>{t('codex.sessionManager.exportModal.emptyFilteredTitle', '当前筛选无会话')}</h3>
                          <p>{t('codex.sessionManager.exportModal.emptyFilteredDesc', '调整搜索、来源或选择状态后再试。')}</p>
                        </div>
                      )}
                    </>
                  ) : (
                    <div className="codex-session-restore-modal__empty">
                      <Folder size={32} className="empty-icon" />
                      <h3>{t('codex.sessionManager.exportModal.emptyTitle', '没有可导出的会话')}</h3>
                    </div>
                  )}
                </>
              ) : null}
            </div>
            <div className="modal-footer">
              <button
                className="btn btn-secondary"
                type="button"
                onClick={handleCloseExportModal}
                disabled={loadingExportPreview || exporting}
              >
                {t('common.cancel', '取消')}
              </button>
              <button
                className="btn btn-primary"
                type="button"
                onClick={() => void handleConfirmExportSessions()}
                disabled={loadingExportPreview || exporting || !exportPath || exportableSessionIds.length === 0}
              >
                <Download size={14} className={exporting ? 'icon-spin' : undefined} />
                {t('codex.sessionManager.exportModal.confirm', '确认导出')} ({exportableSessionIds.length})
              </button>
            </div>
          </div>
        </div>
  );
}
