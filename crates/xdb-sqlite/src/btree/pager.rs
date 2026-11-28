//! Metered LRU page cache over a [`PageSource`] with memoized [`BtreePage`] views.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use xdb_core::{Meter, NoopMeter};

use crate::errors::{SqliteError, SqliteResult};
use crate::format::{BtreePage, DatabaseInfo};
use crate::page_source::PageSource;

/// Default cache budget in bytes (SQLite default `cache_size = -2000`, i.e. 2000 KiB).
pub const DEFAULT_CACHE_BYTES: usize = 2_048_000;

/// Cache hit/miss statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PagerStats {
    pub hits: u64,
    pub misses: u64,
}

#[derive(Debug)]
struct CacheEntry {
    data: Arc<[u8]>,
    btree: Option<Arc<BtreePage>>,
    tick: u64,
}

#[derive(Debug)]
struct PagerInner {
    source: Box<dyn PageSource>,
    capacity: usize,
    stats: PagerStats,
    cache: HashMap<u32, CacheEntry>,
    /// Approximate-LRU eviction queue storing `(pgno, inserted_tick)`.
    lru: VecDeque<(u32, u64)>,
    tick: u64,
    meter: Arc<dyn Meter>,
    noop_meter: bool,
    tracked_bytes: u64,
}

impl Drop for PagerInner {
    fn drop(&mut self) {
        if self.tracked_bytes > 0 {
            self.meter.release_memory(self.tracked_bytes);
            self.tracked_bytes = 0;
        }
    }
}

/// Thread-safe, metered LRU pager handle (cheaply cloneable via `Arc`).
#[derive(Debug, Clone)]
pub struct Pager {
    db: DatabaseInfo,
    meter: Arc<dyn Meter>,
    noop_meter: bool,
    inner: Arc<Mutex<PagerInner>>,
}

impl Pager {
    pub fn new(
        source: Box<dyn PageSource>,
        db: DatabaseInfo,
        cache_pages: usize,
        meter: Option<Arc<dyn Meter>>,
    ) -> Self {
        let meter = meter.unwrap_or_else(|| Arc::new(NoopMeter));
        let noop_meter = meter.is_noop();
        let capacity = cache_pages.max(1);
        Self {
            db,
            meter: meter.clone(),
            noop_meter,
            inner: Arc::new(Mutex::new(PagerInner {
                source,
                capacity,
                stats: PagerStats::default(),
                cache: HashMap::new(),
                lru: VecDeque::new(),
                tick: 0,
                meter,
                noop_meter,
                tracked_bytes: 0,
            })),
        }
    }

    #[inline]
    pub fn db(&self) -> &DatabaseInfo {
        &self.db
    }

    #[inline]
    pub fn page_size(&self) -> u32 {
        self.db.page_size
    }

    #[inline]
    pub fn page_count(&self) -> u32 {
        self.db.page_count
    }

    #[inline]
    pub fn meter(&self) -> &Arc<dyn Meter> {
        &self.meter
    }

    #[inline]
    pub fn is_noop_meter(&self) -> bool {
        self.noop_meter
    }

    pub fn stats(&self) -> PagerStats {
        self.inner.lock().expect("pager poisoned").stats
    }

    pub fn cached_pages(&self) -> usize {
        self.inner.lock().expect("pager poisoned").cache.len()
    }

    pub fn clear(&self) {
        let mut guard = self.inner.lock().expect("pager poisoned");
        let inner = &mut *guard;
        inner.cache.clear();
        inner.lru.clear();
        if inner.tracked_bytes > 0 {
            inner.meter.release_memory(inner.tracked_bytes);
            inner.tracked_bytes = 0;
        }
    }

    /// Clear cached pages and close the underlying [`PageSource`] (releasing locks).
    pub fn close(&self) -> SqliteResult<()> {
        let mut guard = self.inner.lock().expect("pager poisoned");
        let inner = &mut *guard;
        inner.cache.clear();
        inner.lru.clear();
        if inner.tracked_bytes > 0 {
            inner.meter.release_memory(inner.tracked_bytes);
            inner.tracked_bytes = 0;
        }
        inner.source.close()
    }

    /// Validate that `pgno` names a readable page (`1..=page_count`, not `lock_byte_page`).
    #[inline]
    pub fn check_pgno(&self, pgno: u32, what: &str) -> SqliteResult<()> {
        if pgno < 1 || pgno > self.db.page_count {
            return Err(SqliteError::corrupt(format!(
                "{what}: page {pgno} is outside 1..={}",
                self.db.page_count
            )));
        }
        if pgno == self.db.lock_byte_page {
            return Err(SqliteError::corrupt(format!(
                "{what}: page {pgno} is the lock-byte page"
            )));
        }
        Ok(())
    }

    /// Prime the cache with an already-read page (e.g. page 1 read during `Btree::open`).
    pub fn prime(&self, pgno: u32, data: Vec<u8>) -> SqliteResult<()> {
        self.prime_shared(pgno, data.into())
    }

    /// Prime the cache with an already-shared immutable page buffer (zero copy).
    pub fn prime_shared(&self, pgno: u32, data: Arc<[u8]>) -> SqliteResult<()> {
        if data.len() != self.db.page_size as usize {
            return Ok(());
        }
        let mut guard = self.inner.lock().expect("pager poisoned");
        let inner = &mut *guard;
        if inner.cache.contains_key(&pgno) {
            return Ok(());
        }
        inner.stats.misses += 1;
        Self::insert_locked(inner, pgno, data, None, self.db.page_size as u64)
    }

    /// Shared immutable bytes of page `pgno`.
    pub fn get_page(&self, pgno: u32, what: &str) -> SqliteResult<Arc<[u8]>> {
        self.check_pgno(pgno, what)?;
        let mut guard = self.inner.lock().expect("pager poisoned");
        let inner = &mut *guard;
        Self::fetch_page_locked(inner, pgno, self.db.page_size)
    }

    /// Memoized parsed [`BtreePage`] for page `pgno`.
    pub fn get_btree_page(&self, pgno: u32, what: &str) -> SqliteResult<Arc<BtreePage>> {
        self.check_pgno(pgno, what)?;
        let mut guard = self.inner.lock().expect("pager poisoned");
        let inner = &mut *guard;
        inner.tick += 1;
        let tick = inner.tick;
        let half_cap = (inner.capacity >> 1).max(1) as u64;
        if let Some(entry) = inner.cache.get_mut(&pgno) {
            if !inner.noop_meter {
                inner.meter.charge_steps(1)?;
            }
            inner.stats.hits += 1;
            let bt = entry.btree.clone();
            let data = if bt.is_none() {
                Some(entry.data.clone())
            } else {
                None
            };
            if tick.saturating_sub(entry.tick) > half_cap {
                entry.tick = tick;
                inner.lru.push_back((pgno, tick));
                if inner.lru.len() > inner.capacity.saturating_mul(2) {
                    let cache = &inner.cache;
                    inner
                        .lru
                        .retain(|(p, t)| cache.get(p).is_some_and(|e| e.tick == *t));
                }
            }
            if let Some(bt) = bt {
                return Ok(bt);
            }
            let parsed = Arc::new(BtreePage::parse(pgno, data.expect("data cloned"), &self.db)?);
            if let Some(entry) = inner.cache.get_mut(&pgno) {
                entry.btree = Some(parsed.clone());
            }
            return Ok(parsed);
        }
        // Cache miss: load page bytes, parse BtreePage, and insert both in a single cache insert!
        if !inner.noop_meter {
            inner.meter.charge_page_reads(1)?;
        }
        inner.stats.misses += 1;
        let arc = inner.source.read_page_shared(pgno)?;
        if arc.len() != self.db.page_size as usize {
            return Err(SqliteError::corrupt(format!(
                "PageSource returned {} bytes for page {pgno}, expected {}",
                arc.len(),
                self.db.page_size
            )));
        }
        let parsed = Arc::new(BtreePage::parse(pgno, arc.clone(), &self.db)?);
        Self::insert_locked(
            inner,
            pgno,
            arc,
            Some(parsed.clone()),
            u64::from(self.db.page_size),
        )?;
        Ok(parsed)
    }

    fn fetch_page_locked(
        inner: &mut PagerInner,
        pgno: u32,
        page_size: u32,
    ) -> SqliteResult<Arc<[u8]>> {
        inner.tick += 1;
        let tick = inner.tick;
        let half_cap = (inner.capacity >> 1).max(1) as u64;
        if let Some(entry) = inner.cache.get_mut(&pgno) {
            if !inner.noop_meter {
                inner.meter.charge_steps(1)?;
            }
            inner.stats.hits += 1;
            let data = entry.data.clone();
            if tick.saturating_sub(entry.tick) > half_cap {
                entry.tick = tick;
                inner.lru.push_back((pgno, tick));
                if inner.lru.len() > inner.capacity.saturating_mul(2) {
                    let cache = &inner.cache;
                    inner
                        .lru
                        .retain(|(p, t)| cache.get(p).is_some_and(|e| e.tick == *t));
                }
            }
            return Ok(data);
        }

        if !inner.noop_meter {
            inner.meter.charge_page_reads(1)?;
        }
        inner.stats.misses += 1;
        let arc = inner.source.read_page_shared(pgno)?;
        if arc.len() != page_size as usize {
            return Err(SqliteError::corrupt(format!(
                "PageSource returned {} bytes for page {pgno}, expected {page_size}",
                arc.len()
            )));
        }
        Self::insert_locked(inner, pgno, arc.clone(), None, u64::from(page_size))?;
        Ok(arc)
    }

    fn insert_locked(
        inner: &mut PagerInner,
        pgno: u32,
        data: Arc<[u8]>,
        btree: Option<Arc<BtreePage>>,
        page_bytes: u64,
    ) -> SqliteResult<()> {
        if inner.cache.len() >= inner.capacity {
            while let Some((old_pgno, old_tick)) = inner.lru.pop_front() {
                let stale = inner
                    .cache
                    .get(&old_pgno)
                    .is_some_and(|e| e.tick == old_tick);
                if stale {
                    inner.cache.remove(&old_pgno);
                    if !inner.noop_meter {
                        inner.meter.release_memory(page_bytes);
                    }
                    inner.tracked_bytes = inner.tracked_bytes.saturating_sub(page_bytes);
                    break;
                }
            }
            // Fallback if queue was out of sync.
            if inner.cache.len() >= inner.capacity {
                if let Some(&k) = inner.cache.keys().next() {
                    inner.cache.remove(&k);
                    if !inner.noop_meter {
                        inner.meter.release_memory(page_bytes);
                    }
                    inner.tracked_bytes = inner.tracked_bytes.saturating_sub(page_bytes);
                }
            }
        }
        if !inner.noop_meter {
            inner.meter.reserve_memory(page_bytes)?;
            inner.tracked_bytes = inner.tracked_bytes.saturating_add(page_bytes);
        }
        inner.tick += 1;
        let tick = inner.tick;
        inner.cache.insert(
            pgno,
            CacheEntry { data, btree, tick },
        );
        inner.lru.push_back((pgno, tick));
        Ok(())
    }
}
