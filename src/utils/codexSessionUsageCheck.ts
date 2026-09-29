import type { CodexSessionUsageSyncResult } from '../types/codex';

// One check per mounted manager; view changes share an in-flight request.
export function createInitialSessionUsageCheck(sync: () => Promise<CodexSessionUsageSyncResult>) {
  let pending: Promise<CodexSessionUsageSyncResult> | undefined;
  let completed = false;
  return (): Promise<CodexSessionUsageSyncResult | undefined> => {
    if (completed) return Promise.resolve(undefined);
    return pending ??= sync().finally(() => { completed = true; });
  };
}
