//! Hot rollback-journal detection and in-memory playback (`pager.c:1298-3061`).

use std::collections::HashMap;

use xdb_core::{IoResult, VfsFile};

use crate::format::lock_byte_page;

/// `aJournalMagic` (`pager.c:757`).
pub const JOURNAL_MAGIC: &[u8; 8] = &[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];
const MAX_SECTOR_SIZE: u32 = 0x10000;
const SQLITE_MAX_PAGE_SIZE: u32 = 65536;
/// Reader's default sector size under `SQLITE_POWERSAFE_OVERWRITE = 1`.
pub const READER_SECTOR_SIZE: u32 = 512;
pub const MAX_SEGMENT_DETAILS: usize = 4096;

/// `pager_cksum` (`pager.c:2289`): initializer plus every 200th byte from `page_size - 200` down.
pub fn journal_checksum(cksum_init: u32, data: &[u8], offset: usize, page_size: usize) -> u32 {
    let mut c = cksum_init;
    let mut i = page_size as isize - 200;
    while i > 0 {
        c = c.wrapping_add(u32::from(data[offset + i as usize]));
        i -= 200;
    }
    c
}

#[inline]
fn u32_be(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// `pagerIsSuperJrnlName` (`pager.c:1298`): ends in `"-mj"` + 9 hex digits with `'9'` at 7th.
pub fn is_super_journal_name(name: &[u8]) -> bool {
    let n = name.len();
    if n < 12 {
        return false;
    }
    if &name[n - 12..n - 9] != b"-mj" || name[n - 3] != b'9' {
        return false;
    }
    name[n - 9..].iter().all(u8::is_ascii_hexdigit)
}

/// `readSuperJournal` (`pager.c:1338`).
pub fn read_super_journal(
    file: &mut dyn VfsFile,
    journal_size: u64,
    max_pathname: usize,
) -> IoResult<Option<Vec<u8>>> {
    let n_super = 1u64 + max_pathname as u64;
    if journal_size < 16 {
        return Ok(None);
    }
    let mut tail = [0u8; 16];
    if file.read(&mut tail, journal_size - 16)? < 16 {
        return Ok(None);
    }
    let len = u64::from(u32_be(&tail, 0));
    if len >= n_super || len > journal_size - 16 || len == 0 {
        return Ok(None);
    }
    let mut cksum = u32_be(&tail, 4);
    let mut name = vec![0u8; len as usize];
    if file.read(&mut name, journal_size - 16 - len)? < len as usize {
        return Ok(None);
    }
    for &b in &name {
        // Signed C `char` on x86/x64 (matches SQLite builds verified by the oracle).
        cksum = cksum.wrapping_sub(i32::from(b as i8) as u32);
    }
    let c_len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    let c_name = &name[..c_len];
    if &tail[8..16] != JOURNAL_MAGIC || !is_super_journal_name(c_name) || cksum != 0 {
        return Ok(None);
    }
    Ok(Some(c_name.to_vec()))
}

/// Why journal playback stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JournalStopReason {
    NoMoreHeaders,
    BadMagic,
    InvalidHeaderSizes,
    InvalidPageNumber,
    ChecksumMismatch,
    TruncatedRecord,
}

/// One journal header and the records read after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalSegment {
    pub offset: u64,
    pub n_rec_field: u32,
    pub n_rec: u32,
    pub db_size_field: u32,
    pub records: u32,
    pub applied: u32,
    pub skipped: u32,
}

/// Recorded outcome of `pager_playback` without mutating any files.
#[derive(Debug, Clone)]
pub struct JournalPlayback {
    pub played: bool,
    pub page_size: u32,
    pub sector_size: u32,
    pub original_page_count: u32,
    pub segments: Vec<JournalSegment>,
    pub segment_count: usize,
    pub records_played: u32,
    pub records_applied: u32,
    pub records_skipped: u32,
    pub stop_reason: JournalStopReason,
    /// Page number (1-based) -> byte offset in the journal of the last restored page image.
    pub records: HashMap<u32, u64>,
}

/// `pager_playback` (`pager.c:2872-3061`) for a hot journal (`isHot = 1`), recorded in memory.
pub fn playback_journal(
    file: &mut dyn VfsFile,
    journal_size: u64,
    pager_page_size: u32,
) -> IoResult<JournalPlayback> {
    let sz_j = journal_size;
    let mut segments = Vec::new();
    let mut records = HashMap::new();
    let mut sector = READER_SECTOR_SIZE as u64;
    let mut page_size = pager_page_size;
    let mut lck_pgno = lock_byte_page(page_size);
    let mut played = false;
    let mut header_sector: u32 = 0;
    let mut header_page_size: u32 = 0;
    let mut db_size: u32 = 0;
    let mut segment_count: usize = 0;
    let mut records_applied: u32 = 0;
    let mut records_skipped: u32 = 0;
    let stop_reason;
    let mut journal_off: u64 = 0;
    let mut hdr = [0u8; 28];
    let mut rec: Vec<u8> = Vec::new();

    loop {
        let hdr_off = if journal_off == 0 {
            0
        } else {
            (((journal_off - 1) / sector) + 1) * sector
        };
        journal_off = hdr_off;
        // On the very first header, `sector` is not known until we read the 28-byte header
        // (SQLite allows `i_sector_size` down to 32 bytes, smaller than `READER_SECTOR_SIZE`).
        let min_hdr_bytes = if journal_off == 0 { 28 } else { sector };
        if hdr_off.saturating_add(min_hdr_bytes) > sz_j {
            stop_reason = JournalStopReason::NoMoreHeaders;
            break;
        }
        file.read(&mut hdr, hdr_off)?;
        if &hdr[..8] != JOURNAL_MAGIC {
            stop_reason = JournalStopReason::BadMagic;
            break;
        }
        let mut n_rec = u32_be(&hdr, 8);
        let n_rec_field = n_rec;
        let cksum_init = u32_be(&hdr, 12);
        let mx_pg = u32_be(&hdr, 16);

        if journal_off == 0 {
            let i_sector_size = u32_be(&hdr, 20);
            let mut i_page_size = u32_be(&hdr, 24);
            if i_page_size == 0 {
                i_page_size = pager_page_size;
            }
            if i_page_size < 512
                || i_sector_size < 32
                || i_page_size > SQLITE_MAX_PAGE_SIZE
                || i_sector_size > MAX_SECTOR_SIZE
                || (i_page_size & (i_page_size - 1)) != 0
                || (i_sector_size & (i_sector_size - 1)) != 0
            {
                stop_reason = JournalStopReason::InvalidHeaderSizes;
                break;
            }
            page_size = i_page_size;
            lck_pgno = lock_byte_page(page_size);
            sector = u64::from(i_sector_size);
            if hdr_off.saturating_add(sector) > sz_j {
                stop_reason = JournalStopReason::NoMoreHeaders;
                break;
            }
            header_page_size = i_page_size;
            header_sector = i_sector_size;
            rec = vec![0u8; page_size as usize + 8];
        }
        journal_off += sector;

        if n_rec == 0xffff_ffff {
            let rem = sz_j.saturating_sub(journal_off) / (u64::from(page_size) + 8);
            n_rec = u32::try_from(rem).unwrap_or(u32::MAX);
        }
        if journal_off == sector {
            played = true;
            db_size = mx_pg;
        }

        let ps = page_size as usize;
        let mut applied: u32 = 0;
        let mut skipped: u32 = 0;
        let mut stop: Option<JournalStopReason> = None;

        for _ in 0..n_rec {
            let rec_off = journal_off;
            let n = if rec_off >= sz_j {
                0
            } else {
                file.read(&mut rec, rec_off)?
            };
            if n < 4 + ps {
                stop = Some(JournalStopReason::TruncatedRecord);
                break;
            }
            let pgno = u32_be(&rec, 0);
            journal_off = rec_off + u64::from(page_size) + 8;
            if pgno == 0 || pgno == lck_pgno {
                stop = Some(JournalStopReason::InvalidPageNumber);
                break;
            }
            if pgno > db_size {
                skipped += 1;
                continue;
            }
            if n < ps + 8 {
                stop = Some(JournalStopReason::TruncatedRecord);
                break;
            }
            if journal_checksum(cksum_init, &rec, 4, ps) != u32_be(&rec, 4 + ps) {
                stop = Some(JournalStopReason::ChecksumMismatch);
                break;
            }
            records.insert(pgno, rec_off + 4);
            applied += 1;
        }

        segment_count += 1;
        records_applied += applied;
        records_skipped += skipped;
        if segments.len() < MAX_SEGMENT_DETAILS {
            segments.push(JournalSegment {
                offset: hdr_off,
                n_rec_field,
                n_rec,
                db_size_field: mx_pg,
                records: applied + skipped,
                applied,
                skipped,
            });
        }
        if let Some(reason) = stop {
            stop_reason = reason;
            break;
        }
    }

    Ok(JournalPlayback {
        played,
        page_size: header_page_size,
        sector_size: header_sector,
        original_page_count: db_size,
        segments,
        segment_count,
        records_played: records_applied + records_skipped,
        records_applied,
        records_skipped,
        stop_reason,
        records,
    })
}
