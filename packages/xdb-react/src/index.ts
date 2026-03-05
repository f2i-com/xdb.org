/**
 * @xdb/react - React hooks and utilities for XDB
 *
 * A local-first, peer-to-peer database integration for React + Tauri applications.
 *
 * @packageDocumentation
 */

// Export all hooks
export {
  useCollection,
  useFind,
  useDbStats,
  useNetworkStatus,
  useDbPath,
  useDbExport,
  useDbImport,
  useSyncEvents,
  usePeerEvents,
} from "./hooks";

// Export all types
export type {
  Record,
  DbStats,
  NetworkStatus,
  CreateRecordPayload,
  UpdateRecordPayload,
  SyncEvent,
  PeerEvent,
  UseCollectionOptions,
  UseCollectionReturn,
  UseFindOptions,
  QueryFilter,
} from "./types";
