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
  useNetworkSettings,
  useImportRecords,
  importDatabase,
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
  NetworkSettings,
  SyncStats,
  ImportScope,
  ImportOutcome,
  CollectionImport,
  ImportSummary,
  CreateRecordPayload,
  UpdateRecordPayload,
  SyncEvent,
  PeerEvent,
  UseCollectionOptions,
  UseCollectionReturn,
  UseFindOptions,
  QueryFilter,
} from "./types";
