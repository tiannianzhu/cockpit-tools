import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { describe, it } from 'node:test';
import ts from 'typescript';
import {
  filterCodexSessionsByKind,
  type CodexSessionKindFilter,
} from '../src/utils/codexSessionFilters.ts';
import type { CodexSessionRecord } from '../src/types/codex.ts';

function makeSession(sessionId: string, sessionKind?: string): CodexSessionRecord {
  return {
    sessionId,
    sessionKind,
    title: sessionId,
    cwd: '/tmp/project',
    locationCount: 0,
    locations: [],
  };
}

const sessions = [
  makeSession('conversation', 'conversation'),
  makeSession('external', 'external'),
  makeSession('subagent', 'subagent'),
  makeSession('legacy'),
];

describe('codex session kind filter', () => {
  it('filters locally and treats missing kind as a conversation', () => {
    assert.deepEqual(
      filterCodexSessionsByKind(sessions, 'conversation').map((item) => item.sessionId),
      ['conversation', 'legacy'],
    );
  });

  it('returns all sessions without changing the list for the all filter', () => {
    const filter: CodexSessionKindFilter = 'all';
    assert.equal(filterCodexSessionsByKind(sessions, filter), sessions);
  });

  it('does not make the backend session loader depend on the local kind filter', () => {
    const source = readFileSync(
      `${process.cwd()}/src/components/codex/CodexSessionManager.tsx`,
      'utf8',
    );
    const file = ts.createSourceFile('CodexSessionManager.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
    let loaderSource: string | undefined;
    const visit = (node: ts.Node) => {
      if (ts.isVariableDeclaration(node) && node.name.getText(file) === 'loadSessions') {
        loaderSource = node.initializer?.getText(file);
      }
      ts.forEachChild(node, visit);
    };
    visit(file);

    assert.ok(loaderSource);
    assert.equal(loaderSource.includes('sessionKindFilter'), false);
    assert.ok(source.includes('useCodexSessionList(sessions, sessionKindFilter, archiveFilter, appliedTitleSearch)'));
  });
});
