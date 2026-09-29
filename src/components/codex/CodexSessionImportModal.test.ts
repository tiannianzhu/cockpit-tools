import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';
import ts from 'typescript';

const component = ts.createSourceFile('import-modal.tsx', readFileSync(
  new URL('./CodexSessionImportModal.tsx', import.meta.url), 'utf8',
), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
let importHandler: ts.Expression | undefined;
function visit(node: ts.Node) {
  if (ts.isVariableDeclaration(node) && node.name.getText(component) === 'handleImportSelectedSessions') {
    importHandler = node.initializer;
  }
  ts.forEachChild(node, visit);
}
visit(component);
assert.ok(importHandler, 'Expected the session import handler');
const handlerCode = ts.transpileModule(`globalThis.runImport = ${importHandler.getText(component)}`, {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS },
}).outputText;

function harness(validationErrors?: Record<string, string>) {
  const events: string[] = [];
  const imports: unknown[][] = [];
  const mappings: unknown[] = [];
  const source = {
    ...(validationErrors === undefined ? {} : {
      validatePaths: async (value: unknown) => {
        events.push('validate');
        mappings.push(value);
        return validationErrors;
      },
    }),
    import: async (...args: unknown[]) => {
      events.push('import');
      imports.push(args);
      return { message: 'Imported' };
    },
  };
  const state: Record<string, any> = {
    source, importPreview: {}, requireTarget: true, effectiveTargetId: 'target', filePath: '/fixture.zip',
    selectedImportIds: ['selected'], selectedImportIdSet: new Set(['selected']),
    importReadyItems: [{ sessionId: 'selected', cwd: ' /old ' }, { sessionId: 'other', cwd: '/unselected' }],
    importCwdMappings: { ' /old ': ' /new ', '/unselected': '/other', '/blank': ' ', '/same': '/same' },
    setImporting: (value: boolean) => { state.importing = value; },
    setImportModalError: (value: unknown) => { state.error = value; },
    setImportPathErrors: (value: unknown) => { state.pathErrors = value; },
    onImportStart: () => { events.push('start'); },
    onMessage: () => {}, onError: () => {}, onChanged: async () => {},
    onClose: () => { events.push('close'); }, t: (_key: string, fallback: string) => fallback,
  };
  vm.runInContext(handlerCode, vm.createContext(state));
  return { state, events, imports, mappings };
}

const plain = (value: unknown) => JSON.parse(JSON.stringify(value));

test('local import validates trimmed mappings for selected source directories before starting transfer', async () => {
  const h = harness({});
  h.state.selectedImportIds.push('blank', 'same');
  h.state.selectedImportIdSet = new Set(h.state.selectedImportIds);
  h.state.importReadyItems.push({ sessionId: 'blank', cwd: '/blank' }, { sessionId: 'same', cwd: '/same' });
  await h.state.runImport();
  assert.deepEqual(plain(h.mappings), [{ '/old': '/new' }]);
  assert.deepEqual(h.events, ['validate', 'start', 'import', 'close']);
  assert.deepEqual(plain(h.imports[0]), ['/fixture.zip', ['selected', 'blank', 'same'], 'target', { '/old': '/new' }]);
  assert.equal(h.state.importing, false);
});

test('invalid local mappings retain the modal and prevent import or transfer start', async () => {
  const h = harness({ '/old': 'cwdTargetMissing' });
  await h.state.runImport();
  assert.deepEqual(plain(h.state.pathErrors), { '/old': 'cwdTargetMissing' });
  assert.deepEqual(h.events, ['validate']);
  assert.equal(h.imports.length, 0);
  assert.equal(h.state.importing, false);
});

test('remote sources without mapping validation keep their original three argument import contract', async () => {
  const h = harness();
  h.state.requireTarget = false;
  h.state.effectiveTargetId = '';
  await h.state.runImport();
  assert.deepEqual(h.events, ['start', 'import', 'close']);
  assert.equal(h.imports[0].length, 3);
  assert.equal(h.imports[0][2], undefined);
  assert.equal(h.mappings.length, 0);
});
