import { useEffect, useRef, useState, type MouseEvent } from 'react';
import { Check, Copy, FileText, FolderOpen, RefreshCw } from 'lucide-react';
import { useTranslation } from 'react-i18next';

interface Props {
  sessionId: string;
  disabled?: boolean;
  opening?: boolean;
  onOpenLocation: (event: MouseEvent<HTMLButtonElement>) => void;
  onOpenFile: (event: MouseEvent<HTMLButtonElement>) => void;
  onError: (message: string) => void;
}
export function CodexSessionRowActions({ sessionId, disabled, opening, onOpenLocation, onOpenFile, onError }: Props) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  async function copy() {
    try {
      await navigator.clipboard.writeText(sessionId);
      setCopied(true);
      clearTimeout(timer.current);
      timer.current = setTimeout(() => setCopied(false), 1200);
    } catch (cause) { onError(String(cause)); }
  }
  return <>
    <button type="button" className={`codex-session-row__copy-button${copied ? ' is-copied' : ''}`}
      disabled={disabled} title={t('codex.sessionManager.actions.copySessionId', '复制会话 ID')}
      aria-label={t('codex.sessionManager.actions.copySessionId', '复制会话 ID')} onClick={() => void copy()}>
      {copied ? <Check size={14} /> : <Copy size={14} />}
    </button>
    <button type="button" className="codex-session-row__copy-button" disabled={disabled}
      title={t('codex.sessionManager.actions.openLocation', '打开位置')} aria-label={t('codex.sessionManager.actions.openLocation', '打开位置')}
      onClick={onOpenLocation}><FolderOpen size={14} /></button>
    <button type="button" className="codex-session-row__copy-button" disabled={disabled} aria-busy={opening}
      title={t('codex.sessionManager.actions.openRollout', '打开会话文件')} aria-label={t('codex.sessionManager.actions.openRollout', '打开会话文件')}
      onClick={onOpenFile}>{opening ? <RefreshCw size={14} className="icon-spin" /> : <FileText size={14} />}</button>
  </>;
}
