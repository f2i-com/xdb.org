//! In-heap WAL recovery for read-only snapshots (stage S1; `wal.c:888-1638`).

use std::collections::HashMap;

use xdb_core::{IoError, IoErrorCode, IoResult, VfsFile};

pub const WAL_HEADER_SIZE: u64 = 32;
pub const WAL_FRAME_HEADER_SIZE: u64 = 24;
/// `wal.c:491`. Low bit selects big-endian checksum words.
pub const WAL_MAGIC: u32 = 0x377f_0682;
/// `wal.c:277`.
pub const WAL_MAX_VERSION: u32 = 3_007_000;
const SQLITE_MAX_PAGE_SIZE: u32 = 65536;

/// Byte offset of frame `i_frame` (1-based) for `page_size` (`walFrameOffset`, `wal.c:498`).
#[inline]
pub const fn wal_frame_offset(i_frame: u32, page_size: u32) -> u64 {
    WAL_HEADER_SIZE + (i_frame as u64 - 1) * (page_size as u64 + WAL_FRAME_HEADER_SIZE)
}

/// `walChecksumBytes` (`wal.c:888`): extend running checksum `[s1, s2]` over `bytes`
/// (length must be a positive multiple of 8).
pub fn wal_checksum_bytes(bytes: &[u8], big_endian: bool, s: &mut [u32; 2]) {
    debug_assert!(!bytes.is_empty() && (bytes.len() & 7) == 0);
    let mut s1 = s[0];
    let mut s2 = s[1];
    for chunk in bytes.chunks_exact(8) {
        let (w0, w1) = if big_endian {
            (
                u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                u32::from_be_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
            )
        } else {
            (
                u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
            )
        };
        s1 = s1.wrapping_add(w0).wrapping_add(s2);
        s2 = s2.wrapping_add(w1).wrapping_add(s1);
    }
    s[0] = s1;
    s[1] = s2;
}

/// Why WAL recovery stopped reading frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WalStopReason {
    EndOfFile,
    NoValidHeader,
    SaltMismatch,
    ZeroPageNumber,
    ChecksumMismatch,
}

/// The 32-byte WAL header as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalHeader {
    pub magic: u32,
    pub big_endian_checksums: bool,
    pub version: u32,
    pub page_size: u32,
    pub checkpoint_sequence: u32,
    pub salt1: u32,
    pub salt2: u32,
    pub checksum1: u32,
    pub checksum2: u32,
}

/// Result of in-heap `walIndexRecover`.
#[derive(Debug, Clone)]
pub struct WalRecovery {
    pub file_size: u64,
    pub header: Option<WalHeader>,
    pub header_valid: bool,
    pub frame_slots: u32,
    pub valid_frames: u32,
    pub mx_frame: u32,
    pub commits: u32,
    pub n_page: u32,
    pub uncommitted_frames: u32,
    pub invalid_frames: u32,
    pub stale_salt_frames: u32,
    pub stop_reason: WalStopReason,
    pub pages_in_wal: usize,
    /// Page number (1-based) -> latest committed frame number (`1..=mx_frame`).
    pub frame_of: HashMap<u32, u32>,
}

#[inline]
fn u32_be(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn short_read(file: &dyn VfsFile, offset: u64) -> IoError {
    IoError::with_path(
        IoErrorCode::IoErrShortRead,
        format!(
            "WAL {} shrank during recovery (short read at offset {offset})",
            file.path()
        ),
        file.path(),
    )
}

fn empty_recovery(
    file_size: u64,
    header: Option<WalHeader>,
    stop_reason: WalStopReason,
) -> WalRecovery {
    WalRecovery {
        file_size,
        header,
        header_valid: false,
        frame_slots: 0,
        valid_frames: 0,
        mx_frame: 0,
        commits: 0,
        n_page: 0,
        uncommitted_frames: 0,
        invalid_frames: 0,
        stale_salt_frames: 0,
        stop_reason,
        pages_in_wal: 0,
        frame_of: HashMap::new(),
    }
}

/// Recover a WAL file into an in-heap `page -> frame` index (`wal.c:1422-1638`).
pub fn recover_wal(file: &mut dyn VfsFile, file_size: u64) -> IoResult<WalRecovery> {
    if file_size <= WAL_HEADER_SIZE {
        return Ok(empty_recovery(file_size, None, WalStopReason::NoValidHeader));
    }
    let mut hdr = [0u8; WAL_HEADER_SIZE as usize];
    if file.read(&mut hdr, 0)? < WAL_HEADER_SIZE as usize {
        return Err(short_read(file, 0));
    }
    let magic = u32_be(&hdr, 0);
    let page_size = u32_be(&hdr, 8);
    let header = WalHeader {
        magic,
        big_endian_checksums: (magic & 1) == 1,
        version: u32_be(&hdr, 4),
        page_size,
        checkpoint_sequence: u32_be(&hdr, 12),
        salt1: u32_be(&hdr, 16),
        salt2: u32_be(&hdr, 20),
        checksum1: u32_be(&hdr, 24),
        checksum2: u32_be(&hdr, 28),
    };

    if (magic & 0xffff_fffe) != WAL_MAGIC
        || (page_size & (page_size.wrapping_sub(1))) != 0
        || !(512..=SQLITE_MAX_PAGE_SIZE).contains(&page_size)
    {
        return Ok(empty_recovery(
            file_size,
            Some(header),
            WalStopReason::NoValidHeader,
        ));
    }

    let big_endian = header.big_endian_checksums;
    let mut cksum = [0u32; 2];
    wal_checksum_bytes(&hdr[..24], big_endian, &mut cksum);
    if cksum[0] != header.checksum1 || cksum[1] != header.checksum2 {
        return Ok(empty_recovery(
            file_size,
            Some(header),
            WalStopReason::NoValidHeader,
        ));
    }

    if header.version != WAL_MAX_VERSION {
        return Err(IoError::with_path(
            IoErrorCode::CantOpen,
            format!(
                "unable to open WAL {}: format version {} is not {} (SQLITE_CANTOPEN)",
                file.path(),
                header.version,
                WAL_MAX_VERSION
            ),
            file.path(),
        ));
    }

    let sz_frame = page_size as u64 + WAL_FRAME_HEADER_SIZE;
    let frame_slots = ((file_size - WAL_HEADER_SIZE) / sz_frame) as u32;
    let mut frame = vec![0u8; sz_frame as usize];
    let mut pgnos = Vec::with_capacity(frame_slots as usize);

    let mut valid_frames: u32 = 0;
    let mut mx_frame: u32 = 0;
    let mut n_page: u32 = 0;
    let mut commits: u32 = 0;
    let mut stop_reason = WalStopReason::EndOfFile;

    for i_frame in 1..=frame_slots {
        let off = wal_frame_offset(i_frame, page_size);
        if file.read(&mut frame, off)? < sz_frame as usize {
            return Err(short_read(file, off));
        }
        if u32_be(&frame, 8) != header.salt1 || u32_be(&frame, 12) != header.salt2 {
            stop_reason = WalStopReason::SaltMismatch;
            break;
        }
        let pgno = u32_be(&frame, 0);
        if pgno == 0 {
            stop_reason = WalStopReason::ZeroPageNumber;
            break;
        }
        wal_checksum_bytes(&frame[..8], big_endian, &mut cksum);
        wal_checksum_bytes(&frame[WAL_FRAME_HEADER_SIZE as usize..], big_endian, &mut cksum);
        if cksum[0] != u32_be(&frame, 16) || cksum[1] != u32_be(&frame, 20) {
            stop_reason = WalStopReason::ChecksumMismatch;
            break;
        }
        valid_frames = i_frame;
        pgnos.push(pgno);
        let n_truncate = u32_be(&frame, 4);
        if n_truncate != 0 {
            mx_frame = i_frame;
            n_page = n_truncate;
            commits += 1;
        }
    }

    let mut frame_of = HashMap::new();
    for (idx, &pgno) in pgnos.iter().take(mx_frame as usize).enumerate() {
        frame_of.insert(pgno, (idx + 1) as u32);
    }

    let mut stale_salt_frames: u32 = 0;
    if valid_frames < frame_slots {
        let mut salts = [0u8; 8];
        for i in (valid_frames + 1)..=frame_slots {
            if file.read(&mut salts, wal_frame_offset(i, page_size) + 8)? < 8 {
                break;
            }
            if u32_be(&salts, 0) != header.salt1 || u32_be(&salts, 4) != header.salt2 {
                stale_salt_frames += 1;
            }
        }
    }

    let pages_in_wal = frame_of.len();
    Ok(WalRecovery {
        file_size,
        header: Some(header),
        header_valid: true,
        frame_slots,
        valid_frames,
        mx_frame,
        commits,
        n_page,
        uncommitted_frames: valid_frames - mx_frame,
        invalid_frames: frame_slots - valid_frames,
        stale_salt_frames,
        stop_reason,
        pages_in_wal,
        frame_of,
    })
}
