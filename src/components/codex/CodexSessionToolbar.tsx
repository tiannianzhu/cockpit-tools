import type { ReactNode } from 'react';
import { Check, Download, RefreshCw, Trash2, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';

interface Props {
  selectedCount: number;
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
export function CodexSessionToolbar({ selectedCount, allSelected, hasSessions, busy, loading,
  onToggleAll, onExport, onTrash, onOpenTrash, onRefresh, selectionExtras, maintenanceExtras }: Props) {
  const { t } = useTranslation();
  const disabled = busy || loading;
  const selectionDisabled = disabled || selectedCount === 0;
  return <div className="codex-session-manager__actions">
    <div className="codex-session-manager__action-group is-selection">
      <button type="button" className="btn btn-secondary codex-session-manager__action-button"
        disabled={disabled || !hasSessions} onClick={onToggleAll}>
        {allSelected ? <X size={14} /> : <Check size={14} />}
        {allSelected ? t('codex.sessionManager.actions.clearSelectedSessions', '取消全选')
          : t('codex.sessionManager.actions.selectAllSessions', '全选全部会话')}
      </button>
      {selectionExtras}
      <button type="button" className="btn btn-secondary codex-session-manager__action-button"
        disabled={selectionDisabled} onClick={onExport}>
        <Download size={14} />{t('codex.sessionManager.actions.exportSessions', '导出会话')} ({selectedCount})
      </button>
      <button type="button" className="btn btn-danger codex-session-manager__action-button"
        disabled={selectionDisabled} onClick={onTrash}>
        <Trash2 size={14} />{t('codex.sessionManager.actions.moveToTrash', '移到废纸篓')} ({selectedCount})
      </button>
    </div>
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
  </div>;
}
