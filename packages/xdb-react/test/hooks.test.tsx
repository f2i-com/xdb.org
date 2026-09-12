import React, { act, StrictMode } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  useCollection, useDbExport, useDbImport, useDbPath, useDbStats,
  useFind, useNetworkStatus, usePeerEvents, useSyncEvents,
} from '../src/hooks';
import type { Record as XdbRecord } from '../src/types';

const tauri = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: tauri.invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen: tauri.listen }));

type Note = { title: string; group?: string };
const record = (id: string, title: string, extras = {}): XdbRecord<Note> => ({
  id, collection: 'notes', data: { title }, deleted: false,
  created_at: '2026-01-01T00:00:00Z', updated_at: '2026-01-01T00:00:00Z', ...extras,
});
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

const roots: Root[] = [];
const events = new Map<string, Set<(event: { payload: any }) => void>>();
async function mount<P, R>(hook: (props: P) => R, props: P, strict = false) {
  let current!: R;
  const root = createRoot(document.createElement('div'));
  roots.push(root);
  function Harness({ value }: { value: P }) { current = hook(value); return null; }
  const render = async (value: P) => {
    await act(async () => {
      root.render(strict ? <StrictMode><Harness value={value} /></StrictMode> : <Harness value={value} />);
    });
  };
  await render(props);
  return {
    get current() { return current; }, render,
    unmount: async () => { await act(async () => root.unmount()); roots.splice(roots.indexOf(root), 1); },
  };
}
async function emit(name: string, payload: unknown) {
  await act(async () => {
    for (const callback of events.get(name) ?? []) callback({ payload });
  });
}

beforeEach(() => {
  (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
  events.clear();
  tauri.invoke.mockReset().mockResolvedValue([]);
  tauri.listen.mockReset().mockImplementation(async (name, callback) => {
    const callbacks = events.get(name) ?? new Set();
    events.set(name, callbacks);
    callbacks.add(callback);
    return () => callbacks.delete(callback);
  });
});
afterEach(async () => {
  await act(async () => { for (const root of roots.splice(0)) root.unmount(); });
  vi.useRealTimers();
});

describe('collection lifecycle and optimistic operations', () => {
  it('keeps a delayed old-app mutation and retained refresh out of the selected app', async () => {
    const pending = deferred<XdbRecord<Note>>();
    tauri.invoke.mockImplementation((command, args) => command === 'create_record'
      ? pending.promise : Promise.resolve([record(args.appId, args.appId)]));
    const hook = await mount((appId: string) => useCollection<Note>('notes', { appId, optimisticUpdates: true }), 'old');
    const oldRefresh = hook.current.refresh;
    let write!: Promise<XdbRecord<Note> | null>;
    await act(async () => { write = hook.current.create({ title: 'Delayed old record' }); });
    await hook.render('new');
    expect(hook.current.records.map(row => row.id)).toEqual(['new']);
    expect(hook.current.mutating).toBe(false);
    await act(async () => { pending.resolve(record('old-result', 'Old result')); await write; await oldRefresh(); });
    expect(hook.current.records.map(row => row.id)).toEqual(['new']);
    expect(hook.current.error).toBeNull();
    expect(tauri.invoke.mock.calls.filter(([command]) => command === 'get_collection')).toHaveLength(2);
  });

  it('ignores a failed old-collection write and a late initial read after switching', async () => {
    const oldRead = deferred<XdbRecord<Note>[]>();
    const oldWrite = deferred<XdbRecord<Note>>();
    tauri.invoke.mockImplementation((command, args) => command === 'create_record' ? oldWrite.promise
      : args.collection === 'old' ? oldRead.promise : Promise.resolve([record('new', 'New')]));
    const hook = await mount((name: string) => useCollection<Note>(name), 'old');
    let write!: Promise<XdbRecord<Note> | null>;
    await act(async () => { write = hook.current.create({ title: 'Old' }); });
    await hook.render('new');
    await act(async () => { oldRead.resolve([record('old', 'Old')]); oldWrite.reject('Old error'); await write; });
    expect(hook.current.records.map(row => row.id)).toEqual(['new']);
    expect(hook.current.error).toBeNull();
    expect(hook.current.loading).toBe(false);
  });

  it('keeps simultaneous creates distinct through sync refresh and one failed write', async () => {
    const writes = [deferred<XdbRecord<Note>>(), deferred<XdbRecord<Note>>()];
    let saved: XdbRecord<Note>[] = [];
    let writeIndex = 0;
    vi.spyOn(Date, 'now').mockReturnValue(1000);
    tauri.invoke.mockImplementation(command => command === 'create_record'
      ? writes[writeIndex++].promise : Promise.resolve(saved));
    const hook = await mount(() => useCollection<Note>('notes', { optimisticUpdates: true }), undefined);
    let first!: Promise<XdbRecord<Note> | null>;
    let second!: Promise<XdbRecord<Note> | null>;
    await act(async () => {
      first = hook.current.create({ title: 'First' }); second = hook.current.create({ title: 'Second' });
    });
    expect(new Set(hook.current.records.map(row => row.id)).size).toBe(2);
    await emit('xdb-sync-event', { type: 'sync_update', collection: 'notes' });
    expect(hook.current.records.map(row => row.data.title)).toEqual(['First', 'Second']);
    saved = [record('first', 'First')];
    await act(async () => { writes[0].resolve(saved[0]); await first; });
    expect(hook.current.mutating).toBe(true);
    expect(hook.current.records.map(row => row.data.title)).toEqual(['First', 'Second']);
    await act(async () => { writes[1].reject('Second failed'); await second; });
    expect(hook.current.records.map(row => row.id)).toEqual(['first']);
    expect(hook.current.error).toBe('Second failed');
    expect(hook.current.mutating).toBe(false);
  });

  it('does not roll a newer optimistic update back when an older one fails', async () => {
    const writes = [deferred<XdbRecord<Note>>(), deferred<XdbRecord<Note>>()];
    let saved = record('note', 'Original');
    let index = 0;
    tauri.invoke.mockImplementation(command => command === 'update_record' ? writes[index++].promise : Promise.resolve([saved]));
    const hook = await mount(() => useCollection<Note>('notes', { optimisticUpdates: true }), undefined);
    let first!: Promise<XdbRecord<Note> | null>;
    let second!: Promise<XdbRecord<Note> | null>;
    await act(async () => { first = hook.current.update('note', { title: 'First' }); second = hook.current.update('note', { title: 'Second' }); });
    await act(async () => { writes[0].reject('First failed'); await first; });
    expect(hook.current.records[0].data.title).toBe('Second');
    saved = record('note', 'Second normalized', { updated_at: '2026-01-03T00:00:00Z' });
    await act(async () => { writes[1].resolve(saved); await second; });
    expect(hook.current.records).toEqual([saved]);
    expect(hook.current.error).toBeNull();
  });

  it('refreshes only relevant app/collection events and supports all CRUD scope arguments', async () => {
    tauri.invoke.mockImplementation(command => Promise.resolve(command === 'get_collection' ? [] : command === 'delete_record' ? true : record('note', 'Note')));
    const hook = await mount(() => useCollection<Note>('notes', { appId: '  work.space  ' }), undefined);
    await emit('xdb-sync-event', { collection: 'notes', type: 'sync_update' });
    await emit('xdb-data-event', { app_id: 'elsewhere', collection: 'notes' });
    await emit('xdb-data-event', { app_id: 'work_space', collection: 'other' });
    expect(tauri.invoke).toHaveBeenCalledTimes(1);
    await emit('xdb-data-event', { app_id: 'work_space', collection: 'notes' });
    await emit('xdb-data-event', { app_id: 'work_space', type: 'import' });
    expect(tauri.invoke).toHaveBeenCalledTimes(3);
    await act(async () => {
      await hook.current.create({ title: 'New' }); await hook.current.update('note', { title: 'Updated' });
      expect(await hook.current.remove('note')).toBe(true); await hook.current.requestSync();
    });
    for (const [, args] of tauri.invoke.mock.calls) expect(args.appId).toBe('  work.space  ');
  });

  it('restores an optimistic deletion on failure without duplicating a concurrently refreshed row', async () => {
    const deletion = deferred<boolean>();
    tauri.invoke.mockImplementation(command => command === 'delete_record' ? deletion.promise : Promise.resolve([record('note', 'Original')]));
    const hook = await mount(() => useCollection<Note>('notes', { optimisticUpdates: true }), undefined);
    let remove!: Promise<boolean>;
    await act(async () => { remove = hook.current.remove('note'); });
    await emit('xdb-sync-event', { collection: 'notes', type: 'sync_update' });
    expect(hook.current.records).toEqual([]);
    await act(async () => { deletion.reject('Cannot delete'); expect(await remove).toBe(false); });
    expect(hook.current.records.map(row => row.id)).toEqual(['note']);
  });
});

describe('subscriptions and snapshots', () => {
  it('keeps listeners stable while invoking the latest callback', async () => {
    const seen: string[] = [];
    const hook = await mount((value: string) => {
      useSyncEvents(() => seen.push(`sync:${value}`));
      usePeerEvents(() => seen.push(`peer:${value}`));
    }, 'first');
    await hook.render('second');
    await emit('xdb-sync-event', {}); await emit('xdb-peer-event', {});
    expect(tauri.listen).toHaveBeenCalledTimes(2);
    expect(seen).toEqual(['sync:second', 'peer:second']);
    await hook.unmount();
    await emit('xdb-sync-event', {});
    expect(seen).toHaveLength(2);
  });

  it('disposes delayed registrations and suppresses callbacks after unmount', async () => {
    const registration = deferred<() => void>();
    const callback = vi.fn();
    const stop = vi.fn();
    tauri.listen.mockReturnValue(registration.promise);
    const hook = await mount(() => usePeerEvents(callback), undefined);
    const handler = tauri.listen.mock.calls[0][1];
    await hook.unmount();
    await act(async () => { handler({ payload: {} }); registration.resolve(stop); });
    expect(callback).not.toHaveBeenCalled();
    expect(stop).toHaveBeenCalledOnce();
  });

  it('handles subscription rejection and StrictMode cleanup without an unhandled promise', async () => {
    const log = vi.spyOn(console, 'error').mockImplementation(() => {});
    tauri.listen.mockRejectedValue(new Error('No event permission'));
    const hook = await mount(() => useCollection('notes'), undefined, true);
    expect(hook.current.error).toContain('No event permission');
    await hook.unmount();
    const listener = await mount(() => useSyncEvents(() => {}), undefined);
    expect(log).toHaveBeenCalled();
    await listener.unmount();
  });

  it('handles a bridge rejecting asynchronous listener cleanup', async () => {
    const log = vi.spyOn(console, 'error').mockImplementation(() => {});
    tauri.listen.mockResolvedValue(() => Promise.reject('Bridge already closed'));
    const hook = await mount(() => usePeerEvents(() => {}), undefined);
    await hook.unmount();
    expect(log).toHaveBeenCalledWith('Bridge already closed');
  });

  it('keeps newest stats, exposes failures, and clears error on recovery', async () => {
    const old = deferred<any>();
    const newer = { record_count: 2, collection_count: 1, db_size_bytes: 50 };
    tauri.invoke.mockReturnValueOnce(old.promise).mockResolvedValue(newer);
    const hook = await mount(() => useDbStats(0, 'work'), undefined);
    await act(async () => { await hook.current.refresh(); old.resolve({ ...newer, record_count: 1 }); });
    expect(hook.current.stats).toEqual(newer);
    tauri.invoke.mockRejectedValueOnce('Database unavailable');
    await act(async () => { await hook.current.refresh(); });
    expect(hook.current.error).toBe('Database unavailable');
    expect(hook.current.stats).toEqual(newer);
    await act(async () => { await hook.current.refresh(); });
    expect(hook.current.error).toBeNull();
    expect(hook.current.loading).toBe(false);
    expect(tauri.invoke).toHaveBeenCalledWith('get_db_stats', { appId: 'work' });
  });

  it('disables nonpositive polling and refreshes network status on peer changes', async () => {
    vi.useFakeTimers();
    tauri.invoke.mockResolvedValue({ peer_id: 'self', connected_peers: [], is_running: true });
    const hook = await mount(() => useNetworkStatus(0), undefined);
    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
    expect(tauri.invoke).toHaveBeenCalledOnce();
    await emit('xdb-peer-event', { type: 'connected', peer_id: 'other' });
    expect(tauri.invoke).toHaveBeenCalledTimes(2);
    expect(hook.current.status?.is_running).toBe(true);
  });

  it('does not pile up polls or starve a response when the backend is slower than its interval', async () => {
    vi.useFakeTimers();
    const pending = deferred<any>();
    tauri.invoke.mockReturnValue(pending.promise);
    const hook = await mount(() => useDbStats(10), undefined);
    await act(async () => { await vi.advanceTimersByTimeAsync(100); });
    expect(tauri.invoke).toHaveBeenCalledOnce();
    await act(async () => { pending.resolve({ record_count: 1, collection_count: 1, db_size_bytes: 1 }); });
    expect(hook.current.stats?.record_count).toBe(1);
    await act(async () => { await vi.advanceTimersByTimeAsync(10); });
    expect(tauri.invoke).toHaveBeenCalledTimes(2);
  });

  it('ignores late path lookups from a previous app', async () => {
    const old = deferred<string>();
    tauri.invoke.mockImplementation((_command, args) => args.appId === 'old' ? old.promise : Promise.resolve('/new/db.sqlite'));
    const hook = await mount((appId: string) => useDbPath(appId), 'old');
    await hook.render('new');
    await act(async () => old.resolve('/old/db.sqlite'));
    expect(hook.current).toBe('/new/db.sqlite');
  });
});

describe('queries and database transfers', () => {
  it('reports filtered total before pagination and sorts metadata without crashing on null payloads', async () => {
    tauri.invoke.mockResolvedValue([
      record('early', 'Keep', { created_at: '2026-01-01' }),
      record('late', 'Keep', { created_at: '2026-01-03' }),
      record('other', 'Skip', { created_at: '2026-01-02' }),
      record('null', '', { data: null }),
    ]);
    const hook = await mount(() => useFind<Note>('notes', {
      appId: 'work', filters: [{ field: 'title', operator: 'contains', value: 'Keep' }],
      sortBy: 'created_at', sortOrder: 'desc', limit: 1,
    }), undefined);
    expect(hook.current.total).toBe(2);
    expect(hook.current.records.map(row => row.id)).toEqual(['late']);
    expect(tauri.invoke).toHaveBeenCalledWith('get_collection', { collection: 'notes', appId: 'work' });
  });

  it('keeps transfer state pending until both operations settle and exposes scoped import errors', async () => {
    const first = deferred<unknown>();
    const second = deferred<unknown>();
    tauri.invoke.mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise).mockRejectedValue('Invalid backup');
    const hook = await mount(() => ({ ...useDbExport('work'), ...useDbImport('work') }), undefined);
    let export1!: Promise<boolean>; let export2!: Promise<boolean>;
    await act(async () => { export1 = hook.current.exportDb('/one'); export2 = hook.current.exportDb('/two'); });
    await act(async () => { first.resolve('/one'); await export1; });
    expect(hook.current.exporting).toBe(true);
    await act(async () => { second.resolve('/two'); await export2; });
    expect(hook.current.exporting).toBe(false);
    await act(async () => { expect(await hook.current.importDb('/bad')).toBe(false); });
    expect(hook.current.error).toBe('Invalid backup');
    expect(tauri.invoke).toHaveBeenCalledWith('import_database', { sourcePath: '/bad', appId: 'work' });
  });
});
