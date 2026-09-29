import { useCallback, useEffect, useMemo, useState } from 'react';
import type { CodexSessionRecord } from '../types/codex';
import { filterCodexSessionRoots, type CodexArchiveFilter, type CodexSessionKindFilter } from '../utils/codexSessionFilters';
import { buildGroups, buildSessionTrees } from '../utils/codexSessionPresentation';

/** Shared list state; loading, search timing, and group expansion belong to each host view. */
export function useCodexSessionList(
  sessions: CodexSessionRecord[],
  kind: CodexSessionKindFilter,
  archive: CodexArchiveFilter,
  query: string,
) {
  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const trees = useMemo(() => buildSessionTrees(sessions), [sessions]);
  const visible = useMemo(() => filterCodexSessionRoots(trees, kind, archive, query), [trees, kind, archive, query]);
  const groups = useMemo(() => buildGroups(visible), [visible]);
  const allIds = useMemo(() => visible.map(session => session.sessionId), [visible]);
  const selectedSet = useMemo(() => new Set(selectedIds), [selectedIds]);
  const selectedSessions = useMemo(() => visible.filter(session => selectedSet.has(session.sessionId)), [visible, selectedSet]);
  const allSelected = allIds.length > 0 && allIds.every(id => selectedSet.has(id));

  useEffect(() => {
    const visibleIds = new Set(allIds);
    setSelectedIds(previous => {
      const next = previous.filter(id => visibleIds.has(id));
      return next.length === previous.length ? previous : next;
    });
  }, [allIds]);

  const toggleSelection = useCallback((ids: string[]) => {
    if (ids.length === 0) return;
    setSelectedIds(previous => {
      const next = new Set(previous);
      if (ids.every(id => next.has(id))) ids.forEach(id => next.delete(id));
      else ids.forEach(id => next.add(id));
      return [...next];
    });
  }, []);

  return { visible, groups, allIds, selectedIds, setSelectedIds, selectedSet,
    selectedSessions, allSelected, toggleSelection };
}
