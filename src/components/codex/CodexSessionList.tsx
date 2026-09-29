import { useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { ChevronDown, ChevronRight, Folder } from 'lucide-react';
import type { CodexSessionRecord } from '../../types/codex';
import { formatRelativeTime, formatSessionId, resolveGroupLabel, type SessionGroup, type SessionTree } from '../../utils/codexSessionPresentation';

interface Props {
  disabled?: boolean;
  groups: SessionGroup[];
  selectedIds: ReadonlySet<string>;
  expandedGroups: string[];
  onToggleGroup: (cwd: string) => void;
  onToggleGroupSelection: (ids: string[]) => void;
  onToggleSession: (id: string) => void;
  renderActions: (session: CodexSessionRecord) => ReactNode;
}

export function CodexSessionList({ groups, selectedIds, expandedGroups, onToggleGroup, onToggleGroupSelection, onToggleSession, renderActions, disabled = false }: Props) {
  const { t, i18n } = useTranslation();
  const isZh = i18n.language.startsWith('zh');
  return <>
    {groups.length > 0 ? (
      <div className="codex-session-manager__list">
        {groups.map((group) => {
          const groupSessionIds = group.sessions.map((item) => item.sessionId);
          const allSelected = groupSessionIds.every((id) => selectedIds.has(id));
          const isExpanded = expandedGroups.includes(group.cwd);
          return (
            <section className="codex-session-folder" key={group.cwd}>
              <div className="codex-session-folder__row">
                <div className="codex-session-folder__left">
                  <button
                    className="codex-session-folder__expand"
                    type="button"
                    onClick={() => onToggleGroup(group.cwd)}
                    aria-label={
                      isExpanded
                        ? t('codex.sessionManager.actions.collapse', '收起')
                        : t('codex.sessionManager.actions.expand', '展开')
                    }
                  >
                    {isExpanded ? <ChevronDown size={16} /> : <ChevronRight size={16} />}
                  </button>
                  <input
                    className="codex-session-folder__checkbox"
                    type="checkbox"
                    disabled={disabled}
                    checked={allSelected && groupSessionIds.length > 0}
                    onChange={() => onToggleGroupSelection(groupSessionIds)}
                  />
                  <Folder size={16} className="codex-session-folder__icon" />
                  <button
                    className="codex-session-folder__label"
                    type="button"
                    onClick={() => onToggleGroup(group.cwd)}
                    title={group.cwd}
                  >
                    {resolveGroupLabel(group.cwd, group.projectName) || t('common.unknown', '未知')}
                  </button>
                </div>
                <span className="codex-session-folder__time">
                  {formatRelativeTime(group.latestUpdatedAt, isZh)}
                </span>
              </div>
              {isExpanded ? (
                <div className="codex-session-folder__children">
                  {group.sessions.map(session => <SessionBranch key={session.sessionId}
                    session={session} selectedIds={selectedIds} onToggleSession={onToggleSession}
                    renderActions={renderActions} disabled={disabled} />)}
                </div>
              ) : null}
            </section>
          );
        })}
      </div>
    ) : null}
  </>;
}

function SessionBranch({ session, selectedIds, onToggleSession, renderActions, child = false, disabled = false }: {
  disabled?: boolean;
  session: SessionTree;
  selectedIds: ReadonlySet<string>;
  onToggleSession: (id: string) => void;
  renderActions: (session: CodexSessionRecord) => ReactNode;
  child?: boolean;
}) {
  const { t, i18n } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const children = session.children ?? [];
  const hasRunningLocation = session.locations.some(location => location.running);
  return <div className={`codex-session-branch${child ? ' codex-session-branch--child' : ''}`}>
    <div className="codex-session-row">
      <div className="codex-session-row__left">
        {!child && <input className="codex-session-row__checkbox" type="checkbox"
          aria-label={t('codex.sessionManager.selectConversation', '选择对话：{{title}}', { title: session.title })}
          disabled={disabled} checked={selectedIds.has(session.sessionId)} onChange={() => onToggleSession(session.sessionId)} />}
        <div className="codex-session-row__content">
          <span className="codex-session-row__title" title={session.title}>{session.title || session.sessionId}</span>
          {session.locations.length > 0 && <span className="codex-session-row__meta">
            {session.locations.map(location => location.instanceName).join(' / ')}
            {hasRunningLocation ? t('codex.sessionManager.locationRunning', '（运行中）') : ''}
          </span>}
          <span className="codex-session-row__meta codex-session-row__session-id" title={session.sessionId}>
            {t('codex.sessionManager.labels.sessionId', '会话 ID')}: {formatSessionId(session.sessionId)}
            {child && session.archived && <> · {t('codex.sessionManager.archive.archived', '已归档')}</>}
          </span>
          {children.length > 0 && <button type="button" className="codex-session-branch__toggle"
            aria-expanded={expanded} onClick={() => setExpanded(value => !value)}>
            {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
            {t('codex.sessionManager.childAgents', '子代理（{{count}}）', { count: children.length })}
          </button>}
        </div>
      </div>
      <div className="codex-session-row__right">
        {renderActions(session)}
        <span className="codex-session-row__time">{formatRelativeTime(session.updatedAt, i18n.language.startsWith('zh'))}</span>
      </div>
    </div>
    {expanded && children.length > 0 && <div className="codex-session-branch__children">
      {children.map(item => <SessionBranch key={item.sessionId} session={item} selectedIds={selectedIds}
        onToggleSession={onToggleSession} renderActions={renderActions} disabled={disabled} child />)}
    </div>}
  </div>;
}
