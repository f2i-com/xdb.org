/**
 * XDB Type Definitions
 *
 * These types match the Rust backend types and are used throughout the
 * React integration layer.
 */

/**
 * A record stored in XDB
 */
export interface Record<T = unknown> {
  /** Unique identifier (UUID) */
  id: string;
  /** Collection name this record belongs to */
  collection: string;
  /** The actual data payload */
  data: T;
  /** ISO timestamp when the record was created */
  created_at: string;
  /** ISO timestamp when the record was last updated */
  updated_at: string;
  /** Whether this record has been soft-deleted */
  deleted: boolean;
}

/**
 * Database statistics
 */
export interface DbStats {
  /** Total number of non-deleted records */
  record_count: number;
  /** Number of distinct collections */
  collection_count: number;
  /** Size of the database file in bytes */
  db_size_bytes: number;
}

/**
 * What the network node actually did (cumulative for its lifetime). There is
 * deliberately no single "synced" flag: a publish that reached the mesh is not
 * a peer acknowledgement, and a peer applying an update is not peer persistence.
 */
export interface SyncStats {
  publishes_sent: number;
  publishes_without_peers: number;
  publish_failures: number;
  updates_applied: number;
  updates_rejected_stale: number;
  updates_skipped_paused: number;
  resets_applied: number;
  sync_requests_sent: number;
  sync_responses_applied: number;
  last_announce_at: string | null;
  last_repair_at: string | null;
  last_update_applied_at: string | null;
}

/**
 * Network status information (honest: enabled vs running vs paused)
 */
export interface NetworkStatus {
  /** This node's peer ID */
  peer_id: string;
  /** List of connected peer IDs */
  connected_peers: string[];
  /** Whether the network is running */
  is_running: boolean;
  /** "local-only" (default) or "trusted-lan" (explicit opt-in) */
  mode: "local-only" | "trusted-lan";
  /** The persisted opt-in */
  enabled: boolean;
  discovery: boolean;
  listening: boolean;
  /** Held after a local-scope restore until resume_sync */
  sync_paused: boolean;
  stats: SyncStats;
}

/**
 * Persisted networking choice. Defaults to local-only.
 */
export interface NetworkSettings {
  enabled: boolean;
  discovery: boolean;
  listen: boolean;
}

/** What a database import means for synchronized data. */
export type ImportScope = "local" | "fork" | "replace";

/** Result of import_database */
export interface ImportOutcome {
  app_id: string;
  scope: ImportScope;
  /** True when synchronization is held until resume_sync */
  sync_paused: boolean;
  reset_collections: string[];
}

/** One collection's batch for import_records */
export interface CollectionImport<T = unknown> {
  collection: string;
  /** Tombstone records absent from `records` (a replicated deletion) before upserting */
  replace?: boolean;
  records: Record<T>[];
}

/** Result of import_records; returned only after the transaction committed. */
export interface ImportSummary {
  collections: Array<{ collection: string; replaced: boolean; imported: number; tombstoned: number; epoch: number }>;
  imported: number;
  tombstoned: number;
}

/**
 * Payload for creating a new record
 */
export interface CreateRecordPayload<T = unknown> {
  /** Collection to create the record in */
  collection: string;
  /** The data to store */
  data: T;
}

/**
 * Payload for updating an existing record
 */
export interface UpdateRecordPayload<T = unknown> {
  /** ID of the record to update */
  id: string;
  /** The new data */
  data: T;
}

/**
 * Sync event payload emitted when data is synced
 */
export interface SyncEvent {
  /** Type of sync event; "reset" is an adopted administrative reset */
  type: "sync_update" | "sync_response" | "reset";
  /** Collection that was synced */
  collection: string;
  /** Reset epoch (reset events only) */
  epoch?: number;
  /** App scope, when provided; legacy events belong to the default database. */
  app_id?: string;
}

/**
 * Peer event payload emitted when peers connect/disconnect
 */
export interface PeerEvent {
  /** Type of peer event */
  type: "connected" | "disconnected";
  /** Peer ID */
  peer_id: string;
  /** Peer addresses (only for connected events) */
  addresses?: string[];
}

/**
 * Options for the useCollection hook
 */
export interface UseCollectionOptions {
  /** App database to use. Omit for the default database. */
  appId?: string;
  /** Whether to automatically refresh on sync events (default: true) */
  autoRefresh?: boolean;
  /** Positive polling interval in ms; zero/negative/non-finite values disable polling. */
  pollInterval?: number;
  /** Enable optimistic updates for create/update/delete (default: false) */
  optimisticUpdates?: boolean;
  /** Initial data to use before first fetch */
  initialData?: unknown[];
  /** Sort by a payload field, or record metadata such as created_at. */
  sortBy?: string;
  /** Sort direction (default: 'asc') */
  sortOrder?: 'asc' | 'desc';
}

/**
 * Return type for the useCollection hook
 */
export interface UseCollectionReturn<T> {
  /** Current records in the collection */
  records: Record<T>[];
  /** Whether the collection is currently loading */
  loading: boolean;
  /** Error message if any operation failed */
  error: string | null;
  /** Whether any mutation is in progress */
  mutating: boolean;
  /** Manually refresh the collection */
  refresh: () => Promise<void>;
  /** Create a new record */
  create: (data: T) => Promise<Record<T> | null>;
  /** Update an existing record */
  update: (id: string, data: T) => Promise<Record<T> | null>;
  /** Delete a record */
  remove: (id: string) => Promise<boolean>;
  /** Request sync from peers */
  requestSync: () => Promise<boolean>;
  /** Clear any error state */
  clearError: () => void;
  /** Get a single record by ID */
  getById: (id: string) => Record<T> | undefined;
}

/**
 * Query filter for find operations
 */
export interface QueryFilter {
  /** Field to filter on */
  field: string;
  /** Operator */
  operator: 'eq' | 'ne' | 'gt' | 'gte' | 'lt' | 'lte' | 'contains' | 'startsWith' | 'endsWith';
  /** Value to compare against */
  value: unknown;
}

/**
 * Options for useFind hook
 */
export interface UseFindOptions {
  /** App database to search. Omit for the default database. */
  appId?: string;
  /** Filter conditions */
  filters?: QueryFilter[];
  /** Sort field */
  sortBy?: string;
  /** Sort direction */
  sortOrder?: 'asc' | 'desc';
  /** Limit results */
  limit?: number;
  /** Skip results (for pagination) */
  offset?: number;
}
