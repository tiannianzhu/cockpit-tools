import assert from 'node:assert/strict';
import test from 'node:test';
import { CODEX_OPEN_HOSTS_EVENT, requestCodexHosts, takePendingCodexHostsRequest } from './codexHostNavigation';

test('settings navigation reaches hosts when Codex mounts after the request', () => {
  const original = Object.getOwnPropertyDescriptor(globalThis, 'window');
  const target = new EventTarget();
  Object.defineProperty(globalThis, 'window', { configurable: true, value: target });
  try {
    let destination: unknown;
    target.addEventListener('app-request-navigate', (event) => {
      destination = (event as CustomEvent).detail;
    });
    requestCodexHosts();
    assert.equal(destination, 'codex');
    assert.equal(takePendingCodexHostsRequest(), true);
    assert.equal(takePendingCodexHostsRequest(), false);

    let opens = 0;
    target.addEventListener(CODEX_OPEN_HOSTS_EVENT, () => {
      if (takePendingCodexHostsRequest()) opens++;
    });
    requestCodexHosts();
    assert.equal(opens, 1);
    assert.equal(takePendingCodexHostsRequest(), false);
  } finally {
    takePendingCodexHostsRequest();
    if (original) Object.defineProperty(globalThis, 'window', original);
    else Reflect.deleteProperty(globalThis, 'window');
  }
});
