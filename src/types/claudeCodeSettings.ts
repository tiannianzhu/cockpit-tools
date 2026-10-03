export interface ClaudeCodeSettings {
  path: string;
  content: string;
  revision: string;
  /** API Key account associated with the current local settings file, when known. */
  account?: { id: string; name: string } | null;
}

export interface ClaudeCodeSyncResult {
  serverId: string;
  serverName: string;
  syncedAt: number;
  revision: string;
  verified: boolean;
  error: string | null;
}

export interface ClaudeCodeSyncPreferences {
  serverIds: string[];
  lastResults: ClaudeCodeSyncResult[];
}

export interface ClaudeCodeSettingsSaved {
  settings: ClaudeCodeSettings;
  syncResults: ClaudeCodeSyncResult[];
  syncError: string | null;
}
