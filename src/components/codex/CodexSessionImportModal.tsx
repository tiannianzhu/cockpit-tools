import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { RefreshCw, Upload, X } from 'lucide-react';
import { ModalErrorMessage, useModalErrorState } from '../ModalErrorMessage';
import { SingleSelectDropdown, type SingleSelectOption } from '../SingleSelectDropdown';
import { useEscClose } from '../../hooks/useEscClose';
import type { CodexSessionImportPreview, CodexSessionImportPreviewItem } from '../../types/codex';

export interface CodexSessionImportSource {
  preview: (path: string, targetId?: string) => Promise<CodexSessionImportPreview>;
  import: (path: string, ids: string[], targetId?: string) => Promise<{ message: string }>;
}

interface Props {
  open: boolean;
  filePath: string;
  source: CodexSessionImportSource;
  onClose: () => void;
  targetOptions?: SingleSelectOption[];
  defaultTargetId?: string;
  requireTarget?: boolean;
  onTargetChange?: (targetId: string) => void;
  onImportStart?: (ids: string[], path: string, targetId?: string) => void;
  onMessage?: (message: string) => void;
  onError?: (message: string) => void;
  onChanged?: () => void | Promise<void>;
}

function formatBytes(value: number): string {
  if (value >= 1024 ** 3) return `${(value / 1024 ** 3).toFixed(1)} GB`;
  if (value >= 1024 ** 2) return `${(value / 1024 ** 2).toFixed(1)} MB`;
  if (value >= 1024) return `${(value / 1024).toFixed(1)} KB`;
  return `${value} B`;
}

export function CodexSessionImportModal({ open, filePath, source, onClose, targetOptions = [], defaultTargetId = '',
  requireTarget = false, onTargetChange, onImportStart, onMessage, onError, onChanged }: Props) {
  const { t } = useTranslation();
  const [importPreview, setImportPreview] = useState<CodexSessionImportPreview | null>(null);
  const [importTargetInstanceId, setImportTargetInstanceId] = useState(defaultTargetId);
  const [selectedImportIds, setSelectedImportIds] = useState<string[]>([]);
  const [loadingImportPreview, setLoadingImportPreview] = useState(false);
  const [importing, setImporting] = useState(false);
  const { message: importModalError, scrollKey: importModalErrorScrollKey, set: setImportModalError } = useModalErrorState();
  const selectedImportIdSet = useMemo(() => new Set(selectedImportIds), [selectedImportIds]);
  const importReadyItems = useMemo(() => importPreview?.items.filter(item => item.status === 'ready') ?? [],
    [importPreview]);
  const allImportReadySelected = importReadyItems.length > 0
    && importReadyItems.every(item => selectedImportIdSet.has(item.sessionId));
  const effectiveTargetId = importTargetInstanceId || defaultTargetId;

  useEffect(() => {
    if (!open) return;
    setImportTargetInstanceId(defaultTargetId);
  }, [open, defaultTargetId]);

  useEffect(() => {
    if (!open || !filePath) return;
    if (requireTarget && !effectiveTargetId) {
      setImportModalError(t('codex.sessionManager.importModal.noTarget', '未发现可导入的 Codex 实例'));
      return;
    }
    let active = true;
    setImportPreview(null);
    setSelectedImportIds([]);
    setImportModalError(null);
    setLoadingImportPreview(true);
    void source.preview(filePath, effectiveTargetId || undefined).then(preview => {
      if (!active) return;
      setImportPreview(preview);
      setSelectedImportIds(preview.items.filter(item => item.status === 'ready').map(item => item.sessionId));
    }).catch(error => {
      if (active) setImportModalError(String(error));
    }).finally(() => {
      if (active) setLoadingImportPreview(false);
    });
    return () => { active = false; };
  }, [open, filePath, effectiveTargetId, requireTarget, source, setImportModalError, t]);

  const handleCloseImportModal = useCallback(() => {
    if (importing || loadingImportPreview) return;
    onClose();
  }, [importing, loadingImportPreview, onClose]);
  useEscClose(open, handleCloseImportModal);
  const handleChangeImportTarget = (targetId: string) => {
    setImportTargetInstanceId(targetId);
    onTargetChange?.(targetId);
  };
  const toggleImportSession = (item: CodexSessionImportPreviewItem) => {
    if (item.status !== 'ready') return;
    setSelectedImportIds(previous => previous.includes(item.sessionId)
      ? previous.filter(id => id !== item.sessionId) : [...previous, item.sessionId]);
  };
  const toggleAllImportReady = () => {
    if (!importReadyItems.length) return;
    setSelectedImportIds(previous => {
      const next = new Set(previous);
      if (importReadyItems.every(item => next.has(item.sessionId))) {
        importReadyItems.forEach(item => next.delete(item.sessionId));
      } else {
        importReadyItems.forEach(item => next.add(item.sessionId));
      }
      return [...next];
    });
  };
  const getImportStatusLabel = (item: CodexSessionImportPreviewItem): string => {
    if (item.status === 'ready') return t('codex.sessionManager.importModal.statusReady', '可导入');
    if (item.status === 'duplicate') return t('codex.sessionManager.importModal.statusDuplicate', '已存在');
    if (item.status === 'conflict') return t('codex.sessionManager.importModal.statusConflict', '冲突');
    return t('codex.sessionManager.importModal.statusInvalid', '无效');
  };
  const handleImportSelectedSessions = async () => {
    if (!importPreview) {
      setImportModalError(t('codex.sessionManager.importModal.noPackage', '请先选择会话包'));
      return;
    }
    if (requireTarget && !effectiveTargetId) {
      setImportModalError(t('codex.sessionManager.targetModal.pickTarget', '请选择目标实例'));
      return;
    }
    if (!selectedImportIds.length) {
      setImportModalError(t('codex.sessionManager.importModal.pickOne', '请至少选择一条可导入会话'));
      return;
    }
    setImporting(true);
    setImportModalError(null);
    try {
      const ids = [...selectedImportIds];
      onImportStart?.(ids, filePath, effectiveTargetId || undefined);
      const result = await source.import(filePath, ids, effectiveTargetId || undefined);
      onMessage?.(result.message);
      await onChanged?.();
      onClose();
    } catch (error) {
      await Promise.resolve().then(() => onChanged?.()).catch(() => undefined);
      setImportModalError(String(error));
      onError?.(String(error));
    } finally {
      setImporting(false);
    }
  };

  if (!open) return null;
  return (
        <div className="modal-overlay">
          <div className="modal codex-session-import-modal" onClick={(event) => event.stopPropagation()}>
            <div className="modal-header">
              <h2>{t('codex.sessionManager.importModal.title', '导入会话')}</h2>
              <button
                className="modal-close"
                type="button"
                onClick={handleCloseImportModal}
                disabled={importing || loadingImportPreview}
                aria-label={t('common.close', '关闭')}
              >
                <X size={18} />
              </button>
            </div>
            <div className="modal-body">
              <ModalErrorMessage message={importModalError} scrollKey={importModalErrorScrollKey} />
              <p className="codex-session-import-modal__hint">
                {t(
                  'codex.sessionManager.importModal.hint',
                  '会话包只导入 rollout 文件和 session_index 条目，不包含账号、Token、API Key 或应用配置；目标实例已有同 ID 会话时会跳过。',
                )}
              </p>
              {requireTarget ? <label className="codex-session-target-modal__field">
                <span>{t('codex.sessionManager.targetModal.targetInstance', '目标实例')}</span>
                <SingleSelectDropdown
                  className="codex-session-target-modal__select"
                  value={importTargetInstanceId}
                  options={targetOptions}
                  onChange={(value) => void handleChangeImportTarget(value)}
                  disabled={importing || loadingImportPreview}
                  ariaLabel={t('codex.sessionManager.targetModal.targetInstance', '目标实例')}
                  menuMaxHeight={240}
                />
              </label> : null}
              {importPreview ? (
                <div className="codex-session-import-modal__summary">
                  <span>
                    {t('codex.sessionManager.importModal.totalCount', {
                      defaultValue: '会话包 {{count}} 条',
                      count: importPreview.totalSessionCount,
                    })}
                  </span>
                  <span>
                    {t('codex.sessionManager.importModal.readyCount', {
                      defaultValue: '可导入 {{count}} 条',
                      count: importPreview.importableSessionCount,
                    })}
                  </span>
                  <button
                    className="btn btn-secondary codex-session-import-modal__select-all"
                    type="button"
                    onClick={toggleAllImportReady}
                    disabled={importing || loadingImportPreview || importReadyItems.length === 0}
                  >
                    {allImportReadySelected
                      ? t('codex.sessionManager.actions.clearSelectedSessions', '取消全选')
                      : t('codex.sessionManager.importModal.selectReady', '选择可导入')}
                  </button>
                </div>
              ) : null}
              {loadingImportPreview ? (
                <div className="codex-session-restore-modal__empty">
                  <RefreshCw size={28} className="icon-spin empty-icon" />
                  <h3>{t('codex.sessionManager.importModal.previewing', '正在预览会话包...')}</h3>
                </div>
              ) : null}
              {!loadingImportPreview && importPreview ? (
                <div className="codex-session-import-list">
                  {importPreview.items.map((item) => (
                    <label
                      className={`codex-session-import-row is-${item.status}`}
                      key={item.sessionId}
                    >
                      <div className="codex-session-import-row__left">
                        <input
                          className="codex-session-row__checkbox"
                          type="checkbox"
                          checked={selectedImportIdSet.has(item.sessionId)}
                          disabled={item.status !== 'ready' || importing}
                          onChange={() => toggleImportSession(item)}
                        />
                        <div className="codex-session-import-row__content">
                          <span className="codex-session-import-row__title" title={item.title}>
                            {item.title || t('codex.sessionManager.untitled', '未命名会话')}
                          </span>
                          <span className="codex-session-import-row__meta" title={item.cwd}>
                            {item.cwd}
                          </span>
                          {item.existingInstanceNames.length > 0 ? (
                            <span className="codex-session-import-row__meta">
                              {t('codex.sessionManager.importModal.existsIn', {
                                defaultValue: '已存在于：{{names}}',
                                names: item.existingInstanceNames.join(' / '),
                              })}
                            </span>
                          ) : null}
                          {item.reason ? (
                            <span className="codex-session-import-row__reason">{item.reason}</span>
                          ) : null}
                        </div>
                      </div>
                      <div className="codex-session-import-row__right">
                        <span className={`codex-session-import-row__status is-${item.status}`}>
                          {getImportStatusLabel(item)}
                        </span>
                        <span className="codex-session-import-row__size">{formatBytes(item.sizeBytes)}</span>
                      </div>
                    </label>
                  ))}
                </div>
              ) : null}
            </div>
            <div className="modal-footer">
              <button
                className="btn btn-secondary"
                type="button"
                onClick={handleCloseImportModal}
                disabled={importing || loadingImportPreview}
              >
                {t('common.cancel', '取消')}
              </button>
              <button
                className="btn btn-primary"
                type="button"
                onClick={() => void handleImportSelectedSessions()}
                disabled={importing || loadingImportPreview || selectedImportIds.length === 0 || (requireTarget && !importTargetInstanceId)}
              >
                <Upload size={14} className={importing ? 'icon-spin' : undefined} />
                {t('codex.sessionManager.importModal.confirm', '导入选中会话')} ({selectedImportIds.length})
              </button>
            </div>
          </div>
        </div>
  );
}
