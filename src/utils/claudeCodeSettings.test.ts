import assert from 'node:assert/strict';
import test from 'node:test';
import {
  CLAUDE_CODE_MODEL_ROLES,
  mergeClaudeCodeModelOptions, updateClaudeCodeModelMapping,
  patchClaudeCodeSettings, readClaudeCodeSettingsFields, parseClaudeCodeSettings,
} from './claudeCodeSettings';

const fixture = JSON.stringify({
  model: 'main-model', language: 'zh', permissions: { allow: ['Read'] },
  env: { ANTHROPIC_AUTH_TOKEN: 'fixture-key', ANTHROPIC_BASE_URL: 'https://provider.example/api', ENABLE_TOOL_SEARCH: 'auto', ANTHROPIC_DEFAULT_FABLE_MODEL: 'fable-model' },
});

test('API and model edits preserve unrelated settings and env', () => {
  const fields = readClaudeCodeSettingsFields(fixture);
  fields.baseUrl = 'https://another.example/api';
  fields.apiKey = 'replacement';
  fields.apiKeyField = 'ANTHROPIC_API_KEY';
  fields.models.SONNET = { model: 'custom-sonnet', name: 'My model' };
  const output = JSON.parse(patchClaudeCodeSettings(fixture, fields));
  assert.deepEqual(output.permissions, { allow: ['Read'] });
  assert.equal(output.language, 'zh');
  assert.equal(output.env.ENABLE_TOOL_SEARCH, 'auto');
  assert.equal(output.env.ANTHROPIC_AUTH_TOKEN, undefined);
  assert.equal(output.env.ANTHROPIC_API_KEY, 'replacement');
  assert.equal(output.env.ANTHROPIC_DEFAULT_FABLE_MODEL, 'fable-model');
  assert.equal(output.env.ANTHROPIC_DEFAULT_SONNET_MODEL_NAME, 'My model');
});

test('default model edits honor existing env precedence and clearing removes both overrides', () => {
  const content = JSON.stringify({ model: 'fallback', env: { ANTHROPIC_MODEL: 'override' } });
  const fields = readClaudeCodeSettingsFields(content);
  assert.equal(fields.model, 'override');
  fields.model = 'edited';
  const output = JSON.parse(patchClaudeCodeSettings(content, fields));
  assert.equal(output.env.ANTHROPIC_MODEL, 'edited');
  assert.equal(output.model, 'fallback');
  fields.model = '';
  const cleared = JSON.parse(patchClaudeCodeSettings(content, fields));
  assert.equal(cleared.env.ANTHROPIC_MODEL, undefined);
  assert.equal(cleared.model, undefined);
});

test('top-level default model stays at top level', () => {
  const fields = readClaudeCodeSettingsFields(fixture);
  fields.model = 'new-default';
  const output = JSON.parse(patchClaudeCodeSettings(fixture, fields));
  assert.equal(output.model, 'new-default');
  assert.equal(output.env.ANTHROPIC_MODEL, undefined);
});

test('unchanged form fields preserve non-string and unknown values', () => {
  const content = JSON.stringify({ env: { ANTHROPIC_DEFAULT_OPUS_MODEL: null, OTHER: 42 }, hooks: {} });
  assert.deepEqual(JSON.parse(patchClaudeCodeSettings(content, readClaudeCodeSettingsFields(content))), JSON.parse(content));
});

test('rejects invalid JSON and non-object document or env', () => {
  for (const content of ['invalid', '[]', 'null', '{"env":[]}']) assert.throws(() => parseClaudeCodeSettings(content));
});

test('model candidates and mapping choices never create a native picker', () => {
  const fields = readClaudeCodeSettingsFields(fixture);
  fields.modelOptions = mergeClaudeCodeModelOptions([], [{ id: 'candidate', displayName: 'Candidate name' }]);
  const next = updateClaudeCodeModelMapping(fields, 'SONNET', 'candidate', fields.modelOptions);
  const output = JSON.parse(patchClaudeCodeSettings(fixture, next));
  assert.equal(output.modelPicker, undefined);
  assert.equal(output.env.ANTHROPIC_DEFAULT_SONNET_MODEL, 'candidate');
  assert.equal(output.env.ANTHROPIC_DEFAULT_SONNET_MODEL_NAME, 'Candidate name');
  assert.deepEqual(output.permissions, { allow: ['Read'] });
});

test('fetching merges all model families, deduplicates IDs, and retains user models and labels', () => {
  const content = JSON.stringify({ model: 'selected-but-unlisted', modelPicker: { options: [
    { model: 'custom-model', label: 'My label', description: 'My description' },
    { model: 'manual-only', label: '' },
  ] } });
  const fields = readClaudeCodeSettingsFields(content);
  const original = structuredClone(fields.modelOptions);
  fields.modelOptions = mergeClaudeCodeModelOptions(fields.modelOptions, [
    { id: 'CUSTOM-MODEL', displayName: 'Do not overwrite' },
    { id: ' independent-model ', displayName: ' Friendly name ' },
    { id: 'independent-model', displayName: 'Duplicate' },
    { id: '   ' },
  ]);
  assert.deepEqual(fields.modelOptions, [...original, { model: 'independent-model', label: 'Friendly name' }]);
  const output = JSON.parse(patchClaudeCodeSettings(content, fields));
  assert.equal(output.model, 'selected-but-unlisted');
  assert.deepEqual(output.modelPicker, JSON.parse(content).modelPicker);
  assert.deepEqual(mergeClaudeCodeModelOptions(original, []), original);
});

test('fetching fills empty labels without changing user labels, model IDs, or metadata', () => {
  const current = [
    { model: 'first-model', label: '', description: 'Keep this description' },
    { model: 'second-model', label: '  ', future: 42 },
    { model: 'named-model', label: 'My own name' },
    { model: 'unlisted-model', label: '' },
  ];
  const original = structuredClone(current);
  const merged = mergeClaudeCodeModelOptions(current, [
    { id: ' FIRST-MODEL ', displayName: ' First friendly name ' },
    { id: 'SECOND-MODEL', displayName: ' ' },
    { id: 'named-model', displayName: 'Do not replace my name' },
  ]);
  assert.deepEqual(merged, [
    { ...current[0], label: 'First friendly name' },
    { ...current[1], label: 'second-model' },
    current[2], current[3],
  ]);
  assert.deepEqual(current, original);
});

test('fetched models fall back to their IDs when upstream names are absent', () => {
  const merged = mergeClaudeCodeModelOptions([], [
    { id: ' first-model ' },
    { id: 'second-model', displayName: null },
    { id: 'third-model', displayName: ' ' },
  ]);
  assert.deepEqual(merged, [
    { model: 'first-model', label: 'first-model' },
    { model: 'second-model', label: 'second-model' },
    { model: 'third-model', label: 'third-model' },
  ]);
});

test('fetching replaces ID fallback labels with upstream names while keeping custom labels', () => {
  const current = [
    { model: 'first-model', label: 'first-model', description: 'Keep metadata' },
    { model: 'second-model', label: 'My own name' },
    { model: 'third-model', label: 'Third-Model' },
  ];
  const merged = mergeClaudeCodeModelOptions(current, [
    { id: 'FIRST-MODEL', displayName: ' First friendly name ' },
    { id: 'second-model', displayName: 'Second upstream name' },
    { id: 'third-model', displayName: 'Third upstream name' },
  ]);
  assert.deepEqual(merged, [
    { ...current[0], label: 'First friendly name' }, current[1], current[2],
  ]);
  assert.equal(current[0].label, 'first-model');
});

test('resetting model candidates preserves current selections and the existing native picker', () => {
  const content = JSON.stringify({ model: 'chosen', env: { CLAUDE_CODE_SUBAGENT_MODEL: 'worker' },
    modelPicker: { replaceBuiltInOptions: false, options: [{ model: 'chosen' }] } });
  const fields = readClaudeCodeSettingsFields(content);
  fields.modelOptions = [];
  const output = JSON.parse(patchClaudeCodeSettings(content, fields));
  assert.deepEqual(output.modelPicker, JSON.parse(content).modelPicker);
  assert.equal(output.model, 'chosen');
  assert.equal(output.env.CLAUDE_CODE_SUBAGENT_MODEL, 'worker');
});

test('unrelated edits leave native picker contents exactly intact, including unknown rows', () => {
  const picker = { future: 42, options: [{ model: 'custom', behavesAs: 'known' }, { futureRow: true }, 'unknown'] };
  const content = JSON.stringify({ modelPicker: picker, env: { OTHER: true } });
  const fields = readClaudeCodeSettingsFields(content);
  fields.baseUrl = 'https://provider.example';
  assert.deepEqual(JSON.parse(patchClaudeCodeSettings(content, fields)).modelPicker, picker);
});

test('mapping names follow model selections, use upstream names, and fall back to typed IDs', () => {
  const fields = readClaudeCodeSettingsFields(fixture);
  const options = [{ model: 'first-model', label: 'First name' }, { model: 'second-model', label: 'Second name' }];
  const first = updateClaudeCodeModelMapping(fields, 'SONNET', ' FIRST-MODEL ', options);
  assert.deepEqual(first.models.SONNET, { model: ' FIRST-MODEL ', name: 'First name' });
  const second = updateClaudeCodeModelMapping(first, 'SONNET', 'second-model', options);
  assert.deepEqual(second.models.SONNET, { model: 'second-model', name: 'Second name' });
  const manual = updateClaudeCodeModelMapping(second, 'SONNET', 'typed-model', options);
  assert.deepEqual(manual.models.SONNET, { model: 'typed-model', name: 'typed-model' });
  const cleared = updateClaudeCodeModelMapping(manual, 'SONNET', '', options);
  assert.deepEqual(cleared.models.SONNET, { model: '', name: '' });
  assert.deepEqual(fields.models.SONNET, { model: '', name: '' });
  assert.equal(cleared.model, fields.model);
  assert.deepEqual(cleared.models.FABLE, fields.models.FABLE);
});

test('mapping names preserve manual overrides and can fill existing ID placeholders', () => {
  const fields = readClaudeCodeSettingsFields(fixture);
  const options = [{ model: 'first-model', label: 'First name' }, { model: 'second-model', label: 'Second name' }];
  fields.models.HAIKU = { model: 'first-model', name: 'My name' };
  const custom = updateClaudeCodeModelMapping(fields, 'HAIKU', 'second-model', options);
  assert.deepEqual(custom.models.HAIKU, { model: 'second-model', name: 'My name' });
  for (const name of ['', 'first-model']) {
    fields.models.SONNET = { model: 'first-model', name };
    const named = updateClaudeCodeModelMapping(fields, 'SONNET', 'first-model', options);
    assert.equal(named.models.SONNET.name, 'First name');
    assert.equal(updateClaudeCodeModelMapping(named, 'SONNET', 'first-model', options), named);
  }
});

test('fetching context updates an unchanged default model and derives a 90 percent threshold', () => {
  const fields = readClaudeCodeSettingsFields(fixture);
  const output = JSON.parse(patchClaudeCodeSettings(fixture, fields, [
    { id: ' MAIN-MODEL ', contextWindow: 256001 },
  ]));
  assert.equal(output.env.CLAUDE_CODE_MAX_CONTEXT_TOKENS, '256001');
  assert.equal(output.autoCompactWindow, 230400);
  assert.equal(output.model, fields.model);
  assert.equal(output.modelPicker, undefined);
  assert.deepEqual(output.permissions, JSON.parse(fixture).permissions);
  assert.equal(output.env.ENABLE_TOOL_SEARCH, 'auto');
});

test('default aliases use the mapped model window and honor the env model override', () => {
  for (const role of CLAUDE_CODE_MODEL_ROLES) {
    const content = JSON.stringify({ model: 'shadowed', env: {
      ANTHROPIC_MODEL: role.toLowerCase(), [`ANTHROPIC_DEFAULT_${role}_MODEL`]: 'mapped-model',
    } });
    const output = JSON.parse(patchClaudeCodeSettings(content, readClaudeCodeSettingsFields(content), [
      { id: 'shadowed', contextWindow: 120000 }, { id: 'mapped-model', contextWindow: 240000 },
    ]));
    assert.equal(output.env.CLAUDE_CODE_MAX_CONTEXT_TOKENS, '240000');
    assert.equal(output.autoCompactWindow, 216000);
    assert.equal(output.env.ANTHROPIC_MODEL, role.toLowerCase());
    assert.equal(output.model, 'shadowed');
  }
});

test('changing the default model or its mapping follows the new window, independently of subagents', () => {
  const models = [{ id: 'first-model', contextWindow: 120000 }, { id: 'second-model', contextWindow: 240001 }];
  const content = JSON.stringify({ model: 'fable', env: { ANTHROPIC_DEFAULT_FABLE_MODEL: 'first-model' } });
  const fields = readClaudeCodeSettingsFields(content);
  const initial = patchClaudeCodeSettings(content, fields, models);
  fields.subagentModel = 'second-model';
  assert.equal(JSON.parse(patchClaudeCodeSettings(initial, fields, models)).autoCompactWindow, 108000);
  fields.models.FABLE.model = 'second-model';
  const mapped = patchClaudeCodeSettings(initial, fields, models);
  assert.equal(JSON.parse(mapped).env.CLAUDE_CODE_MAX_CONTEXT_TOKENS, '240001');
  assert.equal(JSON.parse(mapped).autoCompactWindow, 216000);
  fields.model = 'first-model';
  const direct = JSON.parse(patchClaudeCodeSettings(mapped, fields, models));
  assert.equal(direct.env.CLAUDE_CODE_MAX_CONTEXT_TOKENS, '120000');
  assert.equal(direct.autoCompactWindow, 108000);
});

test('missing or invalid upstream context does not guess a window or overwrite manual limits', () => {
  const content = JSON.stringify({ model: 'selected', autoCompactWindow: 180000,
    env: { CLAUDE_CODE_MAX_CONTEXT_TOKENS: '200000' } });
  const fields = readClaudeCodeSettingsFields(content);
  assert.deepEqual(JSON.parse(patchClaudeCodeSettings(content, fields)), JSON.parse(content));
  for (const contextWindow of [undefined, null, 0, -1, 1.5, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1]) {
    const output = JSON.parse(patchClaudeCodeSettings(content, fields, [
      { id: 'other', contextWindow: 300000 }, { id: 'selected', contextWindow },
    ]));
    assert.deepEqual(output, JSON.parse(content));
  }
});

test('an existing compact window env override follows the new threshold', () => {
  const content = JSON.stringify({ model: 'selected', autoCompactWindow: 100000,
    env: { CLAUDE_CODE_AUTO_COMPACT_WINDOW: '110000' } });
  const output = JSON.parse(patchClaudeCodeSettings(content, readClaudeCodeSettingsFields(content), [
    { id: 'selected', contextWindow: 240000 },
  ]));
  assert.equal(output.autoCompactWindow, 216000);
  assert.equal(output.env.CLAUDE_CODE_AUTO_COMPACT_WINDOW, '216000');
});
