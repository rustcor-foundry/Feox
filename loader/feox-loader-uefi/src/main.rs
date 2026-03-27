#![no_main]
#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

//! Thin UEFI loader that loads the Feox kernel ELF and passes a BootInfo
//! pointer in `rdi`.

extern crate alloc;

use core::arch::asm;
use core::cell::UnsafeCell;
use core::cmp;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use elf::abi::{ET_EXEC, PT_LOAD};
#[cfg(target_arch = "aarch64")]
use elf::abi::EM_AARCH64;
#[cfg(target_arch = "x86_64")]
use elf::abi::EM_X86_64;
use elf::endian::AnyEndian;
use elf::file::Class;
use elf::{ElfBytes, segment::ProgramHeader};
use feox_boot::{BootInfo, MemoryRegion, MemoryRegionKind, PAGE_SIZE, PhysicalAddress};
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::fs::FileSystem;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned};
use uefi::prelude::*;

const KERNEL_PATH_TEXT: &str = "\\EFI\\BOOT\\FEOXKERN.ELF";
const BOOT_MEMORY_MAP_CAPACITY: usize = 1024;
static BOOT_SERVICES_ACTIVE: AtomicBool = AtomicBool::new(true);

macro_rules! loader_logln {
    () => {{
        crate::serial::write_fmt(core::format_args!("\n"));
        if crate::BOOT_SERVICES_ACTIVE.load(core::sync::atomic::Ordering::Relaxed) {
            uefi::println!();
        }
    }};
    ($fmt:expr $(, $($arg:tt)*)?) => {{
        crate::serial::write_fmt(core::format_args!(core::concat!($fmt, "\n") $(, $($arg)*)?));
        if crate::BOOT_SERVICES_ACTIVE.load(core::sync::atomic::Ordering::Relaxed) {
            uefi::println!($fmt $(, $($arg)*)?);
        }
    }};
}

struct BootStorage {
    regions: UnsafeCell<[MemoryRegion; BOOT_MEMORY_MAP_CAPACITY]>,
    info: UnsafeCell<BootInfo>,
}

unsafe impl Sync for BootStorage {}

static BOOT_STORAGE: BootStorage = BootStorage {
    regions: UnsafeCell::new([MemoryRegion::empty(); BOOT_MEMORY_MAP_CAPACITY]),
    info: UnsafeCell::new(BootInfo::empty()),
};

#[cfg(target_arch = "x86_64")]
const EXPECTED_KERNEL_MACHINE: u16 = EM_X86_64;

#[cfg(target_arch = "aarch64")]
const EXPECTED_KERNEL_MACHINE: u16 = EM_AARCH64;

#[derive(Clone, Copy, Debug)]
struct LoadedKernel {
    entry_point: u64,
    image_start: u64,
    image_end: u64,
}

#[derive(Clone, Copy, Debug)]
enum LoadError {
    KernelRead,
    InvalidElf,
    UnsupportedKernel,
    NoLoadSegments,
    SegmentOutOfBounds,
    AddressOverflow,
    PageAllocation,
}

impl LoadError {
    const fn status(self) -> Status {
        match self {
            Self::KernelRead => Status::NOT_FOUND,
            Self::InvalidElf
            | Self::UnsupportedKernel
            | Self::NoLoadSegments
            | Self::SegmentOutOfBounds
            | Self::AddressOverflow
            | Self::PageAllocation => Status::LOAD_ERROR,
        }
    }
}

#[entry]
fn main() -> Status {
    serial::init();
    if uefi::helpers::init().is_err() {
        serial::write_fmt(core::format_args!(
            "feox-loader: failed to initialize UEFI helpers\n"
        ));
        return Status::ABORTED;
    }

    match boot_kernel() {
        Ok(()) => Status::SUCCESS,
        Err(error) => {
            loader_logln!(
                "feox-loader: failed to boot {}: {:?}",
                KERNEL_PATH_TEXT,
                error
            );
            error.status()
        }
    }
}

fn boot_kernel() -> Result<(), LoadError> {
    let image_handle = boot::image_handle();
    let fs_proto = boot::get_image_file_system(image_handle).map_err(|_| LoadError::KernelRead)?;
    let mut fs = FileSystem::new(fs_proto);
    let kernel_bytes = fs
        .read(uefi::cstr16!("\\EFI\\BOOT\\FEOXKERN.ELF"))
        .map_err(|_| LoadError::KernelRead)?;
    let loaded_kernel = load_kernel_image(&kernel_bytes)?;

    drop(fs);

    loader_logln!(
        "feox-loader: entry={:#018x} image={:#018x}-{:#018x}",
        loaded_kernel.entry_point,
        loaded_kernel.image_start,
        loaded_kernel.image_end
    );

    let memory_map = unsafe { boot::exit_boot_services(Some(MemoryType::LOADER_DATA)) };
    BOOT_SERVICES_ACTIVE.store(false, Ordering::Relaxed);
    let boot_info = build_boot_info(&memory_map, loaded_kernel);
    jump_to_kernel(loaded_kernel.entry_point, boot_info)
}

fn load_kernel_image(kernel_bytes: &[u8]) -> Result<LoadedKernel, LoadError> {
    let elf =
        ElfBytes::<AnyEndian>::minimal_parse(kernel_bytes).map_err(|_| LoadError::InvalidElf)?;

    if elf.ehdr.class != Class::ELF64
        || elf.ehdr.e_machine != EXPECTED_KERNEL_MACHINE
        || elf.ehdr.e_type != ET_EXEC
    {
        return Err(LoadError::UnsupportedKernel);
    }

    let segments = elf.segments().ok_or(LoadError::NoLoadSegments)?;
    let mut image_start = u64::MAX;
    let mut image_end = 0;
    let mut found_load_segment = false;

    for segment in segments.iter().filter(|segment| segment.p_type == PT_LOAD) {
        found_load_segment = true;

        let segment_start = segment_load_address(&segment);
        let segment_end = segment_start
            .checked_add(segment.p_memsz)
            .ok_or(LoadError::AddressOverflow)?;

        image_start = cmp::min(image_start, align_down(segment_start));
        image_end = cmp::max(image_end, align_up(segment_end));
    }

    if !found_load_segment {
        return Err(LoadError::NoLoadSegments);
    }

    let page_count = usize::try_from((image_end - image_start) / PAGE_SIZE)
        .map_err(|_| LoadError::AddressOverflow)?;
    let allocation = boot::allocate_pages(
        AllocateType::Address(image_start),
        MemoryType::LOADER_DATA,
        page_count,
    )
    .map_err(|_| LoadError::PageAllocation)?;

    if allocation.as_ptr() as u64 != image_start {
        return Err(LoadError::PageAllocation);
    }

    let total_bytes =
        usize::try_from(image_end - image_start).map_err(|_| LoadError::AddressOverflow)?;
    unsafe { ptr::write_bytes(allocation.as_ptr(), 0, total_bytes) };

    for segment in segments.iter().filter(|segment| segment.p_type == PT_LOAD) {
        copy_load_segment(kernel_bytes, segment)?;
    }

    if elf.ehdr.e_entry < image_start || elf.ehdr.e_entry >= image_end {
        return Err(LoadError::UnsupportedKernel);
    }

    Ok(LoadedKernel {
        entry_point: elf.ehdr.e_entry,
        image_start,
        image_end,
    })
}

fn copy_load_segment(kernel_bytes: &[u8], segment: ProgramHeader) -> Result<(), LoadError> {
    if segment.p_filesz > segment.p_memsz {
        return Err(LoadError::UnsupportedKernel);
    }

    let file_start =
        usize::try_from(segment.p_offset).map_err(|_| LoadError::SegmentOutOfBounds)?;
    let file_size = usize::try_from(segment.p_filesz).map_err(|_| LoadError::SegmentOutOfBounds)?;
    let file_end = file_start
        .checked_add(file_size)
        .ok_or(LoadError::AddressOverflow)?;
    let bytes = kernel_bytes
        .get(file_start..file_end)
        .ok_or(LoadError::SegmentOutOfBounds)?;

    let destination = segment_load_address(&segment) as *mut u8;
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len()) };

    let zero_fill = usize::try_from(segment.p_memsz - segment.p_filesz)
        .map_err(|_| LoadError::AddressOverflow)?;
    unsafe { ptr::write_bytes(destination.wrapping_add(bytes.len()), 0, zero_fill) };

    Ok(())
}

fn build_boot_info(memory_map: &MemoryMapOwned, kernel: LoadedKernel) -> *const BootInfo {
    let regions = unsafe { &mut *BOOT_STORAGE.regions.get() };
    let mut count = 0usize;

    for descriptor in memory_map.entries() {
        let start = descriptor.phys_start;
        let size_bytes = descriptor
            .page_count
            .checked_mul(PAGE_SIZE)
            .unwrap_or_else(|| after_exit_failure("descriptor size overflow"));
        let end = start
            .checked_add(size_bytes)
            .unwrap_or_else(|| after_exit_failure("descriptor end overflow"));

        split_and_push_region(
            regions,
            &mut count,
            start,
            end,
            classify_memory_type(descriptor.ty),
            kernel.image_start,
            kernel.image_end,
        );
    }

    let boot_info = unsafe { &mut *BOOT_STORAGE.info.get() };
    *boot_info = BootInfo::new(&regions[..count]);
    boot_info as *const BootInfo
}

fn split_and_push_region(
    storage: &mut [MemoryRegion; BOOT_MEMORY_MAP_CAPACITY],
    count: &mut usize,
    start: u64,
    end: u64,
    kind: MemoryRegionKind,
    kernel_start: u64,
    kernel_end: u64,
) {
    if start >= end {
        return;
    }

    let overlap_start = cmp::max(start, kernel_start);
    let overlap_end = cmp::min(end, kernel_end);

    if overlap_start >= overlap_end {
        push_region(storage, count, start, end, kind);
        return;
    }

    push_region(storage, count, start, overlap_start, kind);
    push_region(
        storage,
        count,
        overlap_start,
        overlap_end,
        MemoryRegionKind::Kernel,
    );
    push_region(storage, count, overlap_end, end, kind);
}

fn push_region(
    storage: &mut [MemoryRegion; BOOT_MEMORY_MAP_CAPACITY],
    count: &mut usize,
    start: u64,
    end: u64,
    kind: MemoryRegionKind,
) {
    if start >= end {
        return;
    }

    if let Some(previous) = (*count)
        .checked_sub(1)
        .and_then(|index| storage.get_mut(index))
        .filter(|region| region.kind == kind && region.end.as_u64() == start)
    {
        previous.end = PhysicalAddress::new(end);
        return;
    }

    if *count >= storage.len() {
        after_exit_failure("boot memory map capacity exceeded");
    }

    storage[*count] =
        MemoryRegion::new(PhysicalAddress::new(start), PhysicalAddress::new(end), kind);
    *count += 1;
}

const fn classify_memory_type(memory_type: MemoryType) -> MemoryRegionKind {
    match memory_type {
        MemoryType::CONVENTIONAL => MemoryRegionKind::Usable,
        MemoryType::LOADER_CODE
        | MemoryType::LOADER_DATA
        | MemoryType::BOOT_SERVICES_CODE
        | MemoryType::BOOT_SERVICES_DATA => MemoryRegionKind::BootloaderReclaimable,
        MemoryType::MMIO | MemoryType::MMIO_PORT_SPACE => MemoryRegionKind::Mmio,
        _ => MemoryRegionKind::Reserved,
    }
}

#[cfg(target_arch = "x86_64")]
fn jump_to_kernel(entry_point: u64, boot_info: *const BootInfo) -> ! {
    unsafe {
        asm!(
            "mov rdi, {boot_info}",
            "jmp {entry}",
            boot_info = in(reg) boot_info,
            entry = in(reg) entry_point,
            options(noreturn)
        );
    }
}

#[cfg(target_arch = "aarch64")]
fn jump_to_kernel(entry_point: u64, boot_info: *const BootInfo) -> ! {
    let entry: extern "C" fn(*const BootInfo) -> ! =
        unsafe { mem::transmute(entry_point as usize) };
    entry(boot_info)
}

fn segment_load_address(segment: &ProgramHeader) -> u64 {
    if segment.p_paddr != 0 {
        segment.p_paddr
    } else {
        segment.p_vaddr
    }
}

const fn align_down(value: u64) -> u64 {
    value & !(PAGE_SIZE - 1)
}

const fn align_up(value: u64) -> u64 {
    (value + (PAGE_SIZE - 1)) & !(PAGE_SIZE - 1)
}

fn after_exit_failure(_reason: &str) -> ! {
    serial::write_fmt(core::format_args!(
        "feox-loader: fatal after ExitBootServices: {}\n",
        _reason
    ));
    loop {
        core::hint::spin_loop();
    }
}

mod serial {
    use super::{COM1_PORT, DEBUGCON_PORT, LSR_TRANSMIT_HOLDING_REGISTER_EMPTY};
    use core::fmt::{self, Write};

    /// Initializes COM1 for early loader diagnostics.
    pub fn init() {
        unsafe {
            super::outb(COM1_PORT + 1, 0x00);
            super::outb(COM1_PORT + 3, 0x80);
            super::outb(COM1_PORT, 0x03);
            super::outb(COM1_PORT + 1, 0x00);
            super::outb(COM1_PORT + 3, 0x03);
            super::outb(COM1_PORT + 2, 0xC7);
            super::outb(COM1_PORT + 4, 0x0B);
            let _ = super::inb(COM1_PORT);
        }
    }

    /// Writes formatted text to COM1.
    pub fn write_fmt(args: fmt::Arguments<'_>) {
        let _ = SerialWriter.write_fmt(args);
    }

    struct SerialWriter;

    impl Write for SerialWriter {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            for byte in text.bytes() {
                write_byte(byte);
            }
            Ok(())
        }
    }

    fn write_byte(byte: u8) {
        if byte == b'\n' {
            write_raw_byte(b'\r');
        }
        write_raw_byte(byte);
        write_debugcon_byte(byte);
    }

    fn write_raw_byte(byte: u8) {
        while unsafe { super::inb(COM1_PORT + 5) } & LSR_TRANSMIT_HOLDING_REGISTER_EMPTY == 0 {
            core::hint::spin_loop();
        }

        unsafe { super::outb(COM1_PORT, byte) };
    }

    fn write_debugcon_byte(byte: u8) {
        unsafe { super::outb(DEBUGCON_PORT, byte) };
    }
}

const COM1_PORT: u16 = 0x3F8;
const DEBUGCON_PORT: u16 = 0x402;
const LSR_TRANSMIT_HOLDING_REGISTER_EMPTY: u8 = 1 << 5;

unsafe fn outb(port: u16, value: u8) {
    unsafe {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}
