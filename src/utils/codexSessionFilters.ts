import type { CodexSessionRecord } from '../types/codex';

export type CodexSessionKindFilter = 'all' | 'conversation' | 'external' | 'subagent';

export function filterCodexSessionsByKind<T extends CodexSessionRecord>(
  sessions: T[],
  filter: CodexSessionKindFilter,
): T[] {
  if (filter === 'all') {
    return sessions;
  }

  return sessions.filter(
    (session) => (session.sessionKind || 'conversation') === filter,
  );
}

export type CodexArchiveFilter = 'active' | 'archived' | 'all';

export function filterCodexSessionsByArchive<T extends { archived?: boolean }>(
  sessions: T[], filter: CodexArchiveFilter,
): T[] {
  if (filter === 'all') return sessions;
  return sessions.filter((session) => Boolean(session.archived) === (filter === 'archived'));
}

/** Filter only roots, retaining their full descendant tree across archive states. */
export function filterCodexSessionRoots<T extends CodexSessionRecord>(
  sessions: T[], kind: CodexSessionKindFilter, archive: CodexArchiveFilter, query: string,
): T[] {
  const search = query.trim().toLocaleLowerCase();
  return filterCodexSessionsByArchive(filterCodexSessionsByKind(sessions, kind), archive)
    .filter(session => session.title.toLocaleLowerCase().includes(search));
}
