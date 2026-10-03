import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import test from 'node:test';
import vm from 'node:vm';
import ts from 'typescript';
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
