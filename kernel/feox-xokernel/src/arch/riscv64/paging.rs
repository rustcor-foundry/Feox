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

use super::frame;
use super::frame::FRAME_SIZE;

unsafe extern "C" {
    static __kernel_start: u8;
    static __srodata: u8;
    static __sdata: u8;
    static __kernel_end: u8;
}

/// PTE valid bit.
const PTE_V: u64 = 1 << 0;
/// PTE readable (mapping permission; public for `AddressSpace::map` callers).
pub const PTE_R: u64 = 1 << 1;
/// PTE writable.
pub const PTE_W: u64 = 1 << 2;
/// PTE executable.
pub const PTE_X: u64 = 1 << 3;
/// PTE user-accessible (U-mode may access; S-mode only with sstatus.SUM).
pub const PTE_U: u64 = 1 << 4;
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
/// How many low gigapages to identity-map for the bootstrap map (0..4 GiB
/// covers the device space and the low RAM where the kernel loads on both QEMU
/// virt (RAM @ 2 GiB) and the JH7110 (RAM @ 1 GiB)).
const MAPPED_GIGAPAGES: usize = 4;

/// QEMU virt NS16550 UART base (mapped for the future native console).
const UART0_BASE: usize = 0x1000_0000;
/// Size of the low-MMIO window mapped on QEMU: UART (0x1000_0000) plus the
/// virtio-mmio transports (0x1000_1000..0x1000_9000). 64 KiB covers both.
const LOW_MMIO_SIZE: usize = 0x1_0000;

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
        // Bootstrap map: every gigapage R-W-X so the kernel runs wherever it was
        // loaded (RAM base differs per platform). This map is transient — the
        // fine-grained per-section W^X map in build_kernel_address_space
        // replaces it before the kernel does any real work.
        root.entries[gib] = gigapage_pte(pa, PTE_R | PTE_W | PTE_X);
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

/// 2 MiB superpage size (a leaf PTE at sv39 level 1).
const MEGAPAGE: usize = 2 * 1024 * 1024;

/// Returns the 9-bit page-table index for `va` at the given sv39 `level`
/// (0 = 4 KiB leaf level, 2 = root).
const fn vpn(va: usize, level: usize) -> usize {
    (va >> (12 + 9 * level)) & 0x1ff
}

/// Maps a single page of `va -> pa` with `flags`, creating intermediate tables
/// from the frame allocator as needed. `leaf_level` selects the page size
/// (0 = 4 KiB, 1 = 2 MiB superpage).
fn map_one(root: usize, va: usize, pa: usize, flags: u64, leaf_level: usize) {
    let mut table = root;
    let mut level = 2usize;
    while level > leaf_level {
        let entry = (table + vpn(va, level) * 8) as *mut u64;
        // SAFETY: `entry` points inside a live page-table frame (identity
        // mapped while this runs); we are the only writer during build.
        let pte = unsafe { *entry };
        let next = if pte & PTE_V == 0 {
            let frame = frame::alloc().expect("page-table frame allocation failed");
            zero_table(frame);
            // Non-leaf PTE: V set, R/W/X clear (points to the next table).
            unsafe { *entry = ((frame as u64 >> 12) << 10) | PTE_V };
            frame
        } else {
            (((pte >> 10) & ((1 << 44) - 1)) << 12) as usize
        };
        table = next;
        level -= 1;
    }
    let entry = (table + vpn(va, leaf_level) * 8) as *mut u64;
    // SAFETY: leaf slot inside a live page-table frame.
    unsafe { *entry = ((pa as u64 >> 12) << 10) | flags | PTE_V | PTE_A | PTE_D };
}

/// Maps `[va, va + size)` to `[pa, ...)` with `flags`, using 2 MiB superpages
/// where the addresses and remaining length allow and 4 KiB pages otherwise.
fn map_region(root: usize, va: usize, pa: usize, size: usize, flags: u64) {
    let mut done = 0usize;
    while done < size {
        let cva = va + done;
        let cpa = pa + done;
        let remaining = size - done;
        if cva % MEGAPAGE == 0 && cpa % MEGAPAGE == 0 && remaining >= MEGAPAGE {
            map_one(root, cva, cpa, flags, 1);
            done += MEGAPAGE;
        } else {
            map_one(root, cva, cpa, flags, 0);
            done += FRAME_SIZE;
        }
    }
}

/// Zeroes a freshly allocated page-table frame.
fn zero_table(frame: usize) {
    let table = frame as *mut u64;
    for i in 0..ENTRIES {
        // SAFETY: `frame` is a fresh, identity-mapped allocator frame.
        unsafe { *table.add(i) = 0 };
    }
}

/// Builds a fine-grained kernel address space from the frame allocator and
/// switches `satp` to it, replacing the bootstrap gigapage identity map.
///
/// The kernel image is mapped per section with W^X permissions (text R-X,
/// rodata R--, data/bss RW-); the frame pool and the DTB (read-only) are mapped
/// as well. When `map_devices` is set (QEMU virt), the fixed UART/PCIe device
/// windows are mapped too; on other platforms those addresses differ (and the
/// PCIe MMIO window even overlaps RAM), so they are left out and derived from
/// the device tree when those drivers are ported. Identity (VA == PA) is
/// preserved. Returns the physical address of the new root.
pub fn build_kernel_address_space(
    frame_pool_end: usize,
    dtb: usize,
    dtb_size: usize,
    map_devices: bool,
) -> usize {
    let root = frame::alloc().expect("root page-table allocation failed");
    zero_table(root);

    let kstart = addr_of!(__kernel_start) as usize;
    let rodata = addr_of!(__srodata) as usize;
    let data = addr_of!(__sdata) as usize;
    let kend = addr_of!(__kernel_end) as usize;

    // Kernel sections with per-section permissions.
    map_region(root, kstart, kstart, rodata - kstart, PTE_R | PTE_X);
    map_region(root, rodata, rodata, data - rodata, PTE_R);
    map_region(root, data, data, align_up(kend, FRAME_SIZE) - data, PTE_R | PTE_W);

    // Frame pool (allocatable RAM + the page tables themselves) as R/W.
    let pool_start = align_up(kend, FRAME_SIZE);
    if frame_pool_end > pool_start {
        map_region(root, pool_start, pool_start, frame_pool_end - pool_start, PTE_R | PTE_W);
    }

    // Keep the DTB readable (always — it's platform-independent).
    if dtb != 0 {
        let dtb_start = align_down(dtb, FRAME_SIZE);
        let dtb_end = align_up(dtb + dtb_size, FRAME_SIZE);
        map_region(root, dtb_start, dtb_start, dtb_end - dtb_start, PTE_R);
    }

    if map_devices {
        // QEMU virt fixed device windows: the low-MMIO block (UART at
        // 0x1000_0000 + the 8 virtio-mmio transports at 0x1000_1000..),
        // PCIe ECAM config space, and the PCIe MMIO window for BAR assignment.
        map_region(root, UART0_BASE, UART0_BASE, LOW_MMIO_SIZE, PTE_R | PTE_W);
        map_region(
            root,
            super::pci::ECAM_BASE,
            super::pci::ECAM_BASE,
            super::pci::ECAM_SIZE,
            PTE_R | PTE_W,
        );
        map_region(
            root,
            super::pci::MMIO_BASE,
            super::pci::MMIO_BASE,
            super::pci::MMIO_MAP_SIZE,
            PTE_R | PTE_W,
        );
    }

    let satp = SATP_MODE_SV39 | (root as u64 >> 12);
    // SAFETY: the new map covers PC (text), stack/data, the frame pool, and the
    // page tables; switching satp keeps execution valid, and sfence.vma flushes
    // the bootstrap-map TLB entries.
    unsafe {
        asm!(
            "csrw satp, {satp}",
            "sfence.vma",
            satp = in(reg) satp,
            options(nostack),
        );
    }
    root
}

/// Walks the active page table to translate `va`, returning the physical
/// address and the leaf PTE's low flag bits, or `None` if unmapped.
#[must_use]
pub fn translate(va: usize) -> Option<(usize, u64)> {
    let root = (read_satp() & ((1 << 44) - 1)) << 12;
    translate_in(root, va)
}

/// Walks `root`'s tables to translate `va`, without requiring `root` to be the
/// active table. Returns the physical address and the leaf PTE's low flag bits.
fn translate_in(root: usize, va: usize) -> Option<(usize, u64)> {
    let mut table = root;
    let mut level: i32 = 2;
    while level >= 0 {
        let entry = (table + vpn(va, level as usize) * 8) as *const u64;
        // SAFETY: walking live, identity-mapped page-table frames.
        let pte = unsafe { *entry };
        if pte & PTE_V == 0 {
            return None;
        }
        if pte & (PTE_R | PTE_W | PTE_X) != 0 {
            let level_bits = 12 + 9 * (level as usize);
            let mask = (1usize << level_bits) - 1;
            // Mask the PPN to 44 bits (matching the non-leaf path) so reserved
            // / PBMT / N bits above bit 53 never leak into the address.
            let ppn = ((pte >> 10) & ((1 << 44) - 1)) as usize;
            let pa = ((ppn << 12) & !mask) | (va & mask);
            return Some((pa, pte & 0x3ff));
        }
        table = (((pte >> 10) & ((1 << 44) - 1)) << 12) as usize;
        level -= 1;
    }
    None
}

/// An sv39 address space: a root page table you can map into, translate, and
/// activate. The bootstrap kernel map is built by `build_kernel_address_space`;
/// per-process U-mode spaces are built with this type.
///
/// `destroy()` frees the page-table frames explicitly; there is intentionally no
/// `Drop` (so an active space is never freed out from under the hart).
pub struct AddressSpace {
    root: usize,
}

impl AddressSpace {
    /// Creates an empty address space backed by a fresh, zeroed root table.
    #[must_use]
    pub fn new() -> Option<Self> {
        let root = frame::alloc()?;
        zero_table(root);
        Some(Self { root })
    }

    /// Wraps the currently active address space (the `satp` root) so the kernel
    /// map can be modified through the same API. Do NOT `destroy()` the result —
    /// it borrows the live kernel tables.
    #[must_use]
    pub fn from_active() -> Self {
        Self {
            root: (read_satp() & ((1 << 44) - 1)) << 12,
        }
    }

    /// Creates a user address space whose root copies the active (kernel)
    /// root's top-level entries, so kernel text/data, the trap vector and
    /// stack, and the frame pool stay mapped under this space's `satp`.
    /// Mappings made afterwards in root slots that were *empty* at clone time
    /// allocate private table trees — that is what gives per-process isolation
    /// (use VA windows the kernel never touches). Must be torn down with
    /// [`Self::destroy_user`], not `destroy` (the shared subtrees belong to
    /// the kernel). Top-level entries the kernel adds later are not seen by
    /// already-cloned spaces.
    #[must_use]
    pub fn new_user() -> Option<Self> {
        let kernel_root = (read_satp() & ((1 << 44) - 1)) << 12;
        let root = frame::alloc()?;
        for i in 0..ENTRIES {
            // SAFETY: both roots are live, identity-mapped table frames; the
            // fresh root is exclusively ours.
            unsafe {
                *((root + i * 8) as *mut u64) = *((kernel_root + i * 8) as *const u64);
            }
        }
        Some(Self { root })
    }

    /// Tears down a space created by [`Self::new_user`]: frees only the table
    /// trees hanging off root slots that differ from the live kernel root
    /// (the space's private mappings), then the root itself. Backing leaf
    /// pages are NOT freed — the mapper owns them. Must be called while the
    /// kernel address space is active (so the comparison baseline is the same
    /// root that was cloned). Returns the number of table frames freed.
    pub fn destroy_user(self) -> usize {
        let kernel_root = (read_satp() & ((1 << 44) - 1)) << 12;
        debug_assert_ne!(kernel_root, self.root, "cannot destroy the active space");
        let mut freed = 0usize;
        for i in 0..ENTRIES {
            // SAFETY: reading PTE slots in live, identity-mapped table frames.
            let mine = unsafe { *((self.root + i * 8) as *const u64) };
            let shared = unsafe { *((kernel_root + i * 8) as *const u64) };
            if mine != shared && mine & PTE_V != 0 && mine & (PTE_R | PTE_W | PTE_X) == 0 {
                let child = (((mine >> 10) & ((1 << 44) - 1)) << 12) as usize;
                freed += free_table_tree(child, 1);
            }
        }
        frame::free(self.root);
        freed + 1
    }

    /// Physical address of the root page table.
    #[must_use]
    pub fn root(&self) -> usize {
        self.root
    }

    /// The `satp` value selecting this address space (sv39 mode).
    #[must_use]
    pub fn satp(&self) -> u64 {
        SATP_MODE_SV39 | (self.root as u64 >> 12)
    }

    /// Maps `[va, va+size)` -> `[pa, ...)` with `flags`, allocating intermediate
    /// tables as needed.
    pub fn map(&mut self, va: usize, pa: usize, size: usize, flags: u64) {
        map_region(self.root, va, pa, size, flags);
    }

    /// Unmaps `[va, va+size)` by clearing leaf PTEs and flushing the TLB. The
    /// backing physical pages and intermediate tables are left intact (the
    /// caller owns the pages; `destroy` reclaims the tables).
    pub fn unmap(&mut self, va: usize, size: usize) {
        let mut done = 0usize;
        while done < size {
            let cva = va + done;
            if let Some((entry, level)) = self.leaf_entry(cva) {
                // SAFETY: `entry` is a live leaf PTE slot in this space's tables.
                unsafe { *entry = 0 };
                flush_tlb_page(cva);
                done += 1usize << (12 + 9 * level);
            } else {
                done += FRAME_SIZE;
            }
        }
    }

    /// Translates `va` within this space without activating it.
    #[must_use]
    pub fn translate(&self, va: usize) -> Option<(usize, u64)> {
        translate_in(self.root, va)
    }

    /// Switches the current hart to this address space.
    ///
    /// # Safety
    /// This space must map everything the current execution path needs (PC,
    /// stack, the trap vector, and any data touched before the next switch).
    pub unsafe fn activate(&self) {
        let satp = self.satp();
        // SAFETY: caller guarantees the space maps the live execution context.
        unsafe {
            asm!("csrw satp, {0}", "sfence.vma", in(reg) satp, options(nostack));
        }
    }

    /// Frees the page-table frames (root + intermediates) and returns the count
    /// freed. Backing leaf pages are NOT freed — the mapper owns them.
    pub fn destroy(self) -> usize {
        free_table_tree(self.root, 2)
    }

    /// Returns the leaf PTE slot and its level for `va`, if mapped.
    fn leaf_entry(&self, va: usize) -> Option<(*mut u64, usize)> {
        let mut table = self.root;
        let mut level: i32 = 2;
        while level >= 0 {
            let entry = (table + vpn(va, level as usize) * 8) as *mut u64;
            // SAFETY: walking this space's live page-table frames.
            let pte = unsafe { *entry };
            if pte & PTE_V == 0 {
                return None;
            }
            if pte & (PTE_R | PTE_W | PTE_X) != 0 {
                return Some((entry, level as usize));
            }
            table = (((pte >> 10) & ((1 << 44) - 1)) << 12) as usize;
            level -= 1;
        }
        None
    }
}

/// Recursively frees the intermediate page tables under `table` (a node at
/// sv39 `level`) and `table` itself. Leaf entries' backing pages are not freed.
fn free_table_tree(table: usize, level: usize) -> usize {
    let mut freed = 0usize;
    if level > 0 {
        for i in 0..ENTRIES {
            // SAFETY: reading a PTE slot in a live table frame.
            let pte = unsafe { *((table + i * 8) as *const u64) };
            if pte & PTE_V != 0 && pte & (PTE_R | PTE_W | PTE_X) == 0 {
                let child = (((pte >> 10) & ((1 << 44) - 1)) << 12) as usize;
                freed += free_table_tree(child, level - 1);
            }
        }
    }
    frame::free(table);
    freed + 1
}

/// Decodes a PTE's R/W/X permission bits.
#[must_use]
pub fn decode_rwx(flags: u64) -> (bool, bool, bool) {
    (flags & PTE_R != 0, flags & PTE_W != 0, flags & PTE_X != 0)
}

/// Rounds `x` up to a multiple of `align` (a power of two).
const fn align_up(x: usize, align: usize) -> usize {
    (x + align - 1) & !(align - 1)
}

/// Rounds `x` down to a multiple of `align` (a power of two).
const fn align_down(x: usize, align: usize) -> usize {
    x & !(align - 1)
}

/// Switches `satp` to `value` and flushes the TLB.
///
/// # Safety
/// The new root must map everything the current execution path needs (PC,
/// stack, the trap vector, and any data touched before the next switch).
pub unsafe fn write_satp(value: usize) {
    // SAFETY: upheld by the caller.
    unsafe {
        asm!("csrw satp, {0}", "sfence.vma", in(reg) value, options(nostack));
    }
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
