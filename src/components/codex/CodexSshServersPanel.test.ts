import assert from 'node:assert/strict';
import test from 'node:test';
import { loadHookModule, settlePromises } from '../../../tests/helpers/reactHookHarness';

function nodes(tree: any): any[] {
  if (Array.isArray(tree)) return tree.flatMap(nodes);
  if (!tree || typeof tree !== 'object' || !tree.props) return [];
  return [tree, ...nodes(tree.props.children)];
}

function fixture(file: string, exportName: string) {
  const state: any = {
    servers: ['a', 'b'].map(id => ({ id, name: id, host: 'fixture.invalid', username: 'fixture', port: 22,
      codex_home: '~/.codex', auth: { kind: 'agent' }, sync_on_codex_switch: true, last_sync: null })),
    selectedServerIds: ['a'], selectionLoading: false, loading: false, error: null, syncResultsByServerId: {},
    async fetchServers() {},
    async toggleServerSelection(id: string) {
      state.selectedServerIds = state.selectedServerIds.includes(id)
        ? state.selectedServerIds.filter((item: string) => item !== id) : [...state.selectedServerIds, id];
    },
  };
  const accounts = { accounts: [], currentAccount: null, async fetchAccounts() {}, async fetchCurrentAccount() {} };
  const inspection = { entries: Object.fromEntries(state.servers.map((server: any) => [server.id, {
    reading: false, checkedAt: 1, account: { server_id: server.id, auth_mode: 'oauth' },
  }])), async inspect() {}, retain() {} };
  let navigations = 0;
  const t = (key: string, fallback: any, params: Record<string, unknown> = {}) =>
    (typeof fallback === 'string' ? fallback : fallback?.defaultValue ?? key)
      .replace(/{{(\w+)}}/g, (_match: string, name: string) => String(params[name] ?? fallback?.[name] ?? ''));
  const useSshServerStore = Object.assign((selector?: any) => selector ? selector(state) : state, { getState: () => state });
  const module = loadHookModule(new URL(file, import.meta.url), {
    'react-i18next': { useTranslation: () => ({ t }) },
    '@tauri-apps/api/event': { listen: async () => () => {} },
    '@tauri-apps/plugin-dialog': {},
    '../../hooks/useEscClose': { useEscCloseTopmost() {} },
    '../../hooks/useModalFocusTrap': { useModalFocusTrap() {} },
    '../../stores/useSshServerStore': { useSshServerStore },
    '../../stores/useCodexAccountStore': { useCodexAccountStore: (selector: any) => selector(accounts) },
    '../../stores/useSshAccountInspectionStore': { useSshAccountInspectionStore: () => inspection },
    '../../presentation/platformAccountPresentation': {},
    '../../types/codex': { isCodexApiKeyAccount: () => false },
    '../../services/sshServerService': {},
    '../../utils/codexHostNavigation': { requestCodexHosts() { navigations++; } },
  });
  const component = module.exports[exportName];
  return { state, module, navigations: () => navigations, render: () => module.render(() => component({})) };
}

const followButton = (row: any) => nodes(row).find(node => node.type === 'button'
  && node.props['aria-label'] === '跟随本地切换账户');

test('host follow controls independently enable two targets and disable only the chosen target', async () => {
  const s = fixture('./CodexSshServersPanel.tsx', 'CodexSshServersPanel');
  try {
    let rows = nodes(s.render()).filter(node => node.type === 'article');
    assert.deepEqual(rows.map(row => followButton(row).props['aria-pressed']), [true, false]);
    assert.equal(followButton(rows[1]).props.disabled, false);
    followButton(rows[1]).props.onClick();
    await settlePromises();
    rows = nodes(s.module.flush()).filter(node => node.type === 'article');
    assert.deepEqual(rows.map(row => followButton(row).props['aria-pressed']), [true, true]);
    assert.deepEqual(s.state.selectedServerIds, ['a', 'b']);
    followButton(rows[0]).props.onClick();
    await settlePromises();
    rows = nodes(s.module.flush()).filter(node => node.type === 'article');
    assert.deepEqual(rows.map(row => followButton(row).props['aria-pressed']), [false, true]);
    assert.deepEqual(s.state.selectedServerIds, ['b']);
  } finally { s.module.unmount(); }
});

test('settings count all automatic targets and open the host controls while preserving selection', () => {
  const s = fixture('./CodexSshSyncSettingsControl.tsx', 'CodexSshSyncSettingsControl');
  s.state.selectedServerIds = ['a', 'b'];
  try {
    const tree = s.render();
    assert.equal(nodes(tree).find(node => node.props.className === 'row-desc').props.children,
      '2 台主机已选择跟随本地账户');
    const button = nodes(tree).find(node => node.type === 'button');
    button.props.onClick();
    assert.equal(s.navigations(), 1);
    assert.deepEqual(s.state.selectedServerIds, ['a', 'b']);
  } finally { s.module.unmount(); }
});

test('a successful file write with failed client refresh remains a failed host status with retry guidance', () => {
  const s = fixture('./CodexSshServersPanel.tsx', 'CodexSshServersPanel');
  s.state.servers[0].last_sync = { verified: true, error: 'ssh_connection_refresh_failed: account_mismatch' };
  try {
    const status = nodes(s.render()).find(node => node.props.className === 'codex-hosts__sync is-failed');
    assert.ok(status);
    assert.equal(status.props.children, '失败');
    assert.ok(status.props.title.includes('鉴权已同步'));
    assert.ok(status.props.title.includes('重试'));
  } finally { s.module.unmount(); }
});
