//! Early kernel bootstrap flow.
#![allow(clippy::items_after_test_module, clippy::similar_names)]

#[cfg(target_os = "none")]
use core::arch::global_asm;
use core::sync::atomic::AtomicU64;

use crate::arch;
use crate::bootabi::BootHandoff;
use crate::capability;
use crate::memory;
use crate::memory::MemoryRegionKind;
use crate::paging;
use crate::runtime_context::{
    BootstrapCoreContext, RuntimeReadinessState, RuntimeReadySummary, RuntimeServiceCommand,
    RuntimeServiceHeartbeat, RuntimeServiceReport, RuntimeServiceState, RuntimeSnapshot,
};
use crate::{KernelConfig, PROJECT_NAME, PROJECT_STYLE};

/// ACPI RSDP physical address forwarded by the loader, stashed for
/// post-bootstrap probes that need to walk ACPI tables. Zero means
/// the loader did not provide one.
static RSDP_PHYS: AtomicU64 = AtomicU64::new(0);

/// Sub-1-MiB physical frame reserved by the loader for the AP boot
/// trampoline. Zero means the loader could not allocate one.
static AP_TRAMPOLINE_PHYS: AtomicU64 = AtomicU64::new(0);

/// Number of pages in the kernel's bootstrap higher-half stack.
///
/// 4 pages (16 KiB) covered everything up through the synchronous NVMe
/// I/O probe; the async-driven read pushed enough state onto the stack
/// (executor + task cell + async-block state machine + nested poll
/// frames) that we hit a double fault inside `core::array::try_from_fn`.
/// 8 pages (32 KiB) gives the runtime comfortable headroom for now.
/// Number of pages in the kernel's higher-half runtime stack (used after
/// the CR3 handoff). 4 pages (16 KiB) covered the synchronous probes;
/// the async-driven path needs more headroom for the executor + task
/// cell + future state machine, so we now reserve 8 pages (32 KiB).
const TRANSITION_STACK_PAGES: u64 = 8;

static mut TRANSITION_ALIAS_STACK_TOP: u64 = 0;
static mut TRANSITION_ALIAS_ENTRY: u64 = 0;
static mut TRANSITION_DATA_ALIAS: u64 = 0;
static mut TRANSITION_GDT_ALIAS: u64 = 0;
static mut TRANSITION_IDT_ALIAS: u64 = 0;
static mut TRANSITION_HANDLER_DELTA: u64 = 0;
static mut TRANSITION_BOOTSTRAP_CORE_ID: u32 = 0;

const TRANSITION_DATA_MAGIC: u64 = 0x4645_4F58_5452_4E31;

#[repr(u64)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootstrapRuntimeStage {
    Prepared = 1,
    IdentityActive = 2,
    AliasActive = 3,
    ExceptionValidated = 4,
    RuntimeActive = 5,
}

impl BootstrapRuntimeStage {
    const fn as_u64(self) -> u64 {
        self as u64
    }

    const fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            1 => Some(Self::Prepared),
            2 => Some(Self::IdentityActive),
            3 => Some(Self::AliasActive),
            4 => Some(Self::ExceptionValidated),
            5 => Some(Self::RuntimeActive),
            _ => None,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::IdentityActive => "identity-active",
            Self::AliasActive => "alias-active",
            Self::ExceptionValidated => "exception-validated",
            Self::RuntimeActive => "runtime-active",
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct BootstrapRuntimeState {
    magic: u64,
    active_root: u64,
    kernel_window_base: u64,
    kernel_window_end: u64,
    kernel_pages_mapped: u64,
    identity_stack: u64,
    alias_stack: u64,
    stack_pages: u64,
    data_page: u64,
    alias_entry: u64,
    alias_gdt: u64,
    alias_idt: u64,
    stage: u64,
}

impl BootstrapRuntimeState {
    fn stage(self) -> Option<BootstrapRuntimeStage> {
        BootstrapRuntimeStage::from_raw(self.stage)
    }
}

impl RuntimeSnapshot {
    fn from_state(state: BootstrapRuntimeState) -> Option<Self> {
        if state.magic != TRANSITION_DATA_MAGIC {
            return None;
        }

        Some(Self {
            active_root: state.active_root,
            kernel_window_base: state.kernel_window_base,
            kernel_window_end: state.kernel_window_end,
            kernel_pages_mapped: state.kernel_pages_mapped,
            identity_stack: state.identity_stack,
            alias_stack: state.alias_stack,
            stack_pages: state.stack_pages,
            data_page: state.data_page,
            alias_entry: state.alias_entry,
            alias_gdt: state.alias_gdt,
            alias_idt: state.alias_idt,
            stage: state
                .stage()
                .map_or("unknown", BootstrapRuntimeStage::label),
        })
    }
}

#[cfg(target_os = "none")]
global_asm!(
    ".global feox_transition_entry",
    "feox_transition_entry:",
    "mov dx, 0x402",
    "mov al, 0x5b",
    "out dx, al",
    "mov al, 0x54",
    "out dx, al",
    "mov al, 0x5d",
    "out dx, al",
    "jmp {rust_entry}",
    rust_entry = sym transition_stage_entry_rust,
);

#[cfg(target_os = "none")]
unsafe extern "C" {
    fn feox_transition_entry() -> !;
}

#[cfg(not(target_os = "none"))]
extern "C" fn feox_transition_entry() -> ! {
    transition_stage_entry_rust()
}

extern "C" fn transition_stage_entry_rust() -> ! {
    let alias_data_address = unsafe { TRANSITION_DATA_ALIAS };
    crate::kprintln!(
        "paging: transition_handoff active_root={:#018x}",
        memory::active_page_table_root().start_address().as_u64()
    );
    crate::kprintln!(
        "paging: transition_handoff_stack={:#018x}",
        arch::current_stack_pointer()
    );
    crate::runtime_context::push_event("transition-handoff-entered");
    let alias_stack_top = unsafe { TRANSITION_ALIAS_STACK_TOP };
    let alias_entry = unsafe { TRANSITION_ALIAS_ENTRY };
    if alias_data_address != 0 {
        let transition_data = unsafe {
            // SAFETY: the transition data page is kernel-owned bootstrap memory
            // that was explicitly mapped into the active transition root before
            // the CR3 handoff.
            &mut *(alias_data_address as *mut BootstrapRuntimeState)
        };
        crate::kprintln!(
            "paging: runtime_state magic={:#018x} stage={} kernel_window={:#018x}-{:#018x} pages={}",
            transition_data.magic,
            transition_data
                .stage()
                .map_or("unknown", BootstrapRuntimeStage::label),
            transition_data.kernel_window_base,
            transition_data.kernel_window_end,
            transition_data.kernel_pages_mapped,
        );
        transition_data.stage = BootstrapRuntimeStage::IdentityActive.as_u64();
        crate::runtime_context::push_event("identity-handoff-active");
        crate::runtime_context::store_core(BootstrapCoreContext {
            core_id: feox_asi::CoreId(unsafe { TRANSITION_BOOTSTRAP_CORE_ID }),
            active_root: memory::active_page_table_root().start_address().as_u64(),
            stack_pointer: arch::current_stack_pointer(),
            alias_entry,
            stage: BootstrapRuntimeStage::IdentityActive.label(),
        });
    }
    if alias_stack_top != 0 && alias_entry != 0 && arch::current_stack_pointer() != alias_stack_top
    {
        crate::kprintln!(
            "stage: switching to transition alias stack={:#018x} entry={:#018x}",
            alias_stack_top,
            alias_entry
        );
        unsafe {
            // SAFETY: the alias stack top was prepared and mapped in the active
            // transition root before the CR3 handoff, and the alias entry is
            // a prepared high-half mapping of the same kernel image.
            arch::switch_stack_and_jump(alias_stack_top, alias_entry)
        }
    }
    transition_high_stack_entry()
}

extern "C" fn transition_high_stack_entry() -> ! {
    let alias_data_address = unsafe { TRANSITION_DATA_ALIAS };
    let alias_gdt_base = unsafe { TRANSITION_GDT_ALIAS };
    let alias_idt_base = unsafe { TRANSITION_IDT_ALIAS };
    let alias_handler_delta = unsafe { TRANSITION_HANDLER_DELTA };
    crate::kprintln!(
        "paging: transition_alias_stack active_root={:#018x}",
        memory::active_page_table_root().start_address().as_u64()
    );
    crate::kprintln!(
        "paging: transition_alias_stack_pointer={:#018x}",
        arch::current_stack_pointer()
    );
    if alias_data_address != 0 {
        let transition_data = unsafe {
            // SAFETY: the transition data page remains mapped in the active
            // transition root and is owned by the bootstrap kernel path.
            &mut *(alias_data_address as *mut BootstrapRuntimeState)
        };
        crate::kprintln!(
            "paging: runtime_state_post_switch magic={:#018x} stage={} identity_stack={:#018x} alias_stack={:#018x}",
            transition_data.magic,
            transition_data
                .stage()
                .map_or("unknown", BootstrapRuntimeStage::label),
            transition_data.identity_stack,
            transition_data.alias_stack
        );
        transition_data.stage = BootstrapRuntimeStage::AliasActive.as_u64();
        crate::kprintln!(
            "paging: runtime_layout window={:#018x}-{:#018x} entry={:#018x} gdt={:#018x} idt={:#018x} stack_pages={} data_page={:#018x}",
            transition_data.kernel_window_base,
            transition_data.kernel_window_end,
            transition_data.alias_entry,
            transition_data.alias_gdt,
            transition_data.alias_idt,
            transition_data.stack_pages,
            transition_data.data_page,
        );
        crate::runtime_context::push_event("higher-half-alias-active");
        if let Some(snapshot) = RuntimeSnapshot::from_state(*transition_data) {
            crate::runtime_context::store(snapshot);
        }
        crate::runtime_context::store_core(BootstrapCoreContext {
            core_id: feox_asi::CoreId(unsafe { TRANSITION_BOOTSTRAP_CORE_ID }),
            active_root: memory::active_page_table_root().start_address().as_u64(),
            stack_pointer: arch::current_stack_pointer(),
            alias_entry: transition_data.alias_entry,
            stage: BootstrapRuntimeStage::AliasActive.label(),
        });
    }
    if alias_gdt_base != 0 && alias_idt_base != 0 {
        unsafe {
            // SAFETY: both descriptor tables live inside the retained kernel
            // image mapping, and the handler delta retargets the bootstrap
            // exception stubs into the same high-half image window.
            arch::reload_descriptor_tables(alias_gdt_base, alias_idt_base, alias_handler_delta);
        }
        crate::kprintln!(
            "paging: transition_descriptors gdt={:#018x} idt={:#018x} handler_delta={:#018x}",
            alias_gdt_base,
            alias_idt_base,
            alias_handler_delta
        );
        crate::runtime_context::push_event("descriptor-tables-reloaded");
        crate::kprintln!("stage: validating higher-half exception path");
        if alias_data_address != 0 {
            let transition_data = unsafe {
                // SAFETY: the bootstrap runtime state remains mapped in the
                // active transition root while this validation runs.
                &mut *(alias_data_address as *mut BootstrapRuntimeState)
            };
            transition_data.stage = BootstrapRuntimeStage::ExceptionValidated.as_u64();
            if let Some(snapshot) = RuntimeSnapshot::from_state(*transition_data) {
                crate::runtime_context::store(snapshot);
            }
            crate::runtime_context::store_core(BootstrapCoreContext {
                core_id: feox_asi::CoreId(unsafe { TRANSITION_BOOTSTRAP_CORE_ID }),
                active_root: memory::active_page_table_root().start_address().as_u64(),
                stack_pointer: arch::current_stack_pointer(),
                alias_entry: transition_data.alias_entry,
                stage: BootstrapRuntimeStage::ExceptionValidated.label(),
            });
        }
        crate::runtime_context::push_event("higher-half-exception-validation");
        arch::trigger_breakpoint();
        if alias_data_address != 0 {
            let transition_data = unsafe {
                // SAFETY: the bootstrap runtime state remains mapped in the
                // active transition root after the breakpoint returns.
                &mut *(alias_data_address as *mut BootstrapRuntimeState)
            };
            transition_data.stage = BootstrapRuntimeStage::RuntimeActive.as_u64();
            if let Some(snapshot) = RuntimeSnapshot::from_state(*transition_data) {
                crate::runtime_context::store(snapshot);
            }
            crate::runtime_context::store_core(BootstrapCoreContext {
                core_id: feox_asi::CoreId(unsafe { TRANSITION_BOOTSTRAP_CORE_ID }),
                active_root: memory::active_page_table_root().start_address().as_u64(),
                stack_pointer: arch::current_stack_pointer(),
                alias_entry: transition_data.alias_entry,
                stage: BootstrapRuntimeStage::RuntimeActive.label(),
            });
        }
        crate::runtime_context::push_event("higher-half-runtime-active");
        crate::kprintln!("stage: higher-half runtime active");
        runtime_active_entry();
    } else {
        crate::kprintln!("paging: transition_descriptors unavailable");
    }
    crate::kprintln!("stage: transition handoff complete");
    arch::halt_loop()
}

#[cfg(target_os = "none")]
fn run_vm_access_probe() {
    let active_root = arch::active_page_table_root();
    let active_root_via_direct = memory::DIRECT_MAP_BASE.wrapping_add(active_root);
    crate::kprintln!(
        "vm-probe: start active_root={:#018x} direct_map_alias={:#018x}",
        active_root,
        active_root_via_direct
    );

    // Direct-map sanity: read the first PML4 entry of the active root through
    // the direct map. If the direct map is not live this faults; if it is,
    // we get the same value that the page-table walk would return.
    let pml4_entry_0: u64 =
        unsafe { core::ptr::read_volatile(active_root_via_direct as *const u64) };
    crate::kprintln!("vm-probe: direct_map pml4[0]={:#018x}", pml4_entry_0);

    // Exercise the full mem_map / mem_vtop / mem_unmap cycle through the live
    // VM lane, now backed by DirectMapPageTables. This is the validation
    // target named in docs/PAGE_TABLE_ACCESS_PLAN.md.
    run_vm_mem_cycle_probe();

    // Exercise the MMIO zone bring-up: map a known device phys (LAPIC base
    // is the canonical x86 MMIO sentinel), verify the page-table walk
    // returns the right phys and that the cache-disable bit is set, then
    // unmap.
    run_mmio_cycle_probe();

    // Stand up the per-core data area for core 0. After this call,
    // kernel code can use `crate::per_core::current()` (a GS-relative
    // load on x86_64) to reach its own per-CPU block. Single-core
    // only for now; SMP bring-up of secondary cores is a follow-up.
    run_per_core_probe();

    // Walk the ACPI MADT (if firmware provided an RSDP) and report
    // the secondary-processor topology. APs aren't started yet here —
    // this discovery pass returns the LAPIC list used by
    // `run_ap_boot_probe` below.
    let topology = run_acpi_smp_probe();

    // Send INIT-SIPI-SIPI to the first non-BSP LAPIC the MADT
    // reported, place a tiny 16-bit trampoline at the loader-
    // allocated sub-1-MiB frame, and watch for the trampoline's
    // magic word. Validates the LAPIC IPI plumbing without going as
    // far as a Rust ap_entry.
    if let Some(topology) = topology {
        run_ap_boot_probe(&topology);
    }

    // Optional: scan for an NVMe controller, map its BAR0 through the
    // MMIO lane, and read the CAP+VS registers through it. Prints
    // "no controller" and continues if PCI has no NVMe device.
    run_nvme_mmio_probe();

    crate::kprintln!("vm-probe: complete");
}

#[cfg(target_os = "none")]
fn run_per_core_probe() {
    match crate::per_core::initialize_core0() {
        Ok(handle) => {
            crate::kprintln!(
                "per-core-probe: core 0 initialized (cap id={}, gen={})",
                handle.id,
                handle.generation
            );
            let area = crate::per_core::current();
            // Validate via the GS-relative path: load through `gs:[0]`
            // (current()), then re-load self_ptr to confirm it matches
            // what `gs:[0]` already returned.
            let self_ptr = area.self_ptr;
            if area.magic != crate::per_core::PER_CORE_MAGIC {
                crate::kprintln!(
                    "per-core-probe: FAIL bad magic {:#018x}",
                    area.magic
                );
                return;
            }
            if area.core_id != 0 {
                crate::kprintln!("per-core-probe: FAIL core_id={} (expected 0)", area.core_id);
                return;
            }
            if (self_ptr as u64) != (area as *const _ as u64) {
                crate::kprintln!("per-core-probe: FAIL self_ptr mismatch");
                return;
            }
            crate::kprintln!(
                "per-core-probe: ok via GS (core_id={}, self_ptr={:p})",
                area.core_id,
                self_ptr
            );
        }
        Err(msg) => {
            crate::kprintln!("per-core-probe: {}", msg);
        }
    }
}

#[cfg(target_os = "none")]
fn run_acpi_smp_probe() -> Option<crate::acpi::AcpiTopology> {
    let rsdp = RSDP_PHYS.load(core::sync::atomic::Ordering::Acquire);
    if rsdp == 0 {
        crate::kprintln!("acpi-smp-probe: skipped (no rsdp from loader)");
        return None;
    }
    let topology = match unsafe { crate::acpi::parse_topology(rsdp) } {
        Ok(t) => t,
        Err(err) => {
            crate::kprintln!("acpi-smp-probe: parse failed err={:?}", err);
            return None;
        }
    };
    crate::kprintln!(
        "acpi-smp-probe: lapics={} enabled={} local_apic_addr={:#010x}",
        topology.lapic_count,
        topology.enabled_count(),
        topology.local_apic_address
    );
    let mut i = 0usize;
    while i < topology.lapic_count {
        let entry = topology.lapics[i];
        crate::kprintln!(
            "acpi-smp-probe:   cpu[{}] uid={} apic_id={} enabled={} online_capable={}",
            i,
            entry.processor_uid,
            entry.apic_id,
            entry.is_enabled() as u8,
            entry.is_online_capable() as u8
        );
        i += 1;
    }
    Some(topology)
}

#[cfg(target_os = "none")]
fn run_ap_boot_probe(topology: &crate::acpi::AcpiTopology) {
    if topology.lapic_count < 2 {
        crate::kprintln!("ap-boot-probe: skipped (no secondary APs reported)");
        return;
    }
    // The BSP is conventionally apic_id 0; pick the first reported
    // LAPIC with a different ID. (QEMU enumerates BSP first, so this
    // is `lapics[1]` in practice, but we don't assume the order.)
    let bsp = match crate::lapic::read_id() {
        Ok(_) => 0_u8,
        Err(_) => 0_u8,
    };
    let target = topology
        .lapics()
        .iter()
        .find(|e| e.is_enabled() && e.apic_id != bsp);
    let Some(target) = target else {
        crate::kprintln!("ap-boot-probe: no enabled non-BSP LAPIC");
        return;
    };

    if let Err(err) = crate::lapic::initialize(u64::from(topology.local_apic_address)) {
        crate::kprintln!("ap-boot-probe: lapic::initialize err={:?}", err);
        return;
    }
    match (crate::lapic::read_id(), crate::lapic::read_version()) {
        (Ok(id), Ok(ver)) => crate::kprintln!(
            "ap-boot-probe: lapic id={} version={:#010x}",
            id,
            ver
        ),
        (id, ver) => crate::kprintln!(
            "ap-boot-probe: lapic read err id={:?} ver={:?}",
            id,
            ver
        ),
    }

    let tramp = AP_TRAMPOLINE_PHYS.load(core::sync::atomic::Ordering::Acquire);
    if tramp == 0 {
        crate::kprintln!("ap-boot-probe: skipped (loader did not allocate trampoline frame)");
        return;
    }
    crate::kprintln!(
        "ap-boot-probe: target_apic_id={} trampoline_phys={:#018x} sipi_vector={:#04x}",
        target.apic_id,
        tramp,
        (tramp >> 12) as u8
    );
    match crate::smp::bring_up_first_ap(tramp, target.apic_id) {
        Ok(()) => crate::kprintln!("ap-boot-probe: AP alive (magic observed)"),
        Err(err) => crate::kprintln!("ap-boot-probe: bring_up_first_ap err={:?}", err),
    }
}

#[cfg(target_os = "none")]
fn run_nvme_mmio_probe() {
    use crate::memory::{PAGE_SIZE, PhysicalAddress, VirtualAddress};
    use crate::mmio::{mmio_map_bootstrap, mmio_unmap_bootstrap};
    use crate::nvme::ControllerRegisters;
    use crate::paging::{DirectMapPageTables, PageTableRoot};
    use crate::pci::{PCI_CLASS_NVME, bar64, scan_for_class};

    let device = match scan_for_class(PCI_CLASS_NVME) {
        Some(device) => device,
        None => {
            crate::kprintln!("nvme-mmio-probe: no NVMe controller found");
            return;
        }
    };
    crate::kprintln!(
        "nvme-mmio-probe: found {:02x}:{:02x}.{} vid={:#06x} did={:#06x} class={:#08x}",
        device.bus,
        device.device,
        device.function,
        device.vendor_id,
        device.device_id,
        device.class_code
    );

    let bar_phys = bar64(&device, 0);
    if bar_phys == 0 {
        crate::kprintln!("nvme-mmio-probe: BAR0 is zero, controller not configured");
        return;
    }
    crate::kprintln!("nvme-mmio-probe: BAR0 phys={:#018x}", bar_phys);

    // Map the first 8 KiB of the BAR — controller registers (page 0) plus
    // the admin doorbell page (page 1, offset 0x1000 from BAR base).
    let region = match mmio_map_bootstrap(
        PhysicalAddress::new(bar_phys),
        2 * PAGE_SIZE,
        true,
        true,
    ) {
        Ok(region) => region,
        Err(_) => {
            crate::kprintln!("nvme-mmio-probe: BAR mmio_map failed");
            return;
        }
    };
    crate::kprintln!(
        "nvme-mmio-probe: BAR mapped at virt={:#018x}",
        region.virtual_base
    );

    // Walk the active root to confirm the mapping leaf reflects what the
    // MMIO helper installed (phys + UC flags).
    let root = PageTableRoot::active();
    let translation = root.translate_with(
        &DirectMapPageTables,
        VirtualAddress::new(region.virtual_base),
    );
    match translation {
        Ok(Some(t)) => crate::kprintln!(
            "nvme-mmio-probe: walk phys={:#018x} cache_disabled={} write_through={}",
            t.physical_address.as_u64(),
            t.entry.is_cache_disabled(),
            t.entry.is_write_through()
        ),
        _ => crate::kprintln!("nvme-mmio-probe: walk returned no leaf"),
    }

    // Read CAP and VS through the controller register reader.
    let registers = unsafe {
        // SAFETY: the MMIO mapping we just installed covers BAR0's
        // first 8 KiB with UC caching. The pointer stays valid until
        // we unmap below.
        ControllerRegisters::new(region.virtual_base as *mut u8)
    };
    let cap = registers.cap();
    let vs = registers.vs();
    crate::kprintln!(
        "nvme-mmio-probe: CAP={:#018x} mqes={} dstrd={} mpsmin={} mpsmax={}",
        cap.0,
        cap.mqes(),
        cap.dstrd(),
        cap.mpsmin(),
        cap.mpsmax()
    );
    crate::kprintln!(
        "nvme-mmio-probe: VS={:#010x} version={}.{}.{}",
        vs.0,
        vs.major(),
        vs.minor(),
        vs.tertiary()
    );

    // Push into the admin queue: reset → configure → enable → submit
    // Identify Controller → poll completion → decode model/serial.
    // QEMU's NVMe controller advertises DSTRD=0, so the admin doorbell
    // helpers map directly onto 0x1000/0x1004.
    if cap.dstrd() != 0 {
        crate::kprintln!(
            "nvme-admin-probe: skipped — DSTRD={} not 0 (helpers assume 4-byte doorbells)",
            cap.dstrd()
        );
    } else {
        run_nvme_admin_probe(registers, bar_phys, 2 * PAGE_SIZE);
    }

    if mmio_unmap_bootstrap(region).is_err() {
        crate::kprintln!("nvme-mmio-probe: BAR mmio_unmap failed");
        return;
    }
    crate::kprintln!("nvme-mmio-probe: full NVMe BAR cycle ok");
}

#[cfg(target_os = "none")]
struct NvmeProbeQueueState {
    sq_tail: u16,
    cq_head: u16,
    phase: u8,
}

#[cfg(target_os = "none")]
impl NvmeProbeQueueState {
    const fn new() -> Self {
        // The controller writes its first CQE with phase = 1 (the host
        // initialized the queue to zeros, which is phase = 0).
        Self {
            sq_tail: 0,
            cq_head: 0,
            phase: 1,
        }
    }
}

#[cfg(target_os = "none")]
fn nvme_submit_and_wait(
    registers: crate::nvme::ControllerRegisters,
    sq_virt: *mut crate::nvme::SubmissionQueueEntry,
    cq_virt: *mut crate::nvme::CompletionQueueEntry,
    qid: u16,
    sq_entries: u16,
    cq_entries: u16,
    state: &mut NvmeProbeQueueState,
    entry: crate::nvme::SubmissionQueueEntry,
    poll_limit: u32,
) -> Option<crate::nvme::CompletionQueueEntry> {
    unsafe {
        // SAFETY: sq_virt is the freshly allocated SQ page (direct map);
        // we own slot `sq_tail`.
        let slot = sq_virt.add(usize::from(state.sq_tail));
        core::ptr::write_volatile(slot, entry);
    }
    state.sq_tail = (state.sq_tail + 1) % sq_entries;
    registers.ring_sq_tail_doorbell(qid, state.sq_tail);

    let mut waited = 0_u32;
    loop {
        let cqe = unsafe {
            // SAFETY: cq_virt is the freshly allocated CQ page (direct map);
            // the controller writes CQEs into it via DMA.
            core::ptr::read_volatile(cq_virt.add(usize::from(state.cq_head)))
        };
        if cqe.phase() == state.phase {
            state.cq_head += 1;
            if state.cq_head >= cq_entries {
                state.cq_head = 0;
                state.phase ^= 1;
            }
            registers.ring_cq_head_doorbell(qid, state.cq_head);
            return Some(cqe);
        }
        waited += 1;
        if waited >= poll_limit {
            return None;
        }
        core::hint::spin_loop();
    }
}

#[cfg(target_os = "none")]
fn run_nvme_admin_probe(
    registers: crate::nvme::ControllerRegisters,
    bar_phys: u64,
    bar_size: u64,
) {
    use crate::capability::{request_bootstrap_capability, resource, verify_bootstrap_handle};
    use crate::memory::DIRECT_MAP_BASE;
    use crate::nvme::{CompletionQueueEntry, SubmissionQueueEntry};
    use feox_asi::{CapPermissions, CapRequest, PageFlags};

    // Bound for CSTS / CQ polling loops. QEMU's NVMe is essentially
    // instant; this just guards against a wedged controller.
    const POLL_LIMIT: u32 = 1_000_000;
    const ADMIN_ENTRIES: u16 = 8;
    const IO_QUEUE_ID: u16 = 1;
    const IO_ENTRIES: u16 = 8;
    const NSID: u32 = 1;

    fn alloc_dma_page() -> Option<(u64, *mut u8)> {
        let handle = request_bootstrap_capability(&CapRequest::PhysicalPages {
            num_pages: 1,
            flags: PageFlags::CONTIGUOUS,
        })
        .ok()?;
        let view =
            verify_bootstrap_handle(handle, CapPermissions::READ | CapPermissions::WRITE).ok()?;
        let res = resource(view.resource_id)?;
        let phys = res.base.0;
        let virt = DIRECT_MAP_BASE.wrapping_add(phys) as *mut u8;
        unsafe {
            // SAFETY: virt is a kernel-only direct-map alias of a freshly
            // minted physical page; nothing else holds a pointer to it.
            core::ptr::write_bytes(virt, 0, 4096);
        }
        Some((phys, virt))
    }

    // Allocate every DMA buffer up front: admin SQ/CQ, identify scratch,
    // I/O SQ/CQ, and the read data buffer.
    let admin_buffers = (alloc_dma_page(), alloc_dma_page(), alloc_dma_page());
    let io_buffers = (alloc_dma_page(), alloc_dma_page(), alloc_dma_page());
    let (
        Some((admin_sq_phys, admin_sq_virt)),
        Some((admin_cq_phys, admin_cq_virt)),
        Some((id_phys, id_virt)),
    ) = admin_buffers
    else {
        crate::kprintln!("nvme-admin-probe: failed to allocate admin buffers");
        return;
    };
    let (
        Some((io_sq_phys, io_sq_virt)),
        Some((io_cq_phys, io_cq_virt)),
        Some((read_phys, read_virt)),
    ) = io_buffers
    else {
        crate::kprintln!("nvme-admin-probe: failed to allocate I/O buffers");
        return;
    };
    crate::kprintln!(
        "nvme-admin-probe: admin SQ={:#018x} CQ={:#018x} ID={:#018x}",
        admin_sq_phys,
        admin_cq_phys,
        id_phys
    );
    crate::kprintln!(
        "nvme-admin-probe: io SQ={:#018x} CQ={:#018x} read_buf={:#018x}",
        io_sq_phys,
        io_cq_phys,
        read_phys
    );

    // Reset.
    registers.set_cc(0);
    let mut waited = 0_u32;
    while registers.csts().ready() {
        waited += 1;
        if waited >= POLL_LIMIT {
            crate::kprintln!("nvme-admin-probe: timeout waiting CSTS.RDY=0");
            return;
        }
        core::hint::spin_loop();
    }
    crate::kprintln!("nvme-admin-probe: controller reset (CSTS.RDY=0)");

    // Configure admin queues and program CC.
    registers.set_aqa(ADMIN_ENTRIES, ADMIN_ENTRIES);
    registers.set_asq(admin_sq_phys);
    registers.set_acq(admin_cq_phys);
    // CC bit layout (NVMe 1.4 §3.1.6): IOCQES at bits 23:20, IOSQES at
    // bits 19:16, AMS at 13:11, MPS at 10:7, CSS at 6:4, EN at bit 0.
    // QEMU additionally rejects Create I/O CQ if IOSQES != 6 or IOCQES != 4,
    // returning NVME_MAX_QSIZE_EXCEEDED (a misleading name).
    let cc_value = (4_u32 << 20) | (6_u32 << 16) | 1; // IOCQES=4, IOSQES=6, EN=1
    registers.set_cc(cc_value);

    let mut waited = 0_u32;
    loop {
        let csts = registers.csts();
        if csts.fatal() {
            crate::kprintln!("nvme-admin-probe: CSTS.CFS set, controller failed");
            return;
        }
        if csts.ready() {
            break;
        }
        waited += 1;
        if waited >= POLL_LIMIT {
            crate::kprintln!("nvme-admin-probe: timeout waiting CSTS.RDY=1");
            return;
        }
        core::hint::spin_loop();
    }
    crate::kprintln!("nvme-admin-probe: controller enabled (CSTS.RDY=1)");

    let admin_sq = admin_sq_virt.cast::<SubmissionQueueEntry>();
    let admin_cq = admin_cq_virt.cast::<CompletionQueueEntry>();
    let mut admin = NvmeProbeQueueState::new();

    // ---- Identify Controller ----
    let cqe = match nvme_submit_and_wait(
        registers,
        admin_sq,
        admin_cq,
        0,
        ADMIN_ENTRIES,
        ADMIN_ENTRIES,
        &mut admin,
        SubmissionQueueEntry::identify_controller(id_phys, 1),
        POLL_LIMIT,
    ) {
        Some(cqe) => cqe,
        None => {
            crate::kprintln!("nvme-admin-probe: Identify Controller timeout");
            return;
        }
    };
    if cqe.command_id() != 1 || cqe.status_field() != 0 {
        crate::kprintln!(
            "nvme-admin-probe: FAIL Identify cid={} status={:#x}",
            cqe.command_id(),
            cqe.status_field()
        );
        return;
    }
    let id_bytes = unsafe {
        // SAFETY: id_virt aliases the freshly written identify page.
        core::slice::from_raw_parts(id_virt as *const u8, 4096)
    };
    fn trim_ascii(bytes: &[u8]) -> &str {
        core::str::from_utf8(bytes)
            .unwrap_or("?")
            .trim_end_matches(|c: char| c == ' ' || c == '\0')
    }
    crate::kprintln!(
        "nvme-admin-probe: model='{}' serial='{}' firmware='{}'",
        trim_ascii(&id_bytes[24..64]),
        trim_ascii(&id_bytes[4..24]),
        trim_ascii(&id_bytes[64..72]),
    );

    // ---- Create I/O Completion Queue ----
    let cqe = match nvme_submit_and_wait(
        registers,
        admin_sq,
        admin_cq,
        0,
        ADMIN_ENTRIES,
        ADMIN_ENTRIES,
        &mut admin,
        SubmissionQueueEntry::create_io_completion_queue(IO_QUEUE_ID, IO_ENTRIES, io_cq_phys, 2),
        POLL_LIMIT,
    ) {
        Some(cqe) => cqe,
        None => {
            crate::kprintln!("nvme-admin-probe: Create I/O CQ timeout");
            return;
        }
    };
    if cqe.status_field() != 0 {
        crate::kprintln!(
            "nvme-admin-probe: FAIL Create I/O CQ status={:#x}",
            cqe.status_field()
        );
        return;
    }
    crate::kprintln!("nvme-io-probe: created I/O CQ {}", IO_QUEUE_ID);

    // ---- Create I/O Submission Queue ----
    let cqe = match nvme_submit_and_wait(
        registers,
        admin_sq,
        admin_cq,
        0,
        ADMIN_ENTRIES,
        ADMIN_ENTRIES,
        &mut admin,
        SubmissionQueueEntry::create_io_submission_queue(
            IO_QUEUE_ID,
            IO_ENTRIES,
            IO_QUEUE_ID,
            io_sq_phys,
            3,
        ),
        POLL_LIMIT,
    ) {
        Some(cqe) => cqe,
        None => {
            crate::kprintln!("nvme-admin-probe: Create I/O SQ timeout");
            return;
        }
    };
    if cqe.status_field() != 0 {
        crate::kprintln!(
            "nvme-admin-probe: FAIL Create I/O SQ status={:#x}",
            cqe.status_field()
        );
        return;
    }
    crate::kprintln!("nvme-io-probe: created I/O SQ {}", IO_QUEUE_ID);

    // ---- Submit NVM Read of LBA 0 through the kernel block API ----
    // The block module owns the live NVMe device state. The boot probe
    // brings the controller up via the admin queue, then hands the I/O
    // queues to `block::initialize`; the async task calls `block::read`
    // and `.await`s its result. The drive loop calls `block::drain` to
    // pump CQEs (which wake the future via the bound waker dispatcher).
    if let Err(err) = crate::block::initialize(crate::block::BlockDeviceConfig {
        registers,
        sq_virt: io_sq_virt.cast::<SubmissionQueueEntry>(),
        cq_virt: io_cq_virt.cast::<CompletionQueueEntry>(),
        sq_entries: IO_ENTRIES,
        cq_entries: IO_ENTRIES,
        io_qid: IO_QUEUE_ID,
    }) {
        crate::kprintln!("nvme-async-probe: block::initialize err={:?}", err);
        return;
    }

    // Mint the storage device capability so dispatch_storage_submit_read
    // has something to verify args.device against. Released during
    // block::shutdown.
    let device_cap = match crate::block::register_device_capability(bar_phys, bar_size) {
        Ok(handle) => {
            crate::kprintln!(
                "nvme-async-probe: device cap minted (id={}, gen={})",
                handle.id,
                handle.generation
            );
            handle
        }
        Err(err) => {
            crate::kprintln!("nvme-async-probe: device cap mint failed err={:?}", err);
            crate::block::shutdown();
            return;
        }
    };

    // Spawn the background drainer task on the same executor. It pumps
    // CQ completions every poll pass and yields back so other tasks can
    // run. Without it the read task would park forever (the executor
    // re-enqueues waker fires, but nothing would drain the CQ in the
    // first place to fire those wakers).
    let drainer_cell = feox_async::TaskCell::new(feox_asi::CoreId(0));
    let drainer_header = drainer_cell
        .spawn(crate::block::drainer_task())
        .expect("drainer spawn");

    let read_phys_for_task = read_phys;
    let read_virt_for_task = read_virt;
    let cell = feox_async::TaskCell::new(feox_asi::CoreId(0));
    let header = cell
        .spawn(async move {
            crate::kprintln!("nvme-async-probe: submitting NVM Read via block::read");
            let future = match crate::block::read(NSID, 0, read_phys_for_task) {
                Ok(future) => future,
                Err(err) => {
                    crate::kprintln!("nvme-async-probe: submit err={:?}", err);
                    return;
                }
            };
            let result = future.await;
            match result {
                Ok(completion) => {
                    crate::kprintln!(
                        "nvme-async-probe: completion cid={} sct={} sc={} dnr={}",
                        completion.cid,
                        completion.status.sct,
                        completion.status.sc,
                        completion.status.dnr
                    );
                    if !completion.succeeded() {
                        crate::kprintln!("nvme-async-probe: FAIL non-success status");
                        return;
                    }
                    // Decode + print the data the controller DMA'd in.
                    let read_bytes = unsafe {
                        // SAFETY: read_virt_for_task aliases the freshly
                        // DMA'd buffer page (direct map).
                        core::slice::from_raw_parts(read_virt_for_task as *const u8, 32)
                    };
                    let mut hex_buf = [0u8; 64];
                    let mut idx = 0usize;
                    while idx < 32 {
                        let high = read_bytes[idx] >> 4;
                        let low = read_bytes[idx] & 0x0F;
                        hex_buf[idx * 2] =
                            if high < 10 { b'0' + high } else { b'a' + high - 10 };
                        hex_buf[idx * 2 + 1] =
                            if low < 10 { b'0' + low } else { b'a' + low - 10 };
                        idx += 1;
                    }
                    let hex_str = core::str::from_utf8(&hex_buf).unwrap_or("?");
                    crate::kprintln!(
                        "nvme-async-probe: LBA0 ascii='{}' hex={}",
                        trim_ascii(&read_bytes[..20]),
                        hex_str
                    );
                }
                Err(err) => {
                    crate::kprintln!("nvme-async-probe: future err={:?}", err);
                }
            }
        })
        .expect("task cell spawn");

    let mut executor = feox_async::SingleCoreExecutor::<4>::new();
    if !executor.enqueue(drainer_header) {
        crate::kprintln!("nvme-async-probe: executor queue full (drainer)");
        return;
    }
    if !executor.enqueue(header) {
        crate::kprintln!("nvme-async-probe: executor queue full (read)");
        return;
    }

    // Drive loop: `poll_one` one task per pass and stop once the read
    // task is complete. `run_until_idle` would never return because the
    // drainer self-wakes on every poll. Bound the loop so a wedged
    // controller doesn't hang boot.
    let mut polls = 0_u64;
    loop {
        // SAFETY: both task cells live on this stack frame and aren't
        // moved; their headers remain valid for the duration of this
        // loop.
        let progressed = unsafe { executor.poll_one() };
        polls += 1;
        if unsafe { header.as_ref().state() } == feox_async::TaskState::Complete {
            break;
        }
        if !progressed {
            crate::kprintln!(
                "nvme-async-probe: executor unexpectedly idle at poll {}",
                polls
            );
            return;
        }
        if polls >= u64::from(POLL_LIMIT) {
            crate::kprintln!("nvme-async-probe: stuck after {} polls", polls);
            return;
        }
    }
    crate::kprintln!("nvme-async-probe: task complete (polls={})", polls);

    // ---- Storage ABI self-test ----
    // Exercise the syscall dispatch path end-to-end. Walks through
    // feox_syscall_dispatch with AsiOp::StorageSubmitRead and
    // AsiOp::StoragePoll the same way a ring-3 caller would. The block
    // layer is still up (shutdown happens after the I/O queue teardown
    // below) so the submission table sees a live device.
    // Request a fresh PhysicalPages capability for the self-test
    // buffer. v1 takes a `buffer: CapHandle, buffer_offset: u64` instead
    // of a raw phys, so we need the cap handle directly (not just the
    // phys that `alloc_dma_page` exposes).
    let abi_buffer_cap = request_bootstrap_capability(&CapRequest::PhysicalPages {
        num_pages: 1,
        flags: PageFlags::CONTIGUOUS,
    });
    if let Ok(buffer_handle) = abi_buffer_cap {
        // Look up the backing phys + virt for our own buffer verification
        // (the kernel side will translate via cap_to_phys_base, but the
        // self-test needs a kernel-visible alias to decode the bytes).
        let buffer_view =
            verify_bootstrap_handle(buffer_handle, CapPermissions::READ | CapPermissions::WRITE)
                .expect("storage-abi-probe: verify own buffer handle");
        let buffer_res = resource(buffer_view.resource_id)
            .expect("storage-abi-probe: lookup buffer resource");
        let abi_phys = buffer_res.base.0;
        let abi_virt = DIRECT_MAP_BASE.wrapping_add(abi_phys) as *mut u8;
        unsafe {
            // SAFETY: virt aliases the freshly minted physical page; no
            // other live pointer references it.
            core::ptr::write_bytes(abi_virt, 0, 4096);
        }
        crate::kprintln!(
            "storage-abi-probe: buffer cap={{id={}, gen={}}} phys={:#018x}",
            buffer_handle.id,
            buffer_handle.generation,
            abi_phys
        );
        // ---- Negative path: bogus device cap must be rejected ----
        let neg_args = feox_asi::StorageSubmitReadArgs {
            device: feox_asi::CapHandle {
                id: 0,
                generation: 0,
            },
            nsid: NSID,
            lba: 0,
            block_count: 1,
            _reserved: 0,
            buffer: buffer_handle,
            buffer_offset: 0,
        };
        let mut neg_out: u64 = 0;
        let neg_code = crate::arch::x86_64::syscall::feox_syscall_dispatch(
            feox_asi::AsiOp::StorageSubmitRead as u64,
            &neg_args as *const _ as *const u8,
            core::mem::size_of::<feox_asi::StorageSubmitReadArgs>() as u64,
            &mut neg_out,
        );
        // 0xFFFF_0500 + StorageError::InvalidCapability (= 0). Any non-
        // zero code is acceptable for the negative test; the specific
        // value is the storage error base.
        if neg_code == 0 {
            crate::kprintln!("storage-abi-probe: negative path UNEXPECTEDLY accepted bogus cap");
        } else {
            crate::kprintln!(
                "storage-abi-probe: negative path rejected as expected (code={:#x})",
                neg_code
            );
        }

        // ---- Positive path: real device cap ----
        let submit_args = feox_asi::StorageSubmitReadArgs {
            device: device_cap,
            nsid: NSID,
            lba: 0,
            block_count: 1,
            _reserved: 0,
            buffer: buffer_handle,
            buffer_offset: 0,
        };
        let mut submit_out: u64 = 0;
        let submit_code = crate::arch::x86_64::syscall::feox_syscall_dispatch(
            feox_asi::AsiOp::StorageSubmitRead as u64,
            &submit_args as *const _ as *const u8,
            core::mem::size_of::<feox_asi::StorageSubmitReadArgs>() as u64,
            &mut submit_out,
        );
        if submit_code != 0 {
            crate::kprintln!("storage-abi-probe: submit failed code={:#x}", submit_code);
        } else {
            let token = feox_asi::StorageToken(submit_out);
            crate::kprintln!("storage-abi-probe: token={:#x}", token.0);
            let mut completion = feox_asi::StorageCompletion::default();
            let poll_args = feox_asi::StoragePollArgs {
                token,
                out_completion: &mut completion as *mut _,
            };
            let mut abi_polls = 0_u32;
            let mut ready = false;
            let mut last_poll_code: u64 = 0;
            while abi_polls < POLL_LIMIT {
                let mut poll_out: u64 = 0;
                let poll_code = crate::arch::x86_64::syscall::feox_syscall_dispatch(
                    feox_asi::AsiOp::StoragePoll as u64,
                    &poll_args as *const _ as *const u8,
                    core::mem::size_of::<feox_asi::StoragePollArgs>() as u64,
                    &mut poll_out,
                );
                last_poll_code = poll_code;
                if poll_code != 0 {
                    break;
                }
                abi_polls += 1;
                if poll_out == feox_asi::StoragePollResult::Ready as u64 {
                    ready = true;
                    break;
                }
            }
            if !ready {
                crate::kprintln!(
                    "storage-abi-probe: did not reach Ready (polls={} last_code={:#x})",
                    abi_polls,
                    last_poll_code
                );
            } else {
                crate::kprintln!(
                    "storage-abi-probe: ready sct={} sc={} dnr={} polls={}",
                    completion.nvme_sct,
                    completion.nvme_sc,
                    completion.dnr,
                    abi_polls
                );
                let abi_bytes = unsafe {
                    // SAFETY: abi_virt aliases the DMA page just written
                    // by the controller via the syscall-driven read.
                    core::slice::from_raw_parts(abi_virt as *const u8, 20)
                };
                crate::kprintln!(
                    "storage-abi-probe: LBA0 ascii='{}'",
                    trim_ascii(abi_bytes)
                );
            }
        }
    } else {
        crate::kprintln!("storage-abi-probe: capability request failed");
    }

    // ---- Tear down I/O queues (SQ before CQ per spec) ----
    let _ = nvme_submit_and_wait(
        registers,
        admin_sq,
        admin_cq,
        0,
        ADMIN_ENTRIES,
        ADMIN_ENTRIES,
        &mut admin,
        SubmissionQueueEntry::delete_io_submission_queue(IO_QUEUE_ID, 4),
        POLL_LIMIT,
    );
    let _ = nvme_submit_and_wait(
        registers,
        admin_sq,
        admin_cq,
        0,
        ADMIN_ENTRIES,
        ADMIN_ENTRIES,
        &mut admin,
        SubmissionQueueEntry::delete_io_completion_queue(IO_QUEUE_ID, 5),
        POLL_LIMIT,
    );
    crate::kprintln!("nvme-io-probe: I/O queues torn down");
    crate::block::shutdown();

    // Shut the controller back down.
    registers.set_cc(0);
    let mut waited = 0_u32;
    while registers.csts().ready() {
        waited += 1;
        if waited >= POLL_LIMIT {
            crate::kprintln!("nvme-admin-probe: timeout waiting CSTS.RDY=0 on shutdown");
            return;
        }
        core::hint::spin_loop();
    }
    crate::kprintln!("nvme-admin-probe: controller halted (CSTS.RDY=0)");
    crate::kprintln!("nvme-admin-probe: full lifecycle ok");
}

#[cfg(target_os = "none")]
fn run_mmio_cycle_probe() {
    use crate::memory::{PAGE_SIZE, PhysicalAddress, VirtualAddress};
    use crate::mmio::{mmio_map_bootstrap, mmio_unmap_bootstrap};
    use crate::paging::{DirectMapPageTables, PageTableRoot};

    // LAPIC base. A real x86 device MMIO address that no other Feox code
    // accesses, and (importantly) not covered by the direct map since the
    // boot map reports it as Reserved, not Usable.
    const PROBE_PHYS: u64 = 0xFEE0_0000;

    crate::kprintln!(
        "mmio-probe: requesting MMIO mapping phys={:#018x} length={:#x}",
        PROBE_PHYS,
        PAGE_SIZE
    );
    let region = match mmio_map_bootstrap(
        PhysicalAddress::new(PROBE_PHYS),
        PAGE_SIZE,
        true,
        true,
    ) {
        Ok(region) => region,
        Err(_) => {
            crate::kprintln!("mmio-probe: mmio_map failed");
            return;
        }
    };
    crate::kprintln!(
        "mmio-probe: mapped virt={:#018x} phys={:#018x} uncached={} writable={}",
        region.virtual_base,
        region.physical_base,
        region.uncached,
        region.writable
    );

    // Walk the active root and confirm the install landed correctly.
    let root = PageTableRoot::active();
    let source = DirectMapPageTables;
    let translation = match root.translate_with(&source, VirtualAddress::new(region.virtual_base)) {
        Ok(Some(t)) => t,
        _ => {
            crate::kprintln!("mmio-probe: vtop_walk did not return a leaf");
            let _ = mmio_unmap_bootstrap(region);
            return;
        }
    };
    crate::kprintln!(
        "mmio-probe: walk phys={:#018x} writable={} cache_disabled={} write_through={}",
        translation.physical_address.as_u64(),
        translation.entry.is_writable(),
        translation.entry.is_cache_disabled(),
        translation.entry.is_write_through()
    );
    if translation.physical_address.as_u64() != PROBE_PHYS {
        crate::kprintln!("mmio-probe: FAIL walk phys mismatch");
        let _ = mmio_unmap_bootstrap(region);
        return;
    }
    if !translation.entry.is_cache_disabled() || !translation.entry.is_write_through() {
        crate::kprintln!("mmio-probe: FAIL UC flags not set");
        let _ = mmio_unmap_bootstrap(region);
        return;
    }

    crate::kprintln!("mmio-probe: calling mmio_unmap_bootstrap...");
    if mmio_unmap_bootstrap(region).is_err() {
        crate::kprintln!("mmio-probe: mmio_unmap failed");
        return;
    }
    crate::kprintln!("mmio-probe: full mmio_map cycle ok");
}

#[cfg(target_os = "none")]
fn run_vm_mem_cycle_probe() {
    use crate::memory::PAGE_SIZE;
    use crate::vm::{
        mem_map_bootstrap, mem_unmap_bootstrap, mem_vtop_batch_bootstrap, mem_vtop_bootstrap,
    };
    use feox_asi::{
        CapRequest, MapFlags, MemMapArgs, MemVtoPArgs, MemVtoPBatchArgs, PageFlags,
        PhysicalAddress as AsiPhysicalAddress,
    };

    const PROBE_PAGES: usize = 4;
    const PROBE_REGION_BYTES: u64 = (PROBE_PAGES as u64) * PAGE_SIZE;

    crate::kprintln!(
        "vm-mem-probe: requesting {}-page physical capability...",
        PROBE_PAGES
    );
    let handle = match crate::capability::request_bootstrap_capability(&CapRequest::PhysicalPages {
        num_pages: PROBE_PAGES,
        flags: PageFlags::CONTIGUOUS,
    }) {
        Ok(handle) => handle,
        Err(_) => {
            crate::kprintln!("vm-mem-probe: cap request failed");
            return;
        }
    };
    crate::kprintln!(
        "vm-mem-probe: cap handle id={} generation={}",
        handle.id,
        handle.generation
    );

    crate::kprintln!("vm-mem-probe: calling mem_map_bootstrap (multi-page)...");
    let region = match mem_map_bootstrap(MemMapArgs {
        handle,
        offset_bytes: 0,
        length_bytes: PROBE_REGION_BYTES,
        flags: MapFlags::READ | MapFlags::WRITE,
        out_region: core::ptr::null_mut(),
    }) {
        Ok(region) => region,
        Err(_) => {
            crate::kprintln!("vm-mem-probe: mem_map failed");
            return;
        }
    };
    crate::kprintln!(
        "vm-mem-probe: mapped region base={:#018x} length={:#x}",
        region.base,
        region.length_bytes
    );

    // Single-address vtop on the first page — keeps coverage for the
    // single-call code path.
    crate::kprintln!("vm-mem-probe: calling mem_vtop_bootstrap (single)...");
    let single_phys = match mem_vtop_bootstrap(MemVtoPArgs {
        handle,
        virtual_address: region.base,
        out_physical_address: core::ptr::null_mut(),
    }) {
        Ok(AsiPhysicalAddress(p)) => p,
        Err(_) => {
            crate::kprintln!("vm-mem-probe: mem_vtop (single) failed");
            let _ = mem_unmap_bootstrap(region);
            return;
        }
    };
    crate::kprintln!("vm-mem-probe: single vtop result={:#018x}", single_phys);

    // Batch vtop across all 4 pages.
    let vas: [u64; PROBE_PAGES] = [
        region.base,
        region.base + PAGE_SIZE,
        region.base + 2 * PAGE_SIZE,
        region.base + 3 * PAGE_SIZE,
    ];
    let mut phys_batch: [AsiPhysicalAddress; PROBE_PAGES] =
        [AsiPhysicalAddress(0); PROBE_PAGES];

    crate::kprintln!(
        "vm-mem-probe: calling mem_vtop_batch_bootstrap (count={})...",
        PROBE_PAGES
    );
    let written = match mem_vtop_batch_bootstrap(MemVtoPBatchArgs {
        handle,
        virtual_addresses: vas.as_ptr(),
        physical_addresses: phys_batch.as_mut_ptr(),
        count: PROBE_PAGES,
    }) {
        Ok(written) => written,
        Err(_) => {
            crate::kprintln!("vm-mem-probe: mem_vtop_batch failed");
            let _ = mem_unmap_bootstrap(region);
            return;
        }
    };
    crate::kprintln!("vm-mem-probe: batch vtop wrote {} translations", written);

    let mut index = 0usize;
    while index < PROBE_PAGES {
        crate::kprintln!(
            "vm-mem-probe:   batch[{}] va={:#018x} phys={:#018x}",
            index,
            vas[index],
            phys_batch[index].0
        );
        index += 1;
    }

    // The cap was minted with PageFlags::CONTIGUOUS, so successive page
    // translations must be PAGE_SIZE apart in phys.
    let mut contiguous = true;
    let mut probe = 1usize;
    while probe < PROBE_PAGES {
        if phys_batch[probe].0 != phys_batch[probe - 1].0 + PAGE_SIZE {
            contiguous = false;
            break;
        }
        probe += 1;
    }
    if contiguous {
        crate::kprintln!("vm-mem-probe: batch phys is contiguous");
    } else {
        crate::kprintln!("vm-mem-probe: FAIL batch phys NOT contiguous");
        let _ = mem_unmap_bootstrap(region);
        return;
    }

    // Cross-check: single vtop and batch[0] must agree.
    if phys_batch[0].0 != single_phys {
        crate::kprintln!(
            "vm-mem-probe: FAIL single vtop {:#x} != batch[0] {:#x}",
            single_phys,
            phys_batch[0].0
        );
        let _ = mem_unmap_bootstrap(region);
        return;
    }

    crate::kprintln!("vm-mem-probe: calling mem_unmap_bootstrap (multi-page)...");
    if mem_unmap_bootstrap(region).is_err() {
        crate::kprintln!("vm-mem-probe: mem_unmap failed");
        return;
    }
    crate::kprintln!("vm-mem-probe: full mem_map cycle ok");
}

fn runtime_active_entry() -> ! {
    crate::runtime_context::push_event("runtime-service-entered");
    crate::kprintln!("stage: runtime service entered");

    let owner_core =
        crate::runtime_context::core().map_or(feox_asi::CoreId(0), |core| core.core_id);
    crate::runtime_context::store_service(RuntimeServiceState {
        owner_core,
        phase: "entered",
        iterations: 0,
        last_action: "service-entered",
    });

    if let Some(runtime) = crate::runtime_context::snapshot() {
        crate::kprintln!(
            "runtime: summary root={:#018x} window={:#018x}-{:#018x} pages={} data_page={:#018x}",
            runtime.active_root,
            runtime.kernel_window_base,
            runtime.kernel_window_end,
            runtime.kernel_pages_mapped,
            runtime.data_page
        );
    }
    if let Some(core) = crate::runtime_context::core() {
        crate::kprintln!(
            "runtime: owner core={} stage={} stack={:#018x} entry={:#018x}",
            core.core_id.0,
            core.stage,
            core.stack_pointer,
            core.alias_entry
        );
    }

    let _ = crate::runtime_context::enqueue_command(RuntimeServiceCommand::RefreshSnapshot);

    let events = crate::runtime_context::events();
    let mut event_index = 0usize;
    while event_index < events.len() {
        if let Some(event) = events[event_index] {
            crate::kprintln!("runtime: retained_event[{}]={}", event_index, event);
        }
        event_index += 1;
    }

    let mut service_iteration = 0_u64;
    while let Some(command) = crate::runtime_context::dequeue_command() {
        service_iteration += 1;
        crate::kprintln!(
            "runtime: command phase={} iteration={}",
            command.label(),
            service_iteration
        );
        match command {
            RuntimeServiceCommand::RefreshSnapshot => {
                crate::runtime_context::push_event("runtime-service-poll");
                crate::runtime_context::store_service(RuntimeServiceState {
                    owner_core,
                    phase: "poll",
                    iterations: service_iteration,
                    last_action: "retained-snapshot-scan",
                });
                if let Some(service) = crate::runtime_context::service() {
                    crate::kprintln!(
                        "runtime: service core={} phase={} iterations={} action={}",
                        service.owner_core.0,
                        service.phase,
                        service.iterations,
                        service.last_action
                    );
                }
                if crate::runtime_context::snapshot().is_some() {
                    let _ = crate::runtime_context::enqueue_command(
                        RuntimeServiceCommand::RefreshAccounting,
                    );
                } else {
                    let _ =
                        crate::runtime_context::enqueue_command(RuntimeServiceCommand::EnterIdle);
                }
            }
            RuntimeServiceCommand::RefreshAccounting => {
                if let Some(runtime) = crate::runtime_context::snapshot() {
                    let mut retained_events = 0_u64;
                    let events = crate::runtime_context::events();
                    let mut event_index = 0usize;
                    while event_index < events.len() {
                        if events[event_index].is_some() {
                            retained_events += 1;
                        }
                        event_index += 1;
                    }
                    let kernel_window_bytes = runtime
                        .kernel_window_end
                        .saturating_sub(runtime.kernel_window_base);
                    let stack_bytes = runtime.stack_pages.saturating_mul(memory::PAGE_SIZE);
                    crate::runtime_context::push_event("runtime-service-accounting");
                    crate::runtime_context::store_service(RuntimeServiceState {
                        owner_core,
                        phase: "accounting",
                        iterations: service_iteration,
                        last_action: "retained-runtime-accounting",
                    });
                    crate::runtime_context::store_service_report(RuntimeServiceReport {
                        kernel_window_bytes,
                        stack_bytes,
                        retained_events,
                    });
                    if let Some(service) = crate::runtime_context::service() {
                        crate::kprintln!(
                            "runtime: service_accounting core={} phase={} iterations={} action={}",
                            service.owner_core.0,
                            service.phase,
                            service.iterations,
                            service.last_action
                        );
                    }
                    if let Some(report) = crate::runtime_context::service_report() {
                        crate::kprintln!(
                            "runtime: accounting window_bytes={:#018x} stack_bytes={:#018x} retained_events={}",
                            report.kernel_window_bytes,
                            report.stack_bytes,
                            report.retained_events
                        );
                    }
                    if retained_events > 0 {
                        let _ = crate::runtime_context::enqueue_command(
                            RuntimeServiceCommand::ReportTimeline,
                        );
                    } else {
                        let _ = crate::runtime_context::enqueue_command(
                            RuntimeServiceCommand::EnterIdle,
                        );
                    }
                } else {
                    let _ =
                        crate::runtime_context::enqueue_command(RuntimeServiceCommand::EnterIdle);
                }
            }
            RuntimeServiceCommand::ReportTimeline => {
                crate::runtime_context::push_event("runtime-service-timeline");
                crate::runtime_context::store_service(RuntimeServiceState {
                    owner_core,
                    phase: "timeline",
                    iterations: service_iteration,
                    last_action: "retained-event-report",
                });
                if let Some(service) = crate::runtime_context::service() {
                    crate::kprintln!(
                        "runtime: service_timeline core={} phase={} iterations={} action={}",
                        service.owner_core.0,
                        service.phase,
                        service.iterations,
                        service.last_action
                    );
                }
                let events = crate::runtime_context::events();
                let mut timeline_index = 0usize;
                while timeline_index < events.len() {
                    if let Some(event) = events[timeline_index] {
                        crate::kprintln!("runtime: timeline_event[{}]={}", timeline_index, event);
                    }
                    timeline_index += 1;
                }
                if crate::runtime_context::service_report().is_some() {
                    let _ = crate::runtime_context::enqueue_command(
                        RuntimeServiceCommand::UpdateHeartbeat,
                    );
                } else {
                    let _ =
                        crate::runtime_context::enqueue_command(RuntimeServiceCommand::EnterIdle);
                }
            }
            RuntimeServiceCommand::UpdateHeartbeat => {
                crate::runtime_context::push_event("runtime-service-heartbeat");
                crate::runtime_context::store_service(RuntimeServiceState {
                    owner_core,
                    phase: "heartbeat",
                    iterations: service_iteration,
                    last_action: "retained-heartbeat-update",
                });
                let previous_beats = crate::runtime_context::service_heartbeat()
                    .map_or(0, |heartbeat| heartbeat.beats);
                let observed_events = crate::runtime_context::service_report()
                    .map_or(0, |report| report.retained_events);
                crate::runtime_context::store_service_heartbeat(RuntimeServiceHeartbeat {
                    beats: previous_beats.saturating_add(1),
                    last_iteration: service_iteration,
                    observed_events,
                });
                if let Some(service) = crate::runtime_context::service() {
                    crate::kprintln!(
                        "runtime: service_heartbeat core={} phase={} iterations={} action={}",
                        service.owner_core.0,
                        service.phase,
                        service.iterations,
                        service.last_action
                    );
                }
                if let Some(heartbeat) = crate::runtime_context::service_heartbeat() {
                    crate::kprintln!(
                        "runtime: heartbeat beats={} last_iteration={} observed_events={}",
                        heartbeat.beats,
                        heartbeat.last_iteration,
                        heartbeat.observed_events
                    );
                    if heartbeat.beats < 2 {
                        let _ = crate::runtime_context::enqueue_command(
                            RuntimeServiceCommand::RefreshSnapshot,
                        );
                    } else {
                        let _ = crate::runtime_context::enqueue_command(
                            RuntimeServiceCommand::PublishReady,
                        );
                    }
                } else {
                    let _ =
                        crate::runtime_context::enqueue_command(RuntimeServiceCommand::EnterIdle);
                }
            }
            RuntimeServiceCommand::PublishReady => {
                crate::runtime_context::push_event("runtime-service-ready");
                crate::runtime_context::store_service(RuntimeServiceState {
                    owner_core,
                    phase: "ready",
                    iterations: service_iteration,
                    last_action: "publish-runtime-ready",
                });
                let settled_beats = crate::runtime_context::service_heartbeat()
                    .map_or(0, |heartbeat| heartbeat.beats);
                crate::runtime_context::store_runtime_readiness(RuntimeReadinessState {
                    ready: true,
                    published_iteration: service_iteration,
                    settled_beats,
                });
                if let Some(runtime) = crate::runtime_context::snapshot() {
                    crate::runtime_context::store(RuntimeSnapshot {
                        stage: "runtime-ready",
                        ..runtime
                    });
                }
                if let Some(core) = crate::runtime_context::core() {
                    crate::runtime_context::store_core(BootstrapCoreContext {
                        stage: "runtime-ready",
                        ..core
                    });
                }
                if let Some(service) = crate::runtime_context::service() {
                    crate::kprintln!(
                        "runtime: service_ready core={} phase={} iterations={} action={}",
                        service.owner_core.0,
                        service.phase,
                        service.iterations,
                        service.last_action
                    );
                }
                if let Some(readiness) = crate::runtime_context::runtime_readiness() {
                    crate::kprintln!(
                        "runtime: readiness ready={} published_iteration={} settled_beats={}",
                        readiness.ready,
                        readiness.published_iteration,
                        readiness.settled_beats
                    );
                }
                if let Some(runtime) = crate::runtime_context::snapshot() {
                    crate::kprintln!(
                        "runtime: ready_state stage={} root={:#018x}",
                        runtime.stage,
                        runtime.active_root
                    );
                }
                let _ = crate::runtime_context::enqueue_command(
                    RuntimeServiceCommand::PublishReadySummary,
                );
            }
            RuntimeServiceCommand::PublishReadySummary => {
                crate::runtime_context::push_event("runtime-service-ready-summary");
                crate::runtime_context::store_service(RuntimeServiceState {
                    owner_core,
                    phase: "ready-summary",
                    iterations: service_iteration,
                    last_action: "publish-ready-summary",
                });
                if let Some(runtime) = crate::runtime_context::snapshot() {
                    let retained_events = crate::runtime_context::service_report()
                        .map_or(0, |report| report.retained_events);
                    crate::runtime_context::store_ready_summary(RuntimeReadySummary {
                        active_root: runtime.active_root,
                        kernel_pages_mapped: runtime.kernel_pages_mapped,
                        retained_events,
                    });
                }
                if let Some(service) = crate::runtime_context::service() {
                    crate::kprintln!(
                        "runtime: service_ready_summary core={} phase={} iterations={} action={}",
                        service.owner_core.0,
                        service.phase,
                        service.iterations,
                        service.last_action
                    );
                }
                if let Some(summary) = crate::runtime_context::ready_summary() {
                    crate::kprintln!(
                        "runtime: ready_summary root={:#018x} pages={} retained_events={}",
                        summary.active_root,
                        summary.kernel_pages_mapped,
                        summary.retained_events
                    );
                }
                #[cfg(target_os = "none")]
                run_vm_access_probe();
                let _ = crate::runtime_context::enqueue_command(RuntimeServiceCommand::EnterIdle);
            }
            RuntimeServiceCommand::EnterIdle => {
                crate::runtime_context::push_event("runtime-service-idle");
                crate::runtime_context::store_service(RuntimeServiceState {
                    owner_core,
                    phase: "idle",
                    iterations: service_iteration,
                    last_action: "idle-loop",
                });
                if let Some(service) = crate::runtime_context::service() {
                    crate::kprintln!(
                        "runtime: service_idle core={} phase={} iterations={} action={}",
                        service.owner_core.0,
                        service.phase,
                        service.iterations,
                        service.last_action
                    );
                }
            }
        }
    }
    crate::kprintln!("stage: runtime service idle");
    arch::halt_loop()
}

#[cfg(test)]
mod tests {
    use super::{
        BootstrapRuntimeStage, BootstrapRuntimeState, RuntimeSnapshot, TRANSITION_DATA_MAGIC,
    };

    #[test]
    fn runtime_snapshot_reflects_retained_bootstrap_state() {
        let state = BootstrapRuntimeState {
            magic: TRANSITION_DATA_MAGIC,
            active_root: 0x11e000,
            kernel_window_base: 0xffff_9000_0000_0000,
            kernel_window_end: 0xffff_9000_0001_8031,
            kernel_pages_mapped: 25,
            identity_stack: 0x11d000,
            alias_stack: 0xffff_9000_0200_4000,
            stack_pages: 4,
            data_page: 0x11d000,
            alias_entry: 0xffff_9000_0000_0970,
            alias_gdt: 0xffff_9000_0000_f740,
            alias_idt: 0xffff_9000_0001_7031,
            stage: BootstrapRuntimeStage::ExceptionValidated.as_u64(),
        };

        let snapshot = RuntimeSnapshot::from_state(state).expect("valid runtime snapshot");

        assert_eq!(snapshot.active_root, 0x11e000);
        assert_eq!(snapshot.kernel_pages_mapped, 25);
        assert_eq!(snapshot.stack_pages, 4);
        assert_eq!(snapshot.stage, "exception-validated");
    }

    #[test]
    fn runtime_snapshot_rejects_bad_magic() {
        let state = BootstrapRuntimeState {
            magic: 0,
            active_root: 0,
            kernel_window_base: 0,
            kernel_window_end: 0,
            kernel_pages_mapped: 0,
            identity_stack: 0,
            alias_stack: 0,
            stack_pages: 0,
            data_page: 0,
            alias_entry: 0,
            alias_gdt: 0,
            alias_idt: 0,
            stage: 0,
        };

        assert_eq!(RuntimeSnapshot::from_state(state), None);
    }
}

/// Initializes the earliest architecture support and halts in a known-good
/// state so higher layers can be added incrementally.
pub fn bootstrap(config: KernelConfig, handoff: Option<BootHandoff<'_>>) -> ! {
    arch::early_init();
    // Claim the bootstrap runtime context for the bootstrap core before any
    // store_* call is made. The CAS in claim_bootstrap_context panics in
    // debug builds if a second core tries to claim.
    crate::runtime_context::claim_bootstrap_context(config.bootstrap_core);
    capability::init_bootstrap_process(feox_asi::ProcessId(0));
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
            if let Some(rsdp) = handoff.rsdp_phys() {
                RSDP_PHYS.store(rsdp, core::sync::atomic::Ordering::Release);
                crate::kprintln!("memory: rsdp_phys={:#018x}", rsdp);
            } else {
                crate::kprintln!("memory: rsdp_phys=<not provided>");
            }
            if let Some(tramp) = handoff.ap_trampoline_phys() {
                AP_TRAMPOLINE_PHYS.store(tramp, core::sync::atomic::Ordering::Release);
                crate::kprintln!("memory: ap_trampoline_phys={:#018x}", tramp);
            } else {
                crate::kprintln!("memory: ap_trampoline_phys=<not provided>");
            }
            let registered_resources =
                capability::seed_bootstrap_resources_from_handoff(handoff.memory_map())
                    .unwrap_or(0);
            let mut minted_capabilities = 0usize;
            let mut resource_index = 0usize;
            while resource_index < capability::resource_count_public() {
                if capability::mint_bootstrap_root_capability(
                    crate::capability::ResourceId(resource_index as u32),
                    feox_asi::CapPermissions::all(),
                )
                .is_ok()
                {
                    minted_capabilities += 1;
                }
                resource_index += 1;
            }
            crate::runtime_context::push_event("bootstrap-capabilities-ready");
            let runtime_layout = memory::BootstrapRuntimeLayout::new();
            crate::kprintln!(
                "memory: boot_map regions={} usable={} MiB top={:#018x}",
                handoff.memory_map().len(),
                handoff.usable_bytes() / (1024 * 1024),
                    handoff
                        .highest_physical_address()
                        .map_or(0, |address| address.as_u64())
            );
            crate::kprintln!(
                "capability: bootstrap_owner={} resources={} active_caps={}",
                capability::owner().0,
                registered_resources,
                minted_capabilities
            );

            let mut reservations =
                memory::EarlyKernelReservations::for_bootstrap(kernel_image, pml4);
            let boot_map = memory::BootMemoryMap::new(handoff.memory_map());
            let allocator =
                memory::FrameAllocator::with_reservations(boot_map, reservations.as_view());
            let next_frame = allocator
                .clone()
                .allocate()
                .map_or(0, |frame| frame.start_address().as_u64());
            let transition_virtual_address = runtime_layout.kernel_window_base();
            let transition_data_alias = runtime_layout.data_window_base();
            let transition_stage_entry_alias = runtime_layout.alias_for_kernel_address(
                kernel_image,
                memory::VirtualAddress::new(feox_transition_entry as *const () as usize as u64),
            );
            let identity_transition_entry =
                memory::VirtualAddress::new(feox_transition_entry as *const () as usize as u64);
            let identity_gdt = memory::VirtualAddress::new(arch::x86_64::gdt::table_base());
            let identity_idt = memory::VirtualAddress::new(arch::x86_64::idt::table_base());
            let transition_high_stack_entry_alias = runtime_layout.alias_for_kernel_address(
                kernel_image,
                memory::VirtualAddress::new(
                    transition_high_stack_entry as *const () as usize as u64,
                ),
            );
            let transition_gdt_alias =
                runtime_layout.alias_for_kernel_address(kernel_image, identity_gdt);
            let transition_idt_alias =
                runtime_layout.alias_for_kernel_address(kernel_image, identity_idt);
            let reserved_ranges = reservations.len();
            let kernel_image_reserved_start = reservations
                .region_for_kind(memory::ReservationKind::KernelImage)
                .map_or(0, |region| region.start().as_u64());
            let kernel_image_reserved_end = reservations
                .region_for_kind(memory::ReservationKind::KernelImage)
                .map_or(0, |region| region.end().as_u64());
            let active_pml4_reserved = reservations
                .region_for_kind(memory::ReservationKind::ActivePageTableRoot)
                .map_or(0, |region| region.start().as_u64());

            crate::kprintln!(
                "memory: reserved_ranges={} kernel_image_reserved={:#018x}-{:#018x}",
                reserved_ranges,
                kernel_image_reserved_start,
                kernel_image_reserved_end
            );
            crate::kprintln!(
                "memory: active_pml4_reserved={:#018x}",
                active_pml4_reserved
            );
            crate::kprintln!(
                "memory: handoff accepted, first_usable_frame={:#018x}",
                next_frame
            );
            let (
                bootstrap_page_table_frame,
                transition_root_ready,
                transition_pages_mapped,
                direct_map_pages_installed,
                direct_map_4k_pages_installed,
                transition_stack_top,
                transition_stack_alias_top,
                transition_data_identity,
                transition_data_alias_mapped,
                transition_map_result,
                transition_translate_result,
                identity_entry_translate_result,
                identity_stack_translate_result,
                identity_data_translate_result,
            ) = {
                let mut transition_stack_frames = [None; TRANSITION_STACK_PAGES as usize];
                let mut stack_page = 0usize;
                while stack_page < transition_stack_frames.len() {
                    let mut allocator =
                        memory::FrameAllocator::with_reservations(boot_map, reservations.as_view());
                    let Some(frame) = allocator.allocate() else {
                        break;
                    };
                    reservations
                        .reserve_frame(memory::ReservationKind::BootstrapPerCoreState, frame);
                    transition_stack_frames[stack_page] = Some(frame);
                    stack_page += 1;
                }
                let transition_stack_top = transition_stack_frames
                    .iter()
                    .flatten()
                    .next_back()
                    .map(|frame| frame.start_address().as_u64() + memory::PAGE_SIZE)
                    .unwrap_or(0);
                let transition_stack_alias_top = transition_stack_frames
                    .iter()
                    .flatten()
                    .next_back()
                    .map(|frame| {
                        runtime_layout.stack_window_base().as_u64()
                            + ((frame.start_address().as_u64()
                                - transition_stack_frames[0].unwrap().start_address().as_u64())
                                + memory::PAGE_SIZE)
                    })
                    .unwrap_or(0);
                let mut allocator =
                    memory::FrameAllocator::with_reservations(boot_map, reservations.as_view());
                let transition_data_frame = allocator.allocate();
                if let Some(frame) = transition_data_frame {
                    reservations
                        .reserve_frame(memory::ReservationKind::BootstrapPerCoreState, frame);
                }
                let transition_data_identity = transition_data_frame
                    .map(|frame| frame.start_address().as_u64())
                    .unwrap_or(0);
                let transition_data_alias_mapped = if transition_data_identity != 0 {
                    transition_data_alias.as_u64()
                } else {
                    0
                };
                let mut paging_allocator =
                    paging::BootstrapPagingAllocator::new(boot_map, &mut reservations);
                match paging_allocator.allocate_table_frame() {
                    Some(bootstrap_page_table_root) => {
                        let mut live_page_tables = paging::BootstrapIdentityMappedPageTables;
                        let page_root = paging::PageTableRoot::new(bootstrap_page_table_root);
                        let transition_root_ready = paging::PageTableFrameMutSource::table_mut(
                            &mut live_page_tables,
                            bootstrap_page_table_root,
                        )
                        .map(|table| table.fill(0))
                        .is_some();
                        let mut transition_pages_mapped = 0_u64;
                        let mut direct_map_pages_installed = 0_u64;
                        let mut direct_map_4k_pages_installed = 0_u64;
                        let transition_map_result = if transition_root_ready {
                            let kernel_pages =
                                kernel_image.size_bytes().div_ceil(memory::PAGE_SIZE).max(1);
                            let kernel_page_flags = 1_u64 << 1;
                            let mut map_result = Ok(());
                            let mut page_index = 0;
                            while page_index < kernel_pages {
                                let page_offset = page_index * memory::PAGE_SIZE;
                                let identity_virtual = memory::VirtualAddress::new(
                                    kernel_image.start.as_u64() + page_offset,
                                );
                                let mapped_virtual = memory::VirtualAddress::new(
                                    transition_virtual_address.as_u64() + page_offset,
                                );
                                let mapped_physical = memory::PhysicalFrame::containing(
                                    memory::PhysicalAddress::new(
                                        kernel_image.start.as_u64() + page_offset,
                                    ),
                                );
                                if page_root
                                    .map_4k_with(
                                        &mut live_page_tables,
                                        &mut paging_allocator,
                                        identity_virtual,
                                        mapped_physical,
                                        kernel_page_flags,
                                    )
                                    .is_err()
                                {
                                    map_result = Err("identity_map_failed");
                                    break;
                                }
                                if page_root
                                    .map_4k_with(
                                        &mut live_page_tables,
                                        &mut paging_allocator,
                                        mapped_virtual,
                                        mapped_physical,
                                        kernel_page_flags,
                                    )
                                    .is_err()
                                {
                                    map_result = Err("transition_map_failed");
                                    break;
                                }
                                transition_pages_mapped += 1;
                                page_index += 1;
                            }
                            // Stack pages are writable but not executable.
                            let stack_page_flags =
                                (1_u64 << 1) | paging::PageTableEntry::FLAG_NO_EXECUTE;
                            if let Some(first_stack_frame) = transition_stack_frames[0] {
                                let mut stack_index = 0usize;
                                while stack_index < transition_stack_frames.len() {
                                    let Some(stack_frame) = transition_stack_frames[stack_index]
                                    else {
                                        map_result = Err("transition_stack_incomplete");
                                        break;
                                    };
                                    let stack_offset = stack_frame.start_address().as_u64()
                                        - first_stack_frame.start_address().as_u64();
                                    let identity_stack_virtual = memory::VirtualAddress::new(
                                        stack_frame.start_address().as_u64(),
                                    );
                                    let alias_stack_virtual = memory::VirtualAddress::new(
                                        runtime_layout.stack_window_base().as_u64() + stack_offset,
                                    );
                                    if page_root
                                        .map_4k_with(
                                            &mut live_page_tables,
                                            &mut paging_allocator,
                                            identity_stack_virtual,
                                            stack_frame,
                                            stack_page_flags,
                                        )
                                        .is_err()
                                    {
                                        map_result = Err("identity_stack_map_failed");
                                        break;
                                    }
                                    if page_root
                                        .map_4k_with(
                                            &mut live_page_tables,
                                            &mut paging_allocator,
                                            alias_stack_virtual,
                                            stack_frame,
                                            stack_page_flags,
                                        )
                                        .is_err()
                                    {
                                        map_result = Err("transition_stack_map_failed");
                                        break;
                                    }
                                    stack_index += 1;
                                }
                            } else {
                                map_result = Err("transition_stack_unavailable");
                            }
                            if map_result.is_ok() {
                                if let Some(data_frame) = transition_data_frame {
                                    // Data pages are writable but not executable.
                                    let data_page_flags =
                                        (1_u64 << 1) | paging::PageTableEntry::FLAG_NO_EXECUTE;
                                    let identity_data_virtual = memory::VirtualAddress::new(
                                        data_frame.start_address().as_u64(),
                                    );
                                    if page_root
                                        .map_4k_with(
                                            &mut live_page_tables,
                                            &mut paging_allocator,
                                            identity_data_virtual,
                                            data_frame,
                                            data_page_flags,
                                        )
                                        .is_err()
                                    {
                                        map_result = Err("identity_data_map_failed");
                                    }
                                    if map_result.is_ok()
                                        && page_root
                                            .map_4k_with(
                                                &mut live_page_tables,
                                                &mut paging_allocator,
                                                transition_data_alias,
                                                data_frame,
                                                data_page_flags,
                                            )
                                            .is_err()
                                    {
                                        map_result = Err("transition_data_map_failed");
                                    }
                                } else {
                                    map_result = Err("transition_data_unavailable");
                                }
                            }
                            if map_result.is_ok()
                                && page_root
                                    .prepare_4k_pages_with(
                                        &mut live_page_tables,
                                        &mut paging_allocator,
                                        runtime_layout.vm_window_base(),
                                        runtime_layout.vm_window_size(),
                                    )
                                    .is_err()
                            {
                                map_result = Err("transition_vm_window_prepare_failed");
                            }
                            // (The bootstrap page-table access window slot at
                            // `runtime_layout.page_table_access_window_base()`
                            // is no longer prebuilt or self-mapped — the
                            // direct map below replaces it. The address-space
                            // slot stays reserved in the layout for future
                            // use; nothing live runs through it.)
                            //
                            // Install the permanent direct map of physical RAM
                            // (docs/VIRTUAL_ADDRESS_LAYOUT.md). Every Usable
                            // region is covered exactly: a 4 KiB head fills
                            // the bytes before the first 2 MiB boundary, a
                            // 2 MiB-page bulk covers the aligned interior,
                            // and a 4 KiB tail finishes any leftover at the
                            // end. Non-Usable phys (kernel image, MMIO,
                            // BIOS ROM, ACPI) stays unmapped through the
                            // direct map; callers that need those use the
                            // bootstrap kernel-image alias or a dedicated
                            // MMIO mapping in the locked MMIO zone.
                            if map_result.is_ok() {
                                const HUGE_PAGE_SIZE: u64 = 1 << 21;
                                let direct_flags = (1_u64 << 1)
                                    | paging::PageTableEntry::FLAG_NO_EXECUTE;
                                let regions = boot_map.regions();
                                let mut region_idx = 0usize;
                                while region_idx < regions.len() && map_result.is_ok() {
                                    let region = regions[region_idx];
                                    region_idx += 1;
                                    if !matches!(region.kind, MemoryRegionKind::Usable) {
                                        continue;
                                    }
                                    let raw_start = region.start.as_u64();
                                    let raw_end = region.end.as_u64();
                                    if raw_end <= raw_start {
                                        continue;
                                    }
                                    let aligned_start = (raw_start + HUGE_PAGE_SIZE - 1)
                                        & !(HUGE_PAGE_SIZE - 1);
                                    let aligned_end = raw_end & !(HUGE_PAGE_SIZE - 1);
                                    let has_bulk = aligned_end > aligned_start
                                        && aligned_start >= raw_start
                                        && aligned_end <= raw_end;

                                    // 4 KiB head: [raw_start, aligned_start)
                                    let head_end = if has_bulk {
                                        aligned_start
                                    } else {
                                        raw_end
                                    };
                                    let mut phys = raw_start;
                                    while phys < head_end && map_result.is_ok() {
                                        let frame = memory::PhysicalFrame::containing(
                                            memory::PhysicalAddress::new(phys),
                                        );
                                        let va = memory::VirtualAddress::new(
                                            memory::DIRECT_MAP_BASE.wrapping_add(phys),
                                        );
                                        if page_root
                                            .map_4k_with(
                                                &mut live_page_tables,
                                                &mut paging_allocator,
                                                va,
                                                frame,
                                                direct_flags,
                                            )
                                            .is_err()
                                        {
                                            map_result =
                                                Err("transition_direct_map_head_failed");
                                            break;
                                        }
                                        phys = phys.wrapping_add(memory::PAGE_SIZE);
                                        direct_map_4k_pages_installed += 1;
                                    }

                                    if !has_bulk {
                                        continue;
                                    }

                                    // 2 MiB bulk: [aligned_start, aligned_end)
                                    phys = aligned_start;
                                    while phys < aligned_end && map_result.is_ok() {
                                        let frame = memory::PhysicalFrame::containing(
                                            memory::PhysicalAddress::new(phys),
                                        );
                                        let va = memory::VirtualAddress::new(
                                            memory::DIRECT_MAP_BASE.wrapping_add(phys),
                                        );
                                        if page_root
                                            .map_2m_with(
                                                &mut live_page_tables,
                                                &mut paging_allocator,
                                                va,
                                                frame,
                                                direct_flags,
                                            )
                                            .is_err()
                                        {
                                            map_result =
                                                Err("transition_direct_map_failed");
                                            break;
                                        }
                                        phys = phys.wrapping_add(HUGE_PAGE_SIZE);
                                        direct_map_pages_installed += 1;
                                    }

                                    // 4 KiB tail: [aligned_end, raw_end)
                                    phys = aligned_end;
                                    while phys < raw_end && map_result.is_ok() {
                                        let frame = memory::PhysicalFrame::containing(
                                            memory::PhysicalAddress::new(phys),
                                        );
                                        let va = memory::VirtualAddress::new(
                                            memory::DIRECT_MAP_BASE.wrapping_add(phys),
                                        );
                                        if page_root
                                            .map_4k_with(
                                                &mut live_page_tables,
                                                &mut paging_allocator,
                                                va,
                                                frame,
                                                direct_flags,
                                            )
                                            .is_err()
                                        {
                                            map_result =
                                                Err("transition_direct_map_tail_failed");
                                            break;
                                        }
                                        phys = phys.wrapping_add(memory::PAGE_SIZE);
                                        direct_map_4k_pages_installed += 1;
                                    }
                                }
                            }
                            // Prebuild the MMIO sub-window's page-table
                            // intermediates so device drivers can install
                            // leaf entries post-handoff without a runtime
                            // allocator. The MMIO zone itself is 8 TiB; this
                            // prebuild reserves only MMIO_PREBUILT_SIZE of
                            // it (currently 64 MiB).
                            if map_result.is_ok()
                                && page_root
                                    .prepare_4k_pages_with(
                                        &mut live_page_tables,
                                        &mut paging_allocator,
                                        memory::VirtualAddress::new(memory::MMIO_BASE),
                                        memory::MMIO_PREBUILT_SIZE,
                                    )
                                    .is_err()
                            {
                                map_result = Err("transition_mmio_prebuild_failed");
                            }
                            // Prebuild PML4/PDPT/PD/PT chains for every
                            // potential core's per-core slot so per_core
                            // bring-up (whether for core 0 today or a
                            // secondary AP tomorrow) can install leaf
                            // entries post-handoff without a frame
                            // allocator. Each stride only reserves
                            // PER_CORE_PREBUILT_PER_CORE_SIZE of intermediates,
                            // so the total is PER_CORE_MAX_CORES * a small
                            // fixed amount (currently 32 * ~12 KiB).
                            if map_result.is_ok() {
                                let mut idx = 0_u64;
                                while idx < memory::PER_CORE_MAX_CORES {
                                    let slot_base = memory::PER_CORE_BASE
                                        .wrapping_add(idx.wrapping_mul(memory::PER_CORE_STRIDE));
                                    if page_root
                                        .prepare_4k_pages_with(
                                            &mut live_page_tables,
                                            &mut paging_allocator,
                                            memory::VirtualAddress::new(slot_base),
                                            memory::PER_CORE_PREBUILT_PER_CORE_SIZE,
                                        )
                                        .is_err()
                                    {
                                        map_result =
                                            Err("transition_per_core_prebuild_failed");
                                        break;
                                    }
                                    idx = idx.saturating_add(1);
                                }
                            }
                            map_result
                        } else {
                            Err("transition_root_not_identity_mapped")
                        };
                        let transition_translate_result = if transition_map_result.is_ok() {
                            page_root.translate_with(&live_page_tables, transition_virtual_address)
                        } else {
                            Ok(None)
                        };
                        let identity_entry_translate_result = if transition_map_result.is_ok() {
                            page_root.translate_with(&live_page_tables, identity_transition_entry)
                        } else {
                            Ok(None)
                        };
                        let identity_stack_translate_result = if transition_map_result.is_ok() {
                            page_root.translate_with(
                                &live_page_tables,
                                memory::VirtualAddress::new(transition_stack_top.saturating_sub(8)),
                            )
                        } else {
                            Ok(None)
                        };
                        let identity_data_translate_result =
                            if transition_map_result.is_ok() && transition_data_identity != 0 {
                                page_root.translate_with(
                                    &live_page_tables,
                                    memory::VirtualAddress::new(transition_data_identity),
                                )
                            } else {
                                Ok(None)
                            };

                        (
                            bootstrap_page_table_root.start_address().as_u64(),
                            transition_root_ready,
                            transition_pages_mapped,
                            direct_map_pages_installed,
                            direct_map_4k_pages_installed,
                            transition_stack_top,
                            transition_stack_alias_top,
                            transition_data_identity,
                            transition_data_alias_mapped,
                            transition_map_result,
                            transition_translate_result,
                            identity_entry_translate_result,
                            identity_stack_translate_result,
                            identity_data_translate_result,
                        )
                    }
                    None => (
                        0,
                        false,
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        Err("out_of_table_frames"),
                        Ok(None),
                        Ok(None),
                        Ok(None),
                        Ok(None),
                    ),
                }
            };
            crate::kprintln!(
                "paging: bootstrap_table_frame={:#018x}",
                bootstrap_page_table_frame
            );
            if transition_root_ready {
                crate::kprintln!(
                    "paging: transition_root_ready={:#018x}",
                    bootstrap_page_table_frame
                );
                crate::kprintln!(
                    "paging: transition_pages_mapped={}",
                    transition_pages_mapped
                );
                crate::kprintln!(
                    "paging: direct_map_pages_2m={} pages_4k={} coverage_bytes={:#x}",
                    direct_map_pages_installed,
                    direct_map_4k_pages_installed,
                    direct_map_pages_installed * (1 << 21)
                        + direct_map_4k_pages_installed * memory::PAGE_SIZE
                );
                if let Some(entry) = transition_stage_entry_alias {
                    crate::kprintln!("paging: transition_entry={:#018x}", entry.as_u64());
                } else {
                    crate::kprintln!("paging: transition_entry=unavailable");
                }
                crate::kprintln!(
                    "paging: transition_stack={:#018x}",
                    transition_stack_alias_top
                );
                crate::kprintln!(
                    "paging: identity_transition_stack={:#018x}",
                    transition_stack_top
                );
                crate::kprintln!(
                    "paging: transition_data_alias={:#018x}",
                    transition_data_alias_mapped
                );
                crate::kprintln!(
                    "paging: identity_transition_data={:#018x}",
                    transition_data_identity
                );
                if let Some(entry) = transition_high_stack_entry_alias {
                    crate::kprintln!("paging: transition_alias_entry={:#018x}", entry.as_u64());
                } else {
                    crate::kprintln!("paging: transition_alias_entry=unavailable");
                }
            } else {
                crate::kprintln!(
                    "paging: transition_root_error=transition_root_not_identity_mapped"
                );
            }
            match transition_map_result {
                Ok(()) => {
                    crate::kprintln!(
                        "paging: transition_map_base virtual={:#018x} physical={:#018x}",
                        transition_virtual_address.as_u64(),
                        kernel_image.start.as_u64()
                    );
                    match transition_translate_result {
                        Ok(Some(translation)) => crate::kprintln!(
                            "paging: transition_translate physical={:#018x}",
                            translation.physical_address.as_u64()
                        ),
                        Ok(None) => crate::kprintln!("paging: transition_translate missing"),
                        Err(error) => {
                            crate::kprintln!("paging: transition_translate_error={:?}", error)
                        }
                    }
                    match identity_entry_translate_result {
                        Ok(Some(translation)) => crate::kprintln!(
                            "paging: identity_entry_translate physical={:#018x}",
                            translation.physical_address.as_u64()
                        ),
                        Ok(None) => crate::kprintln!("paging: identity_entry_translate missing"),
                        Err(error) => {
                            crate::kprintln!("paging: identity_entry_translate_error={:?}", error)
                        }
                    }
                    match identity_stack_translate_result {
                        Ok(Some(translation)) => crate::kprintln!(
                            "paging: identity_stack_translate physical={:#018x}",
                            translation.physical_address.as_u64()
                        ),
                        Ok(None) => crate::kprintln!("paging: identity_stack_translate missing"),
                        Err(error) => {
                            crate::kprintln!("paging: identity_stack_translate_error={:?}", error)
                        }
                    }
                    match identity_data_translate_result {
                        Ok(Some(translation)) => crate::kprintln!(
                            "paging: identity_data_translate physical={:#018x}",
                            translation.physical_address.as_u64()
                        ),
                        Ok(None) => crate::kprintln!("paging: identity_data_translate missing"),
                        Err(error) => {
                            crate::kprintln!("paging: identity_data_translate_error={:?}", error)
                        }
                    }
                    if let (Some(entry), Some(stack)) = (
                        Some(identity_transition_entry),
                        Some(memory::VirtualAddress::new(transition_stack_top)),
                    ) {
                        if transition_data_identity != 0 {
                            let transition_data = unsafe {
                                // SAFETY: the identity data page is kernel-owned bootstrap
                                // memory and is mapped into the transition root before handoff.
                                &mut *(transition_data_identity as *mut BootstrapRuntimeState)
                            };
                            transition_data.magic = TRANSITION_DATA_MAGIC;
                            transition_data.active_root = bootstrap_page_table_frame;
                            transition_data.kernel_window_base =
                                transition_virtual_address.as_u64();
                            transition_data.kernel_window_end =
                                runtime_layout.kernel_window_end(kernel_image).as_u64();
                            transition_data.kernel_pages_mapped = transition_pages_mapped;
                            transition_data.identity_stack = transition_stack_top;
                            transition_data.alias_stack = transition_stack_alias_top;
                            transition_data.stack_pages = TRANSITION_STACK_PAGES;
                            transition_data.data_page = transition_data_identity;
                            transition_data.alias_entry =
                                transition_high_stack_entry_alias.map_or(0, |entry| entry.as_u64());
                            transition_data.alias_gdt =
                                transition_gdt_alias.map_or(0, |base| base.as_u64());
                            transition_data.alias_idt =
                                transition_idt_alias.map_or(0, |base| base.as_u64());
                            transition_data.stage = BootstrapRuntimeStage::Prepared.as_u64();
                        }
                        unsafe {
                            TRANSITION_BOOTSTRAP_CORE_ID = config.bootstrap_core.0;
                            TRANSITION_ALIAS_STACK_TOP = transition_stack_alias_top;
                            TRANSITION_ALIAS_ENTRY =
                                transition_high_stack_entry_alias.map_or(0, |entry| entry.as_u64());
                            TRANSITION_DATA_ALIAS = transition_data_alias_mapped;
                            TRANSITION_GDT_ALIAS =
                                transition_gdt_alias.map_or(0, |base| base.as_u64());
                            TRANSITION_IDT_ALIAS =
                                transition_idt_alias.map_or(0, |base| base.as_u64());
                            TRANSITION_HANDLER_DELTA = runtime_layout.handler_delta(kernel_image);
                        }
                        crate::kprintln!("stage: switching to transition root (identity handoff)");
                        unsafe {
                            // SAFETY: the transition root retains low identity mappings for
                            // the current kernel image, including the dedicated transition
                            // entry and current bootstrap stack.
                            arch::switch_page_table_root_and_jump(
                                bootstrap_page_table_frame,
                                stack.as_u64(),
                                entry.as_u64(),
                            )
                        }
                    } else {
                        crate::kprintln!("paging: transition_switch_error=missing_entry_or_stack");
                    }
                }
                Err(error) => crate::kprintln!("paging: transition_map_error={}", error),
            }
            log_memory_kind_summary(handoff);
            log_reservation_summary(&reservations);
        }
        None => crate::kprintln!("memory: no boot handoff present"),
    }
    crate::kprintln!("stage: early bootstrap complete");

    arch::halt_loop()
}

fn log_memory_kind_summary(handoff: BootHandoff<'_>) {
    let mut usable = 0usize;
    let mut kernel = 0usize;
    let mut reserved = 0usize;
    let mut mmio = 0usize;
    let mut reclaimable = 0usize;

    for region in handoff.memory_map() {
        match region.kind {
            MemoryRegionKind::Usable => usable += 1,
            MemoryRegionKind::Kernel => kernel += 1,
            MemoryRegionKind::Reserved => reserved += 1,
            MemoryRegionKind::Mmio => mmio += 1,
            MemoryRegionKind::BootloaderReclaimable => reclaimable += 1,
        }
    }

    crate::kprintln!(
        "memory: region_kinds usable={} kernel={} reserved={} mmio={} reclaimable={}",
        usable,
        kernel,
        reserved,
        mmio,
        reclaimable
    );
}

fn log_reservation_summary(reservations: &memory::EarlyKernelReservations) {
    crate::kprintln!(
        "memory: reservation_kinds legacy_low={} kernel_image={} active_root={} bootstrap_pt={} bootstrap_per_core={}",
        reservations.count_by_kind(memory::ReservationKind::LegacyLowMemory),
        reservations.count_by_kind(memory::ReservationKind::KernelImage),
        reservations.count_by_kind(memory::ReservationKind::ActivePageTableRoot),
        reservations.count_by_kind(memory::ReservationKind::BootstrapPageTables),
        reservations.count_by_kind(memory::ReservationKind::BootstrapPerCoreState),
    );
}
