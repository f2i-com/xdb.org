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
  CreateRecordPayload,
  UpdateRecordPayload,
  SyncEvent,
  PeerEvent,
  UseCollectionOptions,
  UseCollectionReturn,
  UseFindOptions,
  QueryFilter,
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
  return aStr < bStr ? -1 : 1;
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
    autoRefresh = true,
    pollInterval,
    optimisticUpdates = false,
    initialData,
    sortBy,
    sortOrder = 'asc',
  } = options;

  const [records, setRecords] = useState<Record<T>[]>(
    (initialData as Record<T>[]) ?? []
  );
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [mutating, setMutating] = useState(false);
  const mountedRef = useRef(true);
  const requestIdRef = useRef(0);

  // Sort function for records
  const sortRecords = useCallback((data: Record<T>[]): Record<T>[] => {
    if (!sortBy) return data;
    return [...data].sort((a, b) => {
      const aVal = (a.data as globalThis.Record<string, unknown>)[sortBy];
      const bVal = (b.data as globalThis.Record<string, unknown>)[sortBy];
      const cmp = compareUnknown(aVal, bVal);
      return sortOrder === 'asc' ? cmp : -cmp;
    });
  }, [sortBy, sortOrder]);

  const refresh = useCallback(async () => {
    const requestId = ++requestIdRef.current;

    try {
      setLoading(true);
      const data = await invoke<Record<T>[]>("get_collection", {
        collection: collectionName,
      });
      if (mountedRef.current && requestId === requestIdRef.current) {
        setRecords(sortRecords(data));
        setError(null);
      }
    } catch (e) {
      if (mountedRef.current && requestId === requestIdRef.current) {
        setError(String(e));
      }
    } finally {
      if (mountedRef.current && requestId === requestIdRef.current) {
        setLoading(false);
      }
    }
  }, [collectionName, sortRecords]);

  const create = useCallback(
    async (data: T): Promise<Record<T> | null> => {
      const tempId = `temp-${Date.now()}`;
      let optimisticRecord: Record<T> | null = null;

      try {
        setMutating(true);

        // Optimistic update
        if (optimisticUpdates) {
          optimisticRecord = {
            id: tempId,
            collection: collectionName,
            data,
            created_at: new Date().toISOString(),
            updated_at: new Date().toISOString(),
            deleted: false,
          };
          setRecords(prev => sortRecords([...prev, optimisticRecord!]));
        }

        const payload: CreateRecordPayload<T> = {
          collection: collectionName,
          data,
        };
        const record = await invoke<Record<T>>("create_record", { payload });

        // Replace optimistic record with real one
        if (optimisticUpdates) {
          setRecords(prev => sortRecords(prev.map(r => r.id === tempId ? record : r)));
        } else {
          await refresh();
        }

        return record;
      } catch (e) {
        // Revert optimistic update on error
        if (optimisticUpdates && optimisticRecord) {
          setRecords(prev => prev.filter(r => r.id !== tempId));
        }
        setError(String(e));
        return null;
      } finally {
        setMutating(false);
      }
    },
    [collectionName, refresh, optimisticUpdates, sortRecords]
  );

  const update = useCallback(
    async (id: string, data: T): Promise<Record<T> | null> => {
      let previousRecord: Record<T> | undefined;

      try {
        setMutating(true);

        // Optimistic update
        if (optimisticUpdates) {
          setRecords(prev => {
            const idx = prev.findIndex(r => r.id === id);
            if (idx !== -1) {
              previousRecord = prev[idx];
              const updated = [...prev];
              updated[idx] = {
                ...prev[idx],
                data,
                updated_at: new Date().toISOString(),
              };
              return sortRecords(updated);
            }
            return prev;
          });
        }

        const payload: UpdateRecordPayload<T> = { id, data };
        const record = await invoke<Record<T>>("update_record", { payload });

        if (!optimisticUpdates) {
          await refresh();
        }

        return record;
      } catch (e) {
        // Revert optimistic update on error
        if (optimisticUpdates && previousRecord) {
          setRecords(prev => sortRecords(prev.map(r => r.id === id ? previousRecord! : r)));
        }
        setError(String(e));
        return null;
      } finally {
        setMutating(false);
      }
    },
    [refresh, optimisticUpdates, sortRecords]
  );

  const remove = useCallback(
    async (id: string): Promise<boolean> => {
      let removedRecord: Record<T> | undefined;

      try {
        setMutating(true);

        // Optimistic update
        if (optimisticUpdates) {
          setRecords(prev => {
            removedRecord = prev.find(r => r.id === id);
            return prev.filter(r => r.id !== id);
          });
        }

        await invoke("delete_record", { id });

        if (!optimisticUpdates) {
          await refresh();
        }

        return true;
      } catch (e) {
        // Revert optimistic update on error
        if (optimisticUpdates && removedRecord) {
          setRecords(prev => sortRecords([...prev, removedRecord!]));
        }
        setError(String(e));
        return false;
      } finally {
        setMutating(false);
      }
    },
    [refresh, optimisticUpdates, sortRecords]
  );

  const requestSync = useCallback(async (): Promise<boolean> => {
    try {
      await invoke("request_sync", { collection: collectionName });
      return true;
    } catch (e) {
      setError(String(e));
      return false;
    }
  }, [collectionName]);

  // Clear error helper
  const clearError = useCallback(() => {
    setError(null);
  }, []);

  // Get by ID helper
  const getById = useCallback((id: string): Record<T> | undefined => {
    return records.find(r => r.id === id);
  }, [records]);

  // Initial load
  useEffect(() => {
    mountedRef.current = true;
    refresh();

    return () => {
      mountedRef.current = false;
      requestIdRef.current += 1;
    };
  }, [refresh]);

  // Listen for sync events
  useEffect(() => {
    if (!autoRefresh) return;

    const unlistenSync = listen<SyncEvent>("xdb-sync-event", (event) => {
      if (event.payload.collection === collectionName) {
        refresh();
      }
    });

    return () => {
      unlistenSync.then((f) => f());
    };
  }, [collectionName, refresh, autoRefresh]);

  // Optional polling
  useEffect(() => {
    if (!pollInterval) return;

    const interval = setInterval(refresh, pollInterval);
    return () => clearInterval(interval);
  }, [refresh, pollInterval]);

  return {
    records,
    loading,
    error,
    mutating,
    refresh,
    create,
    update,
    remove,
    requestSync,
    clearError,
    getById,
  };
}

/**
 * Hook for finding/filtering records in a collection
 *
 * @param collectionName - The name of the collection to search
 * @param options - Query options (filters, sort, pagination)
 * @returns Filtered records
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
  const { filters = [], sortBy, sortOrder = 'asc', limit, offset = 0 } = options;

  const { records: allRecords, loading, error, refresh } = useCollection<T>(collectionName);

  // Apply filters, sort, and pagination client-side
  const records = useMemo(() => {
    let result = [...allRecords];

    // Apply filters
    for (const filter of filters) {
      result = result.filter(record => {
        const value = (record.data as globalThis.Record<string, unknown>)[filter.field];
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
            return String(value).toLowerCase().includes(String(filter.value).toLowerCase());
          case 'startsWith':
            return String(value).toLowerCase().startsWith(String(filter.value).toLowerCase());
          case 'endsWith':
            return String(value).toLowerCase().endsWith(String(filter.value).toLowerCase());
          default:
            return true;
        }
      });
    }

    // Apply sort
    if (sortBy) {
      result.sort((a, b) => {
        const aVal = (a.data as globalThis.Record<string, unknown>)[sortBy];
        const bVal = (b.data as globalThis.Record<string, unknown>)[sortBy];
        const cmp = compareUnknown(aVal, bVal);
        return sortOrder === 'asc' ? cmp : -cmp;
      });
    }

    // Apply pagination
    if (limit !== undefined) {
      result = result.slice(offset, offset + limit);
    } else if (offset > 0) {
      result = result.slice(offset);
    }

    return result;
  }, [allRecords, filters, sortBy, sortOrder, limit, offset]);

  return {
    records,
    total: allRecords.length,
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
export function useDbStats(pollInterval = 5000) {
  const [stats, setStats] = useState<DbStats | null>(null);
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    try {
      const data = await invoke<DbStats>("get_db_stats");
      setStats(data);
    } catch (e) {
      console.error("Failed to get db stats:", e);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    refresh();
    const interval = setInterval(refresh, pollInterval);
    return () => clearInterval(interval);
  }, [refresh, pollInterval]);

  return { stats, loading, refresh };
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
  const [status, setStatus] = useState<NetworkStatus | null>(null);
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    try {
      const data = await invoke<NetworkStatus>("get_network_status");
      setStatus(data);
    } catch (e) {
      console.error("Failed to get network status:", e);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    refresh();
    const interval = setInterval(refresh, pollInterval);
    return () => clearInterval(interval);
  }, [refresh, pollInterval]);

  // Listen for peer events
  useEffect(() => {
    const unlisten = listen<PeerEvent>("xdb-peer-event", () => {
      refresh();
    });

    return () => {
      unlisten.then((f) => f());
    };
  }, [refresh]);

  return { status, loading, refresh };
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
export function useDbPath() {
  const [path, setPath] = useState<string>("");

  useEffect(() => {
    invoke<string>("get_db_path").then(setPath).catch(console.error);
  }, []);

  return path;
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
export function useDbExport() {
  const [exporting, setExporting] = useState(false);

  const exportDb = useCallback(async (path: string): Promise<boolean> => {
    try {
      setExporting(true);
      await invoke("export_database", { path });
      return true;
    } catch (e) {
      console.error("Failed to export database:", e);
      return false;
    } finally {
      setExporting(false);
    }
  }, []);

  return { exportDb, exporting };
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
export function useDbImport() {
  const [importing, setImporting] = useState(false);

  const importDb = useCallback(async (sourcePath: string): Promise<boolean> => {
    try {
      setImporting(true);
      await invoke("import_database", { sourcePath });
      return true;
    } catch (e) {
      console.error("Failed to import database:", e);
      return false;
    } finally {
      setImporting(false);
    }
  }, []);

  return { importDb, importing };
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
  useEffect(() => {
    const unlisten = listen<SyncEvent>("xdb-sync-event", (event) => {
      callback(event.payload);
    });

    return () => {
      unlisten.then((f) => f());
    };
  }, [callback]);
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
  useEffect(() => {
    const unlisten = listen<PeerEvent>("xdb-peer-event", (event) => {
      callback(event.payload);
    });

    return () => {
      unlisten.then((f) => f());
    };
  }, [callback]);
}
