import assert from 'node:assert/strict';
import test from 'node:test';
import { loadHookModule } from '../../tests/helpers/reactHookHarness';
import * as filters from '../utils/codexSessionFilters';
import * as presentation from '../utils/codexSessionPresentation';
import type { CodexSessionRecord } from '../types/codex';

function session(id: string, extra: Partial<CodexSessionRecord> = {}): CodexSessionRecord {
  return { sessionId: id, title: id, cwd: '/project', sessionKind: 'conversation',
    archived: false, locationCount: 1, locations: [], ...extra };
}
function setup(initial: CodexSessionRecord[]) {
  const harness = loadHookModule(new URL('./useCodexSessionList.ts', import.meta.url), {
    '../utils/codexSessionFilters': filters,
    '../utils/codexSessionPresentation': presentation,
  });
  const input = { sessions: initial, kind: 'all', archive: 'all', query: '' };
  const render = () => harness.render(() => harness.exports.useCodexSessionList(input.sessions, input.kind, input.archive, input.query));
  return { harness, input, render };
}

test('selection includes main conversations only and narrows with local filters', () => {
  const { input, render } = setup([
    session('active'), session('child', { parentThreadId: 'active', sessionKind: 'subagent' }),
    session('archived', { archived: true }), session('external', { sessionKind: 'external' }),
  ]);
  let view = render();
  assert.deepEqual(Array.from(view.allIds), ['active', 'archived', 'external']);
  assert.equal(view.visible[0].children[0].sessionId, 'child');
  view.toggleSelection(view.allIds);
  assert.equal(render().allSelected, true);
  input.archive = 'active'; input.kind = 'conversation';
  view = render();
  assert.deepEqual(Array.from(view.selectedIds), ['active']);
  input.query = 'unmatched';
  view = render();
  assert.equal(view.selectedIds.length, 0);
  assert.equal(view.allSelected, false);
  input.query = '';
  assert.equal(render().selectedIds.length, 0);
});

test('group and all toggles use current state even before the next render', () => {
  const { render } = setup([session('a'), session('b')]);
  let view = render();
  view.toggleSelection(['a']);
  view.toggleSelection(['a', 'b']);
  view = render();
  assert.deepEqual(Array.from(view.selectedIds), ['a', 'b']);
  assert.equal(view.allSelected, true);
  view.toggleSelection(['a', 'b']);
  view = render();
  assert.equal(view.selectedIds.length, 0);
  view.toggleSelection([]);
  assert.equal(render().selectedIds, view.selectedIds);
});

test('refresh removes missing selections while preserving selections across metadata updates', () => {
  const { input, render } = setup([session('a'), session('b')]);
  let view = render();
  view.toggleSelection(view.allIds);
  view = render();
  const selected = view.selectedIds;
  input.sessions = [session('a', { title: 'renamed' }), session('b')];
  assert.equal(render().selectedIds, selected);
  input.sessions = [session('b')];
  view = render();
  assert.deepEqual(Array.from(view.selectedIds), ['b']);
  assert.equal(view.selectedSessions[0].sessionId, 'b');
  view.setSelectedIds([]);
  assert.equal(render().selectedSessions.length, 0);
});
