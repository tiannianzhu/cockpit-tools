import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  buildCodexProviderCatalogDefinition,
  patchCodexProviderModel,
  buildUpstreamModelDefinitions,
  deriveCodexProviderCatalogFields,
  parseCodexProviderCatalogDefinition,
  reconcileCodexProviderCatalogDefinition,
} from './codexProviderCatalogDefinition';

const source = {
  base_model: 'official-template',
  models: [
    {
      slug: 'model-a', context_window: 12000, max_context_window: 12000,
      input_modalities: ['text', 'image'],
      supported_reasoning_levels: [{ effort: 'low' }, { effort: 'high' }],
      default_reasoning_level: 'high',
      custom_parameter: { nested: ['keep'] },
    },
    { slug: 'model-b', context_window: 8000, input_modalities: ['text'] },
  ],
};

test('imports full fields and derives editor catalog without losing unknown fields', () => {
  const definition = parseCodexProviderCatalogDefinition(source);
  const fields = deriveCodexProviderCatalogFields(definition);
  assert.deepEqual(fields.modelCatalog, ['model-a', 'model-b']);
  assert.deepEqual(fields.modelContextWindows, { 'model-a': 12000, 'model-b': 8000 });
  assert.equal(fields.modelCapabilities['model-a'].supportsVision, true);
  assert.equal(fields.modelCapabilities['model-b'].supportsVision, false);
  source.models[0].custom_parameter?.nested.push('later');
  assert.deepEqual(definition.models[0].custom_parameter, { nested: ['keep'] });
});

test('rejects malformed model identity, context and reasoning', () => {
  const mutate = (change: (value: typeof source) => void) => {
    const value = structuredClone(source);
    change(value);
    assert.throws(() => parseCodexProviderCatalogDefinition(value));
  };
  mutate((value) => { value.models[1].slug = 'MODEL-A'; });
  mutate((value) => { value.models[0].context_window = -1; });
  mutate((value) => { value.models[0].default_reasoning_level = 'other'; });
  assert.equal('base_model' in parseCodexProviderCatalogDefinition(source), false);
  mutate((value) => { value.models[0].input_modalities = ['image']; });
  mutate((value) => { value.models[0].input_modalities = ['text', 'text']; });
  mutate((value) => { value.models[0].supported_reasoning_levels = [{ effort: 'low' }, { effort: 'low' }]; });
  const inherited = structuredClone(source);
  delete (inherited.models[0] as { supported_reasoning_levels?: unknown }).supported_reasoning_levels;
  assert.doesNotThrow(() => parseCodexProviderCatalogDefinition(inherited));
});

test('reconciles edits and adds models without assuming their capabilities', () => {
  const definition = parseCodexProviderCatalogDefinition(source);
  const result = reconcileCodexProviderCatalogDefinition(definition, ['model-b'], {}, {
    'model-b': { supportsVision: true },
  });
  assert.equal('base_model' in result, false);
  assert.deepEqual(result.models.map((model) => model.slug), ['model-b']);
  assert.equal(result.models[0].context_window, undefined);
  assert.deepEqual(result.models[0].input_modalities, ['text', 'image']);
  assert.deepEqual(reconcileCodexProviderCatalogDefinition(definition, ['new-model'], {}, {}).models, [{ slug: 'new-model', input_modalities: ['text'] }]);
  const edited = reconcileCodexProviderCatalogDefinition(definition, ['model-a'], { 'model-a': 16000 }, {});
  assert.equal(edited.models[0].context_window, 16000);
  assert.equal(edited.models[0].max_context_window, 16000);
});


test('generates a definition without importing and preserves explicit parameters', () => {
  const draft = patchCodexProviderModel(undefined, 'custom', {
    supported_reasoning_levels: [{ effort: 'low', description: 'Quick' }],
    default_reasoning_level: 'low', extra: { keep: true },
  });
  const result = buildCodexProviderCatalogDefinition(['custom'], { custom: 24000 }, {
    custom: { supportsVision: true },
  }, draft)!;
  assert.equal('base_model' in result, false);
  assert.equal('require_explicit_capabilities' in result, false);
  assert.deepEqual(result.models[0].input_modalities, ['text', 'image']);
  assert.equal(result.models[0].context_window, 24000);
  assert.deepEqual(result.models[0].extra, { keep: true });
  assert.deepEqual(result.models[0].supported_reasoning_levels, draft.models[0].supported_reasoning_levels);
  assert.equal(draft.models[0].context_window, undefined);
  assert.equal(buildCodexProviderCatalogDefinition([], {}, {}, draft), undefined);
});

test('clearing context and reasoning removes dependent fields without discarding other metadata', () => {
  const original = parseCodexProviderCatalogDefinition(source);
  const patched = patchCodexProviderModel(original, 'MODEL-A', {
    supported_reasoning_levels: [], default_reasoning_level: null, slug: 'ignored',
  });
  const result = buildCodexProviderCatalogDefinition(['model-a'], {}, {}, patched)!;
  assert.equal(result.models[0].context_window, undefined);
  assert.equal(result.models[0].max_context_window, undefined);
  assert.equal(result.models[0].supported_reasoning_levels, undefined);
  assert.equal(result.models[0].default_reasoning_level, undefined);
  assert.deepEqual(result.models[0].custom_parameter, original.models[0].custom_parameter);
  assert.equal(original.models[0].default_reasoning_level, 'high');
});


test('import and editing use Codex reasoning level order without changing the default', () => {
  const source = { base_model: 'auto', models: [{ slug: 'sample',
    supported_reasoning_levels: [{ effort: 'max' }, { effort: 'high' }, { effort: 'low' }, { effort: 'none' }],
    default_reasoning_level: 'max',
  }] };
  const parsed = parseCodexProviderCatalogDefinition(source);
  assert.deepEqual(parsed.models[0].supported_reasoning_levels, [
    { effort: 'none' }, { effort: 'low' }, { effort: 'high' }, { effort: 'max' },
  ]);
  assert.equal(parsed.models[0].default_reasoning_level, 'max');
  assert.equal(source.models[0].supported_reasoning_levels[0].effort, 'max');
  const edited = patchCodexProviderModel(parsed, 'sample', {
    supported_reasoning_levels: [{ effort: 'high' }, { effort: 'low' }], default_reasoning_level: 'high',
  });
  assert.deepEqual(edited.models[0].supported_reasoning_levels, [{ effort: 'low' }, { effort: 'high' }]);
});

test('upstream replaces model metadata, uses Codex order and defaults to the first valid effort', () => {
  const fetched = buildUpstreamModelDefinitions([{ id: 'sample', displayName: 'Sample',
    contextWindow: 200, supportsVision: false, reasoningEfforts: ['unknown', 'high', 'low', 'high'] }, { id: 'new' }]);
  assert.deepEqual(fetched.models[0], { slug: 'sample', display_name: 'Sample', context_window: 200,
    max_context_window: 200, input_modalities: ['text'],
    supported_reasoning_levels: [{ effort: 'low', description: 'Low' }, { effort: 'high', description: 'High' }],
    default_reasoning_level: 'high' });
  assert.deepEqual(fetched.models.map((model) => model.slug), ['sample', 'new']);
  const sparse = buildUpstreamModelDefinitions([{ id: 'sample' }]);
  assert.deepEqual(sparse.models, [{ slug: 'sample', input_modalities: ['text'] }]);
  const fields = deriveCodexProviderCatalogFields(sparse);
  assert.deepEqual(fields.modelContextWindows, {});
  assert.deepEqual(buildCodexProviderCatalogDefinition(fields.modelCatalog, fields.modelContextWindows,
    fields.modelCapabilities, sparse), sparse);
  assert.deepEqual(buildUpstreamModelDefinitions([]).models, []);
  assert.throws(() => buildUpstreamModelDefinitions([{ id: 'sample', reasoningEfforts: ['unsupported'] }]));
});
