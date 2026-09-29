import { useTranslation } from 'react-i18next';
import { Search, X } from 'lucide-react';
import { SingleSelectDropdown } from '../SingleSelectDropdown';
import type { CodexArchiveFilter, CodexSessionKindFilter } from '../../utils/codexSessionFilters';

interface Props {
  query: string;
  onQueryChange: (value: string) => void;
  onClear: () => void;
  canClear: boolean;
  disabled: boolean;
  archive: CodexArchiveFilter;
  onArchiveChange: (value: CodexArchiveFilter) => void;
  kind: CodexSessionKindFilter;
  onKindChange: (value: CodexSessionKindFilter) => void;
}

export function CodexSessionSearch(props: Props) {
  const { t } = useTranslation();
  return <div className="codex-session-manager__search codex-session-manager__search--filters">
    <label className="codex-session-search-field">
      <div className="codex-session-search-field__control">
        <Search size={14} />
        <input type="text" value={props.query} onChange={(event) => props.onQueryChange(event.target.value)}
          aria-label={t('codex.sessionManager.search.titlePlaceholder', '按标题搜索')}
          placeholder={t('codex.sessionManager.search.titlePlaceholder', '按标题搜索')} disabled={props.disabled} />
      </div>
    </label>
    <button className="btn btn-secondary codex-session-manager__search-button" type="button"
      onClick={props.onClear} disabled={props.disabled || !props.canClear}>
      <X size={14} />{t('codex.sessionManager.search.clear', '清空')}
    </button>
    <SingleSelectDropdown className="codex-session-manager__kind-filter" value={props.archive}
      disabled={props.disabled} onChange={(value) => props.onArchiveChange(value as CodexArchiveFilter)}
      ariaLabel={t('codex.sessionManager.archive.label', '归档状态')} menuWidth={160}
      options={[
        { value: 'active', label: t('codex.sessionManager.archive.active', '未归档') },
        { value: 'archived', label: t('codex.sessionManager.archive.archived', '已归档') },
        { value: 'all', label: t('codex.sessionManager.archive.all', '全部归档状态') },
      ]} />
    <SingleSelectDropdown className="codex-session-manager__kind-filter" value={props.kind}
      disabled={props.disabled} onChange={(value) => props.onKindChange(value as CodexSessionKindFilter)}
      ariaLabel={t('codex.sessionManager.kindFilter', '会话类型')} menuWidth={160} menuMaxHeight={220}
      options={[
        { value: 'conversation', label: t('codex.sessionManager.kind.conversation', '对话') },
        { value: 'external', label: t('codex.sessionManager.kind.external', '非交互任务') },
        { value: 'all', label: t('codex.sessionManager.kind.all', '全部类型') },
      ]} />
  </div>;
}
