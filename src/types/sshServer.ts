export type SshAuthConfig =
  | { kind: 'agent' }
  | { kind: 'private_key_file'; path: string };

export type SshCodexSyncStage =
  | 'pending'
  | 'transferring'
  | 'credentials_synced'
  | 'reloading'
  | 'applied'
  | 'failed'
  | 'superseded';

export interface SshCodexSyncStatus {
  account_id: string;
  account_email: string;
  token_generation: number;
  bundle_hash: string;
  synced_at: number;
  /** A verification of transferred auth.json bytes, not a running Codex process. */
  verified: boolean;
  error: string | null;
  stage?: SshCodexSyncStage;
  job_id?: string;
}

export interface SshCodexSyncResult extends SshCodexSyncStatus {
  server_id: string;
  server_name: string;
}

export interface SshServerAccountInspection {
  connection_address?: string | null;
  server_id: string;
  auth_mode: string;
  account_id?: string | null;
  email?: string | null;
  matched_account_id?: string | null;
  model_provider?: string | null;
  model_provider_name?: string | null;
  base_url?: string | null;
  model?: string | null;
  model_catalog_path?: string | null;
  model_catalog_exists?: boolean;
  catalog_model_count?: number | null;
}

export interface SshServer {
  id: string;
  name: string;
  host: string;
  port: number;
  username: string;
  codex_home: string;
  auth: SshAuthConfig;
  sync_on_codex_switch: boolean;
  created_at: number;
  updated_at: number;
  last_sync: SshCodexSyncStatus | null;
}

export interface SshServerList {
  selected_server_ids: string[];
  servers: SshServer[];
}

export interface SshServerDraft {
  id?: string;
  name: string;
  host: string;
  port?: number;
  username: string;
  codex_home?: string;
  auth: SshAuthConfig;
  sync_on_codex_switch?: boolean;
}
