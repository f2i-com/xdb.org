//! Deterministic resource metering for sandboxed execution (such as ZIPP).
//!
//! Every page read, B-tree traversal step, record decode, and query operator
//! charges a [`Meter`]. Trusted callers use [`NoopMeter`] (zero cost); sandboxed
//! hosts use [`BudgetMeter`] or wire their own VM meter directly into this trait.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Which sandbox ceiling was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitKind {
    /// Deterministic work-unit budget (instruction / page / cell / row steps).
    Steps,
    /// Tracked heap memory budget in bytes.
    Memory,
    /// Maximum number of database pages read.
    PageReads,
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LimitKind::Steps => f.write_str("step budget exceeded"),
            LimitKind::Memory => f.write_str("memory budget exceeded"),
            LimitKind::PageReads => f.write_str("page read limit exceeded"),
        }
    }
}

/// Error returned when a sandbox resource limit is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimitError {
    pub kind: LimitKind,
    pub used: u64,
    pub limit: u64,
}

impl fmt::Display for ResourceLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "resource limit exceeded ({}: used {}, limit {})",
            self.kind, self.used, self.limit
        )
    }
}

impl std::error::Error for ResourceLimitError {}

/// Ceilings enforced by [`BudgetMeter`]. `None` leaves that dimension unbounded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    pub max_steps: Option<u64>,
    pub max_memory_bytes: Option<u64>,
    pub max_page_reads: Option<u64>,
}

impl ResourceLimits {
    /// No limits enforced.
    pub const UNLIMITED: Self = Self {
        max_steps: None,
        max_memory_bytes: None,
        max_page_reads: None,
    };

    /// Conservative default limits suitable for untrusted sandboxed workloads.
    pub const SANDBOX_DEFAULT: Self = Self {
        max_steps: Some(50_000_000),
        max_memory_bytes: Some(32 * 1024 * 1024),
        max_page_reads: Some(65_536),
    };
}

/// Resource accounting hook shared by the storage and execution layers.
///
/// Implementations must be safe to share across internal handles (`Send + Sync`)
/// and should fail closed once a terminal limit is tripped.
pub trait Meter: fmt::Debug + Send + Sync {
    /// Charge `cost` deterministic work units.
    fn charge_steps(&self, cost: u64) -> Result<(), ResourceLimitError>;

    /// Charge `count` page reads against the page-read limit (and optionally steps).
    fn charge_page_reads(&self, count: u64) -> Result<(), ResourceLimitError> {
        self.charge_steps(count.saturating_mul(16))
    }

    /// Reserve `bytes` of tracked heap memory.
    fn reserve_memory(&self, bytes: u64) -> Result<(), ResourceLimitError>;

    /// Release `bytes` previously passed to [`Meter::reserve_memory`].
    fn release_memory(&self, bytes: u64);
}

/// Zero-overhead meter that imposes no limits.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopMeter;

impl Meter for NoopMeter {
    #[inline]
    fn charge_steps(&self, _cost: u64) -> Result<(), ResourceLimitError> {
        Ok(())
    }

    #[inline]
    fn charge_page_reads(&self, _count: u64) -> Result<(), ResourceLimitError> {
        Ok(())
    }

    #[inline]
    fn reserve_memory(&self, _bytes: u64) -> Result<(), ResourceLimitError> {
        Ok(())
    }

    #[inline]
    fn release_memory(&self, _bytes: u64) {}
}

/// Snapshot of current [`BudgetMeter`] usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterUsage {
    pub steps: u64,
    pub memory_bytes: u64,
    pub peak_memory_bytes: u64,
    pub page_reads: u64,
    pub exhausted: bool,
}

/// Atomic, sticky-exhaustion resource meter for sandboxed databases.
///
/// Once any ceiling is exceeded, `exhausted` becomes sticky so subsequent calls
/// cannot sneak past a tripped limit (matching ZIPP's `safe-sandbox` semantics).
#[derive(Debug)]
pub struct BudgetMeter {
    limits: ResourceLimits,
    steps: AtomicU64,
    memory_bytes: AtomicU64,
    peak_memory_bytes: AtomicU64,
    page_reads: AtomicU64,
    exhausted: AtomicBool,
}

impl BudgetMeter {
    pub fn new(limits: ResourceLimits) -> Self {
        Self {
            limits,
            steps: AtomicU64::new(0),
            memory_bytes: AtomicU64::new(0),
            peak_memory_bytes: AtomicU64::new(0),
            page_reads: AtomicU64::new(0),
            exhausted: AtomicBool::new(false),
        }
    }

    pub fn limits(&self) -> ResourceLimits {
        self.limits
    }

    pub fn usage(&self) -> MeterUsage {
        MeterUsage {
            steps: self.steps.load(Ordering::Relaxed),
            memory_bytes: self.memory_bytes.load(Ordering::Relaxed),
            peak_memory_bytes: self.peak_memory_bytes.load(Ordering::Relaxed),
            page_reads: self.page_reads.load(Ordering::Relaxed),
            exhausted: self.exhausted.load(Ordering::Relaxed),
        }
    }

    fn check_not_exhausted(&self, kind: LimitKind) -> Result<(), ResourceLimitError> {
        if self.exhausted.load(Ordering::Relaxed) {
            let (used, limit) = match kind {
                LimitKind::Steps => (
                    self.steps.load(Ordering::Relaxed),
                    self.limits.max_steps.unwrap_or(0),
                ),
                LimitKind::Memory => (
                    self.memory_bytes.load(Ordering::Relaxed),
                    self.limits.max_memory_bytes.unwrap_or(0),
                ),
                LimitKind::PageReads => (
                    self.page_reads.load(Ordering::Relaxed),
                    self.limits.max_page_reads.unwrap_or(0),
                ),
            };
            return Err(ResourceLimitError { kind, used, limit });
        }
        Ok(())
    }
}

impl Meter for BudgetMeter {
    fn charge_steps(&self, cost: u64) -> Result<(), ResourceLimitError> {
        self.check_not_exhausted(LimitKind::Steps)?;
        let prev = self.steps.fetch_add(cost, Ordering::Relaxed);
        let used = prev.saturating_add(cost);
        if let Some(limit) = self.limits.max_steps {
            if used > limit {
                self.exhausted.store(true, Ordering::Relaxed);
                return Err(ResourceLimitError {
                    kind: LimitKind::Steps,
                    used,
                    limit,
                });
            }
        }
        Ok(())
    }

    fn charge_page_reads(&self, count: u64) -> Result<(), ResourceLimitError> {
        self.check_not_exhausted(LimitKind::PageReads)?;
        let prev = self.page_reads.fetch_add(count, Ordering::Relaxed);
        let used = prev.saturating_add(count);
        if let Some(limit) = self.limits.max_page_reads {
            if used > limit {
                self.exhausted.store(true, Ordering::Relaxed);
                return Err(ResourceLimitError {
                    kind: LimitKind::PageReads,
                    used,
                    limit,
                });
            }
        }
        self.charge_steps(count.saturating_mul(16))
    }

    fn reserve_memory(&self, bytes: u64) -> Result<(), ResourceLimitError> {
        self.check_not_exhausted(LimitKind::Memory)?;
        let prev = self.memory_bytes.fetch_add(bytes, Ordering::Relaxed);
        let used = prev.saturating_add(bytes);
        if let Some(limit) = self.limits.max_memory_bytes {
            if used > limit {
                self.memory_bytes.fetch_sub(bytes, Ordering::Relaxed);
                self.exhausted.store(true, Ordering::Relaxed);
                return Err(ResourceLimitError {
                    kind: LimitKind::Memory,
                    used,
                    limit,
                });
            }
        }
        let mut peak = self.peak_memory_bytes.load(Ordering::Relaxed);
        while used > peak {
            match self.peak_memory_bytes.compare_exchange_weak(
                peak,
                used,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(cur) => peak = cur,
            }
        }
        Ok(())
    }

    fn release_memory(&self, bytes: u64) {
        let mut cur = self.memory_bytes.load(Ordering::Relaxed);
        loop {
            let next = cur.saturating_sub(bytes);
            match self.memory_bytes.compare_exchange_weak(
                cur,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_meter_enforces_steps_and_stays_sticky() {
        let m = BudgetMeter::new(ResourceLimits {
            max_steps: Some(100),
            max_memory_bytes: Some(1024),
            max_page_reads: Some(5),
        });
        assert!(m.charge_steps(60).is_ok());
        assert!(m.charge_steps(40).is_ok());
        let err = m.charge_steps(1).unwrap_err();
        assert_eq!(err.kind, LimitKind::Steps);
        assert!(m.usage().exhausted);
        // Sticky: even zero-cost or memory reservation fails after exhaustion.
        assert!(m.reserve_memory(1).is_err());
    }

    #[test]
    fn budget_meter_tracks_memory_and_peak() {
        let m = BudgetMeter::new(ResourceLimits {
            max_steps: None,
            max_memory_bytes: Some(100),
            max_page_reads: None,
        });
        m.reserve_memory(60).unwrap();
        m.reserve_memory(40).unwrap();
        m.release_memory(70);
        m.reserve_memory(50).unwrap();
        let u = m.usage();
        assert_eq!(u.memory_bytes, 80);
        assert_eq!(u.peak_memory_bytes, 100);
        assert!(m.reserve_memory(21).is_err());
    }
}
