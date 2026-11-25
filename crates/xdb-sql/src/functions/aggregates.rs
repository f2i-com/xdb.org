//! SQLite 3.54.1 aggregate functions (`count`, `sum`, `total`, `avg`, `min`, `max`,
//! `group_concat`/`string_agg`, `percentile`/`percentile_cont`/`percentile_disc`/`median`,
//! `json_group_array`, `json_group_object`), with `xInverse` for sliding window frames.

use std::cmp::Ordering;

use xdb_core::value::{
    apply_numeric_affinity, mem_compare, real_value, Collation, SqlValue,
};

use crate::errors::{SqlError, SqlResult};
use crate::functions::FunctionContext;
use crate::utf8::{text_bytes_of, value_byte_len};

/// Mutable state for one aggregate evaluation (group or window frame).
pub trait AggregateAcc: std::fmt::Debug {
    /// Process one surviving row's argument values.
    fn step(&mut self, args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<()>;

    /// Whether this aggregate supports O(1) sliding window `inverse()`.
    fn supports_inverse(&self) -> bool {
        false
    }

    /// Remove one row leaving a sliding window frame (only called when `supports_inverse()` is true).
    fn inverse(&mut self, _args: &[SqlValue], _ctx: &FunctionContext<'_>) -> SqlResult<()> {
        Err(SqlError::runtime("aggregate does not support inverse()"))
    }

    /// Current aggregate value without destroying state (for window frames).
    fn value(&self, ctx: &FunctionContext<'_>) -> SqlResult<SqlValue>;

    /// Finalize aggregate value for a group.
    fn finalize(self: Box<Self>, ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
        self.value(ctx)
    }

    /// SQLite `sqlite3SkipAccumulatorLoad` hint for single `min(x)`/`max(x)` queries:
    /// true if the most recent `step()` did NOT establish a new minimum/maximum.
    fn skip_bare_column_load(&self) -> bool {
        false
    }

    /// Estimated live heap bytes held by this accumulator for cumulative sandbox memory tracking.
    fn heap_bytes(&self) -> u64 {
        0
    }
}

/// Returns true if `lower_name` is a built-in aggregate function (for a given arity/star).
pub fn is_builtin_aggregate(lower_name: &str, arity: usize, star: bool) -> bool {
    match lower_name {
        "count" => star || arity <= 1,
        "sum" | "total" | "avg" | "median" | "json_group_array" => !star && arity == 1,
        "min" | "max" => !star && arity == 1,
        "group_concat" => !star && (arity == 1 || arity == 2),
        "string_agg" | "json_group_object" => !star && arity == 2,
        "percentile" | "percentile_cont" | "percentile_disc" => !star && arity == 2,
        _ => false,
    }
}

/// Create a fresh built-in aggregate accumulator.
pub fn make_builtin_aggregate(
    lower_name: &str,
    arity: usize,
    star: bool,
) -> SqlResult<Option<Box<dyn AggregateAcc>>> {
    let acc: Box<dyn AggregateAcc> = match lower_name {
        "count" => {
            if !star && arity > 1 {
                return Err(SqlError::runtime(
                    "wrong number of arguments to function count()",
                ));
            }
            Box::new(CountAcc {
                count: 0,
                count_all: star || arity == 0,
            })
        }
        "sum" => {
            check_arity(lower_name, arity, star, 1, 1)?;
            Box::new(SumAcc::new(SumKind::Sum))
        }
        "total" => {
            check_arity(lower_name, arity, star, 1, 1)?;
            Box::new(SumAcc::new(SumKind::Total))
        }
        "avg" => {
            check_arity(lower_name, arity, star, 1, 1)?;
            Box::new(SumAcc::new(SumKind::Avg))
        }
        "min" if !star && arity == 1 => Box::new(MinMaxAcc::new(false)),
        "max" if !star && arity == 1 => Box::new(MinMaxAcc::new(true)),
        "group_concat" => {
            check_arity(lower_name, arity, star, 1, 2)?;
            Box::new(GroupConcatAcc::new())
        }
        "string_agg" => {
            check_arity(lower_name, arity, star, 2, 2)?;
            Box::new(GroupConcatAcc::new())
        }
        "percentile" => {
            check_arity(lower_name, arity, star, 2, 2)?;
            Box::new(PercentileAcc::new(PercentileKind::Percentile))
        }
        "percentile_cont" => {
            check_arity(lower_name, arity, star, 2, 2)?;
            Box::new(PercentileAcc::new(PercentileKind::Cont))
        }
        "percentile_disc" => {
            check_arity(lower_name, arity, star, 2, 2)?;
            Box::new(PercentileAcc::new(PercentileKind::Disc))
        }
        "median" => {
            check_arity(lower_name, arity, star, 1, 1)?;
            Box::new(PercentileAcc::new(PercentileKind::Median))
        }
        "json_group_array" => {
            check_arity(lower_name, arity, star, 1, 1)?;
            Box::new(JsonGroupArrayAcc::new())
        }
        "json_group_object" => {
            check_arity(lower_name, arity, star, 2, 2)?;
            Box::new(JsonGroupObjectAcc::new())
        }
        _ => return Ok(None),
    };
    Ok(Some(acc))
}

fn check_arity(name: &str, arity: usize, star: bool, min: usize, max: usize) -> SqlResult<()> {
    if star || arity < min || arity > max {
        return Err(SqlError::runtime(format!(
            "wrong number of arguments to function {name}()"
        )));
    }
    Ok(())
}

// ───────────────────────────── count ─────────────────────────────

#[derive(Debug)]
struct CountAcc {
    count: i64,
    count_all: bool,
}

impl AggregateAcc for CountAcc {
    fn step(&mut self, args: &[SqlValue], _ctx: &FunctionContext<'_>) -> SqlResult<()> {
        if self.count_all || args.first().is_some_and(|v| !v.is_null()) {
            self.count += 1;
        }
        Ok(())
    }

    fn supports_inverse(&self) -> bool {
        true
    }

    fn inverse(&mut self, args: &[SqlValue], _ctx: &FunctionContext<'_>) -> SqlResult<()> {
        if self.count_all || args.first().is_some_and(|v| !v.is_null()) {
            self.count -= 1;
        }
        Ok(())
    }

    fn value(&self, _ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
        Ok(SqlValue::Int(self.count))
    }
}

// ───────────────────────────── sum / total / avg ─────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) enum SumKind {
    Sum,
    Total,
    Avg,
}

#[derive(Debug, Clone)]
pub(crate) struct SumAcc {
    kind: SumKind,
    r_sum: f64,
    r_err: f64,
    i_sum_64: i64,
    i_sum_hi: i64,
    cnt: i64,
    real_cnt: i64,
    ovrfl: bool,
    approx: bool,
}

impl SumAcc {
    pub(crate) fn new(kind: SumKind) -> Self {
        Self {
            kind,
            r_sum: 0.0,
            r_err: 0.0,
            i_sum_64: 0,
            i_sum_hi: 0,
            cnt: 0,
            real_cnt: 0,
            ovrfl: false,
            approx: false,
        }
    }

    #[inline]
    fn exact_i128(&self) -> i128 {
        (i128::from(self.i_sum_hi) << 64) + i128::from(self.i_sum_64)
    }

    #[cold]
    #[inline(never)]
    fn record_int_overflow(&mut self, iv: i64) {
        if !self.approx {
            self.ovrfl = true;
        }
        if iv >= 0 {
            self.i_sum_hi = self.i_sum_hi.wrapping_add(1);
        } else {
            self.i_sum_hi = self.i_sum_hi.wrapping_sub(1);
        }
    }

    #[inline]
    pub(crate) fn step_value(&mut self, v: &SqlValue) {
        match v {
            SqlValue::Null => {}
            SqlValue::Int(iv) => {
                self.cnt += 1;
                let (sum, ov) = self.i_sum_64.overflowing_add(*iv);
                self.i_sum_64 = sum;
                if ov {
                    self.record_int_overflow(*iv);
                }
            }
            SqlValue::Real(r) => {
                if r.is_nan() {
                    return;
                }
                self.cnt += 1;
                self.real_cnt += 1;
                self.approx = true;
                self.kbn_step(*r);
            }
            SqlValue::Text(b) => {
                // SQLite `sumStep` (`func.c:1954`): `sqlite3_value_numeric_type` converts only
                // well-formed numeric TEXT; any non-integer result sets sticky `approx = true`.
                if let Some(SqlValue::Int(iv)) = apply_numeric_affinity(b, false) {
                    self.cnt += 1;
                    let (sum, ov) = self.i_sum_64.overflowing_add(iv);
                    self.i_sum_64 = sum;
                    if ov {
                        self.record_int_overflow(iv);
                    }
                } else {
                    self.cnt += 1;
                    self.real_cnt += 1;
                    self.approx = true;
                    self.kbn_step(real_value(v));
                }
            }
            SqlValue::Blob(_) => {
                // `sqlite3_value_numeric_type` leaves BLOB as BLOB -> non-integer real path.
                self.cnt += 1;
                self.real_cnt += 1;
                self.approx = true;
                self.kbn_step(real_value(v));
            }
        }
    }

    #[inline]
    pub(crate) fn inverse_value(&mut self, v: &SqlValue) {
        match v {
            SqlValue::Null => {}
            SqlValue::Int(iv) => {
                self.cnt -= 1;
                let (sum, ov) = self.i_sum_64.overflowing_sub(*iv);
                self.i_sum_64 = sum;
                if ov {
                    if *iv >= 0 {
                        self.i_sum_hi = self.i_sum_hi.wrapping_sub(1);
                    } else {
                        self.i_sum_hi = self.i_sum_hi.wrapping_add(1);
                    }
                }
            }
            SqlValue::Real(r) => {
                if r.is_nan() {
                    return;
                }
                self.cnt -= 1;
                self.real_cnt -= 1;
                self.kbn_step(-*r);
                if self.real_cnt == 0 {
                    self.r_sum = 0.0;
                    self.r_err = 0.0;
                }
            }
            SqlValue::Text(b) => {
                if let Some(SqlValue::Int(iv)) = apply_numeric_affinity(b, false) {
                    self.cnt -= 1;
                    let (sum, ov) = self.i_sum_64.overflowing_sub(iv);
                    self.i_sum_64 = sum;
                    if ov {
                        if iv >= 0 {
                            self.i_sum_hi = self.i_sum_hi.wrapping_sub(1);
                        } else {
                            self.i_sum_hi = self.i_sum_hi.wrapping_add(1);
                        }
                    }
                } else {
                    self.cnt -= 1;
                    self.real_cnt -= 1;
                    self.kbn_step(-real_value(v));
                    if self.real_cnt == 0 {
                        self.r_sum = 0.0;
                        self.r_err = 0.0;
                    }
                }
            }
            SqlValue::Blob(_) => {
                self.cnt -= 1;
                self.real_cnt -= 1;
                self.kbn_step(-real_value(v));
                if self.real_cnt == 0 {
                    self.r_sum = 0.0;
                    self.r_err = 0.0;
                }
            }
        }
    }

    fn kbn_step(&mut self, r: f64) {
        let s = self.r_sum;
        let t = s + r;
        if s.abs() > r.abs() {
            self.r_err += (s - t) + r;
        } else {
            self.r_err += (r - t) + s;
        }
        self.r_sum = t;
    }

    #[inline]
    fn kbn_add(s: &mut f64, err: &mut f64, r: f64) {
        let old = *s;
        let t = old + r;
        if old.abs() > r.abs() {
            *err += (old - t) + r;
        } else {
            *err += (r - t) + old;
        }
        *s = t;
    }

    fn kbn_total(&self) -> f64 {
        let mut s = self.r_sum;
        let mut err = self.r_err;
        if self.i_sum_hi == 0 {
            if self.i_sum_64 != 0 {
                // Split into two bit-exact f64 parts (|hi| multiple of 2^14 -> <= 49 sig bits, |lo| < 2^14).
                let hi = (self.i_sum_64 / 16384) * 16384;
                let lo = self.i_sum_64 - hi;
                if hi != 0 {
                    Self::kbn_add(&mut s, &mut err, hi as f64);
                }
                if lo != 0 {
                    Self::kbn_add(&mut s, &mut err, lo as f64);
                }
            }
        } else {
            // Split exact 128-bit integer sum into three <=44-bit power-of-2-scaled chunks
            // so every chunk converts to f64 without rounding before KBN compensation!
            const C1: i128 = 1i128 << 44;
            const C2: i128 = 1i128 << 88;
            let full = self.exact_i128();
            let top = full / C2;
            let rem1 = full - top * C2;
            let mid = rem1 / C1;
            let lo = rem1 - mid * C1;
            if top != 0 {
                Self::kbn_add(&mut s, &mut err, (top as f64) * (C2 as f64));
            }
            if mid != 0 {
                Self::kbn_add(&mut s, &mut err, (mid as f64) * (C1 as f64));
            }
            if lo != 0 {
                Self::kbn_add(&mut s, &mut err, lo as f64);
            }
        }
        if err.is_finite() {
            s + err
        } else {
            s
        }
    }
    #[inline]
    pub(crate) fn value_as(&self, kind: SumKind) -> SqlResult<SqlValue> {
        match kind {
            SumKind::Sum => {
                if self.cnt <= 0 {
                    Ok(SqlValue::Null)
                } else if self.approx || self.real_cnt > 0 {
                    Ok(SqlValue::real(self.kbn_total()))
                } else if self.ovrfl || self.i_sum_hi != 0 {
                    Err(SqlError::runtime("integer overflow"))
                } else {
                    Ok(SqlValue::Int(self.i_sum_64))
                }
            }
            SumKind::Total => {
                let r = if self.approx || self.real_cnt > 0 {
                    self.kbn_total()
                } else if self.i_sum_hi == 0 {
                    self.i_sum_64 as f64
                } else {
                    self.exact_i128() as f64
                };
                Ok(SqlValue::real(r))
            }
            SumKind::Avg => {
                if self.cnt <= 0 {
                    Ok(SqlValue::Null)
                } else {
                    let r = if self.approx || self.real_cnt > 0 {
                        self.kbn_total()
                    } else if self.i_sum_hi == 0 {
                        self.i_sum_64 as f64
                    } else {
                        self.exact_i128() as f64
                    };
                    Ok(SqlValue::real(r / (self.cnt as f64)))
                }
            }
        }
    }
}

impl AggregateAcc for SumAcc {
    fn step(&mut self, args: &[SqlValue], _ctx: &FunctionContext<'_>) -> SqlResult<()> {
        self.step_value(&args[0]);
        Ok(())
    }

    fn supports_inverse(&self) -> bool {
        true
    }

    fn inverse(&mut self, args: &[SqlValue], _ctx: &FunctionContext<'_>) -> SqlResult<()> {
        self.inverse_value(&args[0]);
        Ok(())
    }

    fn value(&self, _ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
        self.value_as(self.kind)
    }
}

// ───────────────────────────── min / max ─────────────────────────────

#[derive(Debug)]
struct MinMaxAcc {
    is_max: bool,
    best: Option<SqlValue>,
    skip_load: bool,
}

impl MinMaxAcc {
    fn new(is_max: bool) -> Self {
        Self {
            is_max,
            best: None,
            skip_load: false,
        }
    }
}

impl AggregateAcc for MinMaxAcc {
    fn step(&mut self, args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<()> {
        let v = &args[0];
        if v.is_null() {
            self.skip_load = self.best.is_some();
        } else if let Some(ref current) = self.best {
            let cmp = mem_compare(current, v, ctx.collation);
            if (self.is_max && cmp == Ordering::Less) || (!self.is_max && cmp == Ordering::Greater)
            {
                self.best = Some(v.clone());
                self.skip_load = false;
            } else {
                self.skip_load = true;
            }
        } else {
            self.best = Some(v.clone());
            self.skip_load = false;
        }
        Ok(())
    }

    fn value(&self, _ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
        Ok(self.best.clone().unwrap_or(SqlValue::Null))
    }

    fn skip_bare_column_load(&self) -> bool {
        self.skip_load
    }
}

// ───────────────────────────── group_concat / string_agg ─────────────────────────────

#[derive(Debug)]
struct GroupConcatAcc {
    allocated: bool,
    too_big: bool,
    buf: Vec<u8>,
    head: usize,
    /// Byte lengths of each appended `(sep_len, val_len)` pair to support amortized O(1) `inverse()`.
    items: std::collections::VecDeque<(usize, usize)>,
}

impl GroupConcatAcc {
    fn new() -> Self {
        Self {
            allocated: false,
            too_big: false,
            buf: Vec::new(),
            head: 0,
            items: std::collections::VecDeque::new(),
        }
    }

    #[inline]
    fn active_len(&self) -> usize {
        self.buf.len().saturating_sub(self.head)
    }
}

impl AggregateAcc for GroupConcatAcc {
    fn step(&mut self, args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<()> {
        let Some(val_bytes) = text_bytes_of(&args[0]) else {
            return Ok(());
        };
        let limit = ctx.limits.max_length;
        self.allocated = true;
        if self.too_big {
            return Ok(());
        }
        let first = self.items.is_empty();
        let sep_bytes: Option<std::borrow::Cow<'_, [u8]>> = if first {
            None
        } else if args.len() == 1 {
            Some(std::borrow::Cow::Borrowed(b","))
        } else {
            text_bytes_of(&args[1])
        };
        let sep_len = sep_bytes.as_ref().map_or(0, |s| s.len());
        let add_len = sep_len.saturating_add(val_bytes.len());
        if self.active_len().saturating_add(add_len).saturating_add(1) > limit {
            self.too_big = true;
            self.buf.clear();
            self.head = 0;
            return Ok(());
        }
        if let Some(m) = ctx.meter {
            let total = (self.active_len() + add_len + self.items.len() * 16) as u64;
            m.reserve_memory(total)?;
            m.release_memory(total);
            if add_len >= 256 {
                m.charge_steps((add_len / 256) as u64)?;
            }
        }
        if let Some(ref sep) = sep_bytes {
            self.buf.extend_from_slice(sep);
        }
        self.buf.extend_from_slice(&val_bytes);
        self.items.push_back((sep_len, val_bytes.len()));
        Ok(())
    }

    fn supports_inverse(&self) -> bool {
        true
    }

    fn inverse(&mut self, args: &[SqlValue], _ctx: &FunctionContext<'_>) -> SqlResult<()> {
        if args[0].is_null() || self.too_big {
            return Ok(());
        }
        let _ = value_byte_len(&args[0]);
        if let Some((_, val_len)) = self.items.pop_front() {
            let next_sep = self.items.front_mut().map_or(0, |(s, _)| {
                let prev = *s;
                *s = 0;
                prev
            });
            let remove = (val_len + next_sep).min(self.active_len());
            self.head += remove;
            if self.items.is_empty() {
                self.buf.clear();
                self.head = 0;
                self.allocated = false;
            } else if self.head >= 4096 && self.head * 2 >= self.buf.len() {
                self.buf.drain(..self.head);
                self.head = 0;
            }
        }
        Ok(())
    }

    fn value(&self, _ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
        if self.too_big {
            return Err(SqlError::too_big());
        }
        if !self.allocated {
            return Ok(SqlValue::Null);
        }
        Ok(SqlValue::Text(self.buf[self.head..].to_vec().into_boxed_slice()))
    }

    fn heap_bytes(&self) -> u64 {
        (self.buf.len() as u64).saturating_add((self.items.len() as u64).saturating_mul(16))
    }
}

// ───────────────────────────── percentile / median ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PercentileKind {
    /// `percentile(Y, P)`: P in [0, 100].
    Percentile,
    /// `percentile_cont(Y, P)`: P in [0, 1].
    Cont,
    /// `percentile_disc(Y, P)`: P in [0, 1], discrete lower value.
    Disc,
    /// `median(Y)`: P = 50%.
    Median,
}

#[derive(Debug)]
struct PercentileAcc {
    kind: PercentileKind,
    p: Option<f64>,
    values: Vec<f64>,
}

impl PercentileAcc {
    fn new(kind: PercentileKind) -> Self {
        Self {
            kind,
            p: if kind == PercentileKind::Median {
                Some(0.5)
            } else {
                None
            },
            values: Vec::new(),
        }
    }
}

impl AggregateAcc for PercentileAcc {
    fn step(&mut self, args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<()> {
        if self.kind != PercentileKind::Median {
            let p_arg = &args[1];
            let p_val = match p_arg {
                SqlValue::Int(i) => *i as f64,
                SqlValue::Real(r) if r.is_finite() => *r,
                SqlValue::Text(b) => match xdb_core::value::apply_numeric_affinity(b, false) {
                    Some(SqlValue::Int(i)) => i as f64,
                    Some(SqlValue::Real(r)) if r.is_finite() => r,
                    _ => {
                        return Err(SqlError::runtime(
                            "2nd argument to percentile() is not a number",
                        ));
                    }
                },
                _ => {
                    return Err(SqlError::runtime(
                        "2nd argument to percentile() is not a number",
                    ));
                }
            };
            let frac = match self.kind {
                PercentileKind::Percentile => {
                    if !(0.0..=100.0).contains(&p_val) {
                        return Err(SqlError::runtime(
                            "2nd argument to percentile() is out of range [0..100]",
                        ));
                    }
                    p_val / 100.0
                }
                PercentileKind::Cont | PercentileKind::Disc => {
                    if !(0.0..=1.0).contains(&p_val) {
                        return Err(SqlError::runtime(
                            "2nd argument to percentile function is out of range [0..1]",
                        ));
                    }
                    p_val
                }
                PercentileKind::Median => unreachable!(),
            };
            if let Some(prev) = self.p {
                if (prev - frac).abs() > 1e-5 {
                    return Err(SqlError::runtime(
                        "2nd argument to percentile() is not the same for all input rows",
                    ));
                }
            } else {
                self.p = Some(frac);
            }
        }

        let fn_name = match self.kind {
            PercentileKind::Percentile => "percentile",
            PercentileKind::Median => "median",
            PercentileKind::Cont => "percentile_cont",
            PercentileKind::Disc => "percentile_disc",
        };
        let v = &args[0];
        if v.is_null() {
            return Ok(());
        }
        match v {
            SqlValue::Int(i) => self.values.push(*i as f64),
            SqlValue::Real(r) => {
                if !r.is_finite() {
                    return Err(SqlError::runtime(format!("Inf input to {fn_name}()")));
                }
                self.values.push(*r);
            }
            SqlValue::Text(_) | SqlValue::Blob(_) => {
                return Err(SqlError::runtime(format!(
                    "input to {fn_name}() is not numeric"
                )));
            }
            SqlValue::Null => {}
        }
        if let Some(m) = ctx.meter {
            let total = (self.values.len().saturating_mul(std::mem::size_of::<f64>())) as u64;
            if total > 0 {
                m.reserve_memory(total)?;
                m.release_memory(total);
            }
        }
        Ok(())
    }

    fn heap_bytes(&self) -> u64 {
        (self.values.len().saturating_mul(std::mem::size_of::<f64>())) as u64
    }

    fn value(&self, ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
        if self.values.is_empty() {
            return Ok(SqlValue::Null);
        }
        let Some(frac) = self.p else {
            return Ok(SqlValue::Null);
        };
        let n = self.values.len();
        if let Some(m) = ctx.meter {
            let total = (n.saturating_mul(std::mem::size_of::<f64>())) as u64;
            m.reserve_memory(total)?;
            m.release_memory(total);
            let sort_steps = ((n as u64) * (usize::BITS as u64 - n.leading_zeros() as u64)).max(1);
            m.charge_steps((sort_steps / 16).max(1))?;
        }
        let mut sorted = self.values.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        if self.kind == PercentileKind::Disc {
            // SQLite 3.54.1 `percentCompute` (`func.c:3118-3130`): floor((n - 1) * frac).
            let idx = ((frac * ((n - 1) as f64)) as usize).min(n - 1);
            return Ok(SqlValue::real(sorted[idx]));
        }
        let pos = frac * ((n - 1) as f64);
        let lo = pos.floor() as usize;
        let hi = pos.ceil() as usize;
        let res = if lo == hi {
            sorted[lo]
        } else {
            let w = pos - (lo as f64);
            sorted[lo] * (1.0 - w) + sorted[hi] * w
        };
        Ok(SqlValue::real(res))
    }
}

// ───────────────────────────── json_group_array / json_group_object ─────────────────────────────

#[derive(Debug)]
struct JsonGroupArrayAcc {
    items: std::collections::VecDeque<crate::functions::json::JsonVal>,
    est_bytes: u64,
}

impl JsonGroupArrayAcc {
    fn new() -> Self {
        Self {
            items: std::collections::VecDeque::new(),
            est_bytes: 0,
        }
    }
}

impl AggregateAcc for JsonGroupArrayAcc {
    fn step(&mut self, args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<()> {
        let is_json = ctx.arg_is_json(0);
        let jv = crate::functions::json::json_value_from_sql_metered(&args[0], is_json, ctx)?;
        self.est_bytes = self
            .est_bytes
            .saturating_add(crate::utf8::estimate_value_bytes(&args[0]));
        if let Some(m) = ctx.meter {
            m.reserve_memory(self.est_bytes)?;
            m.release_memory(self.est_bytes);
        }
        self.items.push_back(jv);
        Ok(())
    }

    fn supports_inverse(&self) -> bool {
        true
    }

    fn inverse(&mut self, args: &[SqlValue], _ctx: &FunctionContext<'_>) -> SqlResult<()> {
        if self.items.pop_front().is_some() {
            self.est_bytes = self
                .est_bytes
                .saturating_sub(crate::utf8::estimate_value_bytes(&args[0]));
        }
        Ok(())
    }

    fn value(&self, ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
        crate::functions::json::render_json_array_iter_metered(self.items.iter(), ctx)
    }

    fn heap_bytes(&self) -> u64 {
        self.est_bytes
    }
}

#[derive(Debug)]
struct JsonGroupObjectAcc {
    entries: std::collections::VecDeque<(String, crate::functions::json::JsonVal)>,
    est_bytes: u64,
}

impl JsonGroupObjectAcc {
    fn new() -> Self {
        Self {
            entries: std::collections::VecDeque::new(),
            est_bytes: 0,
        }
    }
}

impl AggregateAcc for JsonGroupObjectAcc {
    fn step(&mut self, args: &[SqlValue], ctx: &FunctionContext<'_>) -> SqlResult<()> {
        let Some(k_bytes) = text_bytes_of(&args[0]) else {
            return Ok(());
        };
        let key = String::from_utf8_lossy(&k_bytes).into_owned();
        let val =
            crate::functions::json::json_value_from_sql_metered(&args[1], ctx.arg_is_json(1), ctx)?;
        self.est_bytes = self
            .est_bytes
            .saturating_add(key.len() as u64 + crate::utf8::estimate_value_bytes(&args[1]) + 32);
        if let Some(m) = ctx.meter {
            m.reserve_memory(self.est_bytes)?;
            m.release_memory(self.est_bytes);
        }
        self.entries.push_back((key, val));
        Ok(())
    }

    fn supports_inverse(&self) -> bool {
        true
    }

    fn inverse(&mut self, args: &[SqlValue], _ctx: &FunctionContext<'_>) -> SqlResult<()> {
        if !args[0].is_null() {
            if let Some((k, _)) = self.entries.pop_front() {
                self.est_bytes = self
                    .est_bytes
                    .saturating_sub(k.len() as u64 + crate::utf8::estimate_value_bytes(&args[1]) + 32);
            }
        }
        Ok(())
    }

    fn value(&self, ctx: &FunctionContext<'_>) -> SqlResult<SqlValue> {
        crate::functions::json::render_json_object_iter_metered(
            self.entries.iter().map(|(k, v)| (k.as_str(), v)),
            ctx,
        )
    }

    fn heap_bytes(&self) -> u64 {
        self.est_bytes
    }
}

/// Compare two values using an optional collation (used by DISTINCT/ORDER BY inside aggregates).
pub fn compare_with_coll(a: &SqlValue, b: &SqlValue, coll: Option<&Collation>) -> Ordering {
    mem_compare(a, b, coll)
}
