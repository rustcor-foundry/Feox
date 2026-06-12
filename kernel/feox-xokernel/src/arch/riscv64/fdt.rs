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
}

/// Validates the DTB header at `dtb` and returns a handle, or `None` if the
/// pointer is null or the magic does not match.
pub fn parse(dtb: usize) -> Option<Fdt> {
    if dtb == 0 {
        return None;
    }
    let base = dtb as *const u8;
    if be_u32(base) != FDT_MAGIC {
        return None;
    }
    Some(Fdt {
        base,
        total_size: be_u32(offset(base, 4)),
        struct_off: be_u32(offset(base, 8)),
        strings_off: be_u32(offset(base, 12)),
    })
}

impl Fdt {
    /// Total size of the DTB blob in bytes (from the header).
    #[must_use]
    pub fn total_size(&self) -> u32 {
        self.total_size
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
        let mut base: Option<u64> = None;
        let mut reg_shift = 0u32;
        let mut reg_io_width = 1u32;

        loop {
            let token = be_u32(p);
            p = offset(p, 4);
            match token {
                FDT_BEGIN_NODE => {
                    depth += 1;
                    if depth < addr_cells.len() {
                        addr_cells[depth] = addr_cells[depth - 1];
                    }
                    if cand_depth == 0 || depth <= cand_depth {
                        cand_depth = depth;
                        matched = false;
                        base = None;
                        reg_shift = 0;
                        reg_io_width = 1;
                    }
                    p = advance_past_cstr(p);
                }
                FDT_END_NODE => {
                    if cand_depth != 0 && depth == cand_depth {
                        if matched {
                            if let Some(base) = base {
                                return Some(UartInfo {
                                    base,
                                    reg_shift,
                                    reg_io_width,
                                });
                            }
                        }
                        cand_depth = 0;
                    }
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
                    let pname = offset(strings, nameoff);

                    if bytes_eq(pname, b"#address-cells") && depth < addr_cells.len() {
                        addr_cells[depth] = be_u32(value);
                    }
                    if depth == cand_depth {
                        if bytes_eq(pname, b"compatible") {
                            matched = bytes_contain(value, len, b"ns16550")
                                || bytes_contain(value, len, b"snps,dw-apb-uart");
                        } else if bytes_eq(pname, b"reg") && depth >= 1 {
                            let (addr, _) = read_cells(value, addr_cells[depth - 1]);
                            base = Some(addr);
                        } else if bytes_eq(pname, b"reg-shift") && len >= 4 {
                            reg_shift = be_u32(value);
                        } else if bytes_eq(pname, b"reg-io-width") && len >= 4 {
                            reg_io_width = be_u32(value);
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
    for _ in 0..cells {
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
