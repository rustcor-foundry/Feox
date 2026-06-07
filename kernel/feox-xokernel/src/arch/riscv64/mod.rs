//! riscv64 architecture support.
//!
//! First bring-up lane: S-mode entry, SBI serial console, and a `wfi` halt
//! loop. Trap handling, sv39 paging, SMP (SBI HSM), and the device-tree intake
//! land in subsequent passes; until then the kernel takes a minimal riscv64
//! path that bypasses the x86-shaped `boot::bootstrap` flow.

pub mod cpu;
pub mod fdt;
pub mod frame;
pub mod heap;
pub mod net;
pub mod nvme;
pub mod paging;
pub mod panic;
pub mod pci;
#[cfg(feature = "runtime")]
pub mod runtime;
pub mod serial;
pub mod smp;
pub mod time;
pub mod trap;

use core::ptr::addr_of;

use crate::{PROJECT_NAME, PROJECT_STYLE};

unsafe extern "C" {
    /// End of the kernel image, defined by the linker script. Its address (not
    /// its value) marks the first byte of RAM free for the frame allocator.
    static __kernel_end: u8;
}

/// Minimal S-mode bring-up entry for the riscv64 milestone-1 path.
///
/// Called from `_start` (see `main.rs`) with the SBI-provided boot arguments:
/// `hartid` is the boot hart's id (`a0`) and `dtb` is the physical address of
/// the flattened device tree (`a1`). This deliberately bypasses
/// `boot::bootstrap`, which is still x86-shaped; it brings the console up,
/// prints the banner, and parks the hart. Subsequent passes grow this into the
/// real bootstrap (traps -> sv39 -> memory -> runtime).
pub fn riscv_main(hartid: usize, dtb: usize) -> ! {
    cpu::disable_interrupts();
    crate::console::init();

    crate::kprintln!();
    crate::kprintln!("{} - {}", PROJECT_NAME, PROJECT_STYLE);
    crate::kprintln!("arch: riscv64 (S-mode)");
    crate::kprintln!("boot hart: {}", hartid);
    crate::kprintln!("device tree: {:#x}", dtb);

    // Milestone 2: install the supervisor trap vector and prove the full
    // save -> dispatch -> resume cycle by taking a deliberate breakpoint. If
    // the trap path were wrong this would never return; reaching the line
    // after `ebreak` is the proof of life.
    trap::init();
    crate::kprintln!("[feox] traps installed (stvec -> trap_entry); testing ebreak...");
    // SAFETY: `ebreak` raises a synchronous breakpoint exception, which the
    // installed handler catches and resumes past. No memory or stack effects.
    unsafe {
        core::arch::asm!("ebreak");
    }
    crate::kprintln!("[feox] breakpoint trap handled; execution resumed.");

    // Milestone 3: switch on sv39 paging via an identity map. Printing after
    // the `satp` switch proves instruction fetch, the stack, and the SBI
    // console all keep working under hardware address translation.
    paging::enable_identity_map();
    crate::kprintln!(
        "[feox] sv39 paging enabled (satp={:#x}); running translated.",
        paging::read_satp()
    );

    // Milestone 4: parse the device tree for the real RAM map and stand up a
    // physical frame allocator over the usable window. Returns whether we are
    // on QEMU virt (whose fixed device/PCIe windows we may touch).
    let on_qemu = init_memory(dtb);

    // Milestone 5: drive the portable feox-async executor on riscv64.
    #[cfg(feature = "runtime")]
    {
        crate::kprintln!("[feox] starting feox-async runtime demo...");
        runtime::demo();
    }

    // Milestone 6: enumerate PCIe over ECAM and exercise the NVMe controller.
    // The ECAM/MMIO windows are QEMU-virt-specific (and the MMIO window overlaps
    // RAM on other SoCs), so only do this on QEMU; real-hardware PCIe is a
    // device-tree-derived driver for later.
    if on_qemu {
        discover_pci();
        // Milestone 8a: bring up virtio-net and prove the link with an ARP
        // round-trip. On real hardware this is the JH7110 dwmac driver behind
        // the same NetDevice interface.
        net::selftest();
    } else {
        crate::kprintln!("[feox] pcie/net: skipped (non-QEMU; DT-derived drivers TODO)");
    }

    // Milestone 7: bring up the secondary harts via the SBI HSM extension.
    smp::bring_up_secondary_harts(hartid);

    // Milestone 9: enable supervisor timer interrupts and take a few ticks.
    let timebase = fdt::parse(dtb)
        .and_then(|tree| tree.timebase_hz())
        .map_or(10_000_000, u64::from);
    crate::kprintln!("[feox] timer: arming (timebase {} Hz)...", timebase);
    time::enable(timebase);
    while time::ticks() < 5 {
        cpu::halt();
    }
    time::disable();
    crate::kprintln!(
        "[feox] timer: {} ticks taken; milestone 9: supervisor timer interrupts.",
        time::ticks()
    );

    crate::kprintln!("[feox] riscv64 bring-up alive; parking boot hart.");

    cpu::hlt_loop()
}

/// Parses the DTB for the RAM region and initializes the frame allocator over
/// the RAM above the kernel image (and below the DTB, which sits high in RAM on
/// QEMU virt). Runs a small alloc/free self-check as proof of life. Returns
/// whether the machine is QEMU virt (so fixed device/PCIe windows are safe to
/// map and probe).
fn init_memory(dtb: usize) -> bool {
    let Some(tree) = fdt::parse(dtb) else {
        crate::kprintln!("[feox] WARNING: invalid or missing DTB at {:#x}", dtb);
        return false;
    };
    let on_qemu = tree.machine_is_qemu();
    crate::kprintln!(
        "[feox] dtb: base={:#x} size={} bytes (machine={})",
        dtb,
        tree.total_size(),
        if on_qemu { "qemu-virt" } else { "other" }
    );

    let Some((ram_base, ram_size)) = tree.memory() else {
        crate::kprintln!("[feox] WARNING: no /memory node found in DTB");
        return false;
    };
    let ram_end = ram_base + ram_size;
    crate::kprintln!(
        "[feox] ram: {:#x}..{:#x} ({} MiB)",
        ram_base,
        ram_end,
        ram_size >> 20
    );

    // Usable RAM starts just past the kernel image. The DTB sits high in RAM
    // on QEMU virt, so cap the window below it (everything at/above the DTB is
    // left reserved for now); otherwise run to the end of RAM.
    let kernel_end = addr_of!(__kernel_end) as usize;
    let usable_end = if (dtb as u64) > kernel_end as u64 && (dtb as u64) < ram_end {
        dtb
    } else {
        ram_end as usize
    };

    frame::init(kernel_end, usable_end);
    crate::kprintln!(
        "[feox] frames: {:#x}..{:#x} ({} frames, {} MiB usable)",
        kernel_end,
        usable_end,
        frame::total(),
        (frame::total() * frame::FRAME_SIZE) >> 20
    );

    // Self-check: allocate three frames, free the middle one, and confirm the
    // next allocation recycles it (proves both bump and free-list paths).
    let f0 = frame::alloc();
    let f1 = frame::alloc();
    let f2 = frame::alloc();
    crate::kprintln!(
        "[feox] frame alloc: {:#x} {:#x} {:#x}",
        f0.unwrap_or(0),
        f1.unwrap_or(0),
        f2.unwrap_or(0)
    );
    if let Some(addr) = f1 {
        frame::free(addr);
    }
    let f3 = frame::alloc();
    crate::kprintln!(
        "[feox] freed middle frame; realloc={:#x} recycled={}",
        f3.unwrap_or(0),
        f3 == f1
    );

    crate::kprintln!(
        "[feox] milestone 4: frame allocator online ({} frames, {} available)",
        frame::total(),
        frame::available()
    );

    // Milestone 10: carve a heap region from the frame pool and bring up the
    // global allocator, then stress it (Vec growth + Box) to prove `alloc`.
    const HEAP_FRAMES: usize = 2048; // 8 MiB
    if let Some(heap_base) = frame::alloc_contiguous(HEAP_FRAMES) {
        let heap_size = HEAP_FRAMES * frame::FRAME_SIZE;
        // SAFETY: a fresh, contiguous, identity-mapped frame-pool region handed
        // out exactly once.
        unsafe { heap::init(heap_base, heap_size) };
        let mut v = alloc::vec::Vec::new();
        for i in 0..1000u64 {
            v.push(i * i);
        }
        let sum: u64 = v.iter().sum();
        let boxed = alloc::boxed::Box::new(0xFEu64);
        crate::kprintln!(
            "[feox] heap: {} MiB online; Vec(len={}) sum={}, Box={:#x}",
            heap_size >> 20,
            v.len(),
            sum,
            *boxed
        );
        drop(v);
        drop(boxed);
        crate::kprintln!("[feox] milestone 10: kernel heap online.");
    } else {
        crate::kprintln!("[feox] heap: out of frames for the kernel heap");
    }

    // Milestone 4b: replace the bootstrap gigapage identity map with a
    // fine-grained kernel address space (per-section W^X) built from the frame
    // allocator, then verify the multi-level walk via translate().
    let root =
        paging::build_kernel_address_space(usable_end, dtb, tree.total_size() as usize, on_qemu);
    crate::kprintln!(
        "[feox] kernel address space active (root={:#x} satp={:#x})",
        root,
        paging::read_satp()
    );
    verify_translations();
    crate::kprintln!(
        "[feox] milestone 4b: sv39 walker + per-section W^X map online."
    );

    on_qemu
}

/// Translates a few representative kernel addresses through the live page table
/// and prints VA -> PA with permissions, proving the multi-level walk and the
/// per-section permissions are correct.
fn verify_translations() {
    let stack_probe = 0u64;
    let probes = [
        ("text  ", riscv_main as *const () as usize),
        ("rodata", "feox".as_ptr() as usize),
        ("stack ", addr_of!(stack_probe) as usize),
    ];
    for (name, va) in probes {
        match paging::translate(va) {
            Some((pa, flags)) => {
                let (r, w, x) = paging::decode_rwx(flags);
                crate::kprintln!(
                    "[feox]   xlate {}: va={:#x} -> pa={:#x} [{}{}{}] identity={}",
                    name,
                    va,
                    pa,
                    if r { 'r' } else { '-' },
                    if w { 'w' } else { '-' },
                    if x { 'x' } else { '-' },
                    va == pa
                );
            }
            None => crate::kprintln!("[feox]   xlate {}: va={:#x} -> UNMAPPED", name, va),
        }
    }
}

/// Enumerates PCIe over ECAM and reports the NVMe controller, if present.
///
/// Milestone 6a: proves memory-mapped device access works on riscv64 (the first
/// non-SBI hardware access) and surfaces the raw BAR0 so the controller-init
/// pass (6b) knows whether the firmware assigned it.
fn discover_pci() {
    match pci::scan_for_class(pci::CLASS_NVME) {
        Some(dev) => {
            crate::kprintln!(
                "[feox] pcie: NVMe controller at {:02x}:{:02x}.{} vendor={:#06x} device={:#06x} class={:#08x}",
                dev.bus,
                dev.device,
                dev.function,
                dev.vendor_id,
                dev.device_id,
                dev.class_code
            );
            crate::kprintln!("[feox]   BAR0 raw={:#010x}", pci::bar_raw(&dev, 0));
            crate::kprintln!("[feox] milestone 6a: PCIe ECAM up; NVMe controller discovered.");

            // Milestone 6b: assign the BAR and bring the controller to ready.
            // Milestone 6c: admin Identify round-trip.
            // Milestone 6d: identify namespace, create an I/O queue, and verify
            // block I/O by writing a pattern to LBA 0 and reading it back.
            if let Some(mut controller) = nvme::init(&dev) {
                if controller.identify_controller()
                    && controller.identify_namespace()
                    && controller.create_io_queues()
                {
                    let _ = controller.block_io_selftest();
                }
            }
        }
        None => {
            crate::kprintln!("[feox] pcie: no NVMe controller found on the root bus");
        }
    }
}
