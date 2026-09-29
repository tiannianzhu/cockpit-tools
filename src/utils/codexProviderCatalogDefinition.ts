const REASONING_LABELS: Record<string, string> = { none: 'None', minimal: 'Minimal', low: 'Low', medium: 'Medium', high: 'High', xhigh: 'Extra High', max: 'Max', ultra: 'Ultra' };

export const CODEX_REASONING_EFFORT_ORDER = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra'];

function orderReasoningLevels(model: Record<string, unknown>) {
  if (!Array.isArray(model.supported_reasoning_levels)) return;
  model.supported_reasoning_levels.sort((a, b) =>
    CODEX_REASONING_EFFORT_ORDER.indexOf(a.effort) - CODEX_REASONING_EFFORT_ORDER.indexOf(b.effort));
}

export interface CodexProviderCatalogDefinition {
  models: Array<Record<string, unknown> & { slug: string }>;
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function positiveInteger(value: unknown): boolean {
  return typeof value === 'number' && Number.isSafeInteger(value) && value > 0;
}

/** Validate imported JSON without discarding fields understood by newer Codex versions. */
export function parseCodexProviderCatalogDefinition(value: unknown): CodexProviderCatalogDefinition {
  if (!record(value) ||
      !Array.isArray(value.models) || value.models.length === 0) {
    throw new Error('Expected a nonempty models array');
  }
  const seen = new Set<string>();
  for (const [index, model] of value.models.entries()) {
    if (!record(model) || typeof model.slug !== 'string' || !model.slug.trim() ||
        model.slug !== model.slug.trim()) {
      throw new Error(`Model ${index + 1} needs a valid slug`);
    }
    if (model.display_name !== undefined && model.display_name !== null && typeof model.display_name !== 'string') {
      throw new Error(`${model.slug}: display_name must be a string`);
    }
    const key = model.slug.toLowerCase();
    if (seen.has(key)) throw new Error(`Duplicate model slug: ${model.slug}`);
    seen.add(key);
    for (const field of ['context_window', 'max_context_window', 'auto_compact_token_limit']) {
      if (model[field] !== undefined && model[field] !== null && !positiveInteger(model[field])) {
        throw new Error(`${model.slug}: ${field} must be a positive integer`);
      }
    }
    if (model.input_modalities !== undefined &&
        (!Array.isArray(model.input_modalities) || model.input_modalities.length === 0 ||
          !model.input_modalities.includes('text') ||
          model.input_modalities.some((item) => item !== 'text' && item !== 'image') ||
          new Set(model.input_modalities).size !== model.input_modalities.length)) {
      throw new Error(`${model.slug}: input_modalities must contain text and optional image without duplicates`);
    }
    if (model.supported_reasoning_levels !== undefined) {
      if (!Array.isArray(model.supported_reasoning_levels) || model.supported_reasoning_levels.length === 0 ||
          model.supported_reasoning_levels.some((item) => !record(item) || typeof item.effort !== 'string' || !CODEX_REASONING_EFFORT_ORDER.includes(item.effort)) ||
          new Set(model.supported_reasoning_levels.map((item) => (item as { effort: string }).effort)).size !== model.supported_reasoning_levels.length) {
        throw new Error(`${model.slug}: supported_reasoning_levels needs effort entries`);
      }
    }
    if (model.default_reasoning_level !== undefined && model.default_reasoning_level !== null) {
      if (typeof model.default_reasoning_level !== 'string' || !model.default_reasoning_level.trim() ||
          (Array.isArray(model.supported_reasoning_levels) &&
            !model.supported_reasoning_levels.some((item) => record(item) && item.effort === model.default_reasoning_level))) {
        throw new Error(`${model.slug}: default_reasoning_level must occur in supported_reasoning_levels when specified`);
      }
    }
  }
  const definition = { models: structuredClone(value.models) } as CodexProviderCatalogDefinition;
  definition.models.forEach(orderReasoningLevels);
  return definition;
}

export function deriveCodexProviderCatalogFields(definition: CodexProviderCatalogDefinition) {
  const modelCatalog = definition.models.map((model) => model.slug);
  const modelContextWindows: Record<string, number> = {};
  const modelCapabilities: Record<string, { supportsVision: boolean }> = {};
  for (const model of definition.models) {
    if (positiveInteger(model.context_window)) modelContextWindows[model.slug] = model.context_window as number;
    if (Array.isArray(model.input_modalities)) {
      modelCapabilities[model.slug.toLowerCase()] = {
        supportsVision: model.input_modalities.includes('image'),
      };
    }
  }
  return { modelCatalog, modelContextWindows, modelCapabilities };
}

/** Match ordinary editor changes to the full override before it is saved. */
export function reconcileCodexProviderCatalogDefinition(
  definition: CodexProviderCatalogDefinition,
  catalog: string[],
  windows: Record<string, number>,
  capabilities: Record<string, { supportsVision?: boolean }>,
): CodexProviderCatalogDefinition {
  const bySlug = new Map(definition.models.map((model) => [model.slug.toLowerCase(), model]));
  const models = catalog.map((slug) => {
    const existing = bySlug.get(slug.toLowerCase());
    const next: Record<string, unknown> & { slug: string } = existing
      ? structuredClone(existing) : { slug, input_modalities: ['text'] };
    next.slug = slug;
    if (windows[slug] !== undefined) {
      const previousWindow = next.context_window;
      next.context_window = windows[slug];
      if (next.max_context_window === previousWindow) next.max_context_window = windows[slug];
      if (typeof next.auto_compact_token_limit === 'number' && next.auto_compact_token_limit > windows[slug]) {
        throw new Error(`${slug}: auto_compact_token_limit exceeds the edited context window`);
      }
    }
    else {
      if (next.max_context_window === next.context_window) delete next.max_context_window;
      delete next.context_window;
    }
    const vision = capabilities[slug.toLowerCase()]?.supportsVision;
    if (vision !== undefined) {
      const modalities = Array.isArray(next.input_modalities) ? next.input_modalities.filter((item) => item !== 'image') : ['text'];
      next.input_modalities = vision ? [...modalities, 'image'] : modalities;
    }
    return next;
  });
  if (models.length === 0) throw new Error('The imported catalog needs at least one model');
  return parseCodexProviderCatalogDefinition({ ...definition, models });
}

/** Build from editor state; importing a JSON definition is optional. */
export function buildCodexProviderCatalogDefinition(
  catalog: string[], windows: Record<string, number>,
  capabilities: Record<string, { supportsVision?: boolean }>,
  existing?: CodexProviderCatalogDefinition,
): CodexProviderCatalogDefinition | undefined {
  if (catalog.length === 0) return undefined;
  return reconcileCodexProviderCatalogDefinition(
    existing ?? { models: [] }, catalog, windows, capabilities,
  );
}

export function patchCodexProviderModel(
  definition: CodexProviderCatalogDefinition | undefined,
  model: string, patch: Record<string, unknown>,
): CodexProviderCatalogDefinition {
  const current = definition ?? { models: [] };
  const models = current.models.map((entry) => structuredClone(entry));
  let target = models.find((entry) => entry.slug.toLowerCase() === model.toLowerCase());
  if (!target) { target = { slug: model, input_modalities: ['text'] }; models.push(target); }
  for (const [key, value] of Object.entries(patch)) {
    if (key === 'slug') continue;
    if (value === null || value === undefined || (key === 'supported_reasoning_levels' && Array.isArray(value) && value.length === 0)) delete target[key];
    else target[key] = structuredClone(value);
  }
  orderReasoningLevels(target);
  return { models };
}

/** Build a fresh definition solely from upstream metadata; never merge old model fields. */
export function buildUpstreamModelDefinitions(
  models: Array<{ id: string; displayName?: string | null; contextWindow?: number | null;
    supportsVision?: boolean | null; reasoningEfforts?: string[] }>,
): CodexProviderCatalogDefinition {
  let result: CodexProviderCatalogDefinition = {
    models: models.map((model) => ({ slug: model.id, input_modalities: ['text'] })) };
  for (const model of models) {
    const patch: Record<string, unknown> = {};
    if (model.displayName?.trim()) patch.display_name = model.displayName.trim();
    if (positiveInteger(model.contextWindow)) {
      patch.context_window = model.contextWindow;
      patch.max_context_window = model.contextWindow;
    }
    if (typeof model.supportsVision === 'boolean') patch.input_modalities = model.supportsVision ? ['text', 'image'] : ['text'];
    const efforts = [...new Set((model.reasoningEfforts ?? []).map((effort) => effort.trim()).filter((effort) => CODEX_REASONING_EFFORT_ORDER.includes(effort)))];
    if (model.reasoningEfforts?.length && !efforts.length) throw new Error(`${model.id}: no supported Codex reasoning levels returned`);
    if (efforts.length) {
      patch.supported_reasoning_levels = efforts.map((effort) => ({ effort, description: REASONING_LABELS[effort] }));
      patch.default_reasoning_level = efforts[0];
    }
    result = patchCodexProviderModel(result, model.id, patch);
  }
  return result;
}
