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
 * Network status information
 */
export interface NetworkStatus {
  /** This node's peer ID */
  peer_id: string;
  /** List of connected peer IDs */
  connected_peers: string[];
  /** Whether the network is running */
  is_running: boolean;
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
  /** Type of sync event */
  type: "sync_update" | "sync_response";
  /** Collection that was synced */
  collection: string;
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
  /** Whether to automatically refresh on sync events (default: true) */
  autoRefresh?: boolean;
  /** Polling interval in ms for background refresh (default: none) */
  pollInterval?: number;
  /** Enable optimistic updates for create/update/delete (default: false) */
  optimisticUpdates?: boolean;
  /** Initial data to use before first fetch */
  initialData?: unknown[];
  /** Sort records by a specific field */
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
