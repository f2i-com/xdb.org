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
        Ok(Self {
            pager,
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
        let page = self.current_page().clone();
        let enc = self.pager.db().text_encoding;
        let pager = self.pager.clone();
        let rec = self
            .record
            .get_or_insert_with(|| RecordView::new(pager, enc));
        rec.reset(
            page.pgno,
            page.data.clone(),
            c.payload_offset,
            c.local_size,
            c.payload_size,
        );
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
        let root = self
            .pager
            .get_btree_page(self.root_page, &format!("root page of b-tree {}", self.root_page))?;
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
        self.pager.meter().charge_steps(1)?;
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
        let parent_pgno = self.current_page().pgno;
        let child = self
            .pager
            .get_btree_page(pgno, &format!("child of page {parent_pgno}"))?;
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
        self.pager.meter().charge_steps(1)?;
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
        self.pager.meter().charge_steps(1)?;
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
        loop {
            n += 1;
            if !self.next()? {
                return Ok(n);
            }
        }
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
                self.core.pager.meter().charge_steps(1)?;
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
                self.core.pager.meter().charge_steps(1)?;
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
