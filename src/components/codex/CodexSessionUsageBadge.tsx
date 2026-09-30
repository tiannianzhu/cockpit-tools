import { useTranslation } from 'react-i18next';
import type { CodexSessionTokenStats } from '../../types/codex';
import { formatTokenStats } from '../../utils/codexSessionPresentation';
import { formatSessionCostEstimate } from '../../utils/codexSessionUsageFormat';

export function CodexSessionUsageBadge({ stats }: { stats?: CodexSessionTokenStats }) {
  const { t } = useTranslation();
  const tokens = formatTokenStats(stats);
  if (!tokens) return null;
  const cost = formatSessionCostEstimate(stats?.estimatedCostUsd);
  const title = t('codex.sessionManager.labels.sessionCostHint',
    'Estimated USD at model and recorded service-tier prices, including cached input and long-context rates. Missing historical tiers use Standard estimates. This session only; child agents are separate. Not the actual subscription or provider bill.');
  return <span className="codex-session-row__tokens" title={title}>
    {tokens} · {cost ?? t('codex.sessionManager.labels.sessionCostUnavailable', 'Cost unavailable')}
  </span>;
}
