//! Byte-level views of the database file after optional in-memory hot-journal rollback.

use std::collections::HashMap;
use std::fmt;

use xdb_core::{IoResult, VfsFile};

use super::journal::JournalPlayback;

/// A read-only byte image of a database file (before WAL overlay).
pub trait ByteImage: fmt::Debug + Send {
    fn size(&self) -> u64;
    fn read(&mut self, buf: &mut [u8], offset: u64) -> IoResult<()>;
    fn close(&mut self) -> IoResult<()>;
}

/// In-memory byte buffer image (for single-image synchronous opens without a VFS).
#[derive(Debug)]
pub struct MemImage {
    bytes: Vec<u8>,
}

impl MemImage {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

impl ByteImage for MemImage {
    fn size(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read(&mut self, buf: &mut [u8], offset: u64) -> IoResult<()> {
        let off = usize::try_from(offset).unwrap_or(usize::MAX);
        if off >= self.bytes.len() {
            buf.fill(0);
            return Ok(());
        }
        let avail = (self.bytes.len() - off).min(buf.len());
        buf[..avail].copy_from_slice(&self.bytes[off..off + avail]);
        if avail < buf.len() {
            buf[avail..].fill(0);
        }
        Ok(())
    }

    fn close(&mut self) -> IoResult<()> {
        Ok(())
    }
}

/// Main database file read straight through.
#[derive(Debug)]
pub struct FileImage {
    file: Box<dyn VfsFile>,
    size: u64,
}

impl FileImage {
    pub fn new(file: Box<dyn VfsFile>, size: u64) -> Self {
        Self { file, size }
    }

    pub fn unlock_and_close(&mut self) -> IoResult<()> {
        let _ = self.file.unlock(xdb_core::LockLevel::None);
        self.file.close()
    }
}

impl ByteImage for FileImage {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&mut self, buf: &mut [u8], offset: u64) -> IoResult<()> {
        let _ = self.file.read(buf, offset)?;
        Ok(())
    }

    fn close(&mut self) -> IoResult<()> {
        self.unlock_and_close()
    }
}

/// `base` with a hot rollback journal overlaid in memory (`pager_truncate` + restored pages).
#[derive(Debug)]
pub struct JournalImage {
    size: u64,
    base: Box<dyn ByteImage>,
    keep: u64,
    journal: Box<dyn VfsFile>,
    page_size: u64,
    records: HashMap<u32, u64>,
}

impl JournalImage {
    pub fn new(
        base: Box<dyn ByteImage>,
        journal: Box<dyn VfsFile>,
        playback: &JournalPlayback,
    ) -> Self {
        let ps = u64::from(playback.page_size);
        let target = u64::from(playback.original_page_count) * ps;
        let cur = base.size();
        let mut size = if cur > target || cur + ps <= target {
            target
        } else {
            cur
        };
        for &pgno in playback.records.keys() {
            size = size.max(u64::from(pgno) * ps);
        }
        Self {
            size,
            base,
            keep: cur.min(target),
            journal,
            page_size: ps,
            records: playback.records.clone(),
        }
    }
}

impl ByteImage for JournalImage {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&mut self, buf: &mut [u8], offset: u64) -> IoResult<()> {
        let len = buf.len() as u64;
        if len == 0 {
            return Ok(());
        }
        let ps = self.page_size;
        let first = (offset / ps) + 1;
        let last = ((offset + len - 1) / ps) + 1;
        if first == last {
            if let Ok(pg) = u32::try_from(first) {
                if let Some(&data_off) = self.records.get(&pg) {
                    let _ = self
                        .journal
                        .read(buf, data_off + (offset - (first - 1) * ps))?;
                    return Ok(());
                }
            }
        }
        let keep = self.keep;
        if offset >= keep {
            buf.fill(0);
        } else if offset + len <= keep {
            self.base.read(buf, offset)?;
        } else {
            let head = (keep - offset) as usize;
            self.base.read(&mut buf[..head], offset)?;
            buf[head..].fill(0);
        }
        for pg_u64 in first..=last {
            let Ok(pg) = u32::try_from(pg_u64) else {
                break;
            };
            let Some(&data_off) = self.records.get(&pg) else {
                continue;
            };
            let p_start = (pg_u64 - 1) * ps;
            let from = offset.max(p_start);
            let to = (offset + len).min(p_start + ps);
            let dst_start = (from - offset) as usize;
            let dst_end = (to - offset) as usize;
            let _ = self
                .journal
                .read(&mut buf[dst_start..dst_end], data_off + (from - p_start))?;
        }
        Ok(())
    }

    fn close(&mut self) -> IoResult<()> {
        let r1 = self.journal.close();
        let r2 = self.base.close();
        r1.and(r2)
    }
}
