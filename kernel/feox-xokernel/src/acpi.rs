//! Minimal ACPI parser for SMP discovery.
//!
//! Walks the RSDP -> XSDT (or RSDT) -> MADT chain and enumerates
//! Local APIC entries so the kernel can learn which processors the
//! firmware reports. Designed to be small enough for boot-time use:
//! no_std, no allocation, fixed-capacity output buffers.
//!
//! ACPI memory is not part of the direct map (it lives in Reserved
//! UEFI regions). The parser uses the MMIO mapping lane to alias each
//! ACPI page-range as cacheable kernel memory for the duration of the
//! parse, then unmaps before returning.

use crate::memory::{PAGE_SIZE, PhysicalAddress};
use crate::mmio::{BootstrapMmioError, MmioRegion, mmio_map_bootstrap, mmio_unmap_bootstrap};

/// Maximum number of Local APICs the parser will record.
pub const MAX_LAPICS: usize = 32;

/// One Local APIC entry decoded from the MADT.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LapicEntry {
    /// `Processor UID` as reported by firmware (per ACPI spec).
    pub processor_uid: u8,
    /// Local APIC ID assigned to this processor.
    pub apic_id: u8,
    /// MADT flags. Bit 0 = enabled; bit 1 = online-capable.
    pub flags: u32,
}

impl LapicEntry {
    /// Returns true when the firmware reports the processor as
    /// enabled (bit 0 of [`Self::flags`]).
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.flags & 0x1 != 0
    }

    /// Returns true when the firmware reports the processor as
    /// online-capable (bit 1 of [`Self::flags`]).
    #[must_use]
    pub const fn is_online_capable(self) -> bool {
        self.flags & 0x2 != 0
    }
}

/// Errors returned by the ACPI parser.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcpiError {
    /// The supplied RSDP physical address was zero.
    MissingRsdp,
    /// The RSDP signature didn't match `"RSD PTR "`.
    InvalidRsdpSignature,
    /// RSDP checksum failed.
    InvalidRsdpChecksum,
    /// The RSDP indicated revision 0 (ACPI 1.0) but no usable RSDT.
    UnsupportedAcpiRevision,
    /// A System Description Table header had an inconsistent length.
    InvalidSdtLength,
    /// A System Description Table checksum failed.
    InvalidSdtChecksum,
    /// No `"APIC"` (MADT) table was found in the XSDT/RSDT.
    MissingMadt,
    /// MMIO mapping for an ACPI page range failed.
    MmioMapFailed,
}

impl From<BootstrapMmioError> for AcpiError {
    fn from(_: BootstrapMmioError) -> Self {
        Self::MmioMapFailed
    }
}

/// Snapshot of the firmware processor topology that this parser
/// can produce in one pass.
#[derive(Clone, Copy, Debug)]
pub struct AcpiTopology {
    /// Local APIC entries written by the parser. Only the first
    /// `lapic_count` entries are meaningful.
    pub lapics: [LapicEntry; MAX_LAPICS],
    /// Number of valid entries in `lapics`.
    pub lapic_count: usize,
    /// MADT-reported local APIC controller address (used as MMIO base
    /// once the kernel starts driving the LAPIC directly).
    pub local_apic_address: u32,
}

impl AcpiTopology {
    /// Returns the valid slice of [`LapicEntry`].
    #[must_use]
    pub fn lapics(&self) -> &[LapicEntry] {
        &self.lapics[..self.lapic_count]
    }

    /// Returns the number of MADT-enabled processors.
    #[must_use]
    pub fn enabled_count(&self) -> usize {
        self.lapics().iter().filter(|e| e.is_enabled()).count()
    }
}

/// Parses the ACPI table chain starting from `rsdp_phys` and returns
/// the firmware-reported processor topology.
///
/// # Safety
///
/// `rsdp_phys` must be a physical address supplied by trusted
/// firmware. The parser bounds every read against the SDT header
/// lengths it reads, but it does trust that the SDT structure is
/// well-formed and that the supplied address actually points at an
/// ACPI table region.
pub unsafe fn parse_topology(rsdp_phys: u64) -> Result<AcpiTopology, AcpiError> {
    if rsdp_phys == 0 {
        return Err(AcpiError::MissingRsdp);
    }

    let rsdp_map = AcpiMap::map(rsdp_phys, 64)?;
    let rsdp_bytes = rsdp_map.as_slice();

    if &rsdp_bytes[0..8] != b"RSD PTR " {
        return Err(AcpiError::InvalidRsdpSignature);
    }
    if checksum_ok(&rsdp_bytes[0..20]) == false {
        return Err(AcpiError::InvalidRsdpChecksum);
    }

    let revision = rsdp_bytes[15];
    let xsdt_phys = if revision >= 2 {
        u64::from_le_bytes(rsdp_bytes[24..32].try_into().expect("rsdp xsdt slice"))
    } else {
        // ACPI 1.0: only a 32-bit RSDT is available.
        let rsdt32 = u32::from_le_bytes(rsdp_bytes[16..20].try_into().expect("rsdp rsdt slice"));
        if rsdt32 == 0 {
            return Err(AcpiError::UnsupportedAcpiRevision);
        }
        u64::from(rsdt32)
    };
    drop(rsdp_map);

    parse_sdt_chain(xsdt_phys, revision >= 2)
}

fn parse_sdt_chain(sdt_phys: u64, use_xsdt: bool) -> Result<AcpiTopology, AcpiError> {
    let header_map = AcpiMap::map(sdt_phys, 36)?;
    let header = header_map.as_slice();
    let total_length = u32::from_le_bytes(header[4..8].try_into().expect("sdt length"));
    if (total_length as usize) < 36 {
        return Err(AcpiError::InvalidSdtLength);
    }
    drop(header_map);

    let sdt_map = AcpiMap::map(sdt_phys, total_length as u64)?;
    let sdt = sdt_map.as_slice();
    if checksum_ok(sdt) == false {
        return Err(AcpiError::InvalidSdtChecksum);
    }

    let entries = &sdt[36..(total_length as usize)];
    let entry_size = if use_xsdt { 8 } else { 4 };
    let mut offset = 0usize;
    while offset + entry_size <= entries.len() {
        let entry_phys = if use_xsdt {
            u64::from_le_bytes(entries[offset..offset + 8].try_into().expect("xsdt entry"))
        } else {
            u64::from(u32::from_le_bytes(
                entries[offset..offset + 4].try_into().expect("rsdt entry"),
            ))
        };
        offset += entry_size;

        if entry_phys == 0 {
            continue;
        }

        let entry_header = AcpiMap::map(entry_phys, 36)?;
        let eh = entry_header.as_slice();
        let signature = &eh[0..4];
        if signature == b"APIC" {
            let entry_length =
                u32::from_le_bytes(eh[4..8].try_into().expect("madt length"));
            drop(entry_header);
            return parse_madt(entry_phys, entry_length as u64);
        }
    }

    Err(AcpiError::MissingMadt)
}

fn parse_madt(madt_phys: u64, length_bytes: u64) -> Result<AcpiTopology, AcpiError> {
    let madt_map = AcpiMap::map(madt_phys, length_bytes)?;
    let madt = madt_map.as_slice();
    if checksum_ok(madt) == false {
        return Err(AcpiError::InvalidSdtChecksum);
    }

    let local_apic_address =
        u32::from_le_bytes(madt[36..40].try_into().expect("madt local apic addr"));
    // 36-byte ACPI header + 4 (local APIC addr) + 4 (flags) = 44 bytes
    // before the variable-length entries.
    let mut offset = 44usize;
    let mut topology = AcpiTopology {
        lapics: [LapicEntry::default(); MAX_LAPICS],
        lapic_count: 0,
        local_apic_address,
    };

    while offset + 2 <= madt.len() {
        let entry_type = madt[offset];
        let entry_len = madt[offset + 1] as usize;
        if entry_len < 2 || offset + entry_len > madt.len() {
            break;
        }
        // Type 0: Processor Local APIC. Layout:
        //   u8 type, u8 length, u8 processor_uid, u8 apic_id, u32 flags
        if entry_type == 0 && entry_len >= 8 {
            let processor_uid = madt[offset + 2];
            let apic_id = madt[offset + 3];
            let flags = u32::from_le_bytes(
                madt[offset + 4..offset + 8]
                    .try_into()
                    .expect("madt lapic flags"),
            );
            if topology.lapic_count < MAX_LAPICS {
                topology.lapics[topology.lapic_count] = LapicEntry {
                    processor_uid,
                    apic_id,
                    flags,
                };
                topology.lapic_count += 1;
            }
        }
        offset += entry_len;
    }

    Ok(topology)
}

fn checksum_ok(bytes: &[u8]) -> bool {
    let sum: u8 = bytes.iter().copied().fold(0u8, |acc, b| acc.wrapping_add(b));
    sum == 0
}

/// RAII helper that aliases an ACPI physical range through the
/// kernel's MMIO map lane and unmaps it on drop.
struct AcpiMap {
    region: MmioRegion,
    page_offset: usize,
    length: usize,
}

impl AcpiMap {
    fn map(phys: u64, length: u64) -> Result<Self, AcpiError> {
        let aligned_phys = phys & !(PAGE_SIZE - 1);
        let page_offset = (phys - aligned_phys) as usize;
        let span = page_offset as u64 + length;
        let pages = span.div_ceil(PAGE_SIZE);
        let map_length = pages * PAGE_SIZE;
        let region = mmio_map_bootstrap(
            PhysicalAddress::new(aligned_phys),
            map_length,
            false, // read-only is fine for parse
            false, // cacheable
        )?;
        Ok(Self {
            region,
            page_offset,
            length: length as usize,
        })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe {
            // SAFETY: mmio_map_bootstrap installed a kernel-only
            // mapping covering page_offset + length bytes.
            core::slice::from_raw_parts(
                (self.region.virtual_base as *const u8).add(self.page_offset),
                self.length,
            )
        }
    }
}

impl Drop for AcpiMap {
    fn drop(&mut self) {
        let _ = mmio_unmap_bootstrap(self.region);
    }
}

#[cfg(test)]
mod tests {
    use super::{LapicEntry, MAX_LAPICS};
    use core::mem::size_of;

    #[test]
    fn lapic_entry_size_is_fixed() {
        // 1 + 1 + 4 = 6, padded to 8 by Rust to align the u32 flags.
        assert!(size_of::<LapicEntry>() >= 6);
    }

    #[test]
    fn lapic_entry_flag_decoders() {
        let enabled = LapicEntry {
            processor_uid: 0,
            apic_id: 0,
            flags: 0x1,
        };
        assert!(enabled.is_enabled());
        assert!(!enabled.is_online_capable());

        let online = LapicEntry {
            processor_uid: 0,
            apic_id: 0,
            flags: 0x2,
        };
        assert!(!online.is_enabled());
        assert!(online.is_online_capable());
    }

    #[test]
    fn lapic_buffer_capacity_is_at_least_32() {
        assert!(MAX_LAPICS >= 32);
    }
}
