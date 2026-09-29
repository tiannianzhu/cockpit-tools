import assert from 'node:assert/strict';
import test from 'node:test';
import { filterCodexSessionsByArchive } from './codexSessionFilters';

const sessions = [
  { id: 'active', archived: false },
  { id: 'archived', archived: true },
  { id: 'legacy' },
];
test('archive filter separates active and archived sessions, including legacy records', () => {
  assert.deepEqual(filterCodexSessionsByArchive(sessions, 'active').map(s => s.id), ['active', 'legacy']);
  assert.deepEqual(filterCodexSessionsByArchive(sessions, 'archived').map(s => s.id), ['archived']);
  assert.deepEqual(filterCodexSessionsByArchive(sessions, 'all'), sessions);
});

test('shared root filtering preserves children across archive states', async () => {
  const { filterCodexSessionRoots } = await import('./codexSessionFilters');
  const { buildSessionTrees } = await import('./codexSessionPresentation');
  const roots = buildSessionTrees([
    { sessionId: 'root', title: 'Project Notes', cwd: '/fixture', archived: false, locationCount: 1, locations: [] },
    { sessionId: 'child', title: 'Other title', cwd: '/other', archived: true, parentThreadId: 'root', sessionKind: 'subagent', locationCount: 1, locations: [] },
  ]);
  const shown = filterCodexSessionRoots(roots, 'conversation', 'active', ' NOTES ');
  assert.equal(shown.length, 1);
  assert.equal(shown[0].children?.[0].sessionId, 'child');
  assert.equal(filterCodexSessionRoots(roots, 'conversation', 'archived', '').length, 0);
});
