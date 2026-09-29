import { useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { ChevronDown, Plus, RefreshCw, Trash2 } from 'lucide-react';
import { SingleSelectDropdown } from '../SingleSelectDropdown';
import { CODEX_REASONING_EFFORT_ORDER, type CodexProviderCatalogDefinition } from '../../utils/codexProviderCatalogDefinition';
import '../../styles/pages/codex-provider-models.css';

export interface CodexProviderModelsEditorProps {
  models: string[];
  contexts: Record<string, string>;
  visionStates: Record<string, boolean>;
  definition: CodexProviderCatalogDefinition | undefined;
  disabled: boolean;
  fetching?: boolean;
  canFetch?: boolean;
  onFetch: () => void;
  onModelsChange: (models: string[]) => void;
  onContextChange: (model: string, value: string) => void;
  onVisionChange: (model: string, value: boolean) => void;
  onModelPatch: (model: string, patch: Record<string, unknown>) => void;
}

const COMMON_EFFORTS = CODEX_REASONING_EFFORT_ORDER;

function reasoningLevels(model: Record<string, unknown> | undefined) {
  const value = model?.supported_reasoning_levels;
  if (!Array.isArray(value)) return [];
  return value.filter((item): item is Record<string, unknown> =>
    item !== null && typeof item === 'object' && typeof item.effort === 'string' && Boolean(item.effort.trim()));
}

export function CodexProviderModelsEditor({
  models, contexts, visionStates, definition, disabled, fetching = false, canFetch = false,
  onFetch, onModelsChange, onContextChange, onVisionChange, onModelPatch,
}: CodexProviderModelsEditorProps) {
  const { t } = useTranslation();
  const [draft, setDraft] = useState('');
  const [addError, setAddError] = useState('');
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const definitionBySlug = new Map(definition?.models.map((item) => [item.slug.toLowerCase(), item]) ?? []);

  function addModel(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const slug = draft.trim();
    if (!slug) return;
    if (/[\s,]/.test(slug)) {
      setAddError(t('codex.modelProviders.modelEditor.invalidId', '模型 ID 不能包含空格或逗号'));
      return;
    }
    if (models.some((model) => model.toLowerCase() === slug.toLowerCase())) {
      setAddError(t('codex.modelProviders.modelEditor.duplicate', '模型 ID 已存在'));
      return;
    }
    onModelsChange([...models, slug]);
    setDraft('');
    setAddError('');
  }

  return (
    <section className="codex-provider-models" aria-label={t('codex.modelProviders.modelEditor.title', '模型列表')}>
      <div className="codex-provider-models-heading">
        <div>
          <h3>{t('codex.modelProviders.modelEditor.title', '模型列表')}</h3>
          <p>{t('codex.modelProviders.modelEditor.hint', '远端应用前，请为每个模型填写上下文窗口并配置支持的推理级别。')}</p>
        </div>
        {canFetch && <button type="button" className="btn btn-secondary codex-provider-models-button" onClick={onFetch}
          disabled={disabled || fetching}>
          <RefreshCw size={14} aria-hidden="true" className={fetching ? 'codex-provider-models-spinning' : undefined} />
          {fetching ? t('codex.modelProviders.modelEditor.fetching', '获取中…') : t('codex.modelProviders.modelEditor.fetch', '从上游获取')}
        </button>}
      </div>

      <form className="codex-provider-models-add" onSubmit={addModel}>
        <input className="form-input" type="text" value={draft} onChange={(event) => { setDraft(event.target.value); setAddError(''); }}
          placeholder={t('codex.modelProviders.modelEditor.modelIdPlaceholder', '输入模型 ID')}
          aria-label={t('codex.modelProviders.modelEditor.modelId', '模型 ID')} aria-invalid={Boolean(addError)}
          disabled={disabled || fetching} />
        <button type="submit" className="btn btn-secondary codex-provider-models-button" disabled={disabled || fetching || !draft.trim()}>
          <Plus size={14} aria-hidden="true" />{t('codex.modelProviders.modelEditor.add', '添加模型')}
        </button>
      </form>
      {addError && <p className="codex-provider-models-error" role="alert">{addError}</p>}

      {models.length === 0 ? <p className="codex-provider-models-empty">{t('codex.modelProviders.modelEditor.empty', '尚未添加模型。')}</p> :
        <div className="codex-provider-models-list">
          {models.map((slug) => {
            const model = definitionBySlug.get(slug.toLowerCase());
            const levels = reasoningLevels(model);
            const currentDefault = typeof model?.default_reasoning_level === 'string' &&
              levels.some((item) => item.effort === model.default_reasoning_level)
              ? model.default_reasoning_level : '';
            const effortOptions = Array.from(new Set([...COMMON_EFFORTS]));
            const isExpanded = Boolean(expanded[slug]);
            const contextLabel = t('codex.modelProviders.modelEditor.context', '上下文窗口');
            const visionLabel = t('codex.modelProviders.modelEditor.vision', '图片输入');
            const reasoningLabel = t('codex.modelProviders.modelEditor.reasoning', '推理级别');

            return <div className="codex-provider-models-item" key={slug}>
              <div className="codex-provider-models-identity">
                <span className="codex-provider-models-slug" title={slug}>{slug}</span>
              </div>
              <div className="codex-provider-models-row">
                  <label className="codex-provider-models-field codex-provider-models-display-name">
                    <span>{t('codex.modelProviders.modelEditor.displayName', '显示名称')}</span>
                    <input className="form-input" type="text" value={typeof model?.display_name === 'string' ? model.display_name : ''}
                      placeholder={slug} aria-label={`${slug} ${t('codex.modelProviders.modelEditor.displayName', '显示名称')}`}
                      disabled={disabled} onChange={(event) => onModelPatch(slug, { display_name: event.target.value })} />
                  </label>
                <label className="codex-provider-models-field">
                  <span>{contextLabel}</span>
                  <input className="form-input" type="number" min="1" step="1" inputMode="numeric"
                    value={contexts[slug] ?? ''} onChange={(event) => onContextChange(slug, event.target.value)}
                    placeholder={t('codex.modelProviders.modelEditor.unset', '未设置')}
                    aria-label={`${slug} ${contextLabel}`} disabled={disabled} />
                </label>
                <label className="codex-provider-models-field codex-provider-models-vision">
                  <span>{visionLabel}</span>
                  <span className="api-model-vision-toggle">
                    <input type="checkbox" checked={(visionStates[slug.toLowerCase()] ?? visionStates[slug]) === true} onChange={(event) => onVisionChange(slug, event.target.checked)}
                      aria-label={`${slug} ${visionLabel}`} disabled={disabled} />
                    <span className="api-model-vision-switch" />
                  </span>
                </label>
                <button type="button" className="codex-provider-models-icon-button" title={t('codex.modelProviders.modelEditor.remove', '移除模型')}
                  aria-label={`${t('codex.modelProviders.modelEditor.remove', '移除模型')} ${slug}`}
                  disabled={disabled} onClick={() => onModelsChange(models.filter((item) => item !== slug))}>
                  <Trash2 size={15} aria-hidden="true" />
                </button>
              </div>
              <button type="button" className="codex-provider-models-expand" aria-expanded={isExpanded}
                onClick={() => setExpanded((current) => ({ ...current, [slug]: !isExpanded }))}>
                <ChevronDown size={14} aria-hidden="true" className={isExpanded ? 'expanded' : undefined} />
                {reasoningLabel}
                <span>{levels.length ? levels.map((item) => item.effort).join(', ') : t('codex.modelProviders.modelEditor.unconfigured', '未配置')}</span>
              </button>
              {isExpanded && <div className="codex-provider-models-reasoning">
                <fieldset disabled={disabled}>
                  <legend>{t('codex.modelProviders.modelEditor.supportedLevels', '支持的级别')}</legend>
                  <div className="codex-provider-models-efforts">
                    {effortOptions.map((effort) => {
                      const checked = levels.some((item) => item.effort === effort);
                      return <label key={effort} className="codex-provider-models-effort">
                        <input type="checkbox" checked={checked} onChange={() => {
                          const next = checked ? levels.filter((item) => item.effort !== effort)
                            : [...levels, { effort, description: effort }];
                          const nextDefault = next.some((item) => item.effort === currentDefault)
                            ? currentDefault : next[0]?.effort ?? null;
                          onModelPatch(slug, {
                            supported_reasoning_levels: next.length ? next : null,
                            default_reasoning_level: nextDefault,
                          });
                        }} />
                        <span>{effort}</span>
                      </label>;
                    })}
                  </div>
                </fieldset>
                <div className="codex-provider-models-default">
                  <label>{t('codex.modelProviders.modelEditor.defaultLevel', '默认级别')}</label>
                  <SingleSelectDropdown value={currentDefault} options={levels.map((item) => ({
                    value: item.effort as string, label: item.effort as string,
                  }))} placeholder={t('codex.modelProviders.modelEditor.unconfigured', '未配置')}
                    onChange={(value) => onModelPatch(slug, { default_reasoning_level: value })}
                    disabled={disabled || levels.length === 0} ariaLabel={`${slug} ${t('codex.modelProviders.modelEditor.defaultLevel', '默认级别')}`}
                    className="codex-provider-models-dropdown" menuPlacement="up" />
                </div>
              </div>}
            </div>;
          })}
        </div>}
    </section>
  );
}
