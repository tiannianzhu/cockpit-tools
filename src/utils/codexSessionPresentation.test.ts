import assert from 'node:assert/strict';
import test from 'node:test';
import { buildGroups, buildSessionTrees, flattenSessionTrees, resolveGroupLabel } from './codexSessionPresentation';
import { filterCodexSessionsByArchive, filterCodexSessionsByKind } from './codexSessionFilters';
import type { CodexSessionRecord } from '../types/codex';

const record = (sessionId: string, cwd: string, updatedAt: number, sessionKind = 'conversation', archived = false): CodexSessionRecord => ({
  sessionId, cwd, updatedAt, sessionKind, archived, title: sessionId, locationCount: 1, locations: [],
});
test('shared list groups and sorts filtered sessions without mutating input', () => {
  const rows = [record('older', '/work/a', 1), record('review', '/work/a', 5, 'subagent'),
    record('archived', '/work/a', 6, 'conversation', true), record('newer', '/work/b', 3), record('same-project', '/work/a', 2)];
  const original = structuredClone(rows);
  const groups = buildGroups(filterCodexSessionsByArchive(filterCodexSessionsByKind(rows, 'conversation'), 'active'));
  assert.deepEqual(groups.map(g => [g.cwd, g.sessions.map(s => s.sessionId)]), [
    ['/work/b', ['newer']], ['/work/a', ['same-project', 'older']],
  ]);
  assert.deepEqual(rows, original);
});
test('project label prefers saved project name, otherwise uses directory basename', () => {
  assert.equal(resolveGroupLabel('/work/project', 'Research'), 'Research');
  assert.equal(resolveGroupLabel('/work/project'), 'project');
});

test('agents follow explicit parent IDs across projects and archive states', () => {
  const root = record('root', '/main', 1);
  const child = { ...record('child', '/other', 2, 'subagent', true), parentThreadId: 'root' };
  const grandchild = { ...record('grandchild', '/other', 3, 'subagent'), parentThreadId: 'child' };
  const orphan = { ...record('orphan', '/main', 4, 'subagent'), parentThreadId: 'missing' };
  const review = record('review', '/main', 5, 'subagent');
  const rows = [grandchild, child, root, orphan, review];
  const original = structuredClone(rows);
  const trees = buildSessionTrees(rows);
  assert.deepEqual(trees.map(row => row.sessionId), ['root']);
  assert.equal(trees[0].children?.[0].sessionId, 'child');
  assert.equal(trees[0].children?.[0].children?.[0].sessionId, 'grandchild');
  const active = filterCodexSessionsByArchive(trees, 'active');
  assert.equal(active[0].children?.[0].archived, true);
  assert.deepEqual(buildGroups(active)[0].sessions.map(row => row.sessionId), ['root']);
  assert.deepEqual(flattenSessionTrees(trees).map(row => row.sessionId), ['root', 'child', 'grandchild']);
  assert.deepEqual(rows, original);
});

test('unattached agents and cyclic relationships are not independently manageable', () => {
  const rows = [
    { ...record('a', '/', 1, 'subagent'), parentThreadId: 'b' },
    { ...record('b', '/', 1, 'subagent'), parentThreadId: 'a' },
    record('Approval review', '/', 1),
    record('external', '/', 1, 'external'),
  ];
  assert.deepEqual(buildSessionTrees(rows).map(row => row.sessionId), ['Approval review', 'external']);
});
