import { useEffect, useRef, useState } from 'react';
import { Check, Eye, EyeOff, RefreshCw, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { SingleSelectDropdown, type SingleSelectOption } from '../SingleSelectDropdown';
import { ModalErrorMessage, useModalErrorState } from '../ModalErrorMessage';
import { useEscCloseTopmost } from '../../hooks/useEscClose';
import * as settingsService from '../../services/claudeCodeSettingsService';
import { listClaudeDesktopGatewayModels } from '../../services/claudeService';
import { listSshServers } from '../../services/sshServerService';
import type { ClaudeCodeSettings, ClaudeCodeSyncResult } from '../../types/claudeCodeSettings';
import type { ClaudeDesktopGatewayModel } from '../../types/claude';
import type { SshServer } from '../../types/sshServer';
import {
  CLAUDE_CODE_MODEL_ROLES, mergeClaudeCodeModelOptions, updateClaudeCodeModelMapping,
  patchClaudeCodeSettings, parseClaudeCodeSettings,
  readClaudeCodeSettingsFields, type ClaudeCodeSettingsFields,
} from '../../utils/claudeCodeSettings';
import { CLAUDE_API_PROVIDER_CUSTOM_ID, CLAUDE_API_PROVIDER_PRESETS, normalizeClaudeApiProviderBaseUrl } from '../../utils/claudeProviderPresets';
import { requestCodexHosts } from '../../utils/codexHostNavigation';
import './ClaudeCodeSettingsModal.css';

interface Props {
  onClose: () => void;
  onApplied: () => void;
}

function inferProviderId(baseUrl: string) {
  return CLAUDE_API_PROVIDER_PRESETS.find((preset) => preset.baseUrls.some(
    (url) => url.replace(/\/+$/, '') === baseUrl.replace(/\/+$/, ''),
  ))?.id ?? CLAUDE_API_PROVIDER_CUSTOM_ID;
}

function ModelInput({ id, label, value, options, disabled, onChange }: {
  id: string; label: string; value: string; options: SingleSelectOption[];
  disabled: boolean; onChange: (value: string) => void;
}) {
  const { t } = useTranslation();
  return <div className="claude-code-config-model-input">
    <input id={id} className="form-input" value={value} spellCheck={false} disabled={disabled}
      placeholder={t('claude.codeConfig.selectModel', '选择或输入模型 ID')}
      onChange={(event) => onChange(event.target.value)} />
    {options.length > 0 && <SingleSelectDropdown value={value} options={options} disabled={disabled}
      menuWidth={280} className="claude-code-config-model-picker"
      menuClassName="claude-code-config-model-menu"
      ariaLabel={`${label} · ${t('claude.codeConfig.chooseModel', '选择模型')}`} onChange={onChange} />}
  </div>;
}

export function ClaudeCodeSettingsModal({ onClose, onApplied }: Props) {
  const { t } = useTranslation();
  const [settings, setSettings] = useState<ClaudeCodeSettings | null>(null);
  const [content, setContent] = useState('');
  const [fields, setFields] = useState<ClaudeCodeSettingsFields | null>(null);
  const [servers, setServers] = useState<SshServer[]>([]);
  const [serverIds, setServerIds] = useState<string[]>([]);
  const [results, setResults] = useState<ClaudeCodeSyncResult[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [keyVisible, setKeyVisible] = useState(false);
  const [providerId, setProviderId] = useState(CLAUDE_API_PROVIDER_CUSTOM_ID);
  const { message: error, scrollKey: errorScrollKey, set: setError } = useModalErrorState();
  const [saved, setSaved] = useState(false);
  const [fetchingModels, setFetchingModels] = useState(false);
  const [fetchedModels, setFetchedModels] = useState<ClaudeDesktopGatewayModel[]>([]);
  const [modelError, setModelError] = useState('');
  const [modelNotice, setModelNotice] = useState('');
  const loadGeneration = useRef(0);
  useEscCloseTopmost(true, () => { if (!busy) onClose(); });

  const load = async () => {
    const generation = ++loadGeneration.current;
    setLoading(true);
    setError(null);
    try {
      const [snapshot, preferences, inventory] = await Promise.all([
        settingsService.readClaudeCodeSettings(), settingsService.readClaudeCodeSyncPreferences(), listSshServers(),
      ]);
      if (generation !== loadGeneration.current) return;
      const initialFields = readClaudeCodeSettingsFields(snapshot.content);
      setFields(initialFields);
      setProviderId(inferProviderId(initialFields.baseUrl));
      setSettings(snapshot);
      setContent(snapshot.content);
      setServers(inventory.servers);
      setServerIds(preferences.serverIds);
      setResults(preferences.lastResults ?? []);
      setKeyVisible(false);
      setSaved(false);
      setFetchedModels([]);
      setModelError('');
      setModelNotice('');
    } catch (cause) {
      if (generation === loadGeneration.current) setError(String(cause).replace(/^Error:\s*/, ''));
    } finally {
      if (generation === loadGeneration.current) setLoading(false);
    }
  };
  useEffect(() => { void load(); return () => { loadGeneration.current++; }; }, []);

  let jsonValid = true;
  try { parseClaudeCodeSettings(content); } catch { jsonValid = false; }
  const disabled = loading || busy || fetchingModels || !fields || !jsonValid;
  const dirty = settings?.content !== content;
  const modelCandidates = mergeClaudeCodeModelOptions(fields?.modelOptions ?? [], fetchedModels);
  const modelOptions = modelCandidates.filter((option) => option.model.trim()).map((option) => ({
    value: option.model,
    label: option.label && option.label !== option.model ? `${option.label} (${option.model})` : option.model,
  }));

  const updateFields = (next: ClaudeCodeSettingsFields, models = fetchedModels) => {
    if (fields && (next.baseUrl !== fields.baseUrl || next.apiKey !== fields.apiKey || next.apiKeyField !== fields.apiKeyField)) {
      setFetchedModels([]);
      models = [];
    }
    setContent(patchClaudeCodeSettings(content, next, models));
    setFields(next);
    setError(null);
    setSaved(false);
    setModelError('');
    setModelNotice('');
  };
  const fetchModels = async () => {
    if (!fields || disabled || !fields.apiKey.trim()) return;
    const baseUrl = normalizeClaudeApiProviderBaseUrl(fields.baseUrl);
    if (baseUrl === null) {
      setModelError(t('claude.apiKey.invalidBaseUrl', '请输入有效的 HTTP / HTTPS Base URL'));
      return;
    }
    const generation = loadGeneration.current;
    setFetchingModels(true);
    setModelError('');
    setModelNotice('');
    try {
      const response = await listClaudeDesktopGatewayModels({
        apiBaseUrl: baseUrl || 'https://api.anthropic.com', apiKey: fields.apiKey.trim(),
        authScheme: fields.apiKeyField === 'ANTHROPIC_API_KEY' ? 'x-api-key' : 'bearer',
      });
      if (generation !== loadGeneration.current) return;
      if (!response.models.length) {
        setModelError(t('claude.codeConfig.noUpstreamModels', '上游未返回可用模型，可直接输入模型 ID。'));
        return;
      }
      const candidates = mergeClaudeCodeModelOptions(fields.modelOptions, response.models);
      setFetchedModels(response.models);
      let next = fields;
      for (const role of CLAUDE_CODE_MODEL_ROLES) {
        if (response.models.some((item) => item.id.trim().toLowerCase() === fields.models[role].model.trim().toLowerCase())) {
          next = updateClaudeCodeModelMapping(next, role, fields.models[role].model, candidates);
        }
      }
      updateFields(next, response.models);
      setModelNotice(t('claude.codeConfig.modelsFetched', '已获取 {{count}} 个模型，可在下方选择。', { count: response.models.length }));
    } catch {
      if (generation === loadGeneration.current) setModelError(t('claude.codeConfig.fetchModelsFailed', '无法获取模型，请检查 Base URL、API Key 和供应商的模型接口后重试。'));
    } finally {
      if (generation === loadGeneration.current) setFetchingModels(false);
    }
  };
  const save = async () => {
    if (!settings || !fields || disabled) return;
    if (normalizeClaudeApiProviderBaseUrl(fields.baseUrl) === null) {
      setError(t('claude.apiKey.invalidBaseUrl', '请输入有效的 HTTP / HTTPS Base URL'));
      return;
    }
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      // Keep the account bound when editing credentials; saving can update its API key or URL.
      const response = await settingsService.saveClaudeCodeSettings(content, settings.revision, serverIds, settings.account?.id ?? null);
      setSettings(response.settings);
      setContent(response.settings.content);
      setFields(readClaudeCodeSettingsFields(response.settings.content));
      setResults(response.syncResults);
      setError(response.syncError);
      setSaved(true);
      onApplied();
    } catch (cause) { setError(String(cause).replace(/^Error:\s*/, '')); }
    finally { setBusy(false); }
  };
  const sync = async () => {
    setBusy(true);
    setError(null);
    try { setResults(await settingsService.syncClaudeCodeSettings(serverIds)); }
    catch (cause) { setError(String(cause).replace(/^Error:\s*/, '')); }
    finally { setBusy(false); }
  };

  return (
    <div className="modal-overlay">
      <div className="modal ghcp-add-modal platform-account-add-modal claude-add-modal claude-code-config-modal"
        role="dialog" aria-modal="true" aria-labelledby="claude-code-config-title">
        <div className="modal-header">
          <h2 id="claude-code-config-title">{t('claude.codeConfig.title', 'Claude Code 配置')}</h2>
          <button className="modal-close" onClick={onClose} disabled={busy} aria-label={t('common.close', '关闭')}><X /></button>
        </div>
        <div className="modal-body">
          <div className="claude-code-config-path">
            <code>{settings?.path ?? '~/.claude/settings.json'}</code>
            <button type="button" className="btn btn-secondary icon-only" onClick={() => void load()} disabled={busy || loading || fetchingModels}
              title={t('claude.codeConfig.reload', '重新读取文件')} aria-label={t('claude.codeConfig.reload', '重新读取文件')}>
              <RefreshCw size={14} className={loading ? 'loading-spinner' : ''} />
            </button>
          </div>
          {settings && <div className="form-hint">
            {settings.account
              ? t('claude.codeConfig.accountBinding', 'Editing API and model settings for {{account}}. Permission settings and hooks remain shared.', { account: settings.account.name })
              : t('claude.codeConfig.noAccountBinding', 'These settings belong to the current file. No saved API Key account is associated; add or switch to an account first to save API and model settings per account.')}
          </div>}
          {loading ? <div className="oauth-loading"><RefreshCw size={20} className="loading-spinner" />{t('common.loading', '加载中...')}</div> : fields && (
            <fieldset disabled={disabled} className="claude-code-config-fields">
              <div className="form-group">
                <label>{t('claude.apiKey.providerLabel', '供应商')}</label>
                <SingleSelectDropdown value={providerId} disabled={disabled}
                  ariaLabel={t('claude.apiKey.providerLabel', '供应商')}
                  options={[...CLAUDE_API_PROVIDER_PRESETS.map((preset) => ({ value: preset.id, label: preset.name })),
                    { value: CLAUDE_API_PROVIDER_CUSTOM_ID, label: t('claude.apiKey.customProvider', '自定义') }]}
                  onChange={(id) => {
                    setProviderId(id);
                    const preset = CLAUDE_API_PROVIDER_PRESETS.find((item) => item.id === id);
                    if (preset && id !== CLAUDE_API_PROVIDER_CUSTOM_ID) updateFields({ ...fields, baseUrl: preset.baseUrls[0] ?? '', apiKeyField: preset.apiKeyField });
                  }} />
              </div>
              <div className="form-group">
                <label htmlFor="claude-config-base-url">{t('claude.apiKey.baseUrlLabel', 'Base URL')}</label>
                <input id="claude-config-base-url" className="form-input" value={fields.baseUrl} spellCheck={false}
                  placeholder={t('claude.apiKey.baseUrlPlaceholder', '留空使用 Anthropic 官方默认地址')}
                  onChange={(event) => { setProviderId(CLAUDE_API_PROVIDER_CUSTOM_ID); updateFields({ ...fields, baseUrl: event.target.value }); }} />
              </div>
              <div className="form-group">
                <label htmlFor="claude-config-api-key">{t('claude.apiKey.keyLabel', 'API Key')}</label>
                <div className="oauth-url-box oauth-manual-input claude-secret-input">
                  <input id="claude-config-api-key" type={keyVisible ? 'text' : 'password'} value={fields.apiKey}
                    autoComplete="off" spellCheck={false} onChange={(event) => updateFields({ ...fields, apiKey: event.target.value })} />
                  <button type="button" className="codex-secret-toggle-btn" onClick={() => setKeyVisible(!keyVisible)}
                    aria-label={keyVisible ? t('claude.apiKey.hide', '隐藏 API Key') : t('claude.apiKey.show', '显示 API Key')}>
                    {keyVisible ? <EyeOff size={16} /> : <Eye size={16} />}
                  </button>
                </div>
              </div>
              <div className="form-group">
                <label>{t('claude.codeConfig.keyField', '认证环境变量')}</label>
                <div className="claude-gateway-segmented claude-gateway-auth-segmented">
                  {(['ANTHROPIC_AUTH_TOKEN', 'ANTHROPIC_API_KEY'] as const).map((key) => (
                    <button key={key} type="button" className={`claude-provider-endpoint-chip ${fields.apiKeyField === key ? 'active' : ''}`}
                      onClick={() => updateFields({ ...fields, apiKeyField: key })}>{key}</button>
                  ))}
                </div>
              </div>
              <section className="form-group claude-code-config-models" aria-labelledby="claude-config-models-title">
                <div className="claude-gateway-models-header">
                  <label id="claude-config-models-title">{t('claude.codeConfig.models', '模型配置')}</label>
                  <button type="button" className="btn btn-secondary" disabled={disabled || !fields.apiKey.trim()}
                    onClick={() => void fetchModels()}>
                    <RefreshCw size={14} aria-hidden="true" className={fetchingModels ? 'loading-spinner' : ''} />
                    {fetchingModels ? t('claude.codeConfig.fetchingModels', '获取中…') : t('claude.codeConfig.fetchModels', '从上游获取')}
                  </button>
                </div>
                <p className="form-hint">{t('claude.codeConfig.modelsHint', '获取模型用于下方选择，也可直接输入模型 ID。上游提供上下文长度时，自动设置默认模型的窗口及 90% 压缩阈值，保存后生效。')}</p>
                {modelError && <p className="form-error" role="alert">{modelError}</p>}
                {modelNotice && <p className="form-hint" role="status">{modelNotice}</p>}
                <div className="claude-code-config-grid">
                  <div className="form-group">
                    <label htmlFor="claude-config-model">{t('claude.codeConfig.defaultModel', '默认模型')}</label>
                    <ModelInput id="claude-config-model" label={t('claude.codeConfig.defaultModel', '默认模型')}
                      value={fields.model} options={modelOptions} disabled={disabled}
                      onChange={(model) => updateFields({ ...fields, model })} />
                  </div>
                  <div className="form-group">
                    <label htmlFor="claude-config-subagent">{t('claude.codeConfig.subagentModel', '子代理模型')}</label>
                    <ModelInput id="claude-config-subagent" label={t('claude.codeConfig.subagentModel', '子代理模型')}
                      value={fields.subagentModel} options={modelOptions} disabled={disabled}
                      onChange={(subagentModel) => updateFields({ ...fields, subagentModel })} />
                  </div>
                </div>
              </section>
              <details className="claude-code-config-details token-format-collapse">
                <summary className="token-format-collapse-summary">{t('claude.codeConfig.modelMappings', '模型映射（可选）')}</summary>
                <div className="token-format">
                  {CLAUDE_CODE_MODEL_ROLES.map((role) => (
                    <div key={role} className="claude-code-config-grid">
                      <div className="form-group">
                        <label htmlFor={`claude-config-${role}`}>{role.charAt(0) + role.slice(1).toLowerCase()}</label>
                        <ModelInput id={`claude-config-${role}`} label={role.charAt(0) + role.slice(1).toLowerCase()}
                          value={fields.models[role].model} options={modelOptions} disabled={disabled}
                          onChange={(model) => updateFields(updateClaudeCodeModelMapping(fields, role, model, modelCandidates))} />
                      </div>
                      <div className="form-group">
                        <label htmlFor={`claude-config-${role}-name`}>{t('claude.codeConfig.menuDisplayName', '菜单显示名称')}</label>
                        <input id={`claude-config-${role}-name`} className="form-input" value={fields.models[role].name}
                          onChange={(event) => updateFields({ ...fields, models: { ...fields.models, [role]: { ...fields.models[role], name: event.target.value } } })} />
                      </div>
                    </div>
                  ))}
                  <p className="form-hint">{t('claude.codeConfig.modelHint', '设置内置别名对应的模型。选择时自动带入名称，手动填写的名称会保留；留空使用默认值。')}</p>
                </div>
              </details>
            </fieldset>
          )}
          {!loading && settings && (
            <>
              <details className="claude-code-config-details token-format-collapse">
                <summary className="token-format-collapse-summary">{t('claude.codeConfig.fullJson', '完整 settings.json')}</summary>
                <div className="token-format">
                  <p className="form-hint">{t('claude.codeConfig.jsonHint', '包含 API Key。可编辑其他设置，保存前会校验 JSON。')}</p>
                  <textarea className="form-input instance-args-input claude-code-config-json" value={content} disabled={busy || fetchingModels}
                    aria-label={t('claude.codeConfig.fullJson', '完整 settings.json')} spellCheck={false}
                    onChange={(event) => {
                      setContent(event.target.value); setSaved(false); setError(null); setModelError(''); setModelNotice(''); setFetchedModels([]);
                      try {
                        const next = readClaudeCodeSettingsFields(event.target.value);
                        setFields(next); setProviderId(inferProviderId(next.baseUrl));
                      } catch { /* Keep last valid form while JSON is being edited. */ }
                    }} />
                  {!jsonValid && <p className="form-error">{t('claude.codeConfig.invalidJson', '请输入有效的 JSON 对象，env 必须是对象。')}</p>}
                </div>
              </details>
              <div className="form-group claude-code-config-remote">
                <div className="claude-code-config-path">
                  <label>{t('claude.codeConfig.remoteSync', '同步到远程主机')}</label>
                  <button type="button" className="btn btn-secondary" disabled={busy || fetchingModels} onClick={() => { onClose(); requestCodexHosts(); }}>
                    {t('claude.codeConfig.manageHosts', '管理主机')}
                  </button>
                </div>
                <p className="form-hint">{t('claude.codeConfig.syncHint', '勾选的主机将在保存配置或切换 CLI 账号后，同步整份文件到 ~/.claude/settings.json。')}</p>
                {servers.length === 0 && <p className="form-hint">{t('claude.codeConfig.noHosts', '暂无远程主机，请先添加 SSH 连接。')}</p>}
                {servers.length > 0 && (
                  <div className="claude-code-config-hosts">
                    {servers.map((server) => {
                      const result = results.find((item) => item.serverId === server.id);
                      return (
                        <label key={server.id} className="claude-code-config-host">
                          <input type="checkbox" checked={serverIds.includes(server.id)} disabled={busy || fetchingModels}
                            onChange={(event) => { setServerIds(event.target.checked ? [...serverIds, server.id] : serverIds.filter((id) => id !== server.id)); setSaved(false); }} />
                          <span className="claude-code-config-host-copy">
                            <span>{server.name || server.host}</span>
                            {result && <span className={result.error ? 'form-error' : 'form-hint'}>
                              {result.error ? t('claude.codeConfig.syncFailed', '同步失败，可重试') : result.revision === settings.revision
                                ? t('claude.codeConfig.syncVerified', '文件已同步（内容一致）') : t('claude.codeConfig.syncPending', '本地配置已更新，待同步')}
                            </span>}
                          </span>
                        </label>
                      );
                    })}
                  </div>
                )}
                <button type="button" className="btn btn-secondary" disabled={busy || fetchingModels || dirty || !serverIds.length || settings.revision === 'missing'} onClick={() => void sync()}>
                  <RefreshCw size={14} className={busy ? 'loading-spinner' : ''} />{t('claude.codeConfig.syncNow', '同步当前文件')}
                </button>
                {dirty && <p className="form-hint">{t('claude.codeConfig.saveBeforeSync', '请先保存修改，再同步。')}</p>}
              </div>
            </>
          )}
          <ModalErrorMessage message={error} scrollKey={errorScrollKey} />
          {saved && <div className="add-status success" role="status"><Check size={16} />{settings?.account
            ? t('claude.codeConfig.accountSaved', 'API and model settings saved for {{account}} and applied to this file.', { account: settings.account.name })
            : t('claude.codeConfig.fileSaved', 'Settings saved to the current file and applied.')}</div>}
        </div>
        <div className="modal-footer">
          <button type="button" className="btn btn-secondary" onClick={onClose} disabled={busy}>{t('common.close', '关闭')}</button>
          <button type="button" className="btn btn-primary" onClick={() => void save()} disabled={disabled}>
            {busy && <RefreshCw size={14} className="loading-spinner" />}{t('claude.codeConfig.save', '保存并应用')}
          </button>
        </div>
      </div>
    </div>
  );
}
