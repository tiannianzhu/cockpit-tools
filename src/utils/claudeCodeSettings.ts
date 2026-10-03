import { type ClaudeApiKeyField } from './claudeProviderPresets';
import { deriveAutoCompactTokenLimit } from './codexModelContext';

export const CLAUDE_CODE_MODEL_ROLES = ['HAIKU', 'SONNET', 'OPUS', 'FABLE'] as const;
export type ClaudeCodeModelRole = (typeof CLAUDE_CODE_MODEL_ROLES)[number];

export interface ClaudeCodeModelOption extends Record<string, unknown> {
  model: string;
  label: string;
}

export interface ClaudeCodeSettingsFields {
  baseUrl: string;
  apiKey: string;
  apiKeyField: ClaudeApiKeyField;
  model: string;
  subagentModel: string;
  modelOptions: ClaudeCodeModelOption[];
  models: Record<ClaudeCodeModelRole, { model: string; name: string }>;
}

type SettingsDocument = Record<string, unknown>;

export function parseClaudeCodeSettings(content: string): SettingsDocument {
  let document: unknown;
  try { document = JSON.parse(content); } catch { throw new Error('CLAUDE_SETTINGS_INVALID_JSON'); }
  if (!document || typeof document !== 'object' || Array.isArray(document)) {
    throw new Error('CLAUDE_SETTINGS_INVALID_OBJECT');
  }
  const result = document as SettingsDocument;
  if (result.env !== undefined && (!result.env || typeof result.env !== 'object' || Array.isArray(result.env))) {
    throw new Error('CLAUDE_SETTINGS_INVALID_ENV');
  }
  return result;
}

function stringValue(value: unknown): string { return typeof value === 'string' ? value : ''; }

function asRecord(value: unknown): SettingsDocument {
  return value && typeof value === 'object' && !Array.isArray(value) ? value as SettingsDocument : {};
}

export function readClaudeCodeSettingsFields(content: string): ClaudeCodeSettingsFields {
  const document = parseClaudeCodeSettings(content);
  const env = (document.env ?? {}) as SettingsDocument;
  const apiKeyField = stringValue(env.ANTHROPIC_AUTH_TOKEN) ? 'ANTHROPIC_AUTH_TOKEN' : 'ANTHROPIC_API_KEY';
  const options = asRecord(document.modelPicker).options;
  return {
    baseUrl: stringValue(env.ANTHROPIC_BASE_URL),
    apiKey: stringValue(env[apiKeyField]),
    apiKeyField,
    model: stringValue(env.ANTHROPIC_MODEL) || stringValue(document.model),
    subagentModel: stringValue(env.CLAUDE_CODE_SUBAGENT_MODEL),
    modelOptions: Array.isArray(options) ? options.flatMap((option) => {
      const row = asRecord(option);
      return typeof row.model === 'string' ? [{ ...row, model: row.model, label: stringValue(row.label) }] : [];
    }) : [],
    models: Object.fromEntries(CLAUDE_CODE_MODEL_ROLES.map((role) => [role, {
      model: stringValue(env[`ANTHROPIC_DEFAULT_${role}_MODEL`]),
      name: stringValue(env[`ANTHROPIC_DEFAULT_${role}_MODEL_NAME`]),
    }])) as ClaudeCodeSettingsFields['models'],
  };
}

/** Patch form fields and a known default model's window; preserve other settings and env values. */
export function patchClaudeCodeSettings(
  content: string, fields: ClaudeCodeSettingsFields,
  models: { id: string; contextWindow?: number | null }[] = [],
): string {
  const document = parseClaudeCodeSettings(content);
  const previous = readClaudeCodeSettingsFields(content);
  const env = (document.env ?? {}) as SettingsDocument;
  const assign = (object: SettingsDocument, key: string, value: string) => {
    if (value.trim()) object[key] = value.trim(); else delete object[key];
  };
  if (fields.baseUrl !== previous.baseUrl) assign(env, 'ANTHROPIC_BASE_URL', fields.baseUrl);
  if (fields.apiKey !== previous.apiKey || fields.apiKeyField !== previous.apiKeyField) {
    delete env.ANTHROPIC_AUTH_TOKEN;
    delete env.ANTHROPIC_API_KEY;
    assign(env, fields.apiKeyField, fields.apiKey);
  }
  if (fields.model !== previous.model) {
    // Keep the existing override location so the env variable cannot shadow a new top-level model.
    if (!fields.model.trim()) { delete env.ANTHROPIC_MODEL; delete document.model; }
    else if (typeof env.ANTHROPIC_MODEL === 'string' && env.ANTHROPIC_MODEL) assign(env, 'ANTHROPIC_MODEL', fields.model);
    else assign(document, 'model', fields.model);
  }
  if (fields.subagentModel !== previous.subagentModel) assign(env, 'CLAUDE_CODE_SUBAGENT_MODEL', fields.subagentModel);
  for (const role of CLAUDE_CODE_MODEL_ROLES) {
    if (fields.models[role].model !== previous.models[role].model) assign(env, `ANTHROPIC_DEFAULT_${role}_MODEL`, fields.models[role].model);
    if (fields.models[role].name !== previous.models[role].name) assign(env, `ANTHROPIC_DEFAULT_${role}_MODEL_NAME`, fields.models[role].name);
  }
  const role = CLAUDE_CODE_MODEL_ROLES.find((role) => role.toLowerCase() === fields.model.trim().toLowerCase());
  const model = (role ? fields.models[role].model : fields.model).trim().toLowerCase();
  const contextWindow = models.find((item) => item.id.trim().toLowerCase() === model)?.contextWindow;
  if (typeof contextWindow === 'number' && Number.isSafeInteger(contextWindow) && contextWindow > 0) {
    const autoCompactWindow = deriveAutoCompactTokenLimit(contextWindow);
    env.CLAUDE_CODE_MAX_CONTEXT_TOKENS = String(contextWindow);
    document.autoCompactWindow = autoCompactWindow;
    // An existing environment override must not shadow the new settings value.
    if (env.CLAUDE_CODE_AUTO_COMPACT_WINDOW !== undefined) {
      env.CLAUDE_CODE_AUTO_COMPACT_WINDOW = String(autoCompactWindow);
    }
  }
  if (document.env !== undefined || Object.keys(env).length) document.env = env;
  return JSON.stringify(document, null, 2) + '\n';
}

/** Merge discovered models while preserving custom labels, metadata, and model selections. */
export function mergeClaudeCodeModelOptions(
  current: ClaudeCodeModelOption[], discovered: { id: string; displayName?: string | null }[],
): ClaudeCodeModelOption[] {
  const result = current.map((option) => ({ ...option }));
  const byId = new Map(result.map((option) => [option.model.trim().toLowerCase(), option]));
  for (const item of discovered) {
    const model = item.id.trim();
    const key = model.toLowerCase();
    if (!model) continue;
    const label = item.displayName?.trim();
    const existing = byId.get(key);
    if (existing) {
      if (!existing.label.trim() || existing.label.trim() === existing.model.trim()) {
        existing.label = label || existing.model.trim();
      }
      continue;
    }
    const option = { model, label: label || model };
    result.push(option);
    byId.set(key, option);
  }
  return result;
}

/** Follow model choices with a suggested menu name, keeping manually overridden names. */
export function updateClaudeCodeModelMapping(
  fields: ClaudeCodeSettingsFields, role: ClaudeCodeModelRole, model: string,
  options: ClaudeCodeModelOption[],
): ClaudeCodeSettingsFields {
  const previous = fields.models[role];
  const displayName = (id: string) => options.find(
    (option) => option.model.trim().toLowerCase() === id.trim().toLowerCase(),
  )?.label.trim() || id.trim();
  const previousName = previous.name.trim();
  const name = !previousName || previousName === previous.model.trim() || previousName === displayName(previous.model)
    ? displayName(model) : previous.name;
  if (model === previous.model && name === previous.name) return fields;
  return { ...fields, models: { ...fields.models, [role]: { model, name } } };
}
