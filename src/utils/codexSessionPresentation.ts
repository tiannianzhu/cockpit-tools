import type { CodexSessionRecord, CodexSessionTokenStats } from '../types/codex';

export type SessionTree = CodexSessionRecord & { children?: SessionTree[] };

export type SessionGroup = {
  cwd: string;
  /** 官方客户端项目名（可重命名），优先用作分组标题。 */
  projectName?: string | null;
  sessions: SessionTree[];
  latestUpdatedAt: number;
};

export function buildGroups(sessions: SessionTree[]): SessionGroup[] {
  const groups = new Map<string, SessionTree[]>();
  sessions.forEach((session) => {
    const bucket = groups.get(session.cwd) ?? [];
    bucket.push(session);
    groups.set(session.cwd, bucket);
  });

  return Array.from(groups.entries())
    .map(([cwd, groupSessions]) => {
      const projectName = groupSessions.find((session) => session.projectName?.trim())?.projectName ?? null;
      return {
        cwd,
        projectName,
        sessions: [...groupSessions].sort(
          (left, right) => (right.updatedAt ?? 0) - (left.updatedAt ?? 0) || left.title.localeCompare(right.title),
        ),
        latestUpdatedAt: Math.max(...groupSessions.map((item) => item.updatedAt ?? 0), 0),
      };
    })
    .sort(
      (left, right) =>
        right.latestUpdatedAt - left.latestUpdatedAt || left.cwd.localeCompare(right.cwd, 'zh-CN'),
    );
}

export function formatRelativeTime(value: number | null | undefined, isZh: boolean): string {
  if (!value) return isZh ? '时间未知' : 'Unknown';
  const diffSeconds = Math.max(0, Math.floor(Date.now() / 1000) - value);
  const minute = 60;
  const hour = 60 * minute;
  const day = 24 * hour;
  const week = 7 * day;

  if (diffSeconds < hour) {
    const minutes = Math.max(1, Math.floor(diffSeconds / minute));
    return isZh ? `${minutes} 分钟` : `${minutes}m`;
  }
  if (diffSeconds < day) {
    const hours = Math.floor(diffSeconds / hour);
    return isZh ? `${hours} 小时` : `${hours}h`;
  }
  if (diffSeconds < week) {
    const days = Math.floor(diffSeconds / day);
    return isZh ? `${days} 天` : `${days}d`;
  }
  const weeks = Math.floor(diffSeconds / week);
  return isZh ? `${weeks} 周` : `${weeks}w`;
}

export function resolveGroupLabel(cwd: string, projectName?: string | null): string {
  const trimmedProjectName = projectName?.trim();
  if (trimmedProjectName) return trimmedProjectName;
  const normalized = cwd.replace(/\\/g, '/').replace(/\/$/, '');
  const parts = normalized.split('/').filter(Boolean);
  return parts[parts.length - 1] || cwd;
}

export function formatSessionId(sessionId: string): string {
  if (sessionId.length <= 18) return sessionId;
  return `${sessionId.slice(0, 8)}...${sessionId.slice(-6)}`;
}

export function formatLargeNumber(value: number): string {
  if (value >= 1_000_000) {
    return `${(value / 1_000_000).toFixed(1)}M`;
  }
  if (value >= 1_000) {
    return `${(value / 1_000).toFixed(1)}K`;
  }
  return value.toLocaleString();
}


export function formatTokenStats(stats?: CodexSessionTokenStats): string {
  if (!stats) {
    return '';
  }
  const input = stats.inputTokens ?? 0;
  const output = stats.outputTokens ?? 0;
  const total = stats.totalTokens ?? 0;
  // #1510: when only total is available, show total-only instead of 0/0.
  if (input === 0 && output === 0) {
    if (total > 0) {
      return `${formatLargeNumber(total)} tokens`;
    }
    return '';
  }
  return `${formatLargeNumber(input)} / ${formatLargeNumber(output)} tokens`;
}


/** Only explicit parent IDs define hierarchy. Unattached agents are never promoted to main chats. */
export function buildSessionTrees(sessions: CodexSessionRecord[]): SessionTree[] {
  const nodes = new Map(sessions.map(session => [session.sessionId, { ...session, children: [] as SessionTree[] }]));
  const roots: SessionTree[] = [];
  for (const node of nodes.values()) {
    if (node.parentThreadId) {
      nodes.get(node.parentThreadId)?.children.push(node);
    } else if (node.sessionKind !== 'subagent') {
      roots.push(node);
    }
  }
  return roots;
}

export function flattenSessionTrees(sessions: SessionTree[]): SessionTree[] {
  return sessions.flatMap(session => [session, ...flattenSessionTrees(session.children ?? [])]);
}
