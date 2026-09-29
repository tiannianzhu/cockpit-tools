import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useTranslation } from 'react-i18next';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { confirm as confirmDialog } from '@tauri-apps/plugin-dialog';
import {
  Check, Pencil, Plus, PlugZap, RefreshCw, Server, Trash2, X, UserRound,
  MessageSquare, Link2,
} from 'lucide-react';
import { useEscCloseTopmost } from '../../hooks/useEscClose';
import { useModalFocusTrap } from '../../hooks/useModalFocusTrap';
import { useSshServerStore } from '../../stores/useSshServerStore';
import { useSshAccountInspectionStore } from '../../stores/useSshAccountInspectionStore';
import { useCodexAccountStore } from '../../stores/useCodexAccountStore';
import * as sshServerService from '../../services/sshServerService';
import { buildCodexAccountPresentation } from '../../presentation/platformAccountPresentation';
import { isCodexApiKeyAccount, type CodexAccount } from '../../types/codex';
import type {
  SshAuthConfig, SshCodexSyncResult, SshCodexSyncStatus, SshServer,
  SshServerDraft,
} from '../../types/sshServer';
import '../../styles/pages/codex-host-management.css';

interface FormState {
  id?: string;
  name: string;
  host: string;
  port: string;
  username: string;
  codexHome: string;
  authKind: 'agent' | 'private_key_file';
  privateKeyPath: string;
  syncOnSwitch: boolean;
}

const emptyForm: FormState = {
  name: '', host: '', port: '', username: '', codexHome: '',
  authKind: 'agent', privateKeyPath: '', syncOnSwitch: false,
};

function formFromServer(server: SshServer): FormState {
  return {
    id: server.id,
    name: server.name,
    host: server.host,
    port: server.port ? String(server.port) : '',
    username: server.username,
    codexHome: server.codex_home || '',
    authKind: server.auth.kind,
    privateKeyPath: server.auth.kind === 'private_key_file' ? server.auth.path : '',
    syncOnSwitch: server.sync_on_codex_switch,
  };
}

function draftFromForm(form: FormState): SshServerDraft {
  const auth: SshAuthConfig = form.authKind === 'private_key_file'
    ? { kind: 'private_key_file', path: form.privateKeyPath.trim() }
    : { kind: 'agent' };
  return {
    id: form.id,
    name: form.name.trim(),
    host: form.host.trim(),
    // Zero asks the backend to use the SSH config or default port.
    port: Number(form.port) || 0,
    username: form.username.trim(),
    codex_home: form.codexHome.trim(),
    auth,
    // Editing must preserve the host's existing auto-sync choice.
    sync_on_codex_switch: form.syncOnSwitch,
  };
}

function syncStatus(sync: SshCodexSyncStatus | null | undefined, t: ReturnType<typeof useTranslation>['t']) {
  if (!sync) return { kind: 'neutral' as const, text: t('codex.ssh.neverSynced', '尚未同步') };
  // Legacy statuses can have a default pending stage without an actual job.
  switch (sync.job_id ? sync.stage : undefined) {
    case 'pending': return { kind: 'progress' as const, text: t('codex.ssh.stagePending', '等待开始传输鉴权') };
    case 'transferring': return { kind: 'progress' as const, text: t('codex.ssh.stageTransferring', '正在传输账户配置') };
    case 'credentials_synced': return { kind: 'progress' as const, text: t('codex.ssh.stageCredentialsSynced', '账户配置已校验，准备重载') };
    case 'reloading': return { kind: 'progress' as const, text: t('codex.ssh.stageReloading', '正在停止旧的远端服务') };
    case 'applied': return { kind: 'applied' as const, text: t('codex.ssh.stageApplied', '配置已同步，旧服务已退出或未运行；桌面重新连接后加载新配置。') };
    case 'superseded': return { kind: 'neutral' as const, text: t('codex.ssh.stageSuperseded', '此同步已被较新的任务替代。') };
    case 'failed': return { kind: 'failed' as const, text: sync.error ?? t('codex.ssh.syncFailed', 'SSH 同步失败') };
    default: return sync.verified
      ? { kind: 'applied' as const, text: t('codex.ssh.legacyBytesVerified', 'auth.json 已传输并完成字节校验；Codex 会在下次连接时重新加载。') }
      : { kind: 'failed' as const, text: sync.error ?? t('codex.ssh.syncFailed', 'SSH 同步失败') };
  }
}

function isSyncInProgress(sync: SshCodexSyncStatus | null | undefined) {
  return Boolean(sync?.job_id && sync.stage && !['applied', 'failed', 'superseded'].includes(sync.stage));
}

function syncSummary(sync: SshCodexSyncStatus | null | undefined, t: ReturnType<typeof useTranslation>['t']) {
  if (!sync) return t('codex.hosts.syncNever', '未同步');
  if (!sync.job_id) return sync.verified ? t('codex.hosts.syncDone', '已同步') : t('codex.hosts.syncFailed', '失败');
  switch (sync.stage) {
    case 'pending': return t('codex.hosts.syncPending', '等待同步');
    case 'transferring': case 'credentials_synced': return t('codex.hosts.syncWorking', '正在同步');
    case 'reloading': return t('codex.hosts.syncReloading', '正在停止旧的远端服务');
    case 'applied': return t('codex.hosts.syncDone', '已同步');
    case 'failed': return t('codex.hosts.syncFailed', '失败');
    case 'superseded': return t('codex.hosts.syncSuperseded', '已被替代');
    default: return sync.verified ? t('codex.hosts.syncDone', '已同步') : t('codex.hosts.syncFailed', '失败');
  }
}

function accountMode(mode: string | null | undefined, t: ReturnType<typeof useTranslation>['t']) {
  switch ((mode ?? '').trim().toLowerCase()) {
    case 'oauth': case 'chatgpt': return t('codex.ssh.authModeOAuth', 'OAuth 账号');
    case 'apikey': case 'api_key': return t('codex.ssh.authModeApiKey', 'API 密钥');
    case 'agentidentity': case 'agent_identity': return t('codex.ssh.authModeAgentIdentity', '代理身份');
    case '': return t('codex.ssh.notSignedIn', '未登录');
    default: return t('codex.ssh.authModeOther', '其他登录方式');
  }
}

function providerName(account: CodexAccount): string | null {
  if (!isCodexApiKeyAccount(account)) return null;
  return account.api_provider_name?.trim() || account.api_provider_id?.trim() || null;
}

interface CodexSshServersPanelProps {
  onOpenSessions?: (serverId: string) => void;
}

export function CodexSshServersPanel({ onOpenSessions }: CodexSshServersPanelProps) {
  const { t } = useTranslation();
  const accounts = useCodexAccountStore((state) => state.accounts);
  const fetchAccounts = useCodexAccountStore((state) => state.fetchAccounts);
  const currentAccount = useCodexAccountStore((state) => state.currentAccount);
  const fetchCurrentAccount = useCodexAccountStore((state) => state.fetchCurrentAccount);
  const {
    servers, selectedServerIds, selectionLoading, syncResultsByServerId,
    loading, error, fetchServers, upsertServer, deleteServer,
    toggleServerSelection, testConnection, syncNow, applySyncResult,
  } = useSshServerStore();
  const [form, setForm] = useState<FormState | null>(null);
  const [accountServerId, setAccountServerId] = useState<string | null>(null);
  const [accountChoice, setAccountChoice] = useState('');
  const [accountSwitchError, setAccountSwitchError] = useState<string | null>(null);
  const [bulkServerIds, setBulkServerIds] = useState<Set<string>>(() => new Set());
  const [bulkOpen, setBulkOpen] = useState(false);
  const [bulkApplying, setBulkApplying] = useState(false);
  const bulkRunRef = useRef(false);
  const [bulkResults, setBulkResults] = useState<Record<string, { kind: 'success' | 'error'; text: string }>>({});
  const [saving, setSaving] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [busyServerIds, setBusyServerIds] = useState<Set<string>>(() => new Set());
  const { entries: inspections, inspect, retain } = useSshAccountInspectionStore();
  const [localMessage, setLocalMessage] = useState<{ kind: 'success' | 'warning' | 'error'; text: string } | null>(null);
  const mounted = useRef(true);
  const serverIdsRef = useRef(new Set<string>());
  const requestGeneration = useRef<Record<string, number>>({});
  const formRef = useRef<HTMLFormElement>(null);
  const accountRef = useRef<HTMLDivElement>(null);
  const bulkRef = useRef<HTMLDivElement>(null);
  const selectedIds = useMemo(() => new Set(selectedServerIds), [selectedServerIds]);
  const accountServer = servers.find((server) => server.id === accountServerId);
  const inspectedAccount = accountServer ? inspections[accountServer.id]?.account : undefined;
  const matchedLocalAccount = accounts.some((account) => account.id === inspectedAccount?.matched_account_id);
  const chosenAccount = accounts.find((account) => account.id === accountChoice);
  const bulkTargets = servers.filter((server) => bulkServerIds.has(server.id));
  const bulkUnavailable = bulkTargets.some((server) => busyServerIds.has(server.id)
    || inspections[server.id]?.reading
    || isSyncInProgress(syncResultsByServerId[server.id] ?? server.last_sync));

  useModalFocusTrap(formRef, Boolean(form));
  useModalFocusTrap(accountRef, Boolean(accountServer));
  useModalFocusTrap(bulkRef, bulkOpen);

  serverIdsRef.current = new Set(servers.map((server) => server.id));
  useEffect(() => {
    mounted.current = true;
    void fetchServers();
    void fetchAccounts();
    void fetchCurrentAccount();
    return () => {
      mounted.current = false;
      for (const id of Object.keys(requestGeneration.current)) requestGeneration.current[id] += 1;
    };
  }, [fetchServers, fetchAccounts, fetchCurrentAccount]);

  useEffect(() => {
    let disposed = false;
    let unlisten: UnlistenFn | null = null;
    listen<SshCodexSyncResult>('codex:ssh-sync-result', (event) => {
      if (!serverIdsRef.current.has(event.payload.server_id)) return;
      applySyncResult(event.payload);
    }).then((dispose) => {
      if (disposed) dispose();
      else unlisten = dispose;
    }).catch(() => {
      // The page remains usable if the native event listener is unavailable.
    });
    return () => { disposed = true; unlisten?.(); };
  }, [applySyncResult]);

  useEscCloseTopmost(Boolean(form || accountServerId || bulkOpen), () => {
    if (bulkOpen) { if (!bulkRunRef.current) setBulkOpen(false); }
    else if (!saving && !accountServerId) setForm(null);
    else if (!busyServerIds.has(accountServerId ?? '')) setAccountServerId(null);
  });

  const setBusy = useCallback((serverId: string, busy: boolean) => {
    setBusyServerIds((current) => {
      const next = new Set(current);
      if (busy) next.add(serverId);
      else next.delete(serverId);
      return next;
    });
  }, []);

  const handleSave = async () => {
    if (!form || saving) return;
    const port = form.port.trim();
    if (port && (!/^\d+$/.test(port) || Number(port) > 65535)) {
      setFormError(t('codex.hosts.invalidPort', '端口须为 0 到 65535 的整数'));
      return;
    }
    setSaving(true);
    setLocalMessage(null);
    setFormError(null);
    try {
      await upsertServer(draftFromForm(form));
      if (mounted.current) {
        setForm(null);
        setLocalMessage({ kind: 'success', text: t('common.saved', '已保存') });
      }
    } catch (err) {
      if (mounted.current) setFormError(String(err));
    } finally {
      if (mounted.current) setSaving(false);
    }
  };

  const handleSelection = async (serverId: string) => {
    setLocalMessage(null);
    try { await toggleServerSelection(serverId); }
    catch (err) { if (mounted.current) setLocalMessage({ kind: 'error', text: String(err) }); }
  };

  const handleTest = async (serverId: string) => {
    const generation = (requestGeneration.current[serverId] ?? 0) + 1;
    requestGeneration.current[serverId] = generation;
    setBusy(serverId, true);
    setLocalMessage(null);
    try {
      await testConnection(serverId);
      if (mounted.current && serverIdsRef.current.has(serverId) && requestGeneration.current[serverId] === generation) {
        setLocalMessage({ kind: 'success', text: t('codex.ssh.connectionOk', '连接成功') });
      }
    } catch (err) {
      if (mounted.current && serverIdsRef.current.has(serverId) && requestGeneration.current[serverId] === generation) {
        setLocalMessage({ kind: 'error', text: String(err) });
      }
    } finally {
      if (mounted.current && requestGeneration.current[serverId] === generation) setBusy(serverId, false);
    }
  };

  const handleInspect = useCallback((serverId: string, force = true) => {
    const state = useSshServerStore.getState();
    const server = state.servers.find((item) => item.id === serverId);
    return server ? inspect({ ...server, last_sync: state.syncResultsByServerId[serverId] ?? server.last_sync }, force)
      : Promise.resolve();
  }, [inspect]);

  // Reuse cached reads across visits. Only new/changed hosts and account syncs
  // invalidate them; explicit refresh bypasses the cache.
  useEffect(() => {
    retain(servers.map((server) => server.id));
    if (refreshing) return;
    for (const server of servers) {
      const sync = syncResultsByServerId[server.id] ?? server.last_sync;
      if (isSyncInProgress(sync) || busyServerIds.has(server.id)) continue;
      void handleInspect(server.id, false);
    }
  }, [servers, syncResultsByServerId, busyServerIds, handleInspect, retain, refreshing]);

  const handleRefresh = async () => {
    setRefreshing(true);
    setLocalMessage(null);
    try {
      await Promise.all([fetchServers(), fetchAccounts(), fetchCurrentAccount()]);
      if (!mounted.current || useSshServerStore.getState().error) return;
      await Promise.all(useSshServerStore.getState().servers.map((server) => handleInspect(server.id)));
    } finally { if (mounted.current) setRefreshing(false); }
  };

  const handleSync = async (serverId: string, accountId?: string) => {
    const generation = (requestGeneration.current[serverId] ?? 0) + 1;
    requestGeneration.current[serverId] = generation;
    setBusy(serverId, true);
    setLocalMessage(null);
    setAccountSwitchError(null);
    try {
      const result = accountId
        ? await sshServerService.switchSshServerAccount(serverId, accountId)
        : await syncNow(serverId);
      // Keep the shared result current even if the user leaves during a switch.
      if (useSshServerStore.getState().servers.some((server) => server.id === serverId)) {
        applySyncResult(result);
        await handleInspect(serverId);
        if (!mounted.current || !serverIdsRef.current.has(serverId) || requestGeneration.current[serverId] !== generation) return;
        const status = syncStatus(result, t);
        if (status.kind === 'failed') setAccountSwitchError(status.text);
        setLocalMessage({
          kind: status.kind === 'failed' ? 'error' : status.kind === 'applied' ? 'success' : 'warning',
          text: status.text,
        });
      }
    } catch (err) {
      if (mounted.current && serverIdsRef.current.has(serverId) && requestGeneration.current[serverId] === generation) {
        setAccountSwitchError(String(err));
      }
    } finally {
      if (mounted.current && requestGeneration.current[serverId] === generation) setBusy(serverId, false);
    }
  };

  const handleBulkSwitch = async () => {
    if (!chosenAccount || bulkTargets.length === 0 || bulkRunRef.current || bulkUnavailable) return;
    const targets = [...bulkTargets];
    const accountId = chosenAccount.id;
    bulkRunRef.current = true;
    setBulkApplying(true);
    setBulkResults({});
    let nextIndex = 0;
    const worker = async () => {
      while (nextIndex < targets.length) {
        const server = targets[nextIndex++];
        if (mounted.current) setBusy(server.id, true);
        try {
          const result = await sshServerService.switchSshServerAccount(server.id, accountId);
          if (useSshServerStore.getState().servers.some((item) => item.id === server.id)) {
            applySyncResult(result);
            await handleInspect(server.id);
          }
          const status = syncStatus(result, t);
          if (mounted.current) setBulkResults((current) => ({ ...current, [server.id]: {
            kind: status.kind === 'applied' ? 'success' : 'error', text: status.text,
          } }));
        } catch (err) {
          if (mounted.current) setBulkResults((current) => ({ ...current, [server.id]: { kind: 'error', text: String(err) } }));
        } finally {
          if (mounted.current) setBusy(server.id, false);
        }
      }
    };
    try {
      await Promise.all(Array.from({ length: Math.min(3, targets.length) }, () => worker()));
    } finally {
      bulkRunRef.current = false;
      if (mounted.current) setBulkApplying(false);
    }
  };

  const handleDelete = async (server: SshServer) => {
    try {
      const confirmed = await confirmDialog(
        t('codex.hosts.deleteConfirm', '仅删除「{{name}}」的主机登记？远端文件和会话不会被删除。', { name: server.name }),
        { title: t('codex.hosts.deleteTitle', '删除主机登记'), kind: 'warning' },
      );
      if (!confirmed || !mounted.current) return;
      setBusy(server.id, true);
      setLocalMessage(null);
      await deleteServer(server.id);
      requestGeneration.current[server.id] = (requestGeneration.current[server.id] ?? 0) + 1;
      if (mounted.current) {
        setAccountServerId((current) => current === server.id ? null : current);
        setLocalMessage({ kind: 'success', text: t('codex.hosts.deleted', '主机登记已删除') });
      }
    } catch (err) {
      if (mounted.current) setLocalMessage({ kind: 'error', text: String(err) });
    } finally { if (mounted.current) setBusy(server.id, false); }
  };

  return (
    <div className="codex-hosts">
      <div className="codex-hosts__toolbar toolbar">
        <div className="toolbar-left codex-hosts__toolbar-copy">
          <div>
            <h2>{t('codex.hosts.title', '远端主机')}</h2>
            <p>{t('codex.hosts.subtitle', '登记 SSH 主机，查看远端 Codex 账号并管理同步。')}</p>
          </div>
          <span className="codex-hosts__count">{t('codex.ssh.serverCount', '{{count}} 台', { count: servers.length })}</span>
        </div>
        <div className="toolbar-right">
          {bulkTargets.length > 0 && <button className="btn btn-secondary" type="button"
            onClick={() => { setAccountChoice(''); setBulkResults({}); setBulkOpen(true); }}>
            <UserRound size={15} />{t('codex.hosts.bulkSwitch', '切换选中主机（{{count}}）', { count: bulkTargets.length })}
          </button>}
          <button className="btn btn-secondary" type="button" disabled={loading || refreshing || busyServerIds.size > 0 || Object.values(inspections).some((entry) => entry.reading) || servers.some((server) => isSyncInProgress(syncResultsByServerId[server.id] ?? server.last_sync))} onClick={() => void handleRefresh()}>
            <RefreshCw size={15} className={loading || refreshing ? 'spin' : undefined} />
            {t('common.refresh', '刷新')}
          </button>
          <button className="btn btn-primary" type="button" onClick={() => { setLocalMessage(null); setFormError(null); setForm({ ...emptyForm }); }}>
            <Plus size={16} />{t('codex.hosts.add', '添加主机')}
          </button>
        </div>
      </div>

      {(error || localMessage) && (
        <div className={`codex-hosts__message is-${localMessage?.kind ?? 'error'}`} role="status">
          <span>{localMessage?.text ?? error}</span>
          {localMessage && <button type="button" aria-label={t('common.close', '关闭')} onClick={() => setLocalMessage(null)}><X size={15} /></button>}
        </div>
      )}

      <div className="codex-hosts__list" aria-label={t('codex.hosts.list', '已登记主机')}>
        {servers.length === 0 ? (
          <div className="codex-hosts__empty">
            <Server size={26} aria-hidden="true" />
            <h3>{t('codex.hosts.emptyTitle', '还没有远端主机')}</h3>
            <p>{t('codex.hosts.emptyBody', '添加一个 SSH 主机，之后可在这里检查连接、账号和同步状态。')}</p>
            <button className="btn btn-primary" type="button" onClick={() => { setFormError(null); setForm({ ...emptyForm }); }}>
              <Plus size={16} />{t('codex.hosts.add', '添加主机')}
            </button>
          </div>
        ) : (
          <>
            <div className="codex-hosts__columns" aria-hidden="true">
              <span>{t('codex.hosts.columnHost', '主机')}</span>
              <span>{t('codex.hosts.currentAccount', '当前账户')}</span>
              <span>{t('codex.hosts.columnState', '状态')}</span>
              <span>{t('codex.hosts.columnActions', '操作')}</span>
            </div>
            {servers.map((server) => {
              const sync = syncResultsByServerId[server.id] ?? server.last_sync;
              const status = syncStatus(sync, t);
              const inspection = inspections[server.id];
              const remoteAccount = inspection?.account;
              const savedAccount = accounts.find((account) => account.id === remoteAccount?.matched_account_id);
              const plan = savedAccount ? buildCodexAccountPresentation(savedAccount, t).planLabel : null;
              const connection = !inspection || inspection.reading ? 'checking' : inspection.error ? 'failed' : 'ok';
              const busy = busyServerIds.has(server.id) || !inspection || inspection.reading || isSyncInProgress(sync);
              const selected = selectedIds.has(server.id);
              const bulkSelected = bulkServerIds.has(server.id);
              const hostAddress = remoteAccount?.connection_address || `${server.username ? `${server.username}@` : ''}${server.host}${server.port > 0 ? `:${server.port}` : ''}`;
              const accountError = inspection?.error;
              const openAccount = () => { setAccountChoice(''); setAccountSwitchError(null); setAccountServerId(server.id); };
              return (
                <article className="codex-hosts__row" key={server.id}>
                  <div className="codex-hosts__identity">
                    <input className="codex-hosts__bulk-check" type="checkbox" checked={bulkSelected}
                      disabled={bulkApplying}
                      aria-label={t('codex.hosts.selectForBulk', '选择 {{name}} 进行批量切换', { name: server.name })}
                      onChange={() => setBulkServerIds((current) => {
                        const next = new Set(current);
                        if (next.has(server.id)) next.delete(server.id); else next.add(server.id);
                        return next;
                      })} />
                    <span className="codex-hosts__server-icon" aria-hidden="true"><Server size={18} /></span>
                    <div className="codex-hosts__identity-copy">
                      <strong>{server.name}</strong>
                      {hostAddress.toLowerCase() !== server.name.trim().toLowerCase() && <code title={`SSH ${hostAddress}`}>SSH {hostAddress}</code>}
                    </div>
                  </div>
                  <div className="codex-hosts__states">
                    <span className="codex-hosts__account-name"
                      title={remoteAccount?.email || remoteAccount?.account_id || undefined}>
                      {remoteAccount
                        ? remoteAccount.email || remoteAccount.account_id || accountMode(remoteAccount.auth_mode, t)
                        : accountError ? t('codex.hosts.readFailed', '读取失败') : t('codex.hosts.reading', '正在读取…')}
                    </span>
                    {remoteAccount && <span className="codex-hosts__sync">
                      {[accountMode(remoteAccount.auth_mode, t), plan].filter(Boolean).join(' · ')}
                      {accountError ? ` · ${t('codex.hosts.staleAccount', '上次读取结果')}` : ''}
                    </span>}
                    {(remoteAccount?.model_provider || remoteAccount?.model || remoteAccount?.base_url) && <span className="codex-hosts__sync" title={[
                      remoteAccount.model_provider_name || remoteAccount.model_provider, remoteAccount.base_url, remoteAccount.model,
                      remoteAccount.model_catalog_path,
                    ].filter(Boolean).join(' · ')}>
                      {[remoteAccount.model_provider_name || remoteAccount.model_provider || remoteAccount.base_url, remoteAccount.model].filter(Boolean).join(' · ')}
                      {remoteAccount.model_catalog_exists
                        ? ` · ${remoteAccount.catalog_model_count == null
                          ? t('codex.hosts.catalogPresent', '模型目录已就绪')
                          : t('codex.hosts.catalogCount', '模型目录 {{count}} 项', { count: remoteAccount.catalog_model_count })}`
                        : remoteAccount.model_catalog_path ? ` · ${t('codex.hosts.catalogMissing', '模型目录缺失')}` : ''}
                    </span>}
                  </div>
                  <div className="codex-hosts__states" aria-live="polite">
                    <span className={`codex-hosts__state is-${connection}`}
                      title={accountError || undefined}>
                      <span className="codex-hosts__dot" />
                      {busy ? (isSyncInProgress(sync) ? syncSummary(sync, t) : t('codex.hosts.reading', '正在读取…'))
                        : accountError ? t('codex.hosts.readFailed', '读取失败')
                        : t('codex.hosts.updated', '已更新')}
                    </span>
                    {inspection?.checkedAt && <span className="codex-hosts__sync" title={new Date(inspection.checkedAt).toLocaleString()}>
                      {t('codex.hosts.lastRead', '上次读取：{{time}}', { time: new Date(inspection.checkedAt).toLocaleString() })}
                    </span>}
                    {sync && <span className={`codex-hosts__sync is-${status.kind}`} title={status.text}>
                      {syncSummary(sync, t)}
                    </span>}
                    {accountError && <button className="codex-hosts__account-link" type="button" disabled={busy}
                      onClick={() => void handleInspect(server.id)}>{t('codex.hosts.retry', '重试')}</button>}
                  </div>
                  <div className="codex-hosts__actions">
                    <button className="action-btn" type="button" disabled={busy} onClick={openAccount}
                      title={t('codex.hosts.switchAccount', '切换账户')} aria-label={t('codex.hosts.switchAccount', '切换账户')}>
                      <UserRound size={16} aria-hidden="true" />
                    </button>
                    {onOpenSessions && <button className="action-btn" type="button" onClick={() => onOpenSessions(server.id)}
                      title={t('codex.hosts.sessions', '会话')} aria-label={t('codex.hosts.sessions', '会话')}>
                      <MessageSquare size={16} aria-hidden="true" />
                    </button>}
                    <button className="action-btn" type="button" disabled={busy || selectionLoading}
                      title={t('common.edit', '编辑')} aria-label={t('common.edit', '编辑')}
                      onClick={() => { setLocalMessage(null); setFormError(null); setForm(formFromServer(server)); }}>
                      <Pencil size={16} aria-hidden="true" />
                    </button>
                    <button className={`action-btn${selected ? ' is-following' : ''}`} type="button"
                      disabled={selectionLoading || busy} aria-pressed={selected}
                      title={t('codex.hosts.followLocal', '跟随本地切换账户')} aria-label={t('codex.hosts.followLocal', '跟随本地切换账户')}
                      onClick={() => void handleSelection(server.id)}>
                      <Link2 size={16} aria-hidden="true" />
                    </button>
                    {accountError && <button className="action-btn" type="button" disabled={busy}
                      title={t('codex.ssh.testConnection', '测试连接')} aria-label={t('codex.ssh.testConnection', '测试连接')}
                      onClick={() => void handleTest(server.id)}>
                      <PlugZap size={16} aria-hidden="true" />
                    </button>}
                    <button className="action-btn is-danger" type="button" disabled={busy || selectionLoading}
                      title={t('codex.hosts.remove', '移除主机')} aria-label={t('codex.hosts.remove', '移除主机')}
                      onClick={() => void handleDelete(server)}>
                      <Trash2 size={16} aria-hidden="true" />
                    </button>
                  </div>
                  {accountError && <div className="codex-hosts__row-error" role="status">{accountError}</div>}
                </article>
              );
            })}
          </>
        )}
      </div>

      {form && createPortal(<div
        className="modal-overlay codex-hosts__overlay"
        onMouseDown={(event) => { if (event.target === event.currentTarget && !saving) setForm(null); }}
      >
        <form
          ref={formRef} tabIndex={-1}
          className="modal codex-hosts__dialog"
          role="dialog" aria-modal="true" aria-labelledby="codex-host-form-title"
          onSubmit={(event) => { event.preventDefault(); void handleSave(); }}
        >
          <div className="modal-header">
            <h2 id="codex-host-form-title">
              {form.id ? t('codex.ssh.editServer', '编辑服务器') : t('codex.hosts.add', '添加主机')}
            </h2>
            <button className="modal-close" type="button" disabled={saving} aria-label={t('common.close', '关闭')} onClick={() => setForm(null)}>
              <X size={18} />
            </button>
          </div>
          <div className="modal-body codex-hosts__form-body">
            <div className="codex-hosts__form-grid">
              <label className="codex-hosts__field codex-hosts__field--wide">
                <span>{t('codex.ssh.host', '主机')}</span>
                <input className="form-input" autoFocus required value={form.host}
                  onChange={(event) => setForm({ ...form, host: event.target.value })}
                  placeholder={t('codex.ssh.hostPlaceholder', 'SSH 别名或地址')} />
              </label>
              <label className="codex-hosts__field codex-hosts__field--wide">
                <span>{t('codex.hosts.displayNameOptional', '显示名称（可选）')}</span>
                <input className="form-input" value={form.name}
                  onChange={(event) => setForm({ ...form, name: event.target.value })}
                  placeholder={form.host || t('codex.hosts.nameDefaultsToHost', '默认使用 SSH 主机或别名')} />
              </label>
            </div>
            <details className="codex-hosts__advanced" open={form.authKind === 'private_key_file' || undefined}>
              <summary>{t('codex.hosts.advanced', '高级连接设置')}</summary>
              <div className="codex-hosts__form-grid">
              <label className="codex-hosts__field">
                <span>{t('codex.ssh.portOptional', '端口（可选）')}</span>
                <input className="form-input" type="number" min="0" max="65535" step="1" value={form.port}
                  onChange={(event) => setForm({ ...form, port: event.target.value })}
                  placeholder={t('codex.ssh.portPlaceholder', 'SSH 配置或默认值')} />
              </label>
              <label className="codex-hosts__field">
                <span>{t('codex.ssh.username', '用户名（可选）')}</span>
                <input className="form-input" value={form.username}
                  onChange={(event) => setForm({ ...form, username: event.target.value })}
                  placeholder={t('codex.ssh.usernamePlaceholder', 'SSH 配置别名可留空')} />
              </label>
              <label className="codex-hosts__field">
                <span>{t('codex.ssh.codexHome', '远端 Codex 目录（可选）')}</span>
                <input className="form-input" value={form.codexHome}
                  onChange={(event) => setForm({ ...form, codexHome: event.target.value })}
                  placeholder="~/.codex" />
              </label>
            </div>
            <fieldset className="codex-hosts__auth">
              <legend>{t('codex.ssh.authMethod', '认证方式')}</legend>
              <div className="codex-hosts__auth-options">
                <label>
                  <input type="radio" name="ssh-auth" checked={form.authKind === 'agent'}
                    onChange={() => setForm({ ...form, authKind: 'agent' })} />
                  {t('codex.ssh.agentAuth', 'SSH Agent')}
                </label>
                <label>
                  <input type="radio" name="ssh-auth" checked={form.authKind === 'private_key_file'}
                    onChange={() => setForm({ ...form, authKind: 'private_key_file' })} />
                  {t('codex.ssh.privateKeyAuth', '私钥文件')}
                </label>
              </div>
            </fieldset>
            {form.authKind === 'private_key_file' && <label className="codex-hosts__field">
              <span>{t('codex.ssh.privateKeyPath', '私钥路径')}</span>
              <input className="form-input" required value={form.privateKeyPath}
                onChange={(event) => setForm({ ...form, privateKeyPath: event.target.value })}
                placeholder="~/.ssh/id_ed25519" />
            </label>}
            </details>
            <p className="codex-hosts__hint">{t('codex.hosts.formHint', '主机别名可沿用 SSH 配置；端口留空或填 0 时使用 SSH 配置或默认值。私钥只填写本机文件路径。')}</p>
            {formError && <div className="codex-hosts__inline-error" role="alert">{formError}</div>}
          </div>
          <div className="modal-footer">
            <button className="btn btn-secondary" type="button" disabled={saving} onClick={() => setForm(null)}>{t('common.cancel', '取消')}</button>
            <button className="btn btn-primary" type="submit" disabled={saving}>
              <Check size={15} />{saving ? t('codex.hosts.saving', '保存中…') : t('common.save', '保存')}
            </button>
          </div>
        </form>
      </div>, document.body)}

      {accountServer && createPortal(<div
        className="modal-overlay codex-hosts__overlay"
        onMouseDown={(event) => {
          if (event.target === event.currentTarget && !busyServerIds.has(accountServer.id)) setAccountServerId(null);
        }}
      >
        <div ref={accountRef} tabIndex={-1} className="modal codex-hosts__dialog codex-hosts__account-dialog"
          role="dialog" aria-modal="true" aria-labelledby="codex-host-account-title">
          <div className="modal-header">
            <div>
              <h2 id="codex-host-account-title">
                {t('codex.hosts.switchFor', '切换 {{name}} 的账户', { name: accountServer.name })}
              </h2>
              <p>{accountServer.host}</p>
            </div>
            <button className="modal-close" type="button" disabled={busyServerIds.has(accountServer.id)}
              aria-label={t('common.close', '关闭')} onClick={() => setAccountServerId(null)}>
              <X size={18} />
            </button>
          </div>
          <div className="modal-body codex-hosts__account-body">
            <div className="codex-hosts__account-reading">
              <div>
                <span className="codex-hosts__overline">{t('codex.ssh.remoteStoredCredentials', '远端已存凭据')}</span>
                <strong>{inspectedAccount?.email || inspectedAccount?.account_id || (inspectedAccount ? accountMode(inspectedAccount.auth_mode, t) : t('codex.hosts.readFailed', '读取失败'))}</strong>
                <span>
                  {inspectedAccount ? accountMode(inspectedAccount.auth_mode, t) : t('codex.hosts.readOnly', '只读取远端状态')}
                  {matchedLocalAccount ? t('codex.ssh.matchedLocal', ' · 本地已保存') : ''}
                </span>
                {(inspectedAccount?.model_provider || inspectedAccount?.model || inspectedAccount?.base_url) && <span>{[
                  inspectedAccount.model_provider_name || inspectedAccount.model_provider, inspectedAccount.model,
                  inspectedAccount.base_url,
                ].filter(Boolean).join(' · ')}</span>}
                {inspectedAccount?.model_catalog_path && <span>{inspectedAccount.model_catalog_path} · {
                  inspectedAccount.model_catalog_exists
                    ? inspectedAccount.catalog_model_count == null
                      ? t('codex.hosts.catalogPresent', '模型目录已就绪')
                      : t('codex.hosts.catalogCount', '模型目录 {{count}} 项', { count: inspectedAccount.catalog_model_count })
                    : t('codex.hosts.catalogMissing', '模型目录缺失')
                }</span>}
              </div>
            </div>
            {inspectedAccount && <p className="codex-hosts__hint">
              {t('codex.ssh.remoteAccountMayBeActive', '读取的是磁盘上保存的凭据。远端 Codex 进程可能仍在使用旧账号，直到重新加载。')}
            </p>}
            {inspections[accountServer.id]?.error && <div className="codex-hosts__inline-error" role="alert">
              {inspections[accountServer.id]?.error}
            </div>}
            {accountSwitchError && <div className="codex-hosts__inline-error" role="alert">{accountSwitchError}</div>}
            <div className="codex-hosts__account-switch">
              <div>
                <h3>{t('codex.hosts.switchTitle', '切换此主机的账号')}</h3>
                <p>{t('codex.hosts.switchHint', '选择本地已保存账号后，明确执行切换。读取远端状态不会切换账号。')}</p>
              </div>
              <div className="codex-hosts__switch-controls">
                <select className="form-input" aria-label={t('codex.ssh.chooseSavedAccount', '选择要切换到的本地账号')}
                  value={accountChoice} onChange={(event) => setAccountChoice(event.target.value)}>
                  <option value="">{t('codex.ssh.chooseSavedAccount', '选择已保存账号')}</option>
                  <option value="__local_current__">{t('codex.hosts.useLocalAccount', '使用本地当前账户')} {currentAccount?.email ? `· ${currentAccount.email}` : ''}</option>
                  {accounts.map((account) => {
                    const presentation = buildCodexAccountPresentation(account, t);
                    const label = [presentation.displayName, providerName(account), presentation.planLabel, account.id.slice(-6)]
                      .filter(Boolean).join(' · ');
                    return <option key={account.id} value={account.id}>{label}</option>;
                  })}
                </select>
                <button className="btn btn-primary" type="button"
                  disabled={!accountChoice || busyServerIds.has(accountServer.id) || inspections[accountServer.id]?.reading || isSyncInProgress(syncResultsByServerId[accountServer.id] ?? accountServer.last_sync)}
                  onClick={() => void handleSync(accountServer.id, accountChoice === '__local_current__' ? undefined : accountChoice)}>
                  <UserRound size={15} />{t('codex.ssh.switchRemoteAccount', '切换此主机')}
                </button>
              </div>
              {chosenAccount && <p className="codex-hosts__selection-detail">
                {[providerName(chosenAccount), chosenAccount.api_base_url, chosenAccount.api_startup_model]
                  .filter(Boolean).join(' · ') || t('codex.hosts.officialCredentials', '官方账户凭据')}
                {chosenAccount.api_provider_mode === 'custom' && <span> · {t('codex.hosts.catalogImportHint', '请先在供应商管理中配置并保存各模型的参数。')}</span>}
              </p>}
            </div>
          </div>
          <div className="modal-footer">
            <button className="btn btn-secondary" type="button" disabled={busyServerIds.has(accountServer.id)}
              onClick={() => setAccountServerId(null)}>{t('common.close', '关闭')}</button>
          </div>
        </div>
      </div>, document.body)}

      {bulkOpen && createPortal(<div className="modal-overlay codex-hosts__overlay"
        onMouseDown={(event) => { if (event.target === event.currentTarget && !bulkRunRef.current) setBulkOpen(false); }}>
        <div ref={bulkRef} tabIndex={-1} className="modal codex-hosts__dialog" role="dialog" aria-modal="true"
          aria-labelledby="codex-host-bulk-title">
          <div className="modal-header">
            <div>
              <h2 id="codex-host-bulk-title">{t('codex.hosts.bulkTitle', '切换选中主机的账户')}</h2>
              <p>{t('codex.hosts.bulkCount', '最多同时切换 3 台主机；{{count}} 台主机的结果分别显示。', { count: bulkTargets.length })}</p>
            </div>
            <button className="modal-close" type="button" disabled={bulkApplying} aria-label={t('common.close', '关闭')}
              onClick={() => { if (!bulkRunRef.current) setBulkOpen(false); }}><X size={18} /></button>
          </div>
          <div className="modal-body codex-hosts__account-body">
            <label className="codex-hosts__field">
              <span>{t('codex.ssh.chooseSavedAccount', '选择已保存账号')}</span>
              <select className="form-input" value={accountChoice} disabled={bulkApplying}
                onChange={(event) => { setAccountChoice(event.target.value); setBulkResults({}); }}>
                <option value="">{t('codex.ssh.chooseSavedAccount', '选择已保存账号')}</option>
                {accounts.map((account) => {
                  const presentation = buildCodexAccountPresentation(account, t);
                  return <option key={account.id} value={account.id}>{[
                    presentation.displayName, providerName(account), presentation.planLabel, account.id.slice(-6),
                  ].filter(Boolean).join(' · ')}</option>;
                })}
              </select>
            </label>
            {chosenAccount && <p className="codex-hosts__selection-detail">
              {[providerName(chosenAccount), chosenAccount.api_base_url, chosenAccount.api_startup_model]
                .filter(Boolean).join(' · ') || t('codex.hosts.officialCredentials', '官方账户凭据')}
              {chosenAccount.api_provider_mode === 'custom' && <span> · {t('codex.hosts.catalogImportHint', '请先在供应商管理中配置并保存各模型的参数。')}</span>}
            </p>}
            <div className="codex-hosts__bulk-targets" aria-live="polite">
              {bulkTargets.map((server) => <div className="codex-hosts__bulk-target" key={server.id}>
                <strong>{server.name}</strong>
                <span className={bulkResults[server.id]?.kind === 'error' ? 'is-error' : undefined}>
                  {bulkResults[server.id]?.text ?? (bulkApplying && busyServerIds.has(server.id)
                    ? t('codex.hosts.switching', '正在切换…') : t('codex.hosts.notStarted', '等待执行'))}
                </span>
              </div>)}
            </div>
          </div>
          <div className="modal-footer">
            <button className="btn btn-secondary" type="button" disabled={bulkApplying}
              onClick={() => { if (!bulkRunRef.current) setBulkOpen(false); }}>{t('common.close', '关闭')}</button>
            <button className="btn btn-primary" type="button" disabled={!chosenAccount || bulkApplying || bulkUnavailable || bulkTargets.length === 0}
              onClick={() => void handleBulkSwitch()}>
              <UserRound size={15} />{bulkApplying ? t('codex.hosts.switching', '正在切换…') : t('codex.hosts.applyToHosts', '应用到选中主机')}
            </button>
          </div>
        </div>
      </div>, document.body)}
    </div>
  );
}
