/** Settings can open the host page even before the Codex suite is mounted. */
export const CODEX_OPEN_HOSTS_EVENT = 'codex-open-hosts';
let pending = false;

export function takePendingCodexHostsRequest(): boolean {
  const requested = pending;
  pending = false;
  return requested;
}

export function requestCodexHosts(): void {
  pending = true;
  window.dispatchEvent(new CustomEvent('app-request-navigate', { detail: 'codex' }));
  window.dispatchEvent(new Event(CODEX_OPEN_HOSTS_EVENT));
}
