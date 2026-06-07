//! sv39 virtual memory for riscv64.
//!
//! Milestone 3 brings up paging with the simplest correct scheme: a single
//! root table of 1 GiB leaf "gigapages" that identity-maps the low physical
//! address space (devices + RAM). Because virtual == physical, the program
//! counter, stack, and page-table memory all stay valid across the `satp`
//! switch, so execution continues uninterrupted once translation is live.
//!
//! This deliberately avoids needing a frame allocator yet: the root table is a
//! single 4 KiB-aligned static. A real multi-level walker with per-section
//! permissions and a frame allocator lands alongside the memory-management
//! pass (DTB intake); this milestone proves the hardware translation path.
//!
//! sv39 layout: 39-bit VA = VPN[2](9) | VPN[1](9) | VPN[0](9) | offset(12).
//! A leaf PTE at the root level maps a 1 GiB-aligned gigapage.

use core::arch::asm;
use core::ptr::{addr_of, addr_of_mut};

/// PTE valid bit.
const PTE_V: u64 = 1 << 0;
/// PTE readable.
const PTE_R: u64 = 1 << 1;
/// PTE writable.
const PTE_W: u64 = 1 << 2;
/// PTE executable.
const PTE_X: u64 = 1 << 3;
/// PTE accessed (set ahead of time so we don't need the A/D-update fault path).
const PTE_A: u64 = 1 << 6;
/// PTE dirty (ditto).
const PTE_D: u64 = 1 << 7;

/// `satp` MODE field value selecting sv39 (mode = 8), shifted into place.
const SATP_MODE_SV39: u64 = 8 << 60;

/// Number of entries in a page table (4 KiB / 8 bytes).
const ENTRIES: usize = 512;
/// Bytes mapped by one root-level (gigapage) entry: 1 GiB.
const GIGAPAGE: u64 = 1 << 30;
/// How many low gigapages to identity-map (0..4 GiB covers QEMU virt's device
/// space and the RAM the kernel and firmware live in).
const MAPPED_GIGAPAGES: usize = 4;
/// Gigapage index that holds RAM on QEMU virt (`0x8000_0000` = 2 GiB).
const RAM_GIGAPAGE: usize = 2;

/// A hardware page table: 512 64-bit PTEs, 4 KiB and 4 KiB-aligned.
#[repr(C, align(4096))]
struct PageTable {
    entries: [u64; ENTRIES],
}

/// The single root page table backing the identity map.
static mut ROOT: PageTable = PageTable {
    entries: [0; ENTRIES],
};

/// Builds a leaf PTE mapping the 1 GiB-aligned physical address `pa`.
const fn gigapage_pte(pa: u64, flags: u64) -> u64 {
    // PPN occupies PTE bits [53:10]; for a 1 GiB-aligned PA the low gigapage
    // PPN bits are zero, satisfying the superpage alignment rule.
    ((pa >> 12) << 10) | flags | PTE_V | PTE_A | PTE_D
}

/// Installs an sv39 identity map over the low physical address space and
/// switches `satp` to it. After this returns, the hart runs translated.
pub fn enable_identity_map() {
    // SAFETY: single hart, single-threaded early boot; this is the only writer
    // of ROOT and it runs once before any concurrency exists.
    let root = unsafe { &mut *addr_of_mut!(ROOT) };

    for gib in 0..MAPPED_GIGAPAGES {
        let pa = (gib as u64) * GIGAPAGE;
        // RAM is executable (kernel text + stacks live here); device space is
        // read/write but never executed.
        let flags = if gib == RAM_GIGAPAGE {
            PTE_R | PTE_W | PTE_X
        } else {
            PTE_R | PTE_W
        };
        root.entries[gib] = gigapage_pte(pa, flags);
    }

    // Pre-paging, the static's link address is its physical address (no
    // relocation), so this is the root table's PA for the satp PPN field.
    let root_pa = addr_of!(ROOT) as u64;
    let satp = SATP_MODE_SV39 | (root_pa >> 12);

    // SAFETY: enabling sv39 with an identity map keeps PC/SP/data addresses
    // valid across the switch; the trailing sfence.vma flushes stale TLB state.
    unsafe {
        asm!(
            "csrw satp, {satp}",
            "sfence.vma",
            satp = in(reg) satp,
            options(nostack),
        );
    }
}

/// Reads the current `satp` value (mode + ASID + root PPN).
#[must_use]
pub fn read_satp() -> usize {
    let value: usize;
    // SAFETY: reading a CSR has no side effects.
    unsafe {
        asm!("csrr {0}, satp", out(reg) value, options(nomem, nostack));
    }
    value
}

/// Flushes the entire local TLB.
pub fn flush_tlb_all() {
    // SAFETY: sfence.vma with no operands flushes all address translations.
    unsafe {
        asm!("sfence.vma", options(nostack));
    }
}

/// Flushes the TLB entry for a single virtual address on the current hart.
///
/// Unused at this stage (the identity map is installed once), but this is the
/// riscv64 primitive behind `arch::invalidate_page` for the memory-management
/// pass.
#[allow(dead_code)]
pub fn flush_tlb_page(vaddr: usize) {
    // SAFETY: sfence.vma rs1=addr, rs2=zero flushes the leaf for that address.
    unsafe {
        asm!("sfence.vma {0}, zero", in(reg) vaddr, options(nostack));
    }
}
