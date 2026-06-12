//! Minimal flattened-device-tree (FDT / DTB) reader.
//!
//! OpenSBI hands the kernel a pointer to the flattened device tree in `a1`.
//! This parses just enough of the (big-endian) DTB to discover the physical
//! RAM region from the `/memory` node, which seeds the frame allocator. A
//! fuller device-tree walk (interrupt controllers, NVMe, etc.) lands with the
//! driver passes; this is deliberately scoped to the memory map.
//!
//! Spec: <https://devicetree-specification.readthedocs.io>. The structure
//! block is a stream of big-endian u32 tokens; node names and the strings
//! block are NUL-terminated and padded to 4-byte boundaries.

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// A validated handle to a flattened device tree in memory.
pub struct Fdt {
    base: *const u8,
    total_size: u32,
    struct_off: u32,
    strings_off: u32,
    /// One past the last byte of the structure block — every token walk is
    /// clamped below this so a corrupt `len`/`nameoff` can't run `p` off into
    /// arbitrary memory (the DTB comes from trusted firmware, but it can be
    /// truncated, or clobbered if it overlaps the frame pool).
    struct_end: u32,
    /// Offset of the memory reservation block (firmware-reserved ranges).
    mem_rsvmap_off: u32,
}

/// Validates the DTB header at `dtb` and returns a handle, or `None` if the
/// pointer is null, the magic does not match, or the header's offsets/sizes
/// place the structure or strings blocks outside the blob.
pub fn parse(dtb: usize) -> Option<Fdt> {
    if dtb == 0 || dtb & 0x3 != 0 {
        return None;
    }
    let base = dtb as *const u8;
    if be_u32(base) != FDT_MAGIC {
        return None;
    }
    // Header fields (devicetree spec §5.2). totalsize bounds everything; a
    // DTB never approaches 16 MiB, so a wild value means a corrupt header.
    let total_size = be_u32(offset(base, 4));
    if !(40..=0x0100_0000).contains(&total_size) {
        return None;
    }
    let struct_off = be_u32(offset(base, 8));
    let strings_off = be_u32(offset(base, 12));
    let mem_rsvmap_off = be_u32(offset(base, 16));
    let size_struct = be_u32(offset(base, 36));
    let size_strings = be_u32(offset(base, 32));
    if mem_rsvmap_off >= total_size {
        return None;
    }
    // Both blocks must lie wholly within [0, totalsize), with the struct
    // block 4-byte aligned (it is a stream of u32 tokens).
    let struct_end = struct_off.checked_add(size_struct)?;
    let strings_end = strings_off.checked_add(size_strings)?;
    if struct_off < 40
        || struct_off & 0x3 != 0
        || struct_end > total_size
        || strings_end > total_size
    {
        return None;
    }
    Some(Fdt {
        base,
        total_size,
        struct_off,
        strings_off,
        struct_end,
        mem_rsvmap_off,
    })
}

impl Fdt {
    /// Total size of the DTB blob in bytes (from the header).
    #[must_use]
    pub fn total_size(&self) -> u32 {
        self.total_size
    }

    /// One past the last byte of the structure block, as a raw pointer.
    fn struct_end_ptr(&self) -> usize {
        self.base as usize + self.struct_end as usize
    }

    /// True if a 4-byte token cannot be read at `p` without leaving the
    /// structure block — the walk loops break on this.
    fn token_oob(&self, p: *const u8) -> bool {
        (p as usize).saturating_add(4) > self.struct_end_ptr()
    }

    /// Clamps a property length so the value bytes (and the post-value pointer
    /// advance) stay inside the structure block, neutralizing a corrupt
    /// `len`. `value` is the start of the property value.
    fn clamp_len(&self, value: *const u8, len: usize) -> usize {
        len.min(self.struct_end_ptr().saturating_sub(value as usize))
    }

    /// Returns true if the root node's `prop` property value contains `needle`.
    /// String properties are NUL-terminated (lists are NUL-separated); this
    /// searches the raw value bytes.
    #[must_use]
    pub fn root_prop_contains(&self, prop: &[u8], needle: &[u8]) -> bool {
        let mut p = offset(self.base, self.struct_off as usize);
        let strings = offset(self.base, self.strings_off as usize);
        let mut depth: i32 = 0;
        loop {
            if self.token_oob(p) {
                break;
            }
            let token = be_u32(p);
            p = offset(p, 4);
            match token {
                FDT_BEGIN_NODE => {
                    depth += 1;
                    p = advance_past_cstr(p);
                }
                FDT_END_NODE => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                FDT_PROP => {
                    let len = be_u32(p) as usize;
                    let nameoff = be_u32(offset(p, 4)) as usize;
                    p = offset(p, 8);
                    let value = p;
                    let len = self.clamp_len(value, len);
                    if depth == 1 && bytes_eq(offset(strings, nameoff), prop) {
                        return bytes_contain(value, len, needle);
                    }
                    p = offset(p, align4(len));
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }
        false
    }

    /// Detects the QEMU `virt` machine. QEMU sets the root `compatible` to
    /// `"riscv-virtio"` and the `model` to `"riscv-virtio,qemu"`, so check both.
    #[must_use]
    pub fn machine_is_qemu(&self) -> bool {
        self.root_prop_contains(b"compatible", b"riscv-virtio")
            || self.root_prop_contains(b"model", b"qemu")
    }

    /// Reads a big-endian `u32` property `prop` from the depth-1 node named
    /// `node` (e.g. `node_u32(b"cpus", b"timebase-frequency")`).
    #[must_use]
    pub fn node_u32(&self, node: &[u8], prop: &[u8]) -> Option<u32> {
        let mut p = offset(self.base, self.struct_off as usize);
        let strings = offset(self.base, self.strings_off as usize);
        let mut depth: i32 = 0;
        let mut in_node = false;
        loop {
            if self.token_oob(p) {
                break;
            }
            let token = be_u32(p);
            p = offset(p, 4);
            match token {
                FDT_BEGIN_NODE => {
                    depth += 1;
                    in_node = depth == 2 && bytes_eq(p, node);
                    p = advance_past_cstr(p);
                }
                FDT_END_NODE => {
                    depth -= 1;
                    in_node = false;
                }
                FDT_PROP => {
                    let len = be_u32(p) as usize;
                    let nameoff = be_u32(offset(p, 4)) as usize;
                    p = offset(p, 8);
                    let value = p;
                    let len = self.clamp_len(value, len);
                    if in_node && len >= 4 && bytes_eq(offset(strings, nameoff), prop) {
                        return Some(be_u32(value));
                    }
                    p = offset(p, align4(len));
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }
        None
    }

    /// Returns the system timebase frequency (Hz) from `/cpus`, if present.
    #[must_use]
    pub fn timebase_hz(&self) -> Option<u32> {
        self.node_u32(b"cpus", b"timebase-frequency")
    }

    /// Returns the lowest reserved-RAM start address falling within `[lo, hi)`,
    /// or `hi` if none. Combines the memory-reservation block (firmware
    /// regions: OpenSBI, TF-A) and `/reserved-memory` child `reg` ranges. The
    /// frame pool is capped below this so the allocator never hands out a
    /// firmware-protected (e.g. PMP-guarded) frame — the failure mode is a
    /// store access fault on the boards (notably the Ky X1), not on QEMU.
    #[must_use]
    pub fn reserved_min_in(&self, lo: u64, hi: u64) -> u64 {
        let mut min = hi;

        // Memory reservation block: { u64 addr, u64 size } big-endian pairs,
        // terminated by an all-zero entry.
        let blob_end = self.base as usize + self.total_size as usize;
        let mut p = offset(self.base, self.mem_rsvmap_off as usize);
        while (p as usize).saturating_add(16) <= blob_end {
            let addr = be_u64(p);
            let size = be_u64(offset(p, 8));
            if addr == 0 && size == 0 {
                break;
            }
            if addr >= lo && addr < min {
                min = addr;
            }
            p = offset(p, 16);
        }

        self.reserved_memory_nodes_min(lo, min)
    }

    /// Walks `/reserved-memory` child nodes, folding their `reg` start
    /// addresses into the running `min` (within `[lo, min)`).
    fn reserved_memory_nodes_min(&self, lo: u64, mut min: u64) -> u64 {
        let mut p = offset(self.base, self.struct_off as usize);
        let strings = offset(self.base, self.strings_off as usize);
        let mut depth: i32 = 0;
        let mut in_rsvmem = false;
        let mut in_child = false;
        let mut addr_cells = 2u32; // reserved-memory mirrors the root cells

        loop {
            if self.token_oob(p) {
                break;
            }
            let token = be_u32(p);
            p = offset(p, 4);
            match token {
                FDT_BEGIN_NODE => {
                    depth += 1;
                    if depth == 2 {
                        in_rsvmem = bytes_eq(p, b"reserved-memory");
                    } else if depth == 3 && in_rsvmem {
                        in_child = true;
                    }
                    p = advance_past_cstr(p);
                }
                FDT_END_NODE => {
                    if depth == 3 {
                        in_child = false;
                    } else if depth == 2 {
                        in_rsvmem = false;
                    }
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                FDT_PROP => {
                    let len = be_u32(p) as usize;
                    let nameoff = be_u32(offset(p, 4)) as usize;
                    p = offset(p, 8);
                    let value = p;
                    let len = self.clamp_len(value, len);
                    let pname = offset(strings, nameoff);
                    if depth == 2 && in_rsvmem && bytes_eq(pname, b"#address-cells") {
                        addr_cells = be_u32(value);
                    } else if depth == 3 && in_child && bytes_eq(pname, b"reg") {
                        let (addr, _) = read_cells(value, addr_cells);
                        if addr >= lo && addr < min {
                            min = addr;
                        }
                    }
                    p = offset(p, align4(len));
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }
        min
    }

    /// Returns the first `/memory` region as `(base, size)` in bytes, decoding
    /// `reg` with the root node's `#address-cells` / `#size-cells`.
    #[must_use]
    pub fn memory(&self) -> Option<(u64, u64)> {
        let mut p = offset(self.base, self.struct_off as usize);
        let strings = offset(self.base, self.strings_off as usize);

        let mut depth: i32 = 0;
        // Device-tree spec defaults if the root omits them.
        let mut root_addr_cells: u32 = 2;
        let mut root_size_cells: u32 = 1;
        let mut in_memory = false;

        loop {
            if self.token_oob(p) {
                break;
            }
            let token = be_u32(p);
            p = offset(p, 4);
            match token {
                FDT_BEGIN_NODE => {
                    depth += 1;
                    // A node at depth 2 named "memory..." is a /memory node.
                    in_memory = depth == 2 && bytes_start_with(p, b"memory");
                    p = advance_past_cstr(p);
                }
                FDT_END_NODE => {
                    depth -= 1;
                    in_memory = false;
                }
                FDT_PROP => {
                    let len = be_u32(p) as usize;
                    let nameoff = be_u32(offset(p, 4)) as usize;
                    p = offset(p, 8);
                    let value = p;
                    let len = self.clamp_len(value, len);
                    let pname = offset(strings, nameoff);

                    // Root-level address/size cell counts govern /memory reg.
                    if depth == 1 {
                        if bytes_eq(pname, b"#address-cells") {
                            root_addr_cells = be_u32(value);
                        } else if bytes_eq(pname, b"#size-cells") {
                            root_size_cells = be_u32(value);
                        }
                    }

                    if in_memory && bytes_eq(pname, b"reg") {
                        let (addr, rest) = read_cells(value, root_addr_cells);
                        let (size, _) = read_cells(rest, root_size_cells);
                        return Some((addr, size));
                    }

                    p = offset(p, align4(len));
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break, // malformed token stream; stop walking
            }
        }
        None
    }
}

/// A console UART discovered from the device tree.
#[derive(Clone, Copy, Debug)]
pub struct UartInfo {
    /// MMIO base physical address.
    pub base: u64,
    /// Register index shift (`reg-shift`; registers at `base + (idx << shift)`).
    pub reg_shift: u32,
    /// Register access width in bytes (`reg-io-width`; 1 or 4).
    pub reg_io_width: u32,
}

impl Fdt {
    /// Finds the first 16550-compatible UART (`ns16550*` / `snps,dw-apb-uart`)
    /// and returns its MMIO base + register layout. The `reg` property is
    /// decoded with the *parent* node's `#address-cells`; `reg-shift` /
    /// `reg-io-width` default to 0 / 1 when absent (QEMU virt). Covers QEMU,
    /// the JH7110 (shift 2, width 4), and the Ky X1 (dw-apb, shift 2).
    #[must_use]
    pub fn uart(&self) -> Option<UartInfo> {
        let mut p = offset(self.base, self.struct_off as usize);
        let strings = offset(self.base, self.strings_off as usize);

        // Per-depth #address-cells, inherited by children (spec default 2).
        let mut addr_cells = [2u32; 16];
        let mut depth: usize = 0;

        // Candidate node being evaluated (its depth; 0 = none). Children of a
        // candidate are walked through without disturbing its state; the
        // verdict lands at the candidate's own END_NODE.
        let mut cand_depth: usize = 0;
        let mut matched = false;
        let mut disabled = false;
        let mut base: Option<u64> = None;
        let mut reg_shift = 0u32;
        let mut reg_io_width = 1u32;

        loop {
            if self.token_oob(p) {
                break;
            }
            let token = be_u32(p);
            p = offset(p, 4);
            match token {
                FDT_BEGIN_NODE => {
                    // Properties precede subnodes (DTB spec), so whatever
                    // node the previous BEGIN opened is fully described the
                    // moment another node begins — evaluate it now.
                    if cand_depth != 0 && matched && !disabled {
                        if let Some(base) = base {
                            return Some(UartInfo {
                                base,
                                reg_shift,
                                reg_io_width,
                            });
                        }
                    }
                    depth += 1;
                    if depth < addr_cells.len() {
                        addr_cells[depth] = addr_cells[depth - 1];
                    }
                    cand_depth = depth;
                    matched = false;
                    disabled = false;
                    base = None;
                    reg_shift = 0;
                    reg_io_width = 1;
                    p = advance_past_cstr(p);
                }
                FDT_END_NODE => {
                    // Leaf nodes end straight after their properties.
                    if cand_depth == depth && matched && !disabled {
                        if let Some(base) = base {
                            return Some(UartInfo {
                                base,
                                reg_shift,
                                reg_io_width,
                            });
                        }
                    }
                    cand_depth = 0;
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                FDT_PROP => {
                    let len = be_u32(p) as usize;
                    let nameoff = be_u32(offset(p, 4)) as usize;
                    p = offset(p, 8);
                    let value = p;
                    let len = self.clamp_len(value, len);
                    let pname = offset(strings, nameoff);

                    if bytes_eq(pname, b"#address-cells") && depth < addr_cells.len() {
                        addr_cells[depth] = be_u32(value);
                    }
                    if depth == cand_depth {
                        if bytes_eq(pname, b"compatible") {
                            matched = bytes_contain(value, len, b"ns16550")
                                || bytes_contain(value, len, b"snps,dw-apb-uart");
                        } else if bytes_eq(pname, b"reg") && depth >= 1 {
                            let (addr, _) = read_cells(value, addr_cells[(depth - 1).min(15)]);
                            base = Some(addr);
                        } else if bytes_eq(pname, b"reg-shift") && len >= 4 {
                            reg_shift = be_u32(value);
                        } else if bytes_eq(pname, b"reg-io-width") && len >= 4 {
                            reg_io_width = be_u32(value);
                        } else if bytes_eq(pname, b"status") {
                            // A disabled UART is clock-gated; an MMIO read can
                            // stall the bus, which the poll bound cannot
                            // escape — never select one.
                            disabled = bytes_contain(value, len, b"disabled");
                        }
                    }
                    p = offset(p, align4(len));
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }
        None
    }

    /// Finds the PLIC (`compatible` containing `"plic"`, e.g.
    /// `sifive,plic-1.0.0` / `riscv,plic0`) and returns its `(base, size)`,
    /// decoded with the parent's `#address-cells` / `#size-cells`.
    #[must_use]
    pub fn plic(&self) -> Option<(u64, u64)> {
        let mut p = offset(self.base, self.struct_off as usize);
        let strings = offset(self.base, self.strings_off as usize);

        let mut addr_cells = [2u32; 16];
        let mut size_cells = [1u32; 16];
        let mut depth: usize = 0;

        let mut cand_depth: usize = 0;
        let mut matched = false;
        let mut reg: Option<(u64, u64)> = None;

        loop {
            if self.token_oob(p) {
                break;
            }
            let token = be_u32(p);
            p = offset(p, 4);
            match token {
                FDT_BEGIN_NODE => {
                    // Properties precede subnodes: evaluate the previous node.
                    if cand_depth != 0 && matched {
                        if let Some(found) = reg {
                            return Some(found);
                        }
                    }
                    depth += 1;
                    if depth < addr_cells.len() {
                        addr_cells[depth] = addr_cells[depth - 1];
                        size_cells[depth] = size_cells[depth - 1];
                    }
                    cand_depth = depth;
                    matched = false;
                    reg = None;
                    p = advance_past_cstr(p);
                }
                FDT_END_NODE => {
                    if cand_depth == depth && matched {
                        if let Some(found) = reg {
                            return Some(found);
                        }
                    }
                    cand_depth = 0;
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                FDT_PROP => {
                    let len = be_u32(p) as usize;
                    let nameoff = be_u32(offset(p, 4)) as usize;
                    p = offset(p, 8);
                    let value = p;
                    let len = self.clamp_len(value, len);
                    let pname = offset(strings, nameoff);

                    if depth < addr_cells.len() {
                        if bytes_eq(pname, b"#address-cells") {
                            addr_cells[depth] = be_u32(value);
                        } else if bytes_eq(pname, b"#size-cells") {
                            size_cells[depth] = be_u32(value);
                        }
                    }
                    if depth == cand_depth {
                        if bytes_eq(pname, b"compatible") {
                            matched = bytes_contain(value, len, b"plic");
                        } else if bytes_eq(pname, b"reg") && depth >= 1 {
                            let (addr, rest) = read_cells(value, addr_cells[(depth - 1).min(15)]);
                            let (size, _) = read_cells(rest, size_cells[(depth - 1).min(15)]);
                            reg = Some((addr, size));
                        }
                    }
                    p = offset(p, align4(len));
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }
        None
    }

    /// Returns the first `interrupts` cell of the node whose `reg` base is
    /// `unit_base` — the PLIC source number of a device discovered by direct
    /// probing (e.g. the virtio-mmio net transport).
    #[must_use]
    pub fn interrupt_at(&self, unit_base: u64) -> Option<u32> {
        let mut p = offset(self.base, self.struct_off as usize);
        let strings = offset(self.base, self.strings_off as usize);

        let mut addr_cells = [2u32; 16];
        let mut depth: usize = 0;

        let mut cand_depth: usize = 0;
        let mut matched = false;
        let mut interrupt: Option<u32> = None;

        loop {
            if self.token_oob(p) {
                break;
            }
            let token = be_u32(p);
            p = offset(p, 4);
            match token {
                FDT_BEGIN_NODE => {
                    if cand_depth != 0 && matched {
                        if let Some(found) = interrupt {
                            return Some(found);
                        }
                    }
                    depth += 1;
                    if depth < addr_cells.len() {
                        addr_cells[depth] = addr_cells[depth - 1];
                    }
                    cand_depth = depth;
                    matched = false;
                    interrupt = None;
                    p = advance_past_cstr(p);
                }
                FDT_END_NODE => {
                    if cand_depth == depth && matched {
                        if let Some(found) = interrupt {
                            return Some(found);
                        }
                    }
                    cand_depth = 0;
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                FDT_PROP => {
                    let len = be_u32(p) as usize;
                    let nameoff = be_u32(offset(p, 4)) as usize;
                    p = offset(p, 8);
                    let value = p;
                    let len = self.clamp_len(value, len);
                    let pname = offset(strings, nameoff);

                    if bytes_eq(pname, b"#address-cells") && depth < addr_cells.len() {
                        addr_cells[depth] = be_u32(value);
                    }
                    if depth == cand_depth {
                        if bytes_eq(pname, b"reg") && depth >= 1 {
                            let (addr, _) = read_cells(value, addr_cells[(depth - 1).min(15)]);
                            matched = addr == unit_base;
                        } else if bytes_eq(pname, b"interrupts") && len >= 4 {
                            interrupt = Some(be_u32(value));
                        }
                    }
                    p = offset(p, align4(len));
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }
        None
    }

    /// Collects the hart ids of MMU-capable, enabled CPUs (`/cpus/cpu@*`
    /// nodes with an `mmu-type` property and no `status = "disabled"`) into
    /// `out`, returning how many were written. This is what makes SMP
    /// hardware-safe: the JH7110's S7 monitor hart carries no `mmu-type` and
    /// is skipped.
    #[must_use]
    pub fn cpu_harts(&self, out: &mut [usize]) -> usize {
        let mut p = offset(self.base, self.struct_off as usize);
        let strings = offset(self.base, self.strings_off as usize);

        let mut depth: i32 = 0;
        let mut in_cpus = false;
        let mut cpus_addr_cells = 1u32; // /cpus traditionally uses 1
        let mut in_cpu = false;

        // Candidate state for the cpu node being scanned.
        let mut hartid: Option<u64> = None;
        let mut has_mmu = false;
        let mut disabled = false;
        let mut count = 0usize;

        loop {
            if self.token_oob(p) {
                break;
            }
            let token = be_u32(p);
            p = offset(p, 4);
            match token {
                FDT_BEGIN_NODE => {
                    depth += 1;
                    if depth == 2 {
                        in_cpus = bytes_eq(p, b"cpus");
                    } else if depth == 3 && in_cpus {
                        in_cpu = bytes_start_with(p, b"cpu@");
                        hartid = None;
                        has_mmu = false;
                        disabled = false;
                    }
                    p = advance_past_cstr(p);
                }
                FDT_END_NODE => {
                    if depth == 3 && in_cpu {
                        if let Some(id) = hartid {
                            if has_mmu && !disabled && count < out.len() {
                                out[count] = id as usize;
                                count += 1;
                            }
                        }
                        in_cpu = false;
                    }
                    if depth == 2 {
                        in_cpus = false;
                    }
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                FDT_PROP => {
                    let len = be_u32(p) as usize;
                    let nameoff = be_u32(offset(p, 4)) as usize;
                    p = offset(p, 8);
                    let value = p;
                    let len = self.clamp_len(value, len);
                    let pname = offset(strings, nameoff);

                    if depth == 2 && in_cpus && bytes_eq(pname, b"#address-cells") {
                        cpus_addr_cells = be_u32(value);
                    } else if depth == 3 && in_cpu {
                        if bytes_eq(pname, b"reg") {
                            let (id, _) = read_cells(value, cpus_addr_cells);
                            hartid = Some(id);
                        } else if bytes_eq(pname, b"mmu-type") {
                            has_mmu = true;
                        } else if bytes_eq(pname, b"status") {
                            disabled = bytes_contain(value, len, b"disabled");
                        }
                    }
                    p = offset(p, align4(len));
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }
        count
    }
}

/// Reads `cells` big-endian u32s starting at `p`, combining them into a u64
/// (most-significant cell first), and returns the value and the advanced
/// pointer.
fn read_cells(p: *const u8, cells: u32) -> (u64, *const u8) {
    let mut value: u64 = 0;
    let mut cursor = p;
    // Device-tree addresses are at most 4 cells (and 2 in practice); cap the
    // count so a corrupt `#address-cells`/`#size-cells` can't walk `cursor`
    // off into arbitrary memory.
    for _ in 0..cells.min(4) {
        value = (value << 32) | u64::from(be_u32(cursor));
        cursor = offset(cursor, 4);
    }
    (value, cursor)
}

/// Reads a big-endian u32 at `p`.
///
/// `p` must point at 4 readable bytes inside the DTB blob.
fn be_u32(p: *const u8) -> u32 {
    // SAFETY: callers only pass pointers within the validated DTB bounds.
    let raw = unsafe { core::ptr::read_unaligned(p.cast::<u32>()) };
    u32::from_be(raw)
}

/// Reads a big-endian u64 at `p` (the memory-reservation block's fields).
fn be_u64(p: *const u8) -> u64 {
    // SAFETY: callers keep `p` within the validated DTB bounds.
    let raw = unsafe { core::ptr::read_unaligned(p.cast::<u64>()) };
    u64::from_be(raw)
}

/// Pointer offset by `bytes`.
fn offset(p: *const u8, bytes: usize) -> *const u8 {
    // SAFETY: callers keep offsets within the DTB blob.
    unsafe { p.add(bytes) }
}

/// Returns true if the NUL-terminated string at `p` begins with `prefix`.
fn bytes_start_with(p: *const u8, prefix: &[u8]) -> bool {
    for (i, &b) in prefix.iter().enumerate() {
        // SAFETY: bounded by prefix length; the DTB string is longer.
        if unsafe { *p.add(i) } != b {
            return false;
        }
    }
    true
}

/// Returns true if the NUL-terminated string at `p` equals `s` exactly.
fn bytes_eq(p: *const u8, s: &[u8]) -> bool {
    if !bytes_start_with(p, s) {
        return false;
    }
    // SAFETY: index s.len() is the byte right after the compared prefix.
    unsafe { *p.add(s.len()) == 0 }
}

/// Advances `p` past a NUL-terminated string and 4-byte-aligns the result.
fn advance_past_cstr(p: *const u8) -> *const u8 {
    let mut len = 0usize;
    // SAFETY: DTB strings are NUL-terminated within the blob.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    offset(p, align4(len + 1))
}

/// Rounds `x` up to the next multiple of 4.
const fn align4(x: usize) -> usize {
    (x + 3) & !3
}

/// Returns true if the `len`-byte region at `p` contains `needle` as a
/// contiguous subsequence.
fn bytes_contain(p: *const u8, len: usize, needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > len {
        return false;
    }
    // SAFETY: callers pass a pointer/length within the DTB blob.
    let hay = unsafe { core::slice::from_raw_parts(p, len) };
    hay.windows(needle.len()).any(|w| w == needle)
}
