import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import test from 'node:test';
import vm from 'node:vm';
import ts from 'typescript';
import { resolveCodexApiProviderPresetId, CODEX_API_PROVIDER_CUSTOM_ID } from '../../utils/codexProviderPresets';
import { isCodexApiKeyAccount } from '../../types/codex';
import type { CodexAccount } from '../../types/codex';
import {
  resolveCodexModelProviderAccountName,
  shouldSyncCodexModelProviderAccountName,
} from '../../utils/codexModelProviderAccountName';

const controller = ts.createSourceFile('controller.tsx', readFileSync(
  new URL('./CodexModelProviderManager.tsx', import.meta.url), 'utf8',
), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
const view = ts.createSourceFile('view.tsx', readFileSync(
  new URL('./CodexModelProviderManagerView.tsx', import.meta.url), 'utf8',
), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);

function expression(source: ts.SourceFile, matches: (node: ts.Node) => boolean): string {
  let found: ts.Node | undefined;
  const visit = (node: ts.Node) => {
    if (matches(node)) found = node;
    ts.forEachChild(node, visit);
  };
  visit(source);
  assert.ok(found, 'Expected the provider editor handler');
  return found.getText(source);
}

function elements(value: any): any[] {
  if (Array.isArray(value)) return value.flatMap(elements);
  return value?.props ? [value, ...elements(value.props.children)] : [];
}

function harness() {
  const key = { id: 'key', name: 'Original', apiKey: 'sk-fixture' };
  const provider = { id: 'provider', name: 'Relay', baseUrl: 'https://relay.example/v1', apiKeys: [key] };
  const writes: unknown[][] = [];
  const renamedAccounts: unknown[][] = [];
  const state: Record<string, any> = {
    exports: {},
    require: createRequire(import.meta.url),
    useCallback: (callback: unknown) => callback,
    // Desktop WebViews may not implement browser prompts. The editor must work without one.
    window: { prompt() { throw new Error('Browser prompts are unavailable'); } },
    currentEditingProvider: provider, providers: [provider], saving: false, editingApiKey: null,
    accounts: [
      { id: 'linked', account_name: key.name, api_provider_id: provider.id, openai_api_key: key.apiKey },
      { id: 'custom', account_name: 'My custom name', api_provider_id: provider.id, openai_api_key: key.apiKey },
      { id: 'other', account_name: key.name, api_provider_id: provider.id, openai_api_key: 'sk-other-fixture' },
    ],
    t: (_name: string, fallback: any) => typeof fallback === 'string' ? fallback
      : fallback.defaultValue.replace('{{error}}', fallback.error),
    setFormError(value: unknown) { state.formError = value; },
    setNotice(value: unknown) { state.notice = value; },
    setSaving(value: boolean) { state.saving = value; },
    setEditingApiKey(value: any) {
      state.editingApiKey = typeof value === 'function' ? value(state.editingApiKey) : value;
    },
    renameApiKeyOnCodexModelProvider: async (...args: unknown[]) => { writes.push(args); },
    updateCodexAccountName: async (...args: unknown[]) => { renamedAccounts.push(args); },
    emitAccountsChanged: async () => {}, reloadProviders: async () => {},
    normalizeCodexModelProviderBaseUrl: (value: string) => value.replace(/\/$/, ''),
    resolveCodexModelProviderAccountName, shouldSyncCodexModelProviderAccountName,
    isCodexApiKeyAccount: () => true, parseServiceError: (error: unknown) => String(error),
    maskApiKey: () => 'sk-****', Check: 'check', X: 'close', KeyRound: 'key-icon',
    Pencil: 'pencil', Trash2: 'trash',
  };
  const context = vm.createContext(state);
  function load(name: string, source: string) {
    vm.runInContext(ts.transpileModule(`globalThis.${name} = ${source}`, {
      compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
    }).outputText, context);
  }
  for (const name of ['handleRenameApiKey', 'handleSaveApiKeyRename']) {
    const declaration = expression(controller, (node) => ts.isVariableDeclaration(node) && node.name.getText(controller) === name);
    load(name, declaration.slice(declaration.indexOf('=') + 1));
  }
  load('renderRow', expression(view, (node) => ts.isArrowFunction(node) &&
    node.getText(view).includes('const isEditing = editingApiKey?.apiKeyId === item.id')));
  const row = () => elements(state.renderRow(key));
  const button = (title: string) => row().find((node) => node.type === 'button' && node.props.title === title)!;
  return { state, writes, renamedAccounts, key, row, button };
}

test('rename opens an inline name editor without a browser prompt and saves only the name', async () => {
  const h = harness();
  h.button('重命名').props.onClick();
  const inputs = h.row().filter((node) => node.type === 'input');
  assert.equal(inputs.length, 1);
  assert.equal(inputs[0].props.type, 'text');
  assert.equal(inputs[0].props.value, 'Original');
  assert.equal(inputs[0].props.autoFocus, true);
  assert.equal(h.writes.length, 0);
  inputs[0].props.onChange({ target: { value: '  新名称 🔑  ' } });
  h.row()[0].props.onSubmit({ preventDefault() {} });
  // Await the submit handler's persistence and account reconciliation.
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.deepEqual(h.writes, [['provider', 'key', '新名称 🔑']]);
  assert.deepEqual(h.renamedAccounts, [['linked', '新名称 🔑']]);
  assert.equal(h.key.apiKey, 'sk-fixture');
  assert.equal(h.state.editingApiKey, null);
  assert.equal(h.state.saving, false);
});

test('cancel and Escape discard the draft while a busy rename cannot submit again', async () => {
  const h = harness();
  h.button('重命名').props.onClick();
  h.button('Cancel').props.onClick();
  assert.equal(h.state.editingApiKey, null);
  h.button('重命名').props.onClick();
  let stopped = false;
  h.row()[0].props.onKeyDown({ key: 'Escape', preventDefault() {}, stopPropagation() { stopped = true; } });
  assert.equal(stopped, true);
  assert.equal(h.state.editingApiKey, null);
  h.button('重命名').props.onClick();
  h.state.saving = true;
  assert.equal(h.button('Save').props.disabled, true);
  await h.state.handleSaveApiKeyRename();
  assert.equal(h.writes.length, 0);
});

test('a failed rename retains the editable draft and reports the error inside the provider modal', async () => {
  const h = harness();
  h.button('重命名').props.onClick();
  h.row().find((node) => node.type === 'input').props.onChange({ target: { value: 'Retry this name' } });
  h.state.renameApiKeyOnCodexModelProvider = async () => { throw new Error('write failed'); };
  await h.state.handleSaveApiKeyRename();
  assert.equal(h.state.editingApiKey.name, 'Retry this name');
  assert.match(h.state.formError, /write failed/);
  assert.equal(h.state.saving, false);
  assert.equal(h.state.notice, null);
  assert.equal(h.renamedAccounts.length, 0);
});

// Execute the actual save handler with isolated IPC, rather than writing real account files.
const source = readFileSync(new URL('./CodexModelProviderManager.tsx', import.meta.url), 'utf8');
const start = source.indexOf('  const handleSaveApiKeyEdit =');
const end = source.indexOf('  }, [', start);
assert.ok(start > 0 && end > start);
const handler = `${source.slice(start, end)}  }, []); globalThis.save = handleSaveApiKeyEdit;`;

test('editing only a provider key retains each linked account mode and its model settings', async () => {
  const provider = {
    id: 'provider', name: 'Official endpoint', baseUrl: 'https://api.openai.com/v1',
    modelCatalog: ['provider-default'], wireApi: 'responses', supportsWebsockets: true,
    apiKeys: [{ id: 'key', apiKey: 'new-key', name: 'Key' }],
  };
  const account = {
    email: 'api@example.com', tokens: { access_token: '', id_token: '' }, created_at: 1, last_used: 1,
    id: 'old-id', auth_mode: 'apikey', openai_api_key: 'old-key', api_base_url: provider.baseUrl,
    api_provider_mode: 'custom', api_model_catalog: ['account-model'], api_wire_api: 'responses',
    api_sync_model_catalog_to_codex: true, api_supports_vision: true,
    api_model_vision_support: { 'account-model': true }, api_vision_routing_model: 'account-model',
    api_model_context_windows: { 'account-model': 256000 }, api_supports_websockets: false,
    account_name: 'Original',
  } as CodexAccount;
  const updates: unknown[][] = [];
  const context: Record<string, any> = {
    useCallback: (fn: unknown) => fn, saving: false, providers: [provider], accounts: [account],
    editingApiKey: { mode: 'credentials', name: 'Key', providerId: 'provider', apiKeyId: 'key', apiKey: 'new-key', originalApiKey: 'old-key' },
    isCodexApiKeyAccount, resolveCodexApiProviderPresetId, CODEX_API_PROVIDER_CUSTOM_ID,
    normalizeCodexModelProviderBaseUrl: (url: string) => url.replace(/\/+$/, '').toLowerCase(),
    resolveProviderWireApi: () => 'responses',
    updateApiKeyOnCodexModelProvider: async () => provider,
    updateCodexApiKeyCredentials: async (...args: unknown[]) => { updates.push(args); },
    reloadProviders: async () => {}, emitAccountsChanged: async () => {},
    setSaving() {}, setNotice() {}, setEditingApiKey() {}, t: (key: string) => key,
  };
  vm.runInNewContext(ts.transpileModule(handler, { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText, context);
  await context.save();
  assert.equal(updates.length, 1);
  assert.equal(updates[0][1], 'new-key');
  assert.equal(updates[0][3], 'custom', 'official URL must not rewrite an existing custom mode');
  assert.deepEqual(updates[0][6], ['account-model']);
  assert.deepEqual(updates[0][8], { 'account-model': true });
  assert.equal(updates[0][9], 'account-model');
  assert.equal(updates[0][11], false);
  assert.equal(updates[0][12], true);
  assert.equal(updates[0][13], 'Original');
  assert.deepEqual(updates[0][14], { 'account-model': 256000 });
  account.api_provider_mode = 'openai_builtin';
  await context.save();
  assert.equal(updates[1][3], 'openai_builtin');
});
