import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { Eye, Minimize2, X } from 'lucide-react';
import { useEscClose } from '../../hooks/useEscClose';
import type { CodexSessionTransferOperation, CodexSessionTransferProgress } from '../../types/codex';

export interface CodexSessionTransferTask {
  id: string;
  operation: CodexSessionTransferOperation;
  status: 'running' | 'success' | 'error';
  progress: CodexSessionTransferProgress;
  message?: string;
  error?: string;
}

function transferId(): string {
  return `session-transfer-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

function initialProgress(id: string, operation: CodexSessionTransferOperation, count: number): CodexSessionTransferProgress {
  return { transferId: id, operation, phase: 'prepare', current: 0, total: count,
    percent: 0, currentLabel: null, running: true };
}

export function useCodexSessionTransfer() {
  const [task, setTask] = useState<CodexSessionTransferTask | null>(null);
  const [visible, setVisible] = useState(false);
  const currentIdRef = useRef<string | null>(null);

  const begin = useCallback((operation: CodexSessionTransferOperation, count: number): string => {
    const id = transferId();
    currentIdRef.current = id;
    setTask({ id, operation, status: 'running', progress: initialProgress(id, operation, count) });
    setVisible(true);
    return id;
  }, []);
  const getCurrentId = useCallback(() => currentIdRef.current, []);
  const update = useCallback((progress: CodexSessionTransferProgress) => {
    if (progress.transferId !== currentIdRef.current) return;
    setTask(current => current?.id === progress.transferId && current.status === 'running' ? {
      ...current, progress, status: progress.running ? 'running' : current.status,
    } : current);
  }, []);
  const succeed = useCallback((id: string, message: string) => {
    setTask(current => current?.id === id ? {
      ...current, status: 'success', message,
      progress: { ...current.progress, phase: 'done', current: current.progress.total,
        percent: 100, running: false },
    } : current);
  }, []);
  const fail = useCallback((id: string, error: string) => {
    setTask(current => current?.id === id ? {
      ...current, status: 'error', error,
      progress: { ...current.progress, running: false },
    } : current);
  }, []);
  const minimize = useCallback(() => setVisible(false), []);
  const reopen = useCallback(() => setVisible(true), []);
  const clear = useCallback(() => {
    if (task?.status === 'running') {
      setVisible(false);
      return;
    }
    currentIdRef.current = null;
    setTask(null);
    setVisible(false);
  }, [task?.status]);

  useEffect(() => {
    let disposed = false;
    let unlisten: UnlistenFn | null = null;
    void listen<CodexSessionTransferProgress>('codex:session-transfer-progress', event => update(event.payload))
      .then(nextUnlisten => {
        if (disposed) nextUnlisten();
        else unlisten = nextUnlisten;
      }).catch(() => {
        // The transfer result still updates when Tauri progress events are unavailable.
      });
    return () => { disposed = true; unlisten?.(); };
  }, [update]);
  useEscClose(visible && task?.status !== 'running', clear);

  return { task, visible, begin, getCurrentId, update, succeed, fail, minimize, reopen, clear };
}

export type CodexSessionTransferController = ReturnType<typeof useCodexSessionTransfer>;

function percent(value: number | null | undefined): number {
  if (typeof value !== 'number' || !Number.isFinite(value)) return 0;
  return Math.max(0, Math.min(100, Math.round(value)));
}

export function CodexSessionTransfer({ transfer }: { transfer: CodexSessionTransferController }) {
  const { t } = useTranslation();
  const { task, visible, minimize, reopen, clear } = transfer;
  if (!task) return null;
  const progress = task.progress;
  const value = percent(progress.percent);
  const running = task.status === 'running';
  const title = task.operation === 'export'
    ? t('codex.sessionManager.transferModal.exportTitle', '导出会话')
    : t('codex.sessionManager.transferModal.importTitle', '导入会话');
  let phaseText: string;
  if (task.operation === 'export') {
    if (progress.phase === 'download') phaseText = t('codex.sessionManager.transferModal.phase.exportDownload', '正在读取会话文件...');
    else if (progress.phase === 'collect') phaseText = t('codex.sessionManager.transferModal.phase.exportCollect', '正在收集会话...');
    else if (progress.phase === 'hash') phaseText = t('codex.sessionManager.transferModal.phase.exportHash', '正在校验会话文件...');
    else if (progress.phase === 'write') phaseText = t('codex.sessionManager.transferModal.phase.exportWrite', '正在写入会话包...');
    else if (progress.phase === 'done') phaseText = t('codex.sessionManager.transferModal.phase.done', '任务已完成');
    else phaseText = t('codex.sessionManager.transferModal.phase.exportPrepare', '正在准备导出...');
  } else {
    if (progress.phase === 'read') phaseText = t('codex.sessionManager.transferModal.phase.importRead', '正在读取会话包...');
    else if (progress.phase === 'write') phaseText = t('codex.sessionManager.transferModal.phase.importWrite', '正在写入目标实例...');
    else if (progress.phase === 'rebuild') phaseText = t('codex.sessionManager.transferModal.phase.importRebuild', '正在刷新官方会话索引...');
    else if (progress.phase === 'done') phaseText = t('codex.sessionManager.transferModal.phase.done', '任务已完成');
    else phaseText = t('codex.sessionManager.transferModal.phase.importPrepare', '正在准备导入...');
  }

  if (!visible) return <div className={`codex-session-transfer-task is-${task.status}`}>
    <div className="codex-session-transfer-task__copy">
      <strong>{title}</strong>
      <span>{running
        ? t('codex.sessionManager.transferModal.runningSummary', { defaultValue: '正在处理 {{current}}/{{total}}',
            current: progress.current, total: progress.total })
        : task.status === 'success'
          ? t('codex.sessionManager.transferModal.completedSummary', '任务已完成')
          : t('codex.sessionManager.transferModal.failedSummary', '任务失败')}</span>
      <div className="codex-session-transfer-task__progress" aria-hidden="true"><span style={{ width: `${value}%` }} /></div>
    </div>
    <div className="codex-session-transfer-task__actions">
      <button className="btn btn-secondary" type="button" onClick={reopen}>
        <Eye size={14} />{t('codex.sessionManager.transferModal.reopen', '查看进度')}
      </button>
      {!running && <button className="btn btn-secondary" type="button" onClick={clear}>
        <X size={14} />{t('codex.sessionManager.transferModal.clear', '清除')}
      </button>}
    </div>
  </div>;

  return <div className="modal-overlay">
    <div className={`modal codex-session-transfer-modal is-${task.status}`} onClick={event => event.stopPropagation()}>
      <div className="modal-header">
        <h2>{title}</h2>
        <button className="modal-close" type="button" onClick={running ? minimize : clear}
          aria-label={running ? t('codex.sessionManager.transferModal.minimize', '最小化') : t('common.close', '关闭')}>
          {running ? <Minimize2 size={18} /> : <X size={18} />}
        </button>
      </div>
      <div className="modal-body">
        <div className="codex-session-transfer-progress" role="status">
          <div className="codex-session-transfer-progress__head"><strong>{phaseText}</strong><span>{value}%</span></div>
          <div className="codex-session-transfer-progress__bar"><span style={{ width: `${value}%` }} /></div>
          <div className="codex-session-transfer-current">
            <span>{t('codex.sessionManager.transferModal.current', { defaultValue: '{{current}} / {{total}}',
              current: progress.current, total: progress.total })}</span>
            {progress.currentLabel ? <span>{progress.currentLabel}</span> : null}
          </div>
        </div>
        {task.message && <div className="codex-session-transfer-result is-success">{task.message}</div>}
        {task.error && <div className="codex-session-transfer-result is-error">{task.error}</div>}
      </div>
      <div className="modal-footer">
        {running ? <button className="btn btn-secondary" type="button" onClick={minimize}>
          <Minimize2 size={14} />{t('codex.sessionManager.transferModal.minimize', '最小化')}
        </button> : <button className="btn btn-primary" type="button" onClick={clear}>
          <X size={14} />{t('common.close', '关闭')}
        </button>}
      </div>
    </div>
  </div>;
}
