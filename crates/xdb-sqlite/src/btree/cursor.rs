//! Read-only B-tree cursors (`TableCursor` and `IndexCursor`) with full structural
//! corruption guards and deterministic sandbox metering.

use std::sync::Arc;

use super::pager::Pager;
use super::record_view::RecordView;
use crate::errors::{SqliteError, SqliteResult};
use crate::format::{BtreePage, CellInfo, BTREE_MAX_DEPTH};

/// Result of [`TableCursor::seek`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeekResult {
    /// No row has rowid `>= key`; cursor is invalid.
    Eof,
    /// Positioned on the row with exactly `key`.
    Exact,
    /// Positioned on the first row with a larger rowid.
    After,
}

/// Positioning seek modes (`OP_SeekLT` / `LE` / `GE` / `GT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum SeekOp {
    Lt = 0,
    Le = 1,
    Ge = 2,
    Gt = 3,
}

/// Search key interface for [`IndexCursor::move_to_key`] / [`IndexCursor::seek_key`].
pub trait RecordKey {
    fn compare_record(&mut self, buf: &[u8], start: usize, n: usize) -> i32;
    fn set_default_rc(&mut self, rc: i8);
    fn is_corrupt(&self) -> bool;
    fn clear_corrupt(&mut self);
}

const COMPARE_PAD: usize = 18;

#[derive(Debug)]
struct CursorCore {
    pager: Pager,
    noop_meter: bool,
    root_page: u32,
    is_table: bool,
    pages: Vec<Arc<BtreePage>>,
    idx: [usize; BTREE_MAX_DEPTH],
    valid: bool,
    cell: CellInfo,
    cell_valid: bool,
    stamp: u64,
    record_stamp: u64,
    record: Option<RecordView>,
    moves: u32,
    dir: i8,
}

impl CursorCore {
    fn new(pager: Pager, root_page: u32, is_table: bool) -> SqliteResult<Self> {
        if root_page < 1 {
            return Err(SqliteError::corrupt(format!(
                "invalid b-tree root page {root_page}"
            )));
        }
        let noop_meter = pager.is_noop_meter();
        Ok(Self {
            pager,
            noop_meter,
            root_page,
            is_table,
            pages: Vec::with_capacity(BTREE_MAX_DEPTH),
            idx: [0; BTREE_MAX_DEPTH],
            valid: false,
            cell: CellInfo::default(),
            cell_valid: false,
            stamp: 1,
            record_stamp: 0,
            record: None,
            moves: 0,
            dir: 0,
        })
    }

    #[inline]
    fn is_valid(&self) -> bool {
        self.valid
    }

    #[inline]
    fn depth(&self) -> usize {
        self.pages.len() - 1
    }

    #[inline]
    fn current_page(&self) -> &Arc<BtreePage> {
        self.pages.last().expect("cursor has page")
    }

    #[inline]
    fn page_number(&self) -> u32 {
        if self.valid {
            self.current_page().pgno
        } else {
            0
        }
    }

    #[inline]
    fn cell_index(&self) -> Option<usize> {
        if self.valid {
            Some(self.idx[self.depth()])
        } else {
            None
        }
    }

    #[inline]
    fn moved(&mut self) {
        self.cell_valid = false;
        self.stamp = self.stamp.wrapping_add(1);
    }

    #[inline]
    fn invalidate(&mut self) {
        self.valid = false;
        self.pages.clear();
        self.moved();
    }

    fn require_valid(&self) -> SqliteResult<()> {
        if !self.valid {
            Err(SqliteError::misuse("cursor is not positioned on an entry"))
        } else {
            Ok(())
        }
    }

    fn current_cell(&mut self) -> SqliteResult<CellInfo> {
        if !self.cell_valid {
            self.require_valid()?;
            let d = self.depth();
            let ix = self.idx[d];
            self.cell = self.pages[d].parse_cell(ix)?;
            self.cell_valid = true;
        }
        Ok(self.cell)
    }

    fn record(&mut self) -> SqliteResult<&mut RecordView> {
        self.require_valid()?;
        if self.record.is_some() && self.record_stamp == self.stamp {
            return Ok(self.record.as_mut().expect("some"));
        }
        let c = self.current_cell()?;
        let cur_pgno = self.current_page().pgno;
        if self.record.as_ref().is_some_and(|r| r.pgno() == cur_pgno) {
            let rec = self.record.as_mut().expect("some");
            rec.reset_same_page(c.payload_offset, c.local_size, c.payload_size);
            self.record_stamp = self.stamp;
            return Ok(rec);
        }
        let page_data = self.current_page().data.clone();
        if self.record.is_none() {
            let enc = self.pager.db().text_encoding;
            let pager = self.pager.clone();
            self.record = Some(RecordView::new(pager, enc));
        }
        let rec = self.record.as_mut().expect("some");
        rec.reset(cur_pgno, page_data, c.payload_offset, c.local_size, c.payload_size);
        self.record_stamp = self.stamp;
        Ok(rec)
    }

    fn step_dir(&mut self, dir: i8) {
        if dir != self.dir {
            self.dir = dir;
            self.moves = 0;
        }
    }

    fn move_to_root(&mut self) -> SqliteResult<bool> {
        self.moved();
        self.moves = 0;
        self.dir = 0;
        self.pages.clear();
        if self.root_page == 1 && self.pager.page_count() == 0 {
            self.valid = false;
            return Ok(false);
        }
        let root = self.pager.get_btree_page(self.root_page, "b-tree root")?;
        if root.is_table != self.is_table {
            return Err(SqliteError::corrupt(format!(
                "page {}: expected the root of {} b-tree, found page type 0x{:02x}",
                root.pgno,
                if self.is_table { "a table" } else { "an index" },
                root.page_type as u8
            )));
        }
        let n_cell = root.n_cell;
        let is_leaf = root.is_leaf;
        let pgno = root.pgno;
        let right_child = root.right_child;
        self.pages.push(root);
        self.idx[0] = 0;
        if n_cell > 0 {
            self.valid = true;
            return Ok(true);
        }
        if !is_leaf {
            if pgno != 1 {
                self.invalidate();
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: interior root page has no cells"
                )));
            }
            if let Err(e) = self.move_to_child(right_child) {
                self.invalidate();
                return Err(e);
            }
            self.valid = true;
            return Ok(true);
        }
        self.valid = false;
        self.pages.clear();
        Ok(false)
    }

    fn move_to_child(&mut self, pgno: u32) -> SqliteResult<()> {
        if !self.noop_meter {
            self.pager.meter().charge_steps(1)?;
        }
        if self.pages.len() >= BTREE_MAX_DEPTH {
            return Err(SqliteError::corrupt(format!(
                "b-tree {}: deeper than {BTREE_MAX_DEPTH} levels",
                self.root_page
            )));
        }
        self.moves += 1;
        if self.moves > self.pager.page_count() {
            return Err(SqliteError::corrupt(format!(
                "b-tree {}: a walk entered more pages than the database holds (cycle or shared subtree)",
                self.root_page
            )));
        }
        let child = self.pager.get_btree_page(pgno, "b-tree child")?;
        if child.n_cell < 1 || child.is_table != self.is_table {
            return Err(SqliteError::corrupt(if child.n_cell < 1 {
                format!("page {pgno}: child page has no cells")
            } else {
                format!(
                    "page {pgno}: child page type 0x{:02x} does not match its {} b-tree",
                    child.page_type as u8,
                    if self.is_table { "table" } else { "index" }
                )
            }));
        }
        self.moved();
        let next_d = self.pages.len();
        self.pages.push(child);
        self.idx[next_d] = 0;
        Ok(())
    }

    fn move_to_parent(&mut self) {
        self.moved();
        self.pages.pop();
    }

    fn move_to_leftmost(&mut self) -> SqliteResult<()> {
        while !self.current_page().is_leaf {
            let d = self.depth();
            let child = self.pages[d].child_at(self.idx[d])?;
            self.move_to_child(child)?;
        }
        Ok(())
    }

    fn move_to_rightmost(&mut self) -> SqliteResult<()> {
        while !self.current_page().is_leaf {
            let d = self.depth();
            let n_cell = self.pages[d].n_cell;
            let right_child = self.pages[d].right_child;
            self.idx[d] = n_cell;
            self.move_to_child(right_child)?;
        }
        let d = self.depth();
        self.idx[d] = self.pages[d].n_cell - 1;
        Ok(())
    }

    fn first(&mut self) -> SqliteResult<bool> {
        if !self.move_to_root()? {
            return Ok(false);
        }
        self.move_to_leftmost()?;
        Ok(true)
    }

    fn last(&mut self) -> SqliteResult<bool> {
        if !self.move_to_root()? {
            return Ok(false);
        }
        self.move_to_rightmost()?;
        Ok(true)
    }

    fn next(&mut self) -> SqliteResult<bool> {
        if !self.valid {
            return Ok(false);
        }
        if !self.noop_meter {
            self.pager.meter().charge_steps(1)?;
        }
        self.step_dir(1);
        self.moved();
        let d = self.depth();
        self.idx[d] += 1;
        let ix = self.idx[d];
        let n_cell = self.pages[d].n_cell;
        let is_leaf = self.pages[d].is_leaf;
        if ix < n_cell {
            if !is_leaf {
                self.move_to_leftmost()?;
            }
            return Ok(true);
        }
        self.next_slow()
    }

    fn next_slow(&mut self) -> SqliteResult<bool> {
        loop {
            let d = self.depth();
            let ix = self.idx[d];
            let n_cell = self.pages[d].n_cell;
            let is_leaf = self.pages[d].is_leaf;
            if ix >= n_cell {
                if !is_leaf {
                    let right = self.pages[d].right_child;
                    self.move_to_child(right)?;
                    self.move_to_leftmost()?;
                    return Ok(true);
                }
                loop {
                    if self.depth() == 0 {
                        self.invalidate();
                        return Ok(false);
                    }
                    self.move_to_parent();
                    let pd = self.depth();
                    if self.idx[pd] < self.pages[pd].n_cell {
                        break;
                    }
                }
                let pd = self.depth();
                if !self.pages[pd].is_table {
                    return Ok(true);
                }
                self.idx[pd] += 1;
                if self.idx[pd] < self.pages[pd].n_cell {
                    self.move_to_leftmost()?;
                    return Ok(true);
                }
                continue;
            }
            if !is_leaf {
                self.move_to_leftmost()?;
            }
            return Ok(true);
        }
    }

    fn prev(&mut self) -> SqliteResult<bool> {
        if !self.valid {
            return Ok(false);
        }
        if !self.noop_meter {
            self.pager.meter().charge_steps(1)?;
        }
        self.step_dir(-1);
        self.moved();
        loop {
            let d = self.depth();
            if !self.pages[d].is_leaf {
                let child = self.pages[d].child_at(self.idx[d])?;
                self.move_to_child(child)?;
                self.move_to_rightmost()?;
                return Ok(true);
            }
            while self.idx[self.depth()] == 0 {
                if self.depth() == 0 {
                    self.invalidate();
                    return Ok(false);
                }
                self.move_to_parent();
            }
            let pd = self.depth();
            self.idx[pd] -= 1;
            if self.pages[pd].is_table && !self.pages[pd].is_leaf {
                continue;
            }
            return Ok(true);
        }
    }

    fn count(&mut self) -> SqliteResult<u64> {
        if !self.first()? {
            return Ok(0);
        }
        let mut n = 0u64;
        if self.is_table {
            loop {
                let d = self.depth();
                let n_cell = self.pages[d].n_cell;
                n += n_cell as u64;
                self.idx[d] = n_cell - 1;
                if !self.next()? {
                    return Ok(n);
                }
            }
        }
        // Index / WITHOUT ROWID B-tree: every cell on every interior and leaf page is an entry,
        // so we sum `n_cell` across all reachable pages in O(pages) time with strict cycle guards.
        self.invalidate();
        let max_pages = self.pager.page_count();
        let mut visited_pages: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut stack: Vec<(u32, usize)> = Vec::with_capacity(BTREE_MAX_DEPTH * 2);
        stack.push((self.root_page, 0));
        while let Some((pgno, depth)) = stack.pop() {
            if !visited_pages.insert(pgno) || visited_pages.len() > max_pages as usize {
                return Err(SqliteError::corrupt(format!(
                    "b-tree rooted at page {} contains a cycle or shared page {pgno}",
                    self.root_page
                )));
            }
            if !self.noop_meter {
                self.pager.meter().charge_steps(1)?;
            }
            let page = self
                .pager
                .get_btree_page(pgno, "index b-tree")?;
            if page.is_table {
                return Err(SqliteError::corrupt(format!(
                    "page {pgno}: expected index b-tree page"
                )));
            }
            n = n.saturating_add(page.n_cell as u64);
            if !page.is_leaf {
                if depth + 1 >= BTREE_MAX_DEPTH {
                    return Err(SqliteError::corrupt(format!(
                        "b-tree rooted at page {} exceeds maximum depth {BTREE_MAX_DEPTH}",
                        self.root_page
                    )));
                }
                let needed_stack = stack.len().saturating_add(page.n_cell + 1);
                if needed_stack > max_pages as usize + 1 {
                    return Err(SqliteError::corrupt(format!(
                        "b-tree rooted at page {} has more child pointers than pages",
                        self.root_page
                    )));
                }
                stack.push((page.right_child, depth + 1));
                for i in 0..page.n_cell {
                    let child = page.child_at(i)?;
                    stack.push((child, depth + 1));
                }
            }
        }
        Ok(n)
    }
}

/// Cursor over a table (intkey) B-tree: rows in `rowid` order.
#[derive(Debug)]
pub struct TableCursor {
    core: CursorCore,
}

impl TableCursor {
    pub fn new(pager: Pager, root_page: u32) -> SqliteResult<Self> {
        Ok(Self {
            core: CursorCore::new(pager, root_page, true)?,
        })
    }

    #[inline]
    pub fn is_valid(&self) -> bool {
        self.core.is_valid()
    }

    #[inline]
    pub fn page_number(&self) -> u32 {
        self.core.page_number()
    }

    #[inline]
    pub fn cell_index(&self) -> Option<usize> {
        self.core.cell_index()
    }

    pub fn first(&mut self) -> SqliteResult<bool> {
        self.core.first()
    }

    pub fn last(&mut self) -> SqliteResult<bool> {
        self.core.last()
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> SqliteResult<bool> {
        self.core.next()
    }

    pub fn prev(&mut self) -> SqliteResult<bool> {
        self.core.prev()
    }

    pub fn count(&mut self) -> SqliteResult<u64> {
        self.core.count()
    }

    pub fn rowid(&mut self) -> SqliteResult<i64> {
        Ok(self.core.current_cell()?.key)
    }

    pub fn payload_size(&mut self) -> SqliteResult<u32> {
        Ok(self.core.current_cell()?.payload_size)
    }

    pub fn record(&mut self) -> SqliteResult<&mut RecordView> {
        self.core.record()
    }

    /// Zero-copy access to `(rowid, local_payload_bytes, encoding)` for the current table
    /// leaf row when the entire record fits inline on the leaf page (no overflow chain).
    #[inline]
    pub(crate) fn current_local_payload(
        &mut self,
    ) -> SqliteResult<Option<(i64, &[u8], xdb_core::TextEncoding)>> {
        if !self.core.noop_meter {
            return Ok(None);
        }
        let cell = self.core.current_cell()?;
        if cell.overflow_page != 0 || cell.local_size as u32 != cell.payload_size {
            return Ok(None);
        }
        let d = self.core.depth();
        let page = &self.core.pages[d];
        let start = cell.payload_offset;
        let end = start + cell.local_size;
        let usable = (self.core.pager.db().usable_size as usize).min(page.data.len());
        if end > usable {
            return Err(SqliteError::corrupt(format!(
                "page {}: local payload runs past usable page end",
                page.pgno
            )));
        }
        Ok(Some((
            cell.key,
            &page.data[start..end],
            self.core.pager.db().text_encoding,
        )))
    }

    /// Snapshot the current leaf `Arc<BtreePage>` and geometry for whole-leaf batch scanning
    /// (zero per-row `CellInfo` / `CursorCore::next()` overhead on un-metered table scans).
    #[inline]
    #[allow(clippy::type_complexity)]
    pub(crate) fn current_leaf_batch(
        &self,
    ) -> Option<(
        Arc<BtreePage>,
        usize,
        usize,
        u32,
        crate::format::PayloadLimits,
        xdb_core::TextEncoding,
    )> {
        if !self.core.valid || !self.core.noop_meter || self.core.pages.is_empty() {
            return None;
        }
        let d = self.core.depth();
        let page = &self.core.pages[d];
        if !page.is_leaf {
            return None;
        }
        let db = self.core.pager.db();
        Some((
            Arc::clone(page),
            self.core.idx[d],
            page.n_cell,
            db.usable_size,
            db.limits,
            db.text_encoding,
        ))
    }

    /// Position this cursor at exact cell index `ci` on the current leaf (used for overflow
    /// or non-inline record fallback inside a whole-leaf batch scan).
    #[inline]
    pub(crate) fn set_leaf_cell_index(&mut self, ci: usize) {
        let d = self.core.depth();
        self.core.idx[d] = ci;
        self.core.moved();
    }

    /// Jump directly from the current leaf to cell `0` of the next leaf in B-tree order.
    #[inline]
    pub(crate) fn advance_to_next_leaf(&mut self) -> SqliteResult<bool> {
        if !self.core.valid || self.core.pages.is_empty() {
            return Ok(false);
        }
        let d = self.core.depth();
        let n_cell = self.core.pages[d].n_cell;
        if n_cell > 0 {
            self.core.idx[d] = n_cell - 1;
        }
        self.core.next()
    }

    /// Position on the row with smallest rowid `>= rowid`.
    pub fn seek(&mut self, rowid: i64) -> SqliteResult<SeekResult> {
        match self.move_to(rowid)? {
            None => Ok(SeekResult::Eof),
            Some(0) => Ok(SeekResult::Exact),
            Some(c) if c > 0 => Ok(SeekResult::After),
            Some(_) => Ok(if self.next()? {
                SeekResult::After
            } else {
                SeekResult::Eof
            }),
        }
    }

    /// `OP_SeekLT / LE / GE / GT` on an integer rowid key.
    pub fn seek_rowid(&mut self, rowid: i64, op: SeekOp) -> SqliteResult<bool> {
        let Some(c) = self.move_to(rowid)? else {
            return Ok(false);
        };
        if op >= SeekOp::Ge {
            if c < 0 || (c == 0 && op == SeekOp::Gt) {
                return self.next();
            }
            return Ok(true);
        }
        if c > 0 || (c == 0 && op == SeekOp::Lt) {
            return self.prev();
        }
        Ok(true)
    }

    /// `sqlite3BtreeTableMoveto`: binary-search down to the leaf for `rowid`.
    pub fn move_to(&mut self, rowid: i64) -> SqliteResult<Option<i32>> {
        let res = self.move_to_inner(rowid);
        if res.is_err() {
            self.core.invalidate();
        }
        res
    }

    fn move_to_inner(&mut self, rowid: i64) -> SqliteResult<Option<i32>> {
        let noop = self.core.noop_meter;
        // Same-leaf and next-leaf locality fast path: avoids `Arc::clone` and root re-descent
        // when seeking ascending rowids from secondary index scans (`WHERE grp = 7`).
        if self.core.valid && !self.core.pages.is_empty() {
            let d = self.core.depth();
            if self.core.pages[d].is_leaf && self.core.pages[d].n_cell > 0 {
                let (first_k, last_k) = {
                    let page = &self.core.pages[d];
                    (page.rowid_at(0)?, page.rowid_at(page.n_cell - 1)?)
                };
                if rowid >= first_k && rowid <= last_k {
                    self.core.moves = 0;
                    self.core.dir = 0;
                    let page = &self.core.pages[d];
                    let mut lwr: isize = 0;
                    let mut upr: isize = page.n_cell as isize - 1;
                    let mut idx: isize = upr >> 1;
                    let c: i32;
                    loop {
                        if !noop {
                            self.core.pager.meter().charge_steps(1)?;
                        }
                        let k = page.rowid_at(idx as usize)?;
                        if k < rowid {
                            lwr = idx + 1;
                            if lwr > upr {
                                c = -1;
                                break;
                            }
                        } else if k > rowid {
                            upr = idx - 1;
                            if lwr > upr {
                                c = 1;
                                break;
                            }
                        } else {
                            self.core.idx[d] = idx as usize;
                            self.core.moved();
                            return Ok(Some(0));
                        }
                        idx = (lwr + upr) >> 1;
                    }
                    self.core.idx[d] = idx as usize;
                    self.core.moved();
                    return Ok(Some(c));
                }
                // Next-leaf sibling check: if `rowid > last_k` and parent has a next child slot
                // whose upper bound covers `rowid`, step directly to that sibling leaf!
                if rowid > last_k && d >= 1 {
                    let p_idx = self.core.idx[d - 1];
                    let parent = &self.core.pages[d - 1];
                    if !parent.is_leaf && p_idx < parent.n_cell {
                        let next_slot = p_idx + 1;
                        let fits_next = if next_slot < parent.n_cell {
                            rowid <= parent.rowid_at(next_slot)?
                        } else {
                            // `next_slot == parent.n_cell` is `right_child` of `parent`: only valid
                            // if all ancestors above `d - 1` have no smaller upper bound (i.e.
                            // `d == 1`, where `parent` IS root).
                            d == 1
                        };
                        if fits_next {
                            let child_pgno = if next_slot < parent.n_cell {
                                parent.child_at(next_slot)?
                            } else {
                                parent.right_child
                            };
                            if !noop {
                                self.core.pager.meter().charge_steps(1)?;
                            }
                            let child_page =
                                self.core.pager.get_btree_page(child_pgno, "b-tree child")?;
                            if child_page.is_table && child_page.is_leaf && child_page.n_cell > 0 {
                                self.core.moves = 0;
                                self.core.dir = 0;
                                self.core.idx[d - 1] = next_slot;
                                self.core.pages[d] = child_page;
                                let page = &self.core.pages[d];
                                let mut lwr: isize = 0;
                                let mut upr: isize = page.n_cell as isize - 1;
                                let mut idx: isize = upr >> 1;
                                let c: i32;
                                loop {
                                    if !noop {
                                        self.core.pager.meter().charge_steps(1)?;
                                    }
                                    let k = page.rowid_at(idx as usize)?;
                                    if k < rowid {
                                        lwr = idx + 1;
                                        if lwr > upr {
                                            c = -1;
                                            break;
                                        }
                                    } else if k > rowid {
                                        upr = idx - 1;
                                        if lwr > upr {
                                            c = 1;
                                            break;
                                        }
                                    } else {
                                        self.core.idx[d] = idx as usize;
                                        self.core.moved();
                                        return Ok(Some(0));
                                    }
                                    idx = (lwr + upr) >> 1;
                                }
                                self.core.idx[d] = idx as usize;
                                self.core.moved();
                                return Ok(Some(c));
                            }
                        }
                    }
                }
            }
        }
        if !self.core.move_to_root()? {
            return Ok(None);
        }
        loop {
            let d = self.core.depth();
            let page = self.core.pages[d].clone();
            let mut lwr: isize = 0;
            let mut upr: isize = page.n_cell as isize - 1;
            let mut idx: isize = upr >> 1;
            let c: i32;
            loop {
                if !noop {
                    self.core.pager.meter().charge_steps(1)?;
                }
                let k = page.rowid_at(idx as usize)?;
                if k < rowid {
                    lwr = idx + 1;
                    if lwr > upr {
                        c = -1;
                        break;
                    }
                } else if k > rowid {
                    upr = idx - 1;
                    if lwr > upr {
                        c = 1;
                        break;
                    }
                } else {
                    self.core.idx[d] = idx as usize;
                    if !page.is_leaf {
                        lwr = idx;
                        c = 0;
                        break;
                    }
                    self.core.moved();
                    return Ok(Some(0));
                }
                idx = (lwr + upr) >> 1;
            }
            if page.is_leaf {
                self.core.idx[d] = idx as usize;
                self.core.moved();
                return Ok(Some(c));
            }
            self.core.idx[d] = lwr as usize;
            let child = page.child_at(lwr as usize)?;
            self.core.move_to_child(child)?;
        }
    }
}

/// Cursor over an index B-tree (indexes and `WITHOUT ROWID` tables): entries in key order.
#[derive(Debug)]
pub struct IndexCursor {
    core: CursorCore,
    probe: Option<RecordView>,
}

impl IndexCursor {
    pub fn new(pager: Pager, root_page: u32) -> SqliteResult<Self> {
        Ok(Self {
            core: CursorCore::new(pager, root_page, false)?,
            probe: None,
        })
    }

    #[inline]
    pub fn is_valid(&self) -> bool {
        self.core.is_valid()
    }

    #[inline]
    pub fn page_number(&self) -> u32 {
        self.core.page_number()
    }

    #[inline]
    pub fn cell_index(&self) -> Option<usize> {
        self.core.cell_index()
    }

    pub fn first(&mut self) -> SqliteResult<bool> {
        self.core.first()
    }

    pub fn last(&mut self) -> SqliteResult<bool> {
        self.core.last()
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> SqliteResult<bool> {
        self.core.next()
    }

    pub fn prev(&mut self) -> SqliteResult<bool> {
        self.core.prev()
    }

    pub fn count(&mut self) -> SqliteResult<u64> {
        self.core.count()
    }

    pub fn payload_size(&mut self) -> SqliteResult<u32> {
        Ok(self.core.current_cell()?.payload_size)
    }

    pub fn record(&mut self) -> SqliteResult<&mut RecordView> {
        self.core.record()
    }

    /// `sqlite3BtreeIndexMoveto` (`btree.c:6068`).
    pub fn move_to_key(&mut self, key: &mut dyn RecordKey) -> SqliteResult<Option<i32>> {
        let res = self.move_to_key_inner(key);
        if res.is_err() {
            self.core.invalidate();
        }
        res
    }

    fn move_to_key_inner(&mut self, key: &mut dyn RecordKey) -> SqliteResult<Option<i32>> {
        if !self.core.move_to_root()? {
            return Ok(None);
        }
        let usable = self.core.pager.db().usable_size;
        let page_count = self.core.pager.page_count();
        key.clear_corrupt();
        loop {
            let d = self.core.depth();
            let page = self.core.pages[d].clone();
            let data = &page.data;
            let end = page.page_size;
            let max_local = page.max_local as usize;
            let max1 = max_local.min(127);
            let ptr = page.child_ptr_size;
            let mut lwr: isize = 0;
            let mut upr: isize = page.n_cell as isize - 1;
            let mut idx: isize = upr >> 1;
            let mut c: i32;
            loop {
                if !self.core.noop_meter {
                    self.core.pager.meter().charge_steps(1)?;
                }
                let pc = page.cell_offset(idx as usize) + ptr;
                if pc >= end {
                    return Err(SqliteError::corrupt(format!(
                        "page {}: cell {idx} runs past the end of the page",
                        page.pgno
                    )));
                }
                let n0 = usize::from(data[pc]);
                if n0 <= max1 {
                    if pc + n0 >= end {
                        return Err(SqliteError::corrupt(format!(
                            "page {}: cell {idx} runs past the end of the page",
                            page.pgno
                        )));
                    }
                    c = key.compare_record(data, pc + 1, n0);
                } else if pc + 1 < end
                    && (data[pc + 1] & 0x80) == 0
                    && (((n0 & 0x7f) << 7) + usize::from(data[pc + 1])) <= max_local
                    && pc + 2 + (((n0 & 0x7f) << 7) + usize::from(data[pc + 1])) <= end
                {
                    let n = ((n0 & 0x7f) << 7) + usize::from(data[pc + 1]);
                    c = key.compare_record(data, pc + 2, n);
                } else {
                    let cell = page.parse_cell(idx as usize)?;
                    self.core.cell_valid = false;
                    let n_cell = cell.payload_size;
                    if n_cell < 2 || (n_cell / usable) > page_count {
                        return Err(SqliteError::corrupt(format!(
                            "page {}: cell {idx} has an invalid key size {n_cell}",
                            page.pgno
                        )));
                    }
                    let pager = self.core.pager.clone();
                    let enc = pager.db().text_encoding;
                    let probe = self
                        .probe
                        .get_or_insert_with(|| RecordView::new(pager, enc));
                    probe.reset(
                        page.pgno,
                        page.data.clone(),
                        cell.payload_offset,
                        cell.local_size,
                        n_cell,
                    );
                    let (buf, start) = probe.payload_for_compare(COMPARE_PAD, false, true)?;
                    c = key.compare_record(&buf, start, n_cell as usize);
                }
                if c < 0 {
                    lwr = idx + 1;
                } else if c > 0 {
                    upr = idx - 1;
                } else {
                    if key.is_corrupt() {
                        return Err(SqliteError::corrupt(format!(
                            "page {}: cell {idx} holds a corrupt index record",
                            page.pgno
                        )));
                    }
                    self.core.idx[d] = idx as usize;
                    self.core.moved();
                    return Ok(Some(0));
                }
                if lwr > upr {
                    break;
                }
                idx = (lwr + upr) >> 1;
            }
            if page.is_leaf {
                self.core.idx[d] = idx as usize;
                self.core.moved();
                return Ok(Some(c));
            }
            self.core.idx[d] = lwr as usize;
            let child = page.child_at(lwr as usize)?;
            self.core.move_to_child(child)?;
        }
    }

    /// `OP_SeekLT / LE / GE / GT` on an index cursor.
    pub fn seek_key(&mut self, key: &mut dyn RecordKey, op: SeekOp) -> SqliteResult<bool> {
        key.set_default_rc(if matches!(op, SeekOp::Gt | SeekOp::Le) {
            -1
        } else {
            1
        });
        let Some(res) = self.move_to_key(key)? else {
            return Ok(false);
        };
        if op >= SeekOp::Ge {
            if res < 0 || (res == 0 && op == SeekOp::Gt) {
                return self.next();
            }
            return Ok(true);
        }
        if res > 0 || (res == 0 && op == SeekOp::Lt) {
            return self.prev();
        }
        Ok(self.is_valid())
    }
}
