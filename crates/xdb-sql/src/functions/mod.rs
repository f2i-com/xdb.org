//! SQLite 3.54.1 built-in function library and extensible registry.

pub mod aggregates;
pub mod core;
pub mod date;
pub mod json;
pub mod like;
pub mod math;
pub mod printf;
pub mod window;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use xdb_core::value::Collation;
use xdb_core::{Meter, SqlValue};

use crate::errors::{SqlError, SqlResult};
use crate::utf8::{DEFAULT_MAX_LENGTH, DEFAULT_MAX_LIKE_PATTERN_LENGTH};

pub use aggregates::{is_builtin_aggregate, make_builtin_aggregate, AggregateAcc};
pub use json::{call_builtin_tvf, eval_json_arrow, TableValuedResult};
pub use like::{like_invoke, CompareInfo, GLOB_INFO, LIKE_CASE_SENSITIVE_INFO, LIKE_INFO};
pub use window::{
    compute_frame_rows, eval_window_only_partition, is_builtin_window_only, PartitionPeers,
    ResolvedFrame,
};

/// Runtime safety limits checked by functions (`SQLITE_LIMIT_LENGTH`, etc.).
#[derive(Debug, Clone, Copy)]
pub struct FunctionLimits {
    pub max_length: usize,
    pub max_like_pattern_length: usize,
}

impl Default for FunctionLimits {
    fn default() -> Self {
        Self {
            max_length: DEFAULT_MAX_LENGTH,
            max_like_pattern_length: DEFAULT_MAX_LIKE_PATTERN_LENGTH,
        }
    }
}

/// Execution context passed into scalar, aggregate, and table-valued functions.
pub struct FunctionContext<'a> {
    /// Active collation for collation-sensitive functions (`min`, `max`, `nullif`).
    pub collation: Option<&'a Collation>,
    /// Length & pattern limits.
    pub limits: FunctionLimits,
    /// Optional sandbox resource meter.
    pub meter: Option<&'a dyn Meter>,
    /// Statement-scoped UTC timestamp in Julian-day milliseconds (default: 2026-01-01 or system UTC).
    pub now_julian_ms: i64,
    /// Deterministic local timezone offset in minutes for `'localtime'`/`'utc'` modifiers (default 0).
    pub local_tz_offset_minutes: i32,
    /// Connection counters for `changes()`, `total_changes()`, and `last_insert_rowid()`.
    pub changes: i64,
    pub total_changes: i64,
    pub last_insert_rowid: i64,
    /// When `Some("a generated column")` etc., non-deterministic calls (`random`, `'now'`) error.
    pub pure_context: Option<&'a str>,
    /// Per-argument JSON subtype bitmask (for `json_array`, `json_object`, `json_set`, etc.).
    pub arg_json_mask: u64,
    /// Shared PRNG state for `random()` / `randomblob()` (survives `with_collation_and_json_mask`).
    rng_state: Arc<AtomicU64>,
}

static RNG_SEED_COUNTER: AtomicU64 = AtomicU64::new(1);

impl<'a> fmt::Debug for FunctionContext<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FunctionContext")
            .field("limits", &self.limits)
            .field("now_julian_ms", &self.now_julian_ms)
            .field("pure_context", &self.pure_context)
            .finish()
    }
}

impl<'a> FunctionContext<'a> {
    pub fn new(meter: Option<&'a dyn Meter>, now_julian_ms: i64) -> Self {
        let nonce = RNG_SEED_COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15);
        Self {
            collation: None,
            limits: FunctionLimits::default(),
            meter,
            now_julian_ms,
            local_tz_offset_minutes: 0,
            changes: 0,
            total_changes: 0,
            last_insert_rowid: 0,
            pure_context: None,
            arg_json_mask: 0,
            rng_state: Arc::new(AtomicU64::new(nonce ^ (now_julian_ms as u64))),
        }
    }

    #[inline]
    pub fn charge_steps(&self, n: u64) -> SqlResult<()> {
        if let Some(m) = self.meter {
            m.charge_steps(n)?;
        }
        Ok(())
    }

    #[inline]
    pub fn require_pure(&self, fn_name: &str) -> SqlResult<()> {
        if let Some(where_ctx) = self.pure_context {
            return Err(SqlError::runtime(format!(
                "non-deterministic use of {fn_name}() in {where_ctx}"
            )));
        }
        Ok(())
    }

    #[inline]
    pub fn arg_is_json(&self, idx: usize) -> bool {
        idx < 64 && ((self.arg_json_mask >> idx) & 1) != 0
    }

    /// SplitMix64 PRNG for `random()` / `randomblob()` (WASM-safe, zero OS deps).
    pub fn fill_random(&self, out: &mut [u8]) {
        let mut i = 0;
        while i < out.len() {
            let s = self
                .rng_state
                .fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
                .wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            let bytes = z.to_le_bytes();
            let take = (out.len() - i).min(8);
            out[i..i + take].copy_from_slice(&bytes[..take]);
            i += take;
        }
    }

    pub fn with_collation_and_json_mask(
        &self,
        collation: Option<&'a Collation>,
        arg_json_mask: u64,
    ) -> FunctionContext<'a> {
        FunctionContext {
            collation,
            limits: self.limits,
            meter: self.meter,
            now_julian_ms: self.now_julian_ms,
            local_tz_offset_minutes: self.local_tz_offset_minutes,
            changes: self.changes,
            total_changes: self.total_changes,
            last_insert_rowid: self.last_insert_rowid,
            pure_context: self.pure_context,
            arg_json_mask,
            rng_state: Arc::clone(&self.rng_state),
        }
    }
}

/// User-defined scalar SQL function callback.
pub type CustomScalarFn = Arc<dyn Fn(&[SqlValue]) -> SqlResult<SqlValue> + Send + Sync>;

/// Registry of custom scalar functions layered on top of SQLite 3.54.1 built-ins.
#[derive(Clone, Default)]
pub struct FunctionRegistry {
    scalars: HashMap<(String, Option<usize>), CustomScalarFn>,
}

impl fmt::Debug for FunctionRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FunctionRegistry")
            .field("custom_scalars", &self.scalars.len())
            .finish()
    }
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a custom scalar function (`arity = None` accepts any number of args).
    pub fn register_scalar(
        &mut self,
        name: impl Into<String>,
        arity: Option<usize>,
        func: CustomScalarFn,
    ) {
        self.scalars.insert((name.into().to_ascii_lowercase(), arity), func);
    }

    /// Returns true if a custom scalar function is registered for `(name, arity)`.
    pub fn has_custom_scalar(&self, name: &str, arity: usize) -> bool {
        let lower = name.to_ascii_lowercase();
        self.scalars.contains_key(&(lower.clone(), Some(arity)))
            || self.scalars.contains_key(&(lower, None))
    }

    /// Invoke a scalar function (custom overrides built-in).
    /// Returns `(value, produces_json_subtype)`.
    pub fn call_scalar(
        &self,
        name: &str,
        args: &[SqlValue],
        ctx: &FunctionContext<'_>,
    ) -> SqlResult<(SqlValue, bool)> {
        let lower = name.to_ascii_lowercase();
        if let Some(f) = self
            .scalars
            .get(&(lower.clone(), Some(args.len())))
            .or_else(|| self.scalars.get(&(lower.clone(), None)))
        {
            return Ok((f(args)?, false));
        }
        if let Some(val) = core::call_core_scalar(&lower, args, ctx)? {
            return Ok((val, false));
        }
        if let Some(val) = math::call_math(&lower, args)? {
            return Ok((val, false));
        }
        if let Some(val) = date::call_date(&lower, args, ctx)? {
            return Ok((val, false));
        }
        if let Some(res) = json::call_json_scalar(&lower, args, ctx)? {
            return Ok(res);
        }
        Err(SqlError::runtime(format!("no such function: {name}")))
    }

    /// Returns true if `(name, arity, star)` resolves to an aggregate function.
    pub fn is_aggregate(&self, name: &str, arity: usize, star: bool) -> bool {
        let lower = name.to_ascii_lowercase();
        if !star
            && (self.scalars.contains_key(&(lower.clone(), Some(arity)))
                || self.scalars.contains_key(&(lower.clone(), None)))
        {
            return false;
        }
        aggregates::is_builtin_aggregate(&lower, arity, star)
    }

    /// Create an aggregate accumulator for `(name, arity, star)`.
    pub fn create_aggregate(
        &self,
        name: &str,
        arity: usize,
        star: bool,
    ) -> SqlResult<Box<dyn AggregateAcc>> {
        let lower = name.to_ascii_lowercase();
        aggregates::make_builtin_aggregate(&lower, arity, star)?
            .ok_or_else(|| SqlError::runtime(format!("no such aggregate function: {name}")))
    }
}

/// Default Julian-day milliseconds (`2026-01-01 00:00:00.000 UTC` on WASM or current system time on native).
pub fn current_julian_ms() -> i64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Ok(dur) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            return 210_866_760_000_000i64.saturating_add(dur.as_millis() as i64);
        }
    }
    // Deterministic fallback for wasm32-unknown-unknown (2026-01-01 00:00:00 UTC)
    210_866_760_000_000i64 + 1_767_225_600_000i64
}
