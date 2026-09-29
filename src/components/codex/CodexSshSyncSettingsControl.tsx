import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { ArrowUpRight, Monitor } from 'lucide-react';
import { useSshServerStore } from '../../stores/useSshServerStore';
import { requestCodexHosts } from '../../utils/codexHostNavigation';

interface CodexSshSyncSettingsControlProps {
  variant?: 'quick' | 'settings';
  onNavigate?: () => void;
}

/** Settings summarize synchronization; the independent host page owns its controls. */
export function CodexSshSyncSettingsControl({ variant = 'settings', onNavigate }: CodexSshSyncSettingsControlProps) {
  const { t } = useTranslation();
  const servers = useSshServerStore((state) => state.servers);
  const selectedServerIds = useSshServerStore((state) => state.selectedServerIds);
  const fetchServers = useSshServerStore((state) => state.fetchServers);
  useEffect(() => { void fetchServers(); }, [fetchServers]);
  const count = servers.filter((server) => selectedServerIds.includes(server.id)).length;
  const hint = count > 0
    ? t('codex.hosts.syncSummary', '{{count}} 台主机已选择跟随本地账户', { count })
    : t('codex.hosts.settingsHint', '在远程主机页面管理连接、账户和自动同步。');
  const openHosts = () => {
    onNavigate?.();
    requestCodexHosts();
  };
  const button = (
    <button type="button" className={variant === 'quick' ? 'qs-btn' : 'btn btn-secondary'} onClick={openHosts}>
      <span>{t('codex.hosts.openPage', '打开远程主机')}</span><ArrowUpRight size={14} />
    </button>
  );
  if (variant === 'quick') return (
    <>
      <div className="qs-row">
        <div className="qs-row-label"><Monitor size={15} /><span>{t('codex.hosts.title', '远程主机')}</span></div>
        <div className="qs-row-control">{button}</div>
      </div>
      <div className="qs-hint">{hint}</div>
    </>
  );
  return (
    <div className="settings-row">
      <div className="row-label">
        <div className="row-title">{t('codex.hosts.title', '远程主机')}</div>
        <div className="row-desc">{hint}</div>
      </div>
      <div className="row-control">{button}</div>
    </div>
  );
}
