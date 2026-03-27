//! Early kernel bootstrap flow.

use crate::arch;
use crate::bootabi::BootHandoff;
use crate::memory;
use crate::{KernelConfig, PROJECT_NAME, PROJECT_STYLE};

/// Initializes the earliest architecture support and halts in a known-good
/// state so higher layers can be added incrementally.
pub fn bootstrap(config: KernelConfig, handoff: Option<BootHandoff<'_>>) -> ! {
    arch::early_init();
    let kernel_image = memory::kernel_image();
    let pml4 = memory::active_page_table_root();

    crate::kprintln!("{} bootstrap", PROJECT_NAME);
    crate::kprintln!("profile: {}", PROJECT_STYLE);
    crate::kprintln!("arch: {}", arch::CURRENT_ARCH);
    crate::kprintln!(
        "bootstrap_core={} max_cores={} nvme_queue_depth={}",
        config.bootstrap_core.0,
        config.max_cores,
        config.nvme_queue_depth
    );
    crate::kprintln!("stage: descriptor tables loaded");
    crate::kprintln!(
        "memory: kernel_image={:#018x}-{:#018x} size={} KiB",
        kernel_image.start.as_u64(),
        kernel_image.end.as_u64(),
        kernel_image.size_bytes() / 1024
    );
    crate::kprintln!(
        "memory: active_pml4={:#018x} page_size={} bytes",
        pml4.start_address().as_u64(),
        memory::PAGE_SIZE
    );
    match handoff {
        Some(handoff) => {
            crate::kprintln!(
                "memory: boot_map regions={} usable={} MiB top={:#018x}",
                handoff.memory_map().len(),
                handoff.usable_bytes() / (1024 * 1024),
                handoff
                    .highest_physical_address()
                    .map_or(0, |address| address.as_u64())
            );

            let allocator =
                memory::FrameAllocator::new(memory::BootMemoryMap::new(handoff.memory_map()));
            let next_frame = allocator
                .clone()
                .allocate()
                .map_or(0, |frame| frame.start_address().as_u64());

            crate::kprintln!(
                "memory: handoff accepted, first_usable_frame={:#018x}",
                next_frame
            );
        }
        None => crate::kprintln!("memory: no boot handoff present"),
    }
    crate::kprintln!("stage: early bootstrap complete");

    arch::halt_loop()
}
