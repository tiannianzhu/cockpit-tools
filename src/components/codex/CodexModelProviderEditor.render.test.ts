import assert from 'node:assert/strict';
import test from 'node:test';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { loadHookModule } from '../../../tests/helpers/reactHookHarness';

const marker = (name: string) => () => React.createElement('span', { 'data-testid': name });
const { exports } = loadHookModule(new URL('./CodexModelProviderEditor.tsx', import.meta.url), {
  'react-dom': { createPortal: (element: React.ReactNode) => element },
  '../../hooks/useModalScrollLock': { useModalScrollLock() {} },
  '../SingleSelectDropdown': { SingleSelectDropdown: marker('dropdown') },
  './CodexProviderModelsEditor': { CodexProviderModelsEditor: marker('full-catalog') },
  './CodexAnthropicProviderFields': { CodexAnthropicProviderFields: marker('anthropic') },
  './CodexProviderModelFetch': { CodexProviderModelFetch: marker('fetch') },
  './CodexModelContextWindowTable': { CodexModelContextWindowTable: marker('contexts') },
  '../ModalErrorMessage': { ModalErrorMessage: marker('error') },
}, { URL, document: { body: {} } });

function render(wireApi: string, selectedPresetId = 'custom') {
  const props = {
    accounts: [], providers: [], accountEditor: null, currentEditingProvider: null,
    editorModelKeyId: null, selectedPreset: null, selectedPresetId,
    selectedSponsorTemplate: null, selectedSponsorTemplateId: null, sponsorProviderTemplates: [],
    existingApiKeySearchQuery: '', editingApiKey: null, saving: false, showModal: true,
    form: { providerId: null, name: 'Relay', baseUrl: 'https://relay.invalid/v1', modelCatalogText: 'model-a',
      modelContextWindowsDraft: {}, visionModelStates: {}, visionModelText: '', visionRoutingModel: '',
      supportsVision: false, supportsWebsockets: false, wireApi, authScheme: 'bearer',
      website: '', apiKeyUrl: '', newApiKeyName: '', newApiKey: '' },
    parseModelCatalogText: (value: string) => value.split('\n'),
    t: (key: string, fallback?: unknown) => typeof fallback === 'string' ? fallback : key,
    previewPaths: { providerStorePath: '/provider.json' },
  };
  return renderToStaticMarkup(React.createElement(exports.CodexModelProviderEditor, props));
}

test('the shared editor selects the full Responses catalog and retains Anthropic controls', () => {
  const responses = render('responses');
  assert.match(responses, /data-testid="full-catalog"/);
  assert.match(responses, />CMPDC<\/span>/);
  assert.doesNotMatch(responses, /data-testid="anthropic"/);
  const anthropic = render('anthropic');
  assert.match(anthropic, /data-testid="anthropic"/);
  assert.match(anthropic, /data-testid="contexts"/);
  assert.doesNotMatch(anthropic, /data-testid="full-catalog"/);
});
