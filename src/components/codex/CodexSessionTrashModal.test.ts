import assert from 'node:assert/strict';
import test from 'node:test';
import { buildTrashTrees, familyRows, type CodexSessionTrashRow } from './CodexSessionTrashModal';

const row = (id: string, sessionId: string, familyId: string, parentThreadId?: string): CodexSessionTrashRow => ({
  id, sessionId, familyId, parentThreadId, cwd: '/project', title: id, deletedAt: 1, sizeBytes: 10,
});

test('trash selection keeps duplicate session IDs in independent batches', () => {
  const rows = [
    row('batch-a', 'same', 'a'),
    row('child-a', 'agent', 'a', 'same'),
    row('batch-b', 'same', 'b'),
    row('child-b', 'agent', 'b', 'same'),
  ];
  const trees = buildTrashTrees(rows);
  assert.deepEqual(trees.map(tree => tree.id), ['batch-a', 'batch-b']);
  assert.deepEqual(familyRows(trees[0]).map(item => item.id), ['batch-a', 'child-a']);
  assert.deepEqual(familyRows(trees[1]).map(item => item.id), ['batch-b', 'child-b']);
  assert.deepEqual(rows, [
    row('batch-a', 'same', 'a'), row('child-a', 'agent', 'a', 'same'),
    row('batch-b', 'same', 'b'), row('child-b', 'agent', 'b', 'same'),
  ]);
});

test('unattached subagents are never offered as main trash actions', () => {
  const trees = buildTrashTrees([
    { ...row('orphan', 'orphan', 'a', 'missing'), sessionKind: 'subagent' },
    { ...row('agent', 'agent', 'a'), sessionKind: 'subagent' },
    row('main', 'main', 'a'),
  ]);
  assert.deepEqual(trees.map(tree => tree.id), ['main']);
});
