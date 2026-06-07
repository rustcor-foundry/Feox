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
