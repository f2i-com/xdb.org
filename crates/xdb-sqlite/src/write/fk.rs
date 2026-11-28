//! SQLite 3.54.1 foreign key validation, referential actions (`CASCADE`, `SET NULL`,
//! `SET DEFAULT`, `RESTRICT`, `NO ACTION`), `PRAGMA foreign_key_check`, and `ALTER TABLE`
//! foreign key DDL rewriters.

use xdb_core::value::{apply_affinity, mem_compare, CollationRegistry};
use xdb_core::{Meter, SqlValue};
use xdb_parser::{
    keyword_code, parse_statement, tk, ColumnConstraint, ForeignKeyDefinition, ParseOptions, Span,
    Statement, TableConstraint,
};
use xdb_sql::QueryOptions;

use crate::database::{ColumnSpec, ScanOptions, SqliteDatabase};
use crate::errors::{SqliteError, SqliteResult};
use crate::schema::{ColumnHiddenKind, SchemaCatalog, TableInfo};
use crate::sql::VirtualColumnPlan;
use crate::write::btree::table_get_record_fields;
use crate::write::pager::WritePager;

/// Maximum cascade recursion depth for `ON DELETE`/`ON UPDATE` foreign key actions.
pub(crate) const MAX_FK_CASCADE_DEPTH: usize = 256;

struct FkMemGuard<'a> {
    meter: Option<&'a dyn Meter>,
    reserved: u64,
}

impl<'a> FkMemGuard<'a> {
    fn new(meter: Option<&'a dyn Meter>) -> Self {
        Self { meter, reserved: 0 }
    }

    fn reserve(&mut self, bytes: u64) -> SqliteResult<()> {
        if bytes > 0 {
            if let Some(m) = self.meter {
                m.reserve_memory(bytes)?;
                self.reserved = self.reserved.saturating_add(bytes);
            }
        }
        Ok(())
    }
}

impl Drop for FkMemGuard<'_> {
    fn drop(&mut self) {
        if self.reserved > 0 {
            if let Some(m) = self.meter {
                m.release_memory(self.reserved);
            }
            self.reserved = 0;
        }
    }
}

/// Resolve the parent table's referenced column indices for `fk`, verifying SQLite's
/// requirement that the parent key columns form the parent's `PRIMARY KEY` or a non-partial
/// `UNIQUE` index with matching column collations and distinct 1-to-1 key columns.
pub(crate) fn resolve_parent_key_cids(
    child_name: &str,
    parent: &TableInfo,
    fk: &ForeignKeyDefinition,
) -> SqliteResult<Vec<usize>> {
    let parent_cids: Vec<usize> = if let Some(ref to_cols) = fk.to {
        if to_cols.len() != fk.from.len() {
            return Err(SqliteError::Sql(format!(
                "foreign key mismatch - \"{child_name}\" referencing \"{}\"",
                parent.name
            )));
        }
        let mut cids = Vec::with_capacity(to_cols.len());
        for col_name in to_cols {
            let Some(cid) = parent
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(col_name))
            else {
                return Err(SqliteError::Sql(format!(
                    "foreign key mismatch - \"{child_name}\" referencing \"{}\"",
                    parent.name
                )));
            };
            cids.push(cid);
        }
        cids
    } else {
        // Implicit parent columns = parent's PRIMARY KEY columns.
        if !parent.primary_key.is_empty() {
            let mut cids = Vec::with_capacity(parent.primary_key.len());
            for &pk_cid in &parent.primary_key {
                if pk_cid < 0 {
                    return Err(SqliteError::Sql(format!(
                        "foreign key mismatch - \"{child_name}\" referencing \"{}\"",
                        parent.name
                    )));
                }
                cids.push(pk_cid as usize);
            }
            if cids.len() != fk.from.len() {
                return Err(SqliteError::Sql(format!(
                    "foreign key mismatch - \"{child_name}\" referencing \"{}\"",
                    parent.name
                )));
            }
            cids
        } else if let Some(ipk) = parent.ipk_column {
            if fk.from.len() != 1 {
                return Err(SqliteError::Sql(format!(
                    "foreign key mismatch - \"{child_name}\" referencing \"{}\"",
                    parent.name
                )));
            }
            vec![ipk]
        } else {
            return Err(SqliteError::Sql(format!(
                "foreign key mismatch - \"{child_name}\" referencing \"{}\"",
                parent.name
            )));
        }
    };

    // Verify parent key columns are covered by IPK or a non-partial UNIQUE/PK index
    // with exact 1-to-1 distinct column bijection and matching collations.
    let covered_by_ipk = parent_cids.len() == 1 && parent.ipk_column == Some(parent_cids[0]);
    let covered_by_unique = covered_by_ipk
        || parent.indexes.iter().any(|ix| {
            if !ix.unique || ix.partial || ix.key_count != parent_cids.len() {
                return false;
            }
            let mut used = vec![false; ix.key_count];
            for &pcid in &parent_cids {
                let pcoll = &parent.columns[pcid].collation;
                let mut found = false;
                for (j, ic) in ix.columns[..ix.key_count].iter().enumerate() {
                    if !used[j]
                        && ic.cid >= 0
                        && ic.cid as usize == pcid
                        && ic.collation.eq_ignore_ascii_case(pcoll)
                    {
                        used[j] = true;
                        found = true;
                        break;
                    }
                }
                if !found {
                    return false;
                }
            }
            true
        });
    if !covered_by_unique {
        return Err(SqliteError::Sql(format!(
            "foreign key mismatch - \"{child_name}\" referencing \"{}\"",
            parent.name
        )));
    }
    Ok(parent_cids)
}

/// Compare two key tuples using the parent table's column affinities and collations.
pub(crate) fn fk_keys_match(
    parent: &TableInfo,
    parent_cids: &[usize],
    child_vals: &[SqlValue],
    parent_vals: &[SqlValue],
    colls: &CollationRegistry,
) -> SqliteResult<bool> {
    if child_vals.len() != parent_cids.len() || parent_vals.len() != parent_cids.len() {
        return Ok(false);
    }
    for (i, &pcid) in parent_cids.iter().enumerate() {
        let pcol = &parent.columns[pcid];
        let coerced_child = apply_affinity(child_vals[i].clone(), pcol.affinity);
        let coll = colls
            .find(&pcol.collation)
            .ok_or_else(|| SqliteError::UnknownCollation(pcol.collation.clone()))?;
        if mem_compare(&coerced_child, &parent_vals[i], Some(coll)) != std::cmp::Ordering::Equal {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Populate virtual generated columns in `full` (sized `table.columns.len()`) when present.
pub(crate) fn eval_virtual_cols_in_place(
    table: &TableInfo,
    full: &mut [SqlValue],
    plan: Option<&VirtualColumnPlan<'_>>,
    opts: &QueryOptions,
) -> SqliteResult<()> {
    if let Some(p) = plan {
        p.evaluate(full, opts).map_err(SqliteError::from)?;
    }
    let _ = table;
    Ok(())
}

fn build_virtual_plan_if_needed<'a>(
    table: &'a TableInfo,
    needed_cids: &[usize],
) -> SqliteResult<Option<VirtualColumnPlan<'a>>> {
    let needs_virt = needed_cids
        .iter()
        .any(|&c| table.columns.get(c).is_some_and(|col| col.hidden == ColumnHiddenKind::Virtual));
    if !needs_virt {
        return Ok(None);
    }
    let plan = VirtualColumnPlan::build_for(table, Some(needed_cids)).map_err(SqliteError::from)?;
    Ok(Some(plan))
}

/// Returns true if `parent` currently contains at least one row matching `child_vals` on `parent_cids`.
pub(crate) fn parent_has_key(
    pager: &WritePager,
    view: &SqliteDatabase,
    parent: &TableInfo,
    parent_cids: &[usize],
    child_vals: &[SqlValue],
    qopts: &QueryOptions,
) -> SqliteResult<bool> {
    // O(log N) fast path when referencing a single-column INTEGER PRIMARY KEY!
    if !parent.without_rowid && parent_cids.len() == 1 && parent.ipk_column == Some(parent_cids[0])
    {
        let pcol = &parent.columns[parent_cids[0]];
        let coerced = apply_affinity(child_vals[0].clone(), pcol.affinity);
        let SqlValue::Int(rid) = coerced else {
            return Ok(false);
        };
        return Ok(
            table_get_record_fields(pager, parent.root_page, rid, pager.encoding())?.is_some(),
        );
    }
    parent_has_key_scan(view, parent, parent_cids, child_vals, qopts)
}

fn parent_has_key_scan(
    db: &SqliteDatabase,
    parent: &TableInfo,
    parent_cids: &[usize],
    child_vals: &[SqlValue],
    qopts: &QueryOptions,
) -> SqliteResult<bool> {
    let reader = db.table(&parent.name)?;
    let meter = qopts.meter.as_deref();
    if !parent.without_rowid && parent_cids.len() == 1 && parent.ipk_column == Some(parent_cids[0])
    {
        let pcol = &parent.columns[parent_cids[0]];
        let coerced = apply_affinity(child_vals[0].clone(), pcol.affinity);
        let SqlValue::Int(rid) = coerced else {
            return Ok(false);
        };
        return Ok(reader.get(rid, None)?.is_some());
    }
    if let Some(vplan) = build_virtual_plan_if_needed(parent, parent_cids)? {
        let mut virt_qopts = qopts.clone();
        virt_qopts.in_trigger = false;
        let stored_cids: Vec<usize> = parent
            .columns
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| c.cid)
            .collect();
        let scan_opts = ScanOptions {
            columns: Some(stored_cids.iter().map(|&c| ColumnSpec::Cid(c)).collect()),
            rowid: false,
        };
        let mut cur = reader.cursor(Some(&scan_opts))?;
        let mut pvals = Vec::with_capacity(parent_cids.len());
        while cur.next()? {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let raw = cur.row()?;
            let mut full = vec![SqlValue::Null; parent.columns.len()];
            for (&cid, val) in stored_cids.iter().zip(raw) {
                full[cid] = val;
            }
            vplan
                .evaluate(&mut full, &virt_qopts)
                .map_err(SqliteError::from)?;
            pvals.clear();
            for &pcid in parent_cids {
                pvals.push(full[pcid].clone());
            }
            if fk_keys_match(parent, parent_cids, child_vals, &pvals, &qopts.collations)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    let scan_opts = ScanOptions {
        columns: Some(parent_cids.iter().map(|&c| ColumnSpec::Cid(c)).collect()),
        rowid: false,
    };
    let mut cur = reader.cursor(Some(&scan_opts))?;
    while cur.next()? {
        if let Some(m) = meter {
            m.charge_steps(1)?;
        }
        let pvals = cur.row()?;
        if fk_keys_match(parent, parent_cids, child_vals, &pvals, &qopts.collations)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Count rows in `child` whose `fk.from` columns match `parent_vals` (using `parent`'s affinity
/// and collation), optionally skipping a specific self-referencing row currently being mutated.
#[allow(clippy::too_many_arguments)]
pub(crate) fn count_child_refs_matching_parent_key(
    db: &SqliteDatabase,
    child: &TableInfo,
    fk: &ForeignKeyDefinition,
    parent: &TableInfo,
    parent_cids: &[usize],
    parent_vals: &[SqlValue],
    skip_self_rowid: Option<i64>,
    skip_self_wr_pk: Option<&[SqlValue]>,
    qopts: &QueryOptions,
) -> SqliteResult<i64> {
    if parent_vals.iter().any(SqlValue::is_null) {
        return Ok(0);
    }
    let meter = qopts.meter.as_deref();
    let child_vplan = build_virtual_plan_if_needed(child, &fk.from)?;
    let mut virt_qopts = qopts.clone();
    virt_qopts.in_trigger = false;
    let reader = db.table(&child.name)?;
    let stored_cids: Vec<usize> = child
        .columns
        .iter()
        .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
        .map(|c| c.cid)
        .collect();
    let scan_opts = ScanOptions {
        columns: Some(stored_cids.iter().map(|&c| ColumnSpec::Cid(c)).collect()),
        rowid: false,
    };
    let wr_pk_cids: Vec<(usize, String)> = if child.without_rowid && skip_self_wr_pk.is_some() {
        child
            .indexes
            .iter()
            .find(|ix| ix.without_rowid_primary_key)
            .map(|ix| {
                ix.columns[..ix.key_count]
                    .iter()
                    .filter(|ic| ic.cid >= 0)
                    .map(|ic| (ic.cid as usize, ic.collation.clone()))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut cur = reader.cursor(Some(&scan_opts))?;
    let mut count = 0i64;
    let mut cvals = Vec::with_capacity(fk.from.len());
    while cur.next()? {
        if let Some(m) = meter {
            m.charge_steps(1)?;
        }
        if !child.without_rowid {
            if let Some(skip_rid) = skip_self_rowid {
                if cur.rowid()? == skip_rid {
                    continue;
                }
            }
        }
        let raw = cur.row()?;
        let mut full = vec![SqlValue::Null; child.columns.len()];
        for (&cid, val) in stored_cids.iter().zip(raw) {
            full[cid] = val;
        }
        if child.without_rowid {
            if let Some(skip_pk) = skip_self_wr_pk {
                if wr_pk_cids.len() == skip_pk.len() {
                    let mut all_eq = true;
                    for (i, (cid, cname)) in wr_pk_cids.iter().enumerate() {
                        let coll = qopts
                            .collations
                            .find(cname)
                            .ok_or_else(|| SqliteError::UnknownCollation(cname.clone()))?;
                        if mem_compare(&full[*cid], &skip_pk[i], Some(coll))
                            != std::cmp::Ordering::Equal
                        {
                            all_eq = false;
                            break;
                        }
                    }
                    if all_eq {
                        continue;
                    }
                }
            }
        }
        eval_virtual_cols_in_place(child, &mut full, child_vplan.as_ref(), &virt_qopts)?;
        cvals.clear();
        let mut any_null = false;
        for &fcid in &fk.from {
            let v = full.get(fcid).cloned().unwrap_or(SqlValue::Null);
            if v.is_null() {
                any_null = true;
                break;
            }
            cvals.push(v);
        }
        if any_null {
            continue;
        }
        if fk_keys_match(parent, parent_cids, &cvals, parent_vals, &qopts.collations)? {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

/// Collect all foreign-key violations (`table, rowid, parent, fkid`) for `PRAGMA foreign_key_check`.
pub(crate) fn collect_fk_violations_db(
    db: &SqliteDatabase,
    filter_table: Option<&str>,
    qopts: &QueryOptions,
) -> SqliteResult<Vec<Vec<SqlValue>>> {
    let schema = db.schema();
    let meter = qopts.meter.as_deref();
    let mut violations = Vec::new();
    let mut mem = FkMemGuard::new(meter);
    let mut virt_qopts = qopts.clone();
    virt_qopts.in_trigger = false;

    if let Some(tname) = filter_table {
        if schema.table(tname).is_none() {
            if schema.view(tname).is_some() {
                return Ok(Vec::new());
            }
            return Err(SqliteError::Sql(format!("no such table: {tname}")));
        }
    }

    for child in &schema.tables {
        if child.system || child.definition.foreign_keys.is_empty() {
            continue;
        }
        if let Some(ft) = filter_table {
            if !child.name.eq_ignore_ascii_case(ft) {
                continue;
            }
        }
        type FkCheckSpec<'a> = (
            i64,
            &'a ForeignKeyDefinition,
            Option<(&'a TableInfo, Vec<usize>)>,
        );
        // Build reverse-declared FK list matching `PRAGMA foreign_key_list` id numbering (`0..`).
        let mut fk_specs: Vec<FkCheckSpec<'_>> = Vec::new();
        let mut all_fk_cids: Vec<usize> = Vec::new();
        for (fk_id, fk) in child.definition.foreign_keys.iter().rev().enumerate() {
            all_fk_cids.extend_from_slice(&fk.from);
            if let Some(parent) = schema.table(&fk.table) {
                let pcids = resolve_parent_key_cids(&child.name, parent, fk)?;
                fk_specs.push((fk_id as i64, fk, Some((parent.as_ref(), pcids))));
            } else if schema.view(&fk.table).is_some()
                || schema.virtual_table(&fk.table).is_some()
            {
                return Err(SqliteError::Sql(format!(
                    "foreign key mismatch - \"{}\" referencing \"{}\"",
                    child.name, fk.table
                )));
            } else {
                fk_specs.push((fk_id as i64, fk, None));
            }
        }
        let child_vplan = build_virtual_plan_if_needed(child, &all_fk_cids)?;

        // Scan child rows once and test each outgoing FK.
        let reader = db.table(&child.name)?;
        let stored_cids: Vec<usize> = child
            .columns
            .iter()
            .filter(|c| c.hidden != ColumnHiddenKind::Virtual)
            .map(|c| c.cid)
            .collect();
        let scan_opts = ScanOptions {
            columns: Some(stored_cids.iter().map(|&c| ColumnSpec::Cid(c)).collect()),
            rowid: false,
        };
        let mut cur = reader.cursor(Some(&scan_opts))?;
        while cur.next()? {
            if let Some(m) = meter {
                m.charge_steps(1)?;
            }
            let rid_val = if child.without_rowid {
                SqlValue::Null
            } else {
                SqlValue::Int(cur.rowid()?)
            };
            let raw = cur.row()?;
            let mut full = vec![SqlValue::Null; child.columns.len()];
            for (&cid, val) in stored_cids.iter().zip(raw) {
                full[cid] = val;
            }
            eval_virtual_cols_in_place(child, &mut full, child_vplan.as_ref(), &virt_qopts)?;
            for &(fk_id, fk, ref parent_opt) in &fk_specs {
                let mut cvals = Vec::with_capacity(fk.from.len());
                let mut any_null = false;
                for &fcid in &fk.from {
                    let v = full.get(fcid).cloned().unwrap_or(SqlValue::Null);
                    if v.is_null() {
                        any_null = true;
                        break;
                    }
                    cvals.push(v);
                }
                if any_null {
                    continue;
                }
                let matched = if let Some((parent, ref pcids)) = *parent_opt {
                    parent_has_key_scan(db, parent, pcids, &cvals, &virt_qopts)?
                } else {
                    false
                };
                if !matched {
                    mem.reserve(
                        128u64
                            .saturating_add(child.name.len() as u64)
                            .saturating_add(fk.table.len() as u64),
                    )?;
                    violations.push(vec![
                        SqlValue::text(child.name.as_str()),
                        rid_val.clone(),
                        SqlValue::text(fk.table.as_str()),
                        SqlValue::Int(fk_id),
                    ]);
                }
            }
        }
    }
    Ok(violations)
}

/// Check if any child table in `schema` has an outgoing FK pointing to `parent_name`.
/// Matches SQLite's linked-list prepending order (reverse table order, reverse FK declaration order).
pub(crate) fn incoming_fks_for<'a>(
    schema: &'a SchemaCatalog,
    parent_name: &str,
) -> Vec<(String, &'a ForeignKeyDefinition)> {
    let mut out = Vec::new();
    for tbl in schema.tables.iter().rev() {
        if tbl.system {
            continue;
        }
        for fk in tbl.definition.foreign_keys.iter().rev() {
            if fk.table.eq_ignore_ascii_case(parent_name) {
                out.push((tbl.name.clone(), fk));
            }
        }
    }
    out
}

/// Check whether `old_key` and `new_key` are equal under parent's column collations.
pub(crate) fn parent_key_unchanged(
    parent: &TableInfo,
    parent_cids: &[usize],
    old_row: &[SqlValue],
    new_row: &[SqlValue],
    colls: &CollationRegistry,
) -> SqliteResult<bool> {
    for &pcid in parent_cids {
        let cname = &parent.columns[pcid].collation;
        let coll = colls
            .find(cname)
            .ok_or_else(|| SqliteError::UnknownCollation(cname.clone()))?;
        let ov = old_row.get(pcid).unwrap_or(&SqlValue::Null);
        let nv = new_row.get(pcid).unwrap_or(&SqlValue::Null);
        if mem_compare(ov, nv, Some(coll)) != std::cmp::Ordering::Equal {
            return Ok(false);
        }
    }
    Ok(true)
}

// ───────────────────────────── ALTER TABLE FK DDL rewriters ─────────────────────────────

fn quote_ident_if_needed(name: &str) -> String {
    let simple = !name.is_empty()
        && name.bytes().enumerate().all(|(i, b)| {
            if i == 0 {
                b.is_ascii_alphabetic() || b == b'_'
            } else {
                b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
            }
        })
        && keyword_code(name.as_bytes()) == tk::ID;
    if simple {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

#[inline]
fn is_ident_boundary_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

fn apply_spans(sql: &str, mut spans: Vec<Span>, replacement: &str) -> String {
    if spans.is_empty() {
        return sql.to_string();
    }
    spans.sort_by_key(|s| (s.start, s.end));
    spans.dedup();
    let already_quoted = replacement.starts_with('"');
    let forced_quoted = if already_quoted {
        None
    } else {
        Some(format!("\"{}\"", replacement.replace('"', "\"\"")))
    };
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + spans.len() * (replacement.len() + 2));
    let mut last = 0usize;
    for sp in spans {
        if sp.start < last || sp.end > sql.len() || sp.start > sp.end {
            continue;
        }
        out.push_str(&sql[last..sp.start]);
        let need_quote = !already_quoted
            && (matches!(bytes.get(sp.start), Some(b'"' | b'`' | b'[' | b'\''))
                || (sp.start > 0 && is_ident_boundary_byte(bytes[sp.start - 1]))
                || (sp.end < bytes.len() && is_ident_boundary_byte(bytes[sp.end])));
        if need_quote {
            out.push_str(forced_quoted.as_deref().unwrap_or(replacement));
        } else {
            out.push_str(replacement);
        }
        last = sp.end;
    }
    out.push_str(&sql[last..]);
    out
}

/// Rewrite `REFERENCES <old_table>` clauses inside a `CREATE TABLE` DDL (including self-references).
pub(crate) fn rewrite_fk_table_name_in_ddl(ddl: &str, old_table: &str, new_table: &str) -> String {
    let Ok(Statement::CreateTable(ct)) = parse_statement(ddl, &ParseOptions::default()) else {
        return ddl.to_string();
    };
    let mut spans = Vec::new();
    for col in &ct.columns {
        for c in &col.constraints {
            if let ColumnConstraint::References { clause, .. } = c {
                if clause.table.name.eq_ignore_ascii_case(old_table) {
                    spans.push(clause.table.span);
                }
            }
        }
    }
    for tc in &ct.constraints {
        if let TableConstraint::ForeignKey { clause, .. } = tc {
            if clause.table.name.eq_ignore_ascii_case(old_table) {
                spans.push(clause.table.span);
            }
        }
    }
    apply_spans(ddl, spans, &quote_ident_if_needed(new_table))
}

/// Rewrite `REFERENCES <parent_table>(..., <old_col>, ...)` clauses inside a `CREATE TABLE` DDL.
pub(crate) fn rewrite_fk_parent_column_in_ddl(
    ddl: &str,
    parent_table: &str,
    old_col: &str,
    new_col: &str,
) -> String {
    let Ok(Statement::CreateTable(ct)) = parse_statement(ddl, &ParseOptions::default()) else {
        return ddl.to_string();
    };
    let mut spans = Vec::new();
    for col in &ct.columns {
        for c in &col.constraints {
            if let ColumnConstraint::References { clause, .. } = c {
                if clause.table.name.eq_ignore_ascii_case(parent_table) {
                    if let Some(ref cols) = clause.columns {
                        for item in cols {
                            if item.name.name.eq_ignore_ascii_case(old_col) {
                                spans.push(item.name.span);
                            }
                        }
                    }
                }
            }
        }
    }
    for tc in &ct.constraints {
        if let TableConstraint::ForeignKey { clause, .. } = tc {
            if clause.table.name.eq_ignore_ascii_case(parent_table) {
                if let Some(ref cols) = clause.columns {
                    for item in cols {
                        if item.name.name.eq_ignore_ascii_case(old_col) {
                            spans.push(item.name.span);
                        }
                    }
                }
            }
        }
    }
    apply_spans(ddl, spans, &quote_ident_if_needed(new_col))
}
