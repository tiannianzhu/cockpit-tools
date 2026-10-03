import { invoke } from '@tauri-apps/api/core';
import type { ClaudeCodeSettings, ClaudeCodeSettingsSaved, ClaudeCodeSyncPreferences, ClaudeCodeSyncResult } from '../types/claudeCodeSettings';

export function readClaudeCodeSettings(): Promise<ClaudeCodeSettings> {
  return invoke('claude_code_read_settings');
}

export function readClaudeCodeSyncPreferences(): Promise<ClaudeCodeSyncPreferences> {
  return invoke('claude_code_read_sync_preferences');
}

export function saveClaudeCodeSettings(content: string, expectedRevision: string, serverIds: string[], accountId?: string | null): Promise<ClaudeCodeSettingsSaved> {
  return invoke('claude_code_save_settings', { content, expectedRevision, serverIds, accountId });
}

export function syncClaudeCodeSettings(serverIds: string[]): Promise<ClaudeCodeSyncResult[]> {
  return invoke('claude_code_sync_settings', { serverIds });
}
