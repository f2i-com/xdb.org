//! The contract between the snapshot layer (main file, WAL overlay, hot-journal overlay)
//! and the B-tree layer.

use std::fmt;

use crate::errors::{SqliteError, SqliteResult};

/// Read-only page-level view of a database snapshot.
///
/// The B-tree layer reads pages exclusively through `PageSource` and never touches
/// raw files, WAL frames, or rollback journals directly.
pub trait PageSource: fmt::Debug + Send {
    /// Page size in bytes (`512..=65536`), from the effective database header.
    fn page_size(&self) -> u32;

    /// Database size in pages for this snapshot (`0` for an empty database).
    fn page_count(&self) -> u32;

    /// Read page `pgno` (1-based) into a fresh `Vec<u8>` of length `page_size()`.
    fn read_page(&mut self, pgno: u32) -> SqliteResult<Vec<u8>>;

    /// Release any underlying files and locks. Idempotent.
    fn close(&mut self) -> SqliteResult<()> {
        Ok(())
    }
}

/// Simple in-memory [`PageSource`] over a flat byte buffer (useful for tests & single-file bytes).
#[derive(Debug, Clone)]
pub struct SlicePageSource {
    data: Vec<u8>,
    page_size: u32,
    page_count: u32,
}

impl SlicePageSource {
    pub fn new(data: Vec<u8>, page_size: u32, page_count: u32) -> Self {
        Self {
            data,
            page_size,
            page_count,
        }
    }
}

impl PageSource for SlicePageSource {
    fn page_size(&self) -> u32 {
        self.page_size
    }

    fn page_count(&self) -> u32 {
        self.page_count
    }

    fn read_page(&mut self, pgno: u32) -> SqliteResult<Vec<u8>> {
        if pgno < 1 || pgno > self.page_count {
            return Err(SqliteError::corrupt(format!(
                "page {pgno} is outside the database (1..={})",
                self.page_count
            )));
        }
        let ps = self.page_size as usize;
        let off = (pgno as usize - 1).saturating_mul(ps);
        let mut buf = vec![0u8; ps];
        if off < self.data.len() {
            let n = ps.min(self.data.len() - off);
            buf[..n].copy_from_slice(&self.data[off..off + n]);
        }
        Ok(buf)
    }
}
