//! SQLite 3.54.1 built-in window functions (`row_number`, `rank`, `dense_rank`,
//! `percent_rank`, `cume_dist`, `ntile`, `lag`, `lead`, `first_value`, `last_value`,
//! `nth_value`) and window frame boundary computation (`ROWS`, `RANGE`, `GROUPS`,
//! `EXCLUDE NO OTHERS | CURRENT ROW | GROUP | TIES`).

use std::cmp::Ordering;

use xdb_core::value::{
    add, apply_numeric_affinity, int_value, mem_compare, must_be_int, subtract, Collation, SqlValue,
};
use xdb_parser::{FrameBound, FrameBoundType, FrameExclude, FrameSpec, FrameUnits, SortOrder};

use crate::errors::{SqlError, SqlResult};

/// Returns true if `lower_name` is a built-in window-only function.
pub fn is_builtin_window_only(lower_name: &str) -> bool {
    matches!(
        lower_name,
        "row_number"
            | "rank"
            | "dense_rank"
            | "percent_rank"
            | "cume_dist"
            | "ntile"
            | "lag"
            | "lead"
            | "first_value"
            | "last_value"
            | "nth_value"
    )
}

/// Information about peer groups inside a sorted partition of length `len`.
#[derive(Debug)]
pub struct PartitionPeers {
    /// For each row index `i` in `0..len`: the peer group id `0..group_count`.
    pub group_of: Vec<usize>,
    /// For each peer group `g`: `(start_row_inclusive, end_row_exclusive)`.
    pub group_bounds: Vec<(usize, usize)>,
}

impl PartitionPeers {
    pub fn build<F>(len: usize, mut same_peer: F) -> Self
    where
        F: FnMut(usize, usize) -> bool,
    {
        if len == 0 {
            return Self {
                group_of: Vec::new(),
                group_bounds: Vec::new(),
            };
        }
        let mut group_of = vec![0usize; len];
        let mut group_bounds = Vec::new();
        let mut start = 0;
        let mut gid = 0;
        for (i, slot) in group_of.iter_mut().enumerate().take(len).skip(1) {
            if !same_peer(i - 1, i) {
                group_bounds.push((start, i));
                start = i;
                gid += 1;
            }
            *slot = gid;
        }
        group_bounds.push((start, len));
        Self {
            group_of,
            group_bounds,
        }
    }
}

/// Evaluate a built-in window-only function for every row in a partition.
///
/// `args_per_row[i]` holds the evaluated argument values for partition row `i`.
/// `frame_rows_for` returns the surviving row indices (in partition order) belonging
/// to the frame of row `i` (only needed by `first_value`, `last_value`, `nth_value`).
pub fn eval_window_only_partition<F>(
    lower_name: &str,
    args_per_row: &[Vec<SqlValue>],
    peers: &PartitionPeers,
    mut frame_rows_for: F,
) -> SqlResult<Vec<SqlValue>>
where
    F: FnMut(usize) -> SqlResult<Vec<usize>>,
{
    let n = args_per_row.len();
    let mut out = vec![SqlValue::Null; n];

    match lower_name {
        "row_number" | "rank" | "dense_rank" | "percent_rank" | "cume_dist" => {
            if let Some(first) = args_per_row.first() {
                if !first.is_empty() {
                    return Err(SqlError::runtime(format!(
                        "wrong number of arguments to function {lower_name}()"
                    )));
                }
            }
            match lower_name {
                "row_number" => {
                    for (i, slot) in out.iter_mut().enumerate() {
                        *slot = SqlValue::Int((i + 1) as i64);
                    }
                }
                "rank" => {
                    for (i, slot) in out.iter_mut().enumerate() {
                        let (g_start, _) = peers.group_bounds[peers.group_of[i]];
                        *slot = SqlValue::Int((g_start + 1) as i64);
                    }
                }
                "dense_rank" => {
                    for (i, slot) in out.iter_mut().enumerate() {
                        *slot = SqlValue::Int((peers.group_of[i] + 1) as i64);
                    }
                }
                "percent_rank" => {
                    for (i, slot) in out.iter_mut().enumerate() {
                        if n <= 1 {
                            *slot = SqlValue::Real(0.0);
                        } else {
                            let (g_start, _) = peers.group_bounds[peers.group_of[i]];
                            *slot = SqlValue::real((g_start as f64) / ((n - 1) as f64));
                        }
                    }
                }
                "cume_dist" => {
                    for (i, slot) in out.iter_mut().enumerate() {
                        let (_, g_end) = peers.group_bounds[peers.group_of[i]];
                        *slot = SqlValue::real((g_end as f64) / (n as f64));
                    }
                }
                _ => unreachable!(),
            }
        }
        "ntile" => {
            for (i, slot) in out.iter_mut().enumerate() {
                if args_per_row[i].len() != 1 {
                    return Err(SqlError::runtime(
                        "wrong number of arguments to function ntile()",
                    ));
                }
                let arg = &args_per_row[i][0];
                if arg.is_null() {
                    return Err(SqlError::runtime(
                        "argument of ntile must be a positive integer",
                    ));
                }
                let buckets = int_value(arg);
                if buckets <= 0 {
                    return Err(SqlError::runtime(
                        "argument of ntile must be a positive integer",
                    ));
                }
                let b = usize::try_from(buckets).unwrap_or(usize::MAX);
                let base = n / b;
                let rem = n % b;
                let bucket = if i < rem.saturating_mul(base + 1) {
                    i / (base + 1) + 1
                } else {
                    rem + (i - rem * (base + 1)) / base.max(1) + 1
                };
                *slot = SqlValue::Int(bucket as i64);
            }
        }
        "lag" | "lead" => {
            let is_lead = lower_name == "lead";
            for (i, slot) in out.iter_mut().enumerate() {
                let args = &args_per_row[i];
                if args.is_empty() || args.len() > 3 {
                    return Err(SqlError::runtime(format!(
                        "wrong number of arguments to function {lower_name}()"
                    )));
                }
                let offset = if args.len() >= 2 {
                    if args[1].is_null() {
                        *slot = args.get(2).cloned().unwrap_or(SqlValue::Null);
                        continue;
                    }
                    int_value(&args[1])
                } else {
                    1
                };
                let target = if is_lead {
                    i64::try_from(i).ok().and_then(|v| v.checked_add(offset))
                } else {
                    i64::try_from(i).ok().and_then(|v| v.checked_sub(offset))
                };
                if let Some(t) = target {
                    if let Ok(idx) = usize::try_from(t) {
                        if idx < n {
                            *slot = args_per_row[idx][0].clone();
                            continue;
                        }
                    }
                }
                *slot = args.get(2).cloned().unwrap_or(SqlValue::Null);
            }
        }
        "first_value" => {
            for (i, slot) in out.iter_mut().enumerate() {
                if args_per_row[i].len() != 1 {
                    return Err(SqlError::runtime(
                        "wrong number of arguments to function first_value()",
                    ));
                }
                let rows = frame_rows_for(i)?;
                *slot = rows
                    .first()
                    .map(|&r| args_per_row[r][0].clone())
                    .unwrap_or(SqlValue::Null);
            }
        }
        "last_value" => {
            for (i, slot) in out.iter_mut().enumerate() {
                if args_per_row[i].len() != 1 {
                    return Err(SqlError::runtime(
                        "wrong number of arguments to function last_value()",
                    ));
                }
                let rows = frame_rows_for(i)?;
                *slot = rows
                    .last()
                    .map(|&r| args_per_row[r][0].clone())
                    .unwrap_or(SqlValue::Null);
            }
        }
        "nth_value" => {
            for (i, slot) in out.iter_mut().enumerate() {
                let args = &args_per_row[i];
                if args.len() != 2 {
                    return Err(SqlError::runtime(
                        "wrong number of arguments to function nth_value()",
                    ));
                }
                if args[1].is_null() {
                    *slot = SqlValue::Null;
                    continue;
                }
                let n_pos = int_value(&args[1]);
                if n_pos <= 0 {
                    *slot = SqlValue::Null;
                    continue;
                }
                let rows = frame_rows_for(i)?;
                *slot = usize::try_from(n_pos - 1)
                    .ok()
                    .and_then(|idx| rows.get(idx))
                    .map(|&r| args_per_row[r][0].clone())
                    .unwrap_or(SqlValue::Null);
            }
        }
        _ => {
            return Err(SqlError::runtime(format!(
                "unknown window function {lower_name}()"
            )));
        }
    }

    Ok(out)
}

/// Evaluated frame specification for a partition.
#[derive(Debug, Clone)]
pub struct ResolvedFrame {
    pub units: FrameUnits,
    pub start_kind: FrameBoundType,
    pub start_offset: Option<SqlValue>,
    pub end_kind: FrameBoundType,
    pub end_offset: Option<SqlValue>,
    pub exclude: FrameExclude,
}

impl ResolvedFrame {
    /// Default SQLite frame:
    /// - With `ORDER BY`: `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`
    /// - Without `ORDER BY`: `RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING`
    pub fn default_for(has_order_by: bool) -> Self {
        Self {
            units: FrameUnits::Range,
            start_kind: FrameBoundType::UnboundedPreceding,
            start_offset: None,
            end_kind: if has_order_by {
                FrameBoundType::CurrentRow
            } else {
                FrameBoundType::UnboundedFollowing
            },
            end_offset: None,
            exclude: FrameExclude::NoOthers,
        }
    }

    pub fn from_ast<E>(spec: &FrameSpec, mut eval_bound: E) -> SqlResult<Self>
    where
        E: FnMut(&FrameBound) -> SqlResult<Option<SqlValue>>,
    {
        let start_offset = eval_bound(&spec.from)?;
        let (end_kind, end_offset) = match &spec.to {
            Some(to) => (to.kind, eval_bound(to)?),
            None => (FrameBoundType::CurrentRow, None),
        };
        Ok(Self {
            units: spec.units,
            start_kind: spec.from.kind,
            start_offset,
            end_kind,
            end_offset,
            exclude: spec.exclude.unwrap_or(FrameExclude::NoOthers),
        })
    }
}

fn check_int_offset(v: &Option<SqlValue>, is_start: bool) -> SqlResult<usize> {
    let label = if is_start { "starting" } else { "ending" };
    let Some(val) = v else {
        return Err(SqlError::runtime("frame offset expression missing"));
    };
    let Some(n) = must_be_int(val) else {
        return Err(SqlError::runtime(format!(
            "frame {label} offset must be a non-negative integer"
        )));
    };
    if n < 0 {
        return Err(SqlError::runtime(format!(
            "frame {label} offset must be a non-negative integer"
        )));
    }
    Ok(usize::try_from(n).unwrap_or(usize::MAX))
}

fn check_range_offset(v: &Option<SqlValue>, is_start: bool) -> SqlResult<SqlValue> {
    let label = if is_start { "starting" } else { "ending" };
    let Some(val) = v else {
        return Err(SqlError::runtime("frame offset expression missing"));
    };
    let num = match val {
        SqlValue::Int(i) => SqlValue::Int(*i),
        SqlValue::Real(r) => SqlValue::Real(*r),
        SqlValue::Text(b) => match apply_numeric_affinity(b, false) {
            Some(n) => n,
            None => {
                return Err(SqlError::runtime(format!(
                    "frame {label} offset must be a non-negative number"
                )));
            }
        },
        SqlValue::Null | SqlValue::Blob(_) => {
            return Err(SqlError::runtime(format!(
                "frame {label} offset must be a non-negative number"
            )));
        }
    };
    if mem_compare(&num, &SqlValue::Int(0), None) == Ordering::Less {
        return Err(SqlError::runtime(format!(
            "frame {label} offset must be a non-negative number"
        )));
    }
    Ok(num)
}

/// Compute the partition row indices belonging to the frame of row `row_idx`.
pub fn compute_frame_rows(
    frame: &ResolvedFrame,
    row_idx: usize,
    part_len: usize,
    peers: &PartitionPeers,
    order_vals: &[SqlValue],
    order_dir: SortOrder,
    order_coll: Option<&Collation>,
) -> SqlResult<Vec<usize>> {
    if part_len == 0 {
        return Ok(Vec::new());
    }
    let (start, end) = match frame.units {
        FrameUnits::Rows => {
            let s = match frame.start_kind {
                FrameBoundType::UnboundedPreceding => 0usize,
                FrameBoundType::UnboundedFollowing => part_len,
                FrameBoundType::CurrentRow => row_idx,
                FrameBoundType::Preceding => {
                    let off = check_int_offset(&frame.start_offset, true)?;
                    row_idx.saturating_sub(off)
                }
                FrameBoundType::Following => {
                    let off = check_int_offset(&frame.start_offset, true)?;
                    row_idx.saturating_add(off).min(part_len)
                }
            };
            let e = match frame.end_kind {
                FrameBoundType::UnboundedPreceding => 0usize,
                FrameBoundType::UnboundedFollowing => part_len,
                FrameBoundType::CurrentRow => row_idx + 1,
                FrameBoundType::Preceding => {
                    let off = check_int_offset(&frame.end_offset, false)?;
                    if off > row_idx {
                        0
                    } else {
                        row_idx - off + 1
                    }
                }
                FrameBoundType::Following => {
                    let off = check_int_offset(&frame.end_offset, false)?;
                    row_idx.saturating_add(off).saturating_add(1).min(part_len)
                }
            };
            (s, e)
        }
        FrameUnits::Groups => {
            let g = peers.group_of[row_idx];
            let ng = peers.group_bounds.len();
            let s = match frame.start_kind {
                FrameBoundType::UnboundedPreceding => 0,
                FrameBoundType::UnboundedFollowing => part_len,
                FrameBoundType::CurrentRow => peers.group_bounds[g].0,
                FrameBoundType::Preceding => {
                    let off = check_int_offset(&frame.start_offset, true)?;
                    peers.group_bounds[g.saturating_sub(off)].0
                }
                FrameBoundType::Following => {
                    let off = check_int_offset(&frame.start_offset, true)?;
                    let tg = g.saturating_add(off);
                    if tg >= ng {
                        part_len
                    } else {
                        peers.group_bounds[tg].0
                    }
                }
            };
            let e = match frame.end_kind {
                FrameBoundType::UnboundedPreceding => 0,
                FrameBoundType::UnboundedFollowing => part_len,
                FrameBoundType::CurrentRow => peers.group_bounds[g].1,
                FrameBoundType::Preceding => {
                    let off = check_int_offset(&frame.end_offset, false)?;
                    if off > g {
                        0
                    } else {
                        peers.group_bounds[g - off].1
                    }
                }
                FrameBoundType::Following => {
                    let off = check_int_offset(&frame.end_offset, false)?;
                    let tg = g.saturating_add(off).min(ng - 1);
                    peers.group_bounds[tg].1
                }
            };
            (s, e)
        }
        FrameUnits::Range => {
            let g = peers.group_of[row_idx];
            let needs_order = matches!(
                frame.start_kind,
                FrameBoundType::Preceding | FrameBoundType::Following
            ) || matches!(
                frame.end_kind,
                FrameBoundType::Preceding | FrameBoundType::Following
            );
            if needs_order && order_vals.len() != part_len {
                return Err(SqlError::runtime(
                    "RANGE with offset PRECEDING/FOLLOWING requires one ORDER BY expression",
                ));
            }
            let s = match frame.start_kind {
                FrameBoundType::UnboundedPreceding => 0,
                FrameBoundType::UnboundedFollowing => part_len,
                FrameBoundType::CurrentRow => peers.group_bounds[g].0,
                FrameBoundType::Preceding | FrameBoundType::Following => {
                    let off = check_range_offset(&frame.start_offset, true)?;
                    let cur = &order_vals[row_idx];
                    if !matches!(cur, SqlValue::Int(_) | SqlValue::Real(_)) {
                        peers.group_bounds[g].0
                    } else {
                        let bound = range_bound(
                            cur,
                            &off,
                            frame.start_kind == FrameBoundType::Preceding,
                            order_dir,
                        );
                        find_range_start(order_vals, &bound, order_dir, order_coll)
                    }
                }
            };
            let e = match frame.end_kind {
                FrameBoundType::UnboundedPreceding => 0,
                FrameBoundType::UnboundedFollowing => part_len,
                FrameBoundType::CurrentRow => peers.group_bounds[g].1,
                FrameBoundType::Preceding | FrameBoundType::Following => {
                    let off = check_range_offset(&frame.end_offset, false)?;
                    let cur = &order_vals[row_idx];
                    if !matches!(cur, SqlValue::Int(_) | SqlValue::Real(_)) {
                        peers.group_bounds[g].1
                    } else {
                        let bound = range_bound(
                            cur,
                            &off,
                            frame.end_kind == FrameBoundType::Preceding,
                            order_dir,
                        );
                        find_range_end(order_vals, &bound, order_dir, order_coll)
                    }
                }
            };
            (s, e)
        }
    };

    if start >= end {
        return Ok(Vec::new());
    }
    let (pg_start, pg_end) = peers.group_bounds[peers.group_of[row_idx]];
    let mut rows = Vec::with_capacity(end - start);
    for r in start..end {
        let excluded = match frame.exclude {
            FrameExclude::NoOthers => false,
            FrameExclude::CurrentRow => r == row_idx,
            FrameExclude::Group => r >= pg_start && r < pg_end,
            FrameExclude::Ties => r >= pg_start && r < pg_end && r != row_idx,
        };
        if !excluded {
            rows.push(r);
        }
    }
    Ok(rows)
}

fn range_bound(cur: &SqlValue, off: &SqlValue, is_preceding: bool, dir: SortOrder) -> SqlValue {
    let sub = match (is_preceding, dir) {
        (true, SortOrder::Asc) | (false, SortOrder::Desc) => true,
        (false, SortOrder::Asc) | (true, SortOrder::Desc) => false,
    };
    if sub {
        subtract(cur, off)
    } else {
        add(cur, off)
    }
}

fn find_range_start(
    vals: &[SqlValue],
    bound: &SqlValue,
    dir: SortOrder,
    coll: Option<&Collation>,
) -> usize {
    for (i, v) in vals.iter().enumerate() {
        if !matches!(v, SqlValue::Int(_) | SqlValue::Real(_)) {
            continue;
        }
        let cmp = mem_compare(v, bound, coll);
        let in_range = match dir {
            SortOrder::Asc => cmp != Ordering::Less,
            SortOrder::Desc => cmp != Ordering::Greater,
        };
        if in_range {
            return i;
        }
    }
    vals.len()
}

fn find_range_end(
    vals: &[SqlValue],
    bound: &SqlValue,
    dir: SortOrder,
    coll: Option<&Collation>,
) -> usize {
    let mut end = 0;
    for (i, v) in vals.iter().enumerate() {
        if !matches!(v, SqlValue::Int(_) | SqlValue::Real(_)) {
            continue;
        }
        let cmp = mem_compare(v, bound, coll);
        let in_range = match dir {
            SortOrder::Asc => cmp != Ordering::Greater,
            SortOrder::Desc => cmp != Ordering::Less,
        };
        if in_range {
            end = i + 1;
        }
    }
    end
}
