//! Pointer-map page arithmetic for auto-vacuum databases (`btree.c:1063`, `btreeInt.h:612-668`).

use super::db_header::lock_byte_page;

/// Pointer-map entry types (`PTRMAP_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PtrmapType {
    RootPage = 1,
    FreePage = 2,
    Overflow1 = 3,
    Overflow2 = 4,
    Btree = 5,
}

impl PtrmapType {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(PtrmapType::RootPage),
            2 => Some(PtrmapType::FreePage),
            3 => Some(PtrmapType::Overflow1),
            4 => Some(PtrmapType::Overflow2),
            5 => Some(PtrmapType::Btree),
            _ => None,
        }
    }
}

/// `ptrmapPageno`: pointer-map page holding `pgno`'s entry (`0` for `pgno < 2`).
#[inline]
pub const fn ptrmap_page_for(pgno: u32, usable_size: u32, page_size: u32) -> u32 {
    if pgno < 2 {
        return 0;
    }
    let per_page = (usable_size / 5) + 1;
    let i_ptr_map = (pgno - 2) / per_page;
    let mut ret = i_ptr_map * per_page + 2;
    if ret == lock_byte_page(page_size) {
        ret += 1;
    }
    ret
}

/// `PTRMAP_ISPAGE`: whether `pgno` is itself a pointer-map page.
#[inline]
pub const fn is_ptrmap_page(pgno: u32, usable_size: u32, page_size: u32) -> bool {
    pgno >= 2 && ptrmap_page_for(pgno, usable_size, page_size) == pgno
}

/// `PTRMAP_PTROFFSET`: byte offset of `pgno`'s entry within `ptrmap_page` (`None` when invalid).
#[inline]
pub const fn ptrmap_entry_offset(ptrmap_page: u32, pgno: u32) -> Option<usize> {
    if pgno <= ptrmap_page {
        None
    } else {
        Some(5 * (pgno - ptrmap_page - 1) as usize)
    }
}
