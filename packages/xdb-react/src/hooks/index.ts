/**
 * XDB React Hooks
 *
 * A collection of React hooks for interacting with XDB in Tauri applications.
 */

import { useCallback, useEffect, useState, useRef, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  Record,
  DbStats,
  NetworkStatus,
  SyncEvent,
  PeerEvent,
  UseCollectionOptions,
  UseCollectionReturn,
  UseFindOptions,
} from "../types";

function compareUnknown(a: unknown, b: unknown): number {
  if (a === b) return 0;
  const aNum = typeof a === 'number' ? a : Number.NaN;
  const bNum = typeof b === 'number' ? b : Number.NaN;
  if (!Number.isNaN(aNum) && !Number.isNaN(bNum)) {
    return aNum < bNum ? -1 : 1;
  }
  const aStr = String(a);
  const bStr = String(b);
  if (aStr === bStr) return 0;
  return aStr < bStr ? -1 : 1;
}

function recordField<T>(record: Record<T>, field: string): unknown {
  const data = record.data;
  // A payload field wins over metadata with the same name for compatibility.
  if (data !== null && typeof data === 'object' && Object.prototype.hasOwnProperty.call(data, field)) {
    return (data as globalThis.Record<string, unknown>)[field];
  }
  return (record as unknown as globalThis.Record<string, unknown>)[field];
}

function normalizedAppId(appId?: string): string {
  return appId?.trim().replace(/[^\p{Alphabetic}\p{Number}_-]/gu, '_') || '_default';
}

function validPollInterval(interval?: number): interval is number {
  return typeof interval === 'number' && Number.isFinite(interval) && interval > 0;
}

/** Keep subscriptions stable across renders and dispose late registrations. */
function useTauriEvent<T>(
  name: string,
  callback: (payload: T) => void,
  enabled = true,
  onError: (error: unknown) => void = console.error,
) {
  const handlers = useRef({ callback, onError });
  useEffect(() => { handlers.current = { callback, onError }; });
  useEffect(() => {
    if (!enabled) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    const fail = (error: unknown) => {
      if (!disposed) handlers.current.onError(error);
    };
    // Tauri types this as void, but some bridge versions return a promise.
    const dispose = (stop: () => void) => { void Promise.resolve().then(stop).catch(console.error); };
    Promise.resolve().then(() => listen<T>(name, event => {
      if (!disposed) handlers.current.callback(event.payload);
    })).then(stop => {
      if (disposed) dispose(stop);
      else unlisten = stop;
    }).catch(fail);
    return () => {
      disposed = true;
      if (unlisten) dispose(unlisten);
    };
  }, [name, enabled]);
}

function useSnapshot<T>(command: string, pollInterval?: number, appId?: string) {
  const [, render] = useState(0);
  const scope = useMemo(() => ({
    active: false, requestId: 0, data: null as T | null,
    loading: true, error: null as string | null,
  }), [command, appId]);
  const refresh = useCallback(async () => {
    if (!scope.active) return;
    const requestId = ++scope.requestId;
    scope.loading = true;
    render(value => value + 1);
    try {
      const data = await invoke<T>(command, { appId });
      if (scope.active && requestId === scope.requestId) {
        scope.data = data;
        scope.error = null;
      }
    } catch (error) {
      if (scope.active && requestId === scope.requestId) scope.error = String(error);
    } finally {
      if (scope.active && requestId === scope.requestId) {
        scope.loading = false;
        render(value => value + 1);
      }
    }
  }, [command, appId, scope]);
  useEffect(() => {
    scope.active = true;
    void refresh();
    return () => { scope.active = false; scope.requestId += 1; };
  }, [scope, refresh]);
  useEffect(() => {
    if (!validPollInterval(pollInterval)) return;
    const interval = setInterval(() => { if (!scope.loading) void refresh(); }, pollInterval);
    return () => clearInterval(interval);
  }, [pollInterval, refresh, scope]);
  return { data: scope.data, loading: scope.loading, error: scope.error, refresh };
}

function useDatabaseTransfer(command: string, pathKey: string, appId?: string) {
  const [, render] = useState(0);
  const scope = useMemo(() => ({ active: false, pending: 0, error: null as string | null }), [command, appId]);
  useEffect(() => {
    scope.active = true;
    return () => { scope.active = false; };
  }, [scope]);
  const run = useCallback(async (path: string): Promise<boolean> => {
    if (!scope.active) return false;
    scope.pending += 1;
    scope.error = null;
    render(value => value + 1);
    try {
      await invoke(command, { [pathKey]: path, appId });
      return true;
    } catch (error) {
      if (scope.active) scope.error = String(error);
      return false;
    } finally {
      scope.pending -= 1;
      if (scope.active) render(value => value + 1);
    }
  }, [command, pathKey, appId, scope]);
  return { run, pending: scope.pending > 0, error: scope.error };
}

/**
 * Hook for managing a collection of records
 *
 * @param collectionName - The name of the collection to manage
 * @param options - Configuration options
 * @returns Collection state and CRUD operations
 *
 * @example
 * ```tsx
 * interface Note {
 *   title: string;
 *   content: string;
 * }
 *
 * function NotesApp() {
 *   const { records, loading, create, update, remove } = useCollection<Note>("notes");
 *
 *   const handleCreate = async () => {
 *     await create({ title: "New Note", content: "Hello!" });
 *   };
 *
 *   if (loading) return <div>Loading...</div>;
 *
 *   return (
 *     <div>
 *       {records.map(record => (
 *         <div key={record.id}>{record.data.title}</div>
 *       ))}
 *       <button onClick={handleCreate}>Add Note</button>
 *     </div>
 *   );
 * }
 * ```
 */
export function useCollection<T>(
  collectionName: string,
  options: UseCollectionOptions = {}
): UseCollectionReturn<T> {
  const {
    appId,
    autoRefresh = true,
    pollInterval,
    optimisticUpdates = false,
    initialData,
    sortBy,
    sortOrder = 'asc',
  } = options;
  const [, render] = useState(0);
  // Each collection/app owns its request and mutation state. A delayed command
  // from a previous workspace must never write into the current workspace.
  const scope = useMemo(() => ({
    active: false,
    records: (initialData as Record<T>[]) ?? [],
    loading: true,
    error: null as string | null,
    requestId: 0,
    nextMutation: 0,
    mutations: new Map<number, (records: Record<T>[]) => Record<T>[]>(),
  }), [collectionName, appId]);
  const currentScope = useRef(scope);
  currentScope.current = scope;
  const publish = useCallback(() => {
    if (scope.active && currentScope.current === scope) render(value => value + 1);
  }, [scope]);

  const refresh = useCallback(async () => {
    if (!scope.active || currentScope.current !== scope) return;
    const requestId = ++scope.requestId;
    scope.loading = true;
    publish();
    try {
      const data = await invoke<Record<T>[]>("get_collection", {
        collection: collectionName, appId,
      });
      if (scope.active && requestId === scope.requestId) {
        scope.records = data;
        scope.error = null;
      }
    } catch (error) {
      if (scope.active && requestId === scope.requestId) scope.error = String(error);
    } finally {
      if (scope.active && requestId === scope.requestId) {
        scope.loading = false;
        publish();
      }
    }
  }, [collectionName, appId, scope, publish]);

  const mutate = useCallback(async <R>(
    command: string,
    args: globalThis.Record<string, unknown>,
    optimistic: (records: Record<T>[], mutationId: number) => Record<T>[],
    commit: (records: Record<T>[], result: R) => Record<T>[],
  ): Promise<R | null> => {
    if (!scope.active || currentScope.current !== scope) return null;
    const mutationId = ++scope.nextMutation;
    // Optimistic changes are overlays, not snapshots to roll back. A refresh or
    // failed concurrent edit cannot discard another edit still in progress.
    scope.mutations.set(mutationId, records => optimisticUpdates
      ? optimistic(records, mutationId) : records);
    scope.error = null;
    publish();
    try {
      const result = await invoke<R>(command, { ...args, appId });
      if (scope.active && currentScope.current === scope) {
        scope.records = commit(scope.records, result);
        scope.mutations.delete(mutationId);
        publish();
        await refresh();
      }
      return result;
    } catch (error) {
      if (scope.active && currentScope.current === scope) {
        scope.mutations.delete(mutationId);
        scope.error = String(error);
        publish();
      }
      return null;
    } finally {
      scope.mutations.delete(mutationId);
      publish();
    }
  }, [scope, appId, optimisticUpdates, publish, refresh]);

  const create = useCallback((data: T): Promise<Record<T> | null> => {
    const timestamp = new Date().toISOString();
    return mutate<Record<T>>("create_record", { payload: { collection: collectionName, data } },
      (records, mutationId) => [...records, {
        id: `temp-${mutationId}`, collection: collectionName, data,
        created_at: timestamp, updated_at: timestamp, deleted: false,
      }],
      (records, record) => [...records.filter(item => item.id !== record.id), record]);
  }, [collectionName, mutate]);

  const update = useCallback((id: string, data: T): Promise<Record<T> | null> => {
    const timestamp = new Date().toISOString();
    return mutate<Record<T>>("update_record", { payload: { id, data } },
      records => records.map(record => record.id === id
        ? { ...record, data, updated_at: timestamp } : record),
      (records, record) => [...records.filter(item => item.id !== id), record]);
  }, [mutate]);

  const remove = useCallback(async (id: string): Promise<boolean> => {
    const result = await mutate<boolean>("delete_record", { id },
      records => records.filter(record => record.id !== id),
      (records, deleted) => deleted ? records.filter(record => record.id !== id) : records);
    return result === true;
  }, [mutate]);

  const requestSync = useCallback(async (): Promise<boolean> => {
    try {
      await invoke("request_sync", { collection: collectionName, appId });
      return true;
    } catch (error) {
      if (scope.active && currentScope.current === scope) {
        scope.error = String(error);
        publish();
      }
      return false;
    }
  }, [collectionName, appId, scope, publish]);

  const clearError = useCallback(() => {
    scope.error = null;
    publish();
  }, [scope, publish]);

  useEffect(() => {
    scope.active = true;
    void refresh();
    return () => {
      scope.active = false;
      scope.requestId += 1;
    };
  }, [scope, refresh]);

  const subscriptionError = useCallback((error: unknown) => {
    scope.error = String(error);
    publish();
  }, [scope, publish]);
  useTauriEvent<SyncEvent>("xdb-sync-event", event => {
    if (event.collection === collectionName && normalizedAppId(appId) === normalizedAppId(event.app_id)) {
      void refresh();
    }
  }, autoRefresh, subscriptionError);
  useTauriEvent<{ collection?: string; app_id?: string }>("xdb-data-event", event => {
    if ((!event.collection || event.collection === collectionName)
      && normalizedAppId(appId) === normalizedAppId(event.app_id)) void refresh();
  }, autoRefresh, subscriptionError);
  useEffect(() => {
    if (!validPollInterval(pollInterval)) return;
    const interval = setInterval(() => { if (!scope.loading) void refresh(); }, pollInterval);
    return () => clearInterval(interval);
  }, [refresh, pollInterval, scope]);

  let records = scope.records;
  for (const optimistic of scope.mutations.values()) records = optimistic(records);
  if (sortBy) records = [...records].sort((a, b) => {
    const order = compareUnknown(recordField(a, sortBy), recordField(b, sortBy));
    return sortOrder === 'asc' ? order : -order;
  });
  const getById = useCallback((id: string) => records.find(record => record.id === id), [records]);
  return {
    records, loading: scope.loading, error: scope.error,
    mutating: scope.mutations.size > 0,
    refresh, create, update, remove, requestSync, clearError, getById,
  };
}

/**
 * Hook for finding/filtering records in a collection
 *
 * @param collectionName - The name of the collection to search
 * @param options - Query options (filters, sort, pagination)
 * @returns Filtered records and the matching total before pagination
 *
 * @example
 * ```tsx
 * function SearchNotes() {
 *   const { records, loading } = useFind<Note>("notes", {
 *     filters: [
 *       { field: "title", operator: "contains", value: "important" }
 *     ],
 *     sortBy: "created_at",
 *     sortOrder: "desc",
 *     limit: 10
 *   });
 *
 *   if (loading) return <div>Searching...</div>;
 *   return <NotesList notes={records} />;
 * }
 * ```
 */
export function useFind<T>(
  collectionName: string,
  options: UseFindOptions = {}
) {
  const { appId, filters = [], sortBy, sortOrder = 'asc', limit, offset = 0 } = options;

  const { records: allRecords, loading, error, refresh } = useCollection<T>(collectionName, { appId });

  // Apply filters, sort, and pagination client-side
  const { records, total } = useMemo(() => {
    let result = [...allRecords];

    // Apply filters
    for (const filter of filters) {
      result = result.filter(record => {
        const value = recordField(record, filter.field);
        switch (filter.operator) {
          case 'eq':
            return value === filter.value;
          case 'ne':
            return value !== filter.value;
          case 'gt':
            return (value as number) > (filter.value as number);
          case 'gte':
            return (value as number) >= (filter.value as number);
          case 'lt':
            return (value as number) < (filter.value as number);
          case 'lte':
            return (value as number) <= (filter.value as number);
          case 'contains':
            return value != null && String(value).toLowerCase().includes(String(filter.value).toLowerCase());
          case 'startsWith':
            return value != null && String(value).toLowerCase().startsWith(String(filter.value).toLowerCase());
          case 'endsWith':
            return value != null && String(value).toLowerCase().endsWith(String(filter.value).toLowerCase());
          default:
            return true;
        }
      });
    }

    // Apply sort
    if (sortBy) {
      result.sort((a, b) => {
        const aVal = recordField(a, sortBy);
        const bVal = recordField(b, sortBy);
        const cmp = compareUnknown(aVal, bVal);
        return sortOrder === 'asc' ? cmp : -cmp;
      });
    }

    const total = result.length;
    // Apply pagination
    if (limit !== undefined) {
      result = result.slice(offset, offset + limit);
    } else if (offset > 0) {
      result = result.slice(offset);
    }

    return { records: result, total };
  }, [allRecords, filters, sortBy, sortOrder, limit, offset]);

  return {
    records,
    total,
    loading,
    error,
    refresh,
  };
}

/**
 * Hook for database statistics
 *
 * @param pollInterval - How often to refresh stats in ms (default: 5000)
 * @returns Database statistics
 *
 * @example
 * ```tsx
 * function StatsPanel() {
 *   const { stats, loading } = useDbStats();
 *
 *   if (loading || !stats) return <div>Loading...</div>;
 *
 *   return (
 *     <div>
 *       <p>Records: {stats.record_count}</p>
 *       <p>Collections: {stats.collection_count}</p>
 *       <p>Size: {stats.db_size_bytes} bytes</p>
 *     </div>
 *   );
 * }
 * ```
 */
export function useDbStats(pollInterval = 5000, appId?: string) {
  const { data: stats, loading, error, refresh } = useSnapshot<DbStats>("get_db_stats", pollInterval, appId);
  useTauriEvent<{ app_id?: string }>("xdb-data-event", event => {
    if (normalizedAppId(event.app_id) === normalizedAppId(appId)) void refresh();
  });
  useTauriEvent<SyncEvent>("xdb-sync-event", event => {
    if (normalizedAppId(event.app_id) === normalizedAppId(appId)) void refresh();
  });
  return { stats, loading, error, refresh };
}

/**
 * Hook for network status
 *
 * @param pollInterval - How often to refresh status in ms (default: 2000)
 * @returns Network status information
 *
 * @example
 * ```tsx
 * function NetworkPanel() {
 *   const { status, loading } = useNetworkStatus();
 *
 *   if (loading || !status) return <div>Loading...</div>;
 *
 *   return (
 *     <div>
 *       <p>Status: {status.is_running ? "Online" : "Offline"}</p>
 *       <p>Peer ID: {status.peer_id}</p>
 *       <p>Connected Peers: {status.connected_peers.length}</p>
 *     </div>
 *   );
 * }
 * ```
 */
export function useNetworkStatus(pollInterval = 2000) {
  const { data: status, loading, error, refresh } = useSnapshot<NetworkStatus>("get_network_status", pollInterval);
  useTauriEvent<PeerEvent>("xdb-peer-event", () => { void refresh(); });
  return { status, loading, error, refresh };
}

/**
 * Hook for getting the database file path
 *
 * @returns The database file path
 *
 * @example
 * ```tsx
 * function DbPathDisplay() {
 *   const path = useDbPath();
 *   return <p>Database: {path}</p>;
 * }
 * ```
 */
export function useDbPath(appId?: string) {
  const { data } = useSnapshot<string>("get_db_path", undefined, appId);
  return data ?? "";
}

/**
 * Hook for database export functionality
 *
 * @returns Export function and loading state
 *
 * @example
 * ```tsx
 * function ExportButton() {
 *   const { exportDb, exporting } = useDbExport();
 *
 *   const handleExport = async () => {
 *     const success = await exportDb("/path/to/backup.sqlite");
 *     if (success) alert("Exported!");
 *   };
 *
 *   return (
 *     <button onClick={handleExport} disabled={exporting}>
 *       {exporting ? "Exporting..." : "Export Database"}
 *     </button>
 *   );
 * }
 * ```
 */
export function useDbExport(appId?: string) {
  const { run: exportDb, pending: exporting, error } = useDatabaseTransfer("export_database", "path", appId);
  return { exportDb, exporting, error };
}

/**
 * Hook for database import functionality
 *
 * @returns Import function and loading state
 *
 * @example
 * ```tsx
 * function ImportButton() {
 *   const { importDb, importing } = useDbImport();
 *
 *   const handleImport = async () => {
 *     const success = await importDb("/path/to/backup.sqlite");
 *     if (success) alert("Imported!");
 *   };
 *
 *   return (
 *     <button onClick={handleImport} disabled={importing}>
 *       {importing ? "Importing..." : "Import Database"}
 *     </button>
 *   );
 * }
 * ```
 */
export function useDbImport(appId?: string) {
  const { run: importDb, pending: importing, error } = useDatabaseTransfer("import_database", "sourcePath", appId);
  return { importDb, importing, error };
}

/**
 * Hook to listen for XDB sync events
 *
 * @param callback - Function to call when a sync event occurs
 *
 * @example
 * ```tsx
 * function SyncListener() {
 *   useSyncEvents((event) => {
 *     console.log(`Synced collection: ${event.collection}`);
 *   });
 *
 *   return null;
 * }
 * ```
 */
export function useSyncEvents(callback: (event: SyncEvent) => void) {
  useTauriEvent("xdb-sync-event", callback);
}

/**
 * Hook to listen for XDB peer events
 *
 * @param callback - Function to call when a peer event occurs
 *
 * @example
 * ```tsx
 * function PeerListener() {
 *   usePeerEvents((event) => {
 *     console.log(`Peer ${event.type}: ${event.peer_id}`);
 *   });
 *
 *   return null;
 * }
 * ```
 */
export function usePeerEvents(callback: (event: PeerEvent) => void) {
  useTauriEvent("xdb-peer-event", callback);
}
