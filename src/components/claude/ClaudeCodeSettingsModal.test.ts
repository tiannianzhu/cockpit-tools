import assert from 'node:assert/strict';
import test from 'node:test';
import { loadHookModule, settlePromises } from '../../../tests/helpers/reactHookHarness';
import type { ClaudeAccount } from '../../types/claude';
import type { ClaudeCodeSettings } from '../../types/claudeCodeSettings';
import * as settingsUtils from '../../utils/claudeCodeSettings';

function elements(value: any): any[] {
  if (!value || typeof value !== 'object') return [];
  if (Array.isArray(value)) return value.flatMap(elements);
  return [value, ...elements(value.props?.children)];
}

async function harness(failPreview = false) {
  const accounts: ClaudeAccount[] = [
    { id: 'first', email: 'First relay', auth_mode: 'api_key', created_at: 1, last_used: 1 },
    { id: 'second', email: 'Custom relay', auth_mode: 'api_key', created_at: 1, last_used: 1 },
    { id: 'oauth', email: 'OAuth', auth_mode: 'oauth', created_at: 1, last_used: 1 },
    { id: 'desktop', email: 'Desktop', auth_mode: 'desktop_gateway', created_at: 1, last_used: 1 },
  ];
  const globals = { permissions: { allow: ['Read'] }, hooks: { keep: true } };
  const snapshot: ClaudeCodeSettings = {
    path: '/fixture/settings.json', revision: 'original-revision',
    account: { id: 'first', name: 'First relay' },
    content: JSON.stringify({ ...globals, model: 'first-model', env: {
      ANTHROPIC_BASE_URL: 'https://first.example/api', ANTHROPIC_AUTH_TOKEN: 'first-key',
    } }),
  };
  const preview: ClaudeCodeSettings = {
    ...snapshot, account: { id: 'second', name: 'Custom relay' },
    content: JSON.stringify({ ...globals, model: 'second-model', env: {
      ANTHROPIC_BASE_URL: 'https://second.example/api', ANTHROPIC_API_KEY: 'second-key',
      CLAUDE_CODE_SUBAGENT_MODEL: 'second-worker',
    } }),
  };
  const reads: (string | undefined)[] = [];
  const writes: any[][] = [];
  let error: string | null = null;
  let applied = 0;
  const h = loadHookModule(new URL('./ClaudeCodeSettingsModal.tsx', import.meta.url), {
    'react-i18next': { useTranslation: () => ({ t: (_key: string, fallback: string) => fallback }) },
    '../SingleSelectDropdown': { SingleSelectDropdown: () => null },
    '../ModalErrorMessage': {
      ModalErrorMessage: () => null,
      useModalErrorState: () => ({ message: error, scrollKey: 0, set: (value: string | null) => { error = value; } }),
    },
    '../../hooks/useEscClose': { useEscCloseTopmost() {} },
    '../../services/claudeCodeSettingsService': {
      async readClaudeCodeSettings(id?: string) {
        reads.push(id);
        if (id && failPreview) throw new Error('Fixture account was removed');
        return structuredClone(id ? preview : snapshot);
      },
      async readClaudeCodeSyncPreferences() { return { serverIds: ['host'], lastResults: [] }; },
      async saveClaudeCodeSettings(...args: any[]) {
        writes.push(args);
        return { settings: { ...(args[3] === 'first' ? snapshot : preview), content: args[0], revision: 'saved-revision' }, syncResults: [], syncError: null };
      },
    },
    '../../services/claudeService': { listClaudeDesktopGatewayModels: async () => ({ models: [] }) },
    '../../services/sshServerService': { listSshServers: async () => ({ servers: [{ id: 'host', name: 'Fixture host' }] }) },
    '../../utils/claudeCodeSettings': settingsUtils,
    '../../utils/codexHostNavigation': { requestCodexHosts() {} },
  });
  let tree = h.render(() => h.exports.ClaudeCodeSettingsModal({
    accounts, onClose() {}, onApplied() { applied++; },
  }));
  await settlePromises();
  tree = h.flush();
  const nodes = () => elements(tree);
  const provider = () => nodes().find((node) => node.props?.ariaLabel === '供应商');
  const input = (id: string) => nodes().find((node) => node.props?.id === id);
  const sync = () => nodes().find((node) => node.type === 'button' && node.props.children?.includes?.('同步当前文件'));
  const save = () => nodes().find((node) => node.type === 'button' && node.props.className === 'btn btn-primary');
  return {
    accounts, snapshot, preview, reads, writes, h, nodes, provider, input, sync, save,
    error: () => error, applied: () => applied,
    async select(id: string) { provider().props.onChange(id); await settlePromises(); tree = h.flush(); },
    async saveDraft() { save().props.onClick(); await settlePromises(); tree = h.flush(); },
  };
}

test('provider choices contain only saved API Key accounts, including custom providers', async () => {
  const h = await harness();
  assert.deepEqual(Array.from(h.provider().props.options, (option: any) => ({ ...option })), [
    { value: 'first', label: 'First relay' }, { value: 'second', label: 'Custom relay' },
  ]);
  assert.equal(h.provider().props.value, 'first');
  assert.deepEqual(h.reads, [undefined]);
  h.input('claude-config-model').props.onChange('unsaved-model');
  await h.select('first');
  assert.deepEqual(h.reads, [undefined], 'reselecting the current account preserves its unsaved draft');
  assert.equal(h.input('claude-config-model').props.value, 'unsaved-model');
  h.h.unmount();
});

test('selecting an account previews its credentials and models, then saves under that account', async () => {
  const h = await harness();
  const originalAccounts = structuredClone(h.accounts);
  await h.select('second');
  assert.deepEqual(h.reads, [undefined, 'second']);
  assert.equal(h.provider().props.value, 'second');
  assert.equal(h.input('claude-config-base-url').props.value, 'https://second.example/api');
  assert.equal(h.input('claude-config-model').props.value, 'second-model');
  assert.equal(h.input('claude-config-subagent').props.value, 'second-worker');
  const key = h.nodes().find((node) => node.type === 'input' && node.props.type === 'password');
  assert.equal(key.props.value, 'second-key');
  assert.equal(h.sync().props.disabled, true, 'a preview cannot sync the still-active first account');
  assert.equal(h.writes.length, 0, 'selection is read-only');
  await h.saveDraft();
  assert.equal(h.writes.length, 1);
  const [content, revision, serverIds, accountId] = h.writes[0];
  assert.equal(accountId, 'second');
  assert.equal(revision, h.snapshot.revision);
  assert.deepEqual(Array.from(serverIds), ['host']);
  assert.deepEqual(JSON.parse(content), JSON.parse(h.preview.content));
  assert.equal(h.sync().props.disabled, false);
  assert.equal(h.applied(), 1);
  assert.deepEqual(h.accounts, originalAccounts);
  h.h.unmount();
});

test('a failed account preview keeps the previous credentials and save binding', async () => {
  const h = await harness(true);
  await h.select('second');
  assert.equal(h.provider().props.value, 'first');
  assert.equal(h.input('claude-config-base-url').props.value, 'https://first.example/api');
  assert.equal(h.input('claude-config-model').props.value, 'first-model');
  assert.equal(h.error(), 'Fixture account was removed');
  await h.saveDraft();
  assert.equal(h.writes[0][3], 'first');
  assert.deepEqual(JSON.parse(h.writes[0][0]), JSON.parse(h.snapshot.content));
  h.h.unmount();
});
