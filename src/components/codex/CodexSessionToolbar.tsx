import type { ReactNode } from 'react';
import { Check, Download, RefreshCw, Trash2, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';

interface Props {
  selectedCount: number;
  countCapacity: number;
  allSelected: boolean;
  hasSessions: boolean;
  busy: boolean;
  loading: boolean;
  onToggleAll: () => void;
  onExport: () => void;
  onTrash: () => void;
  onOpenTrash: () => void;
  onRefresh: () => void;
  selectionExtras?: ReactNode;
  maintenanceExtras?: ReactNode;
}

/** Shared session actions; host-specific capabilities occupy explicit slots. */
export function CodexSessionToolbar({ selectedCount, countCapacity, allSelected, hasSessions, busy, loading,
  onToggleAll, onExport, onTrash, onOpenTrash, onRefresh, selectionExtras, maintenanceExtras }: Props) {
  const { t } = useTranslation();
  const disabled = busy || loading;
  const selectionDisabled = disabled || selectedCount === 0;
  const countStyle = { minWidth: `${String(countCapacity).length + 2}ch` };
  return <div className="codex-session-manager__actions">
    <div className="codex-session-manager__action-group is-maintenance">
      {maintenanceExtras}
      <button type="button" className="btn btn-secondary codex-session-manager__action-button"
        disabled={disabled} onClick={onOpenTrash}>
        <Trash2 size={14} />{t('codex.sessionManager.actions.trash', '废纸篓')}
      </button>
      <button type="button" className="btn btn-secondary codex-session-manager__action-button"
        disabled={disabled} onClick={onRefresh}>
        <RefreshCw size={14} className={loading ? 'icon-spin' : undefined} />{t('common.refresh', '刷新')}
      </button>
    </div>
    {hasSessions && (
      <div className="codex-session-manager__action-group is-selection">
        <button type="button" className="btn btn-secondary codex-session-manager__action-button"
          disabled={disabled || !hasSessions} onClick={onToggleAll}
          title={allSelected ? t('codex.sessionManager.actions.clearSelectedSessions', '取消全选')
            : t('codex.sessionManager.actions.selectAllSessions', '全选全部会话')}
          aria-label={allSelected ? t('codex.sessionManager.actions.clearSelectedSessions', '取消全选')
            : t('codex.sessionManager.actions.selectAllSessions', '全选全部会话')}>
          {allSelected ? <X size={14} /> : <Check size={14} />}
          <span className="codex-session-manager__selection-toggle-label" aria-hidden="true">
            <span style={{ visibility: allSelected ? 'hidden' : 'visible' }}>
              {t('codex.sessionManager.actions.selectAllSessions', '全选全部会话')}
            </span>
            <span style={{ visibility: allSelected ? 'visible' : 'hidden' }}>
              {t('codex.sessionManager.actions.clearSelectedSessions', '取消全选')}
            </span>
          </span>
        </button>
        {selectionExtras}
        <button type="button" className="btn btn-secondary codex-session-manager__action-button"
          disabled={selectionDisabled} onClick={onExport}>
          <Download size={14} />{t('codex.sessionManager.actions.exportSessions', '导出会话')}
          <span className="codex-session-manager__action-count" style={countStyle}>({selectedCount})</span>
        </button>
        <button type="button" className="btn btn-danger codex-session-manager__action-button"
          disabled={selectionDisabled} onClick={onTrash}>
          <Trash2 size={14} />{t('codex.sessionManager.actions.moveToTrash', '移到废纸篓')}
          <span className="codex-session-manager__action-count" style={countStyle}>({selectedCount})</span>
        </button>
      </div>
    )}
  </div>;
}
